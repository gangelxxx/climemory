use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output},
};
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    assert!(run(d.path(), &["init"]).status.success());
    fs::write(d.path().join("memory/config.json"),json!({"memory":{"mode":"read_only","documents_agent":"cheap","chat_agent":"cheap","verification_agent":null,"timeout_seconds":10},"agent":{"profiles":{"cheap":{"provider":"codex","model":"test-cheap","reasoning_effort":"low"}}}}).to_string()).unwrap();
    d
}
fn scenario(root: &Path, calls: Vec<Value>) {
    fs::write(
        root.join("scenario.json"),
        json!({"state_file":root.join("calls.json"),"calls":calls}).to_string(),
    )
    .unwrap();
}
fn command(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_cm"));
    c.current_dir(root)
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", root.join("scenario.json"));
    c
}
fn probe(root: &Path) -> Output {
    command(root).arg("-test_providers").output().unwrap()
}
fn text(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}
#[test]
fn piped_chat_stops_after_terminal_error() {
    let d = fixture();
    let mut child = command(d.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let input = format!("{}\nDo not process this second request\n", "x".repeat(8001));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.lines().any(|line| line == "ERROR"));
    assert!(!stderr.contains("RUNNING"));
    assert!(!d.path().join("calls.json").exists());
}

#[test]
fn public_cli_has_chat_init_help_and_experimental_session_import() {
    let d = fixture();
    let help = text(&run(d.path(), &["help"]));
    assert!(help.contains("cm init"));
    assert!(help.contains("cm ingest-session"));
    assert!(help.contains("cm -test_providers"));
    assert!(!help.contains("cm context"));
    for args in [
        vec!["context", "task"],
        vec!["ask", "settings", "task"],
        vec!["code", "grep", "text"],
        vec!["report", "session", "done"],
        vec!["--dir", "elsewhere"],
        vec!["init", "elsewhere"],
    ] {
        assert!(!run(d.path(), &args).status.success());
    }
    let instructions = fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
    assert!(instructions.contains("CM is a read-only memory chat"));
    assert!(instructions.contains("inspect stdout for status=partial"));
    assert!(!instructions.contains("cm report"));
    assert!(!d.path().join("calls.json").exists());
    let initialized = tempfile::tempdir().unwrap();
    assert!(run(initialized.path(), &["init"]).status.success());
    let config: Value =
        serde_json::from_slice(&fs::read(initialized.path().join("memory/config.json")).unwrap())
            .unwrap();
    assert!(config["memory"]["unified"].get("enabled").is_none());
    assert!(config["memory"].get("documents_prefilter").is_none());
    assert!(config["memory"]["timeouts"]
        .get("coordinator_seconds")
        .is_none());
}

#[test]
fn provider_probe_uses_profile_without_chat_or_memory_input() {
    let d = fixture();
    scenario(
        d.path(),
        vec![
            json!({"final_message":"{\"status\":\"ok\"}","expect_sandbox":"read-only","expect_output_schema":true,"expect_no_native_tools":true,"expect_prompt_contains":["Connectivity test"]}),
        ],
    );
    let out = command(d.path()).arg("-test_providers").output().unwrap();
    let value: Value = serde_json::from_str(&text(&out)).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["checks"][0]["profile"], "cheap");
    assert_eq!(value["checks"][0]["model"], "test-cheap");
    assert!(!d.path().join("memory/runtime/chat/state.json").exists());
}

