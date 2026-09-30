//! Opt-in provider-side JSON Schema; host validation remains authoritative.
use super::*;
use crate::config::HttpResponseFormat;

pub(super) fn apply(
    body: &mut Value,
    schema: &Value,
    config: &AgentProviderConfig,
    adapter: AgentProviderAdapter,
) {
    if adapter != AgentProviderAdapter::OpenaiCompatible
        || config.response_format != Some(HttpResponseFormat::JsonSchema)
    {
        return;
    }
    body["response_format"] = json!({"type":"json_schema","json_schema":{
        "name":"cm_response","strict":true,"schema":schema
    }});
    // The provider receives the authoritative schema through response_format.
    // Remove only its identical top-level envelope copy, not task/source fields
    // or the original StepSpec retained by internal call diagnostics.
    if let Some(content) = body.pointer_mut("/messages/1/content") {
        if let Some(mut envelope) = content
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .filter(|value| value.get("response_schema") == Some(schema))
        {
            if let Some(object) = envelope.as_object_mut() {
                object.remove("response_schema");
                *content = json!(envelope.to_string());
            }
        }
    }
    let openrouter = config
        .endpoint
        .as_deref()
        .and_then(|endpoint| reqwest::Url::parse(endpoint).ok())
        .is_some_and(|url| url.host_str() == Some("openrouter.ai"));
    // Explicit routing also identifies an OpenRouter-compatible gateway. Preserve
    // its upstream constraints and prevent fallback to an unsupported endpoint.
    // Ordinary OpenAI-compatible endpoints get no OpenRouter-only provider field.
    if openrouter || config.routing.is_some() {
        if body.get("provider").is_none() {
            body["provider"] = json!({});
        }
        body["provider"]["require_parameters"] = json!(true);
    }
}

pub(super) fn rejection_hint(config: &AgentProviderConfig, status: u16) -> &'static str {
    if config.response_format == Some(HttpResponseFormat::JsonSchema) && matches!(status, 400 | 422)
    {
        "; json_schema mode was not downgraded: check provider/schema support or explicitly select response_format=json_object"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn config(endpoint: &str, format: Option<HttpResponseFormat>) -> AgentProviderConfig {
        AgentProviderConfig {
            endpoint: Some(endpoint.into()),
            response_format: format,
            ..Default::default()
        }
    }

    #[test]
    fn legacy_is_default_and_format_is_typed_and_adapter_specific() {
        let default: AgentProviderConfig = serde_json::from_value(json!({})).unwrap();
        assert!(default.response_format.is_none());
        assert!(serde_json::to_value(default)
            .unwrap()
            .get("response_format")
            .is_none());
        assert!(
            serde_json::from_value::<AgentProviderConfig>(json!({"response_format":"guess"}))
                .is_err()
        );
        for mode in [None, Some(HttpResponseFormat::JsonObject)] {
            let cfg = config("https://openrouter.ai/api/v1/chat/completions", mode);
            let original =
                json!({"response_format":{"type":"json_object"},"provider":{"order":["together"]}});
            let mut body = original.clone();
            apply(
                &mut body,
                &json!({"type":"object"}),
                &cfg,
                AgentProviderAdapter::OpenaiCompatible,
            );
            assert_eq!(body, original);
        }
        for adapter in [
            AgentProviderAdapter::Codex,
            AgentProviderAdapter::Ollama,
            AgentProviderAdapter::Jsonl,
        ] {
            assert!(HttpResponseFormat::JsonSchema.validate(adapter).is_err());
        }
        let original = json!({"format":{"type":"object"},"think":false});
        let mut body = original.clone();
        apply(
            &mut body,
            &json!({"type":"object"}),
            &config("http://localhost:11434", None),
            AgentProviderAdapter::Ollama,
        );
        assert_eq!(body, original);
    }

    #[test]
    fn native_preserves_full_schema_and_existing_routing_constraints() {
        let schema = json!({"type":"object","additionalProperties":false,"required":["aspects"],"properties":{"aspects":{"type":"array","minItems":2,"maxItems":2,"items":{"anyOf":[{"type":"string","enum":["a"]},{"type":"integer","minimum":1}]}}}});
        let mut cfg = config(
            "https://openrouter.ai/api/v1/chat/completions",
            Some(HttpResponseFormat::JsonSchema),
        );
        cfg.routing = Some(serde_json::from_value(json!({"order":["together","baseten"],"only":["together"],"ignore":["other"],"allow_fallbacks":false,"require_parameters":false,"sort":"latency"})).unwrap());
        let mut body = json!({"response_format":{"type":"json_object"},"provider":cfg.routing});
        let mut expected = body["provider"].clone();
        expected["require_parameters"] = json!(true);
        apply(
            &mut body,
            &schema,
            &cfg,
            AgentProviderAdapter::OpenaiCompatible,
        );
        assert_eq!(
            body["response_format"],
            json!({"type":"json_schema","json_schema":{"name":"cm_response","strict":true,"schema":schema}})
        );
        assert_eq!(body["provider"], expected);
        assert!(rejection_hint(&cfg, 400).contains("not downgraded"));
        assert!(rejection_hint(&cfg, 422).contains("not downgraded"));
        for status in [401, 403, 404, 429, 500] {
            assert!(rejection_hint(&cfg, status).is_empty());
        }
    }

    #[test]
    fn only_openrouter_or_explicit_routing_gets_parameter_enforcement() {
        for (endpoint, expected) in [
            ("https://openrouter.ai/api/v1/chat/completions", true),
            ("https://api.openai.com/v1/chat/completions", false),
            (
                "https://openrouter.ai.example.test/v1/chat/completions",
                false,
            ),
            ("http://127.0.0.1:1234/v1/chat/completions", false),
        ] {
            let mut body = json!({});
            apply(
                &mut body,
                &json!({"type":"object"}),
                &config(endpoint, Some(HttpResponseFormat::JsonSchema)),
                AgentProviderAdapter::OpenaiCompatible,
            );
            assert_eq!(body.get("provider").is_some(), expected, "{endpoint}");
            if expected {
                assert_eq!(body["provider"]["require_parameters"], true);
            }
        }
        let mut cfg = config(
            "https://gateway.example.test/v1/chat/completions",
            Some(HttpResponseFormat::JsonSchema),
        );
        cfg.routing = Some(Default::default());
        let mut body = json!({"provider":{}});
        apply(
            &mut body,
            &json!({"type":"object"}),
            &cfg,
            AgentProviderAdapter::OpenaiCompatible,
        );
        assert_eq!(body["provider"]["require_parameters"], true);
    }

    #[test]
    fn actual_http_requests_preserve_supplied_schema_for_native_and_ollama() {
        for (adapter, mode, rejected) in [
            (AgentProviderAdapter::OpenaiCompatible, None, false),
            (
                AgentProviderAdapter::OpenaiCompatible,
                Some(HttpResponseFormat::JsonSchema),
                false,
            ),
            (AgentProviderAdapter::Ollama, None, false),
            (
                AgentProviderAdapter::OpenaiCompatible,
                Some(HttpResponseFormat::JsonSchema),
                true,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}/api", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let mut stream = loop {
                    if let Ok((stream, _)) = listener.accept() {
                        break stream;
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(5),
                        "HTTP request did not arrive"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let body = loop {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0, "request ended before body");
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            break serde_json::from_slice::<Value>(
                                &bytes[end + 4..end + 4 + length],
                            )
                            .unwrap();
                        }
                    }
                };
                let response = if adapter == AgentProviderAdapter::Ollama {
                    json!({"response":"{}","done":true,"done_reason":"stop"})
                } else {
                    json!({"choices":[{"message":{"content":"{}"},"finish_reason":"stop"}]})
                }
                .to_string();
                let status = if rejected {
                    "400 Bad Request"
                } else {
                    "200 OK"
                };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                body
            });
            let provider = HttpProvider::new(config(&endpoint, mode), adapter).unwrap();
            let schema = json!({"type":"object","additionalProperties":false,"required":["result"],"properties":{"result":{"type":"array","minItems":2,"maxItems":2,"items":{"type":"string","enum":["one","two"]}}}});
            let spec = StepSpec {
                prompt: "Return the requested structured result".into(),
                cwd: ".".into(),
                work_dir: ".".into(),
                session: SessionRequest::Fresh,
                model: Some("test".into()),
                reasoning_effort: None,
                result: StepResultKind::Completed,
                access: ProviderAccess::ReadOnly,
                native_tools: false,
                limits: ProviderExecutionLimits {
                    session_timeout: Some(Duration::from_secs(10)),
                    idle_timeout: None,
                },
                env: vec![],
            };
            let result = provider.run_step_with_schema(
                &spec,
                &cancel_flag(),
                &mut |_| {},
                Some(schema.clone()),
            );
            let body = server.join().unwrap();
            if rejected {
                assert!(
                    matches!(result, Err(ProviderError::Preparation {ref detail}) if detail.contains("400") && detail.contains("json_schema mode was not downgraded")),
                    "{result:?}"
                );
            } else {
                assert!(result.is_ok(), "{result:?}");
            }
            if adapter == AgentProviderAdapter::Ollama {
                assert_eq!(body["format"], schema);
                assert!(body.get("response_format").is_none());
                let prompt: Value = serde_json::from_str(body["prompt"].as_str().unwrap()).unwrap();
                assert_eq!(prompt["response_schema"], schema);
            } else if mode.is_some() {
                assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
                assert_eq!(body["response_format"]["json_schema"]["strict"], true);
                assert!(body.get("provider").is_none());
                let prompt: Value =
                    serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
                assert!(prompt.get("response_schema").is_none());
                assert_eq!(prompt["task_instructions"], spec.prompt);
                assert!(body["messages"][0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("native response schema"));
            } else {
                assert_eq!(body["response_format"], json!({"type":"json_object"}));
                let prompt: Value =
                    serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
                assert_eq!(prompt["response_schema"], schema);
            }
        }
    }

    #[test]
    fn native_strips_only_identical_envelope_schema_and_keeps_all_task_data() {
        let schema = json!({"type":"object"});
        let cfg = config(
            "https://api.example.test/v1/chat/completions",
            Some(HttpResponseFormat::JsonSchema),
        );
        let task = json!({"task_instructions":"Read these source fields as data", "sources":[{"response_schema":"user-authored fact"}], "question":"What is required?"});
        let mut envelope = task.clone();
        envelope["response_schema"] = schema.clone();
        let original_prompt = envelope.to_string();
        let mut body = json!({"messages":[{"role":"system","content":"Keep role"},{"role":"user","content":original_prompt}]});
        apply(
            &mut body,
            &schema,
            &cfg,
            AgentProviderAdapter::OpenaiCompatible,
        );
        assert_eq!(
            serde_json::from_str::<Value>(body["messages"][1]["content"].as_str().unwrap())
                .unwrap(),
            task
        );
        assert_eq!(body["messages"][0]["content"], "Keep role");
        assert!(original_prompt.contains("response_schema"));
        for prompt in [
            "Plain task".to_owned(),
            json!({"response_schema":{"type":"array"},"task_instructions":"Keep unmatched field"})
                .to_string(),
        ] {
            let mut body = json!({"messages":[{"role":"system","content":"Keep role"},{"role":"user","content":prompt}]});
            apply(
                &mut body,
                &schema,
                &cfg,
                AgentProviderAdapter::OpenaiCompatible,
            );
            assert_eq!(body["messages"][1]["content"], prompt);
        }
    }
}