#[test]
fn pretty_provider_probe_is_readable_when_redirected_and_preserves_exit_status() {
    let d = fixture();
    scenario(
        d.path(),
        vec![json!({"final_message":"{\"status\":\"ok\"}"})],
    );
    let out = command(d.path())
        .args(["-test_providers", "--pretty"])
        .output()
        .unwrap();
    let body = text(&out);
    assert!(body.contains("1. Configuration"));
    assert!(body.contains("[1/1] cheap"));
    assert!(body.contains("✓ OK"));
    assert!(body.contains("3. Summary: passed 1"));
    assert!(!body.contains('\r'));
    assert!(!body.contains('\u{1b}'));
    scenario(d.path(), vec![json!({"final_message":"wrong"})]);
    let out = command(d.path())
        .args(["-test_providers", "--pretty"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("✗ ERROR"));
    assert!(!run(d.path(), &["-test_providers", "--unknown"])
        .status
        .success());
    assert!(run(d.path(), &["help", "--pretty"]).status.success());
}

#[test]
fn provider_probe_continues_after_failure_and_reports_unbound_provider() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["agent"]["profiles"]["second"] = cfg["agent"]["profiles"]["cheap"].clone();
    cfg["agent"]["providers"] = json!({"unused":{"adapter":"codex"}});
    fs::write(&path, cfg.to_string()).unwrap();
    scenario(
        d.path(),
        vec![
            json!({"final_message":"wrong"}),
            json!({"final_message":"{\"status\":\"ok\"}"}),
        ],
    );
    let out = command(d.path()).arg("-test_providers").output().unwrap();
    assert!(!out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["checks"][0]["status"], "error");
    assert_eq!(value["checks"][1]["status"], "ok");
    assert_eq!(value["checks"][2]["status"], "not_tested");
}
fn statistics_enabled(root: &Path, enabled: bool) {
    let path = root.join("memory/config.json");
    let mut c: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    c["memory"]["statistics"] = json!({"enabled":enabled});
    fs::write(path, c.to_string()).unwrap();
}
fn statistics_reports(root: &Path) -> Vec<Value> {
    let dir = root.join("memory/runtime/statistics");
    if !dir.exists() {
        return vec![];
    }
    fs::read_dir(dir)
        .unwrap()
        .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect()
}
#[test]
fn statistics_default_off_and_failed_calls_keep_missing_tokens_unknown() {
    let d = fixture();
    let root = d.path();
    scenario(
        root,
        vec![
            json!({"final_message":"{\"status\":\"ok\"}"}),
            json!({"exit_code":21}),
        ],
    );
    text(&probe(root));
    assert!(statistics_reports(root).is_empty());
    statistics_enabled(root, true);
    let out = probe(root);
    assert!(!out.status.success());
    let reports = statistics_reports(root);
    let r = &reports[0];
    assert_eq!(r["status"], "error");
    assert_eq!(r["exchange"]["errors"], 0); // Probe failures are reported in its structured result.
    assert_eq!(r["totals"]["failed_calls"], 1);
    assert!(r["totals"]["tokens"]["input_tokens"]["reported"].is_null());
    assert_eq!(r["totals"]["tokens"]["input_tokens"]["missing_calls"], 1);
    assert!(r["calls"][0]["output_chars"].is_null());
}

#[test]
fn unavailable_statistics_storage_does_not_break_provider_probe() {
    let d = fixture();
    let root = d.path();
    statistics_enabled(root, true);
    fs::create_dir_all(root.join("memory/runtime")).unwrap();
    fs::write(root.join("memory/runtime/statistics"), "blocked test path").unwrap();
    scenario(root, vec![json!({"final_message":"{\"status\":\"ok\"}"})]);
    let out = probe(root);
    assert!(String::from_utf8_lossy(&out.stderr).contains("statistics unavailable"));
    assert!(text(&out).contains("ok"));
}

#[test]
fn detailed_http_logs_explain_connection_failures() {
    let d = fixture();
    let root = d.path();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    fs::write(root.join("memory/config.json"),json!({"memory":{"agent_logs":{"enabled":true},"chat_agent":"cheap","documents_agent":"cheap","verification_agent":null,"timeout_seconds":5},"agent":{"providers":{"local":{"adapter":"openai-compatible","endpoint":format!("http://{address}/v1/chat/completions")}},"profiles":{"cheap":{"provider":"local","model":"test"}}}}).to_string()).unwrap();
    let out = run(root, &["-test_providers"]);
    assert!(!out.status.success());
    let file = fs::read_dir(root.join("memory/runtime/agent-logs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let raw = fs::read_to_string(file).unwrap();
    let rows: Vec<Value> = raw
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let transport = rows
        .iter()
        .find(|r| r["event"] == "http_transport_error")
        .unwrap();
    assert_eq!(transport["data"]["connect"], true);
    assert!(transport["data"]["error"]
        .as_str()
        .unwrap()
        .contains("reqwest"));
    assert_eq!(
        rows.iter()
            .find(|r| r["event"] == "call_finished")
            .unwrap_or_else(|| panic!(
                "missing terminal: {} LOG {raw}",
                String::from_utf8_lossy(&out.stderr)
            ))["data"]["status"],
        "error"
    );
}

#[test]
fn detailed_logs_record_waiting_and_timeout_without_statistics() {
    let d = fixture();
    let root = d.path();
    let path = root.join("memory/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["memory"]["agent_logs"] = json!({"enabled":true});
    cfg["memory"]["timeout_seconds"] = json!(7);
    fs::write(&path, cfg.to_string()).unwrap();
    let mut delayed = json!({"final_message":"{\"status\":\"ok\"}"});
    delayed["delay_ms"] = json!(15000);
    scenario(root, vec![delayed]);
    let out = probe(root);
    assert!(!out.status.success());
    let file = fs::read_dir(root.join("memory/runtime/agent-logs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let raw = fs::read_to_string(file).unwrap();
    let rows: Vec<Value> = raw
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows[0]["event"], "call_started");
    assert!(rows.iter().any(|r| r["event"] == "call_waiting"));
    assert_eq!(
        rows.iter()
            .find(|r| r["event"] == "call_finished")
            .unwrap_or_else(|| panic!(
                "missing terminal: {} LOG {raw}",
                String::from_utf8_lossy(&out.stderr)
            ))["data"]["status"],
        "error"
    );
    assert!(rows.last().unwrap()["data"]["error"]
        .as_str()
        .unwrap()
        .contains("TimedOut"));
    assert!(!root.join("memory/runtime/statistics").exists());
}

#[test]
fn unavailable_detailed_logs_do_not_break_chat() {
    let d = fixture();
    let root = d.path();
    let path = root.join("memory/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["memory"]["agent_logs"] = json!({"enabled":true});
    fs::write(&path, cfg.to_string()).unwrap();
    fs::create_dir_all(root.join("memory/runtime")).unwrap();
    fs::write(root.join("memory/runtime/agent-logs"), "blocked test path").unwrap();
    scenario(root, vec![json!({"final_message":"{\"status\":\"ok\"}"})]);
    let out = probe(root);
    assert!(text(&out).contains("ok"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("agent logs unavailable"));
}

#[test]
fn statistics_capture_http_provider_usage() {
    http_usage_case(None, None, false, false, true, None);
    http_usage_case(Some(8192), Some(false), false, true, true, None);
    http_usage_case(Some(4096), Some(true), true, true, true, Some("low"));
    http_usage_case(None, None, false, true, false, Some("high"));
}

fn http_usage_case(
    max_tokens: Option<u32>,
    reasoning: Option<bool>,
    truncated: bool,
    logs: bool,
    statistics: bool,
    effort: Option<&'static str>,
) {
    use std::io::{BufRead, BufReader, Read};
    let d = fixture();
    let root = d.path();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let log_root = root.to_path_buf();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut length = 0;
        let mut authorized = false;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if line
                .trim()
                .eq_ignore_ascii_case("authorization: Bearer test-inline-credential")
            {
                authorized = true;
            }
            if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
                length = n.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        assert!(authorized);
        assert!(!String::from_utf8_lossy(&body).contains("test-inline-credential"));
        let body: Value = serde_json::from_slice(&body).unwrap();
        if logs {
            // The request and start must already be durable before a response.
            let files: Vec<_> = fs::read_dir(log_root.join("memory/runtime/agent-logs"))
                .unwrap()
                .collect();
            let recorded = fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
            assert!(recorded.contains("call_started"));
            assert!(recorded.contains("http_request"));
            assert!(!recorded.contains("call_finished"));
        }
        if reasoning == Some(false) {
            assert_eq!(
                body["provider"],
                json!({"order":["together","baseten"],"allow_fallbacks":false,"require_parameters":true})
            );
        } else {
            assert!(body.get("provider").is_none());
        }
        assert_eq!(body["max_tokens"], max_tokens.unwrap_or(2048));
        if let Some(enabled) = reasoning {
            assert_eq!(body["reasoning"]["enabled"], enabled);
        } else {
            assert!(body.get("reasoning").is_none() || effort.is_some());
        }
        assert_eq!(body["reasoning"]["effort"], json!(effort));
        let response=json!({"provider":"Together-test-inline-credential","id":"gen-test-test-inline-credential","model":"resolved-model-test-inline-credential","diagnostic_echo":"test-inline-credential", "choices":[{"message":{"content":json!({"status":"ok"}).to_string()},"finish_reason":if truncated {"length"} else {"stop"}}],"usage":{"prompt_tokens":321,"completion_tokens":17,"prompt_tokens_details":{"cached_tokens":111},"completion_tokens_details":{"reasoning_tokens":2}}}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).unwrap();
    });
    fs::write(root.join("memory/config.json"),json!({"memory":{"agent_logs":{"enabled":logs},"statistics":{"enabled":statistics},"chat_agent":"cheap","documents_agent":"cheap","verification_agent":null},"agent":{"providers":{"local":{"adapter":"openai-compatible","endpoint":format!("http://{address}/v1/chat/completions"),"api_key":"test-inline-credential","api_key_env":"CM_TEST_UNUSED_INLINE_CREDENTIAL_ENV","max_output_tokens":max_tokens,"reasoning_enabled":reasoning,"routing":if reasoning == Some(false) { json!({"order":["together","baseten"],"allow_fallbacks":false,"require_parameters":true}) } else { Value::Null }}},"profiles":{"cheap":{"provider":"local","model":"local-test","reasoning_effort":effort}}}}).to_string()).unwrap();
    let out = run(root, &["-test_providers"]);
    worker.join().unwrap();
    if truncated {
        assert!(!out.status.success());
        assert!(!out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stdout).contains("finish_reason=length"));
    } else {
        assert!(text(&out).contains("ok"));
    }
    if logs {
        let files: Vec<_> = fs::read_dir(root.join("memory/runtime/agent-logs"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1);
        let raw = fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
        assert!(!raw.contains("test-inline-credential"));
        let events: Vec<Value> = raw
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(events[0]["event"], "call_started");
        assert_eq!(events.last().unwrap()["event"], "call_finished");
        assert_eq!(
            events.last().unwrap()["data"]["status"],
            if truncated { "error" } else { "completed" }
        );
        let response = events
            .iter()
            .find(|e| e["event"] == "http_response")
            .unwrap();
        assert_eq!(response["data"]["status"], 200);
        assert!(response["data"]["request_to_headers_ms"].is_u64());
        assert!(response["data"]["body_ms"].is_u64());
        assert!(
            response["data"]["total_ms"].as_u64().unwrap()
                >= response["data"]["request_to_headers_ms"].as_u64().unwrap()
        );
        let headers = events
            .iter()
            .find(|e| e["event"] == "http_headers")
            .unwrap();
        assert!(headers["data"]["connection_ms"].is_null());

        assert_eq!(response["data"]["body"]["diagnostic_echo"], "[REDACTED]");
    } else {
        assert!(!root.join("memory/runtime/agent-logs").exists());
    }
    if !statistics {
        assert!(!root.join("memory/runtime/statistics").exists());
        return;
    }
    let reports = statistics_reports(root);
    assert!(!serde_json::to_string(&reports)
        .unwrap()
        .contains("test-inline-credential"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("test-inline-credential"));
    let r = &reports[0];
    assert_eq!(r["totals"]["failed_calls"], if truncated { 1 } else { 0 });
    assert_eq!(
        r["totals"]["tokens"]["reasoning_output_tokens"]["reported"],
        2
    );
    assert_eq!(r["calls"][0]["provider"], "local");
    assert_eq!(r["calls"][0]["upstream_provider"], "Together-[REDACTED]");
    assert_eq!(r["calls"][0]["generation_id"], "gen-test-[REDACTED]");
    assert_eq!(r["calls"][0]["response_model"], "resolved-model-[REDACTED]");
    assert_eq!(r["totals"]["tokens"]["input_tokens"]["reported"], 321);
    assert_eq!(
        r["totals"]["tokens"]["cached_input_tokens"]["reported"],
        111
    );
    assert_eq!(r["totals"]["tokens"]["output_tokens"]["reported"], 17);
}
#[test]
fn shared_http_retries_recover_and_respect_permanent_errors() {
    use std::io::{BufRead, BufReader, Read};
    for mode in ["503", "429", "body", "timeout", "403", "403_body"] {
        let d = fixture();
        let root = d.path();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let expected = if mode.starts_with("403") { 1 } else { 2 };
        let worker = std::thread::spawn(move || {
            for attempt in 0..expected {
                let start = std::time::Instant::now();
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                start.elapsed() < std::time::Duration::from_secs(15),
                                "missing attempt {mode}"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
                        length = n.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if attempt == 0 {
                    if mode == "timeout" {
                        std::thread::sleep(std::time::Duration::from_millis(1200));
                        continue;
                    }
                    if mode == "body" || mode == "403_body" {
                        write!(stream,"HTTP/1.1 {} Error\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{{",if mode=="body" {200}else{403}).unwrap();
                    } else {
                        write!(stream,"HTTP/1.1 {mode} Error\r\nContent-Length: 2\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{{}}").unwrap();
                    }
                } else {
                    let response=json!({"choices":[{"message":{"content":json!({"status":"ok"}).to_string()},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20}}).to_string();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.len(),
                        response
                    )
                    .unwrap();
                }
            }
        });
        fs::write(root.join("memory/config.json"),json!({"memory":{"agent_retries":{"backoff_ms":0,"attempt_timeout_seconds":1},"agent_logs":{"enabled":true},"statistics":{"enabled":true},"timeout_seconds":10,"chat_agent":"cheap","documents_agent":"cheap","verification_agent":null},"agent":{"providers":{"local":{"adapter":"openai-compatible","endpoint":format!("http://{address}/v1/chat/completions")}},"profiles":{"cheap":{"provider":"local","model":"test"}}}}).to_string()).unwrap();
        let out = run(root, &["-test_providers"]);
        worker.join().unwrap();
        assert_eq!(
            out.status.success(),
            expected == 2,
            "{mode}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let reports = statistics_reports(root);
        assert_eq!(reports[0]["calls"].as_array().unwrap().len(), expected);
        assert_eq!(reports[0]["call_counts"]["operation"], 1);
        assert_eq!(reports[0]["call_counts"]["retries"], expected - 1);
        assert_eq!(reports[0]["call_counts"]["total"], expected);
        assert_eq!(reports[0]["call_breakdown"]["operation"]["calls"], 1);
        assert_eq!(
            reports[0]["call_breakdown"]["retries"]["calls"],
            expected - 1
        );

        let file = fs::read_dir(root.join("memory/runtime/agent-logs"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let rows: Vec<Value> = fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "attempt_started")
                .count(),
            expected
        );
        let requests: Vec<_> = rows
            .iter()
            .filter(|r| r["event"] == "http_request")
            .collect();
        assert_eq!(requests.len(), expected);
        for (i, request) in requests.iter().enumerate() {
            assert_eq!(request["data"]["attempt"], i + 1);
        }
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "retry_scheduled")
                .count(),
            expected - 1
        );
    }
}

#[test]
fn cli_languages_apply_to_help_init_and_provider_view() {
    for (flag, title, loaded, init) in [
        (
            "-en",
            "Read-only memory chat",
            "Configuration loaded",
            "CM initialized",
        ),
        (
            "-ru",
            "Чат памяти",
            "Конфигурация загружена",
            "CM инициализирован",
        ),
        ("-zh", "只读记忆聊天", "配置已加载", "CM 已初始化"),
    ] {
        let d = tempfile::tempdir().unwrap();
        assert!(text(&run(d.path(), &["help", "-pretty", flag])).contains(title));
        assert!(text(&run(d.path(), &["-pretty", flag, "init"])).contains(init));
        let d = fixture();
        scenario(
            d.path(),
            vec![json!({"final_message":"{\"status\":\"ok\"}"})],
        );
        let out = command(d.path())
            .args(["-test_providers", "-pretty", flag])
            .output()
            .unwrap();
        let body = text(&out);
        assert!(body.contains(loaded));
        assert!(!body.contains('\r'));
    }
    let d = fixture();
    assert!(!run(d.path(), &["help", "-ru"]).status.success());
    assert!(!run(d.path(), &["help", "-pretty", "-ru", "-zh"])
        .status
        .success());
}

#[test]
fn unused_provider_is_incomplete_coverage_not_connection_failure() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    cfg["agent"]["providers"] = json!({"unused":{"adapter":"codex"}});
    fs::write(&path, cfg.to_string()).unwrap();
    scenario(
        d.path(),
        vec![json!({"final_message":"{\"status\":\"ok\"}"})],
    );
    let out = command(d.path()).arg("-test_providers").output().unwrap();
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["complete"], false);
    assert_eq!(value["checks"][1]["status"], "not_tested");
}
