mod common;
mod thread_agents_classifier;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use tempfile::TempDir;

struct Server {
    endpoint: String,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
    calls: Arc<AtomicUsize>,
}
impl Server {
    fn start(respond: impl Fn(Value) -> Value + Send + 'static) -> Self {
        Self::start_with_review(
            respond,
            |input| json!({"action":"remember","text":"Reviewed.","memory":input["memory_candidate"]}),
        )
    }
    fn start_with_review(
        respond: impl Fn(Value) -> Value + Send + 'static,
        review: impl Fn(Value) -> Value + Send + 'static,
    ) -> Self {
        Self::start_with_handlers(
            respond,
            review,
            |input| json!({"action":"documents","text":input["request"],"memory":null}),
        )
    }
    fn start_with_scope(
        respond: impl Fn(Value) -> Value + Send + 'static,
        scope: impl Fn(Value) -> Value + Send + 'static,
    ) -> Self {
        Self::start_with_handlers(
            respond,
            |input| json!({"action":"remember","text":"Reviewed.","memory":input["memory_candidate"]}),
            scope,
        )
    }
    fn start_with_handlers(
        respond: impl Fn(Value) -> Value + Send + 'static,
        review: impl Fn(Value) -> Value + Send + 'static,
        scope: impl Fn(Value) -> Value + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/api/generate", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = stop.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let join = thread::spawn(move || {
            while !worker.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                let (start, length) = loop {
                    let n = stream.read(&mut chunk).unwrap();
                    if n == 0 {
                        break (0, 0);
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let length = String::from_utf8_lossy(&bytes[..pos])
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap();
                        break (pos + 4, length);
                    }
                };
                if start == 0 {
                    continue;
                }
                while bytes.len() < start + length {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let request: Value = serde_json::from_slice(&bytes[start..start + length]).unwrap();
                if request.get("questions").is_some() {
                    count.fetch_add(1, Ordering::Relaxed);
                    let response = respond(request);
                    if let Some(ms) = response["test_delay_ms"].as_u64() {
                        thread::sleep(Duration::from_millis(ms));
                    }
                    let status = response["test_http_status"].as_u64().unwrap_or(200);
                    let body = response.to_string();
                    // A timeout fixture intentionally lets the client disconnect.
                    let _ = write!(stream, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    continue;
                }
                let prompt = request["prompt"]
                    .as_str()
                    .or_else(|| {
                        request["messages"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|m| m["role"] == "user")
                            .unwrap()["content"]
                            .as_str()
                    })
                    .unwrap();
                let mut input: Value = serde_json::from_str(prompt).unwrap();
                if request.get("format").is_some() {
                    assert_eq!(request["format"], input["response_schema"]);
                }
                input["test_request_model"] = request["model"].clone();
                input["test_context_size"] = request["options"]["num_ctx"].clone();
                input["test_prompt_bytes"] = json!(prompt.len());
                assert_eq!(input["protocol"], "climemory/thread-dialogue-1");
                count.fetch_add(1, Ordering::Relaxed);
                let selecting = input["phase"] == "document_selection";
                let mut answer = if input["phase"] == "document_scope" {
                    scope(input)
                } else if input["phase"] == "memory_review" {
                    review(input)
                } else {
                    respond(input)
                };
                if selecting {
                    if let Some(memory) = answer["memory"].as_object_mut() {
                        memory
                            .entry("reason")
                            .or_insert(json!("Test scope assessment"));
                    }
                }
                let finish = answer.as_object_mut().unwrap().remove("test_finish_reason");
                let delayed = answer.as_object_mut().unwrap().remove("test_delay_ms");
                if let Some(ms) = delayed.as_ref().and_then(Value::as_u64) {
                    thread::sleep(Duration::from_millis(ms));
                }
                let answer = answer.to_string();
                let body = if request.get("messages").is_some() {
                    json!({"choices":[{"message":{"content":answer},"finish_reason":finish}]})
                } else {
                    json!({"response":answer,"done_reason":finish})
                }
                .to_string();
                let written = write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body);
                if delayed.is_none() {
                    written.unwrap();
                }
            }
        });
        Self {
            endpoint,
            stop,
            join: Some(join),
            calls,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.join.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}
fn records(root: &Path, args: &[&str]) -> Vec<Value> {
    let result = common::run(root, args, "").success();
    let records: Vec<Value> = String::from_utf8_lossy(&result.get_output().stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let stderr = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(!stderr.contains("resume with thread agent"));
    let hint = stderr
        .lines()
        .find_map(|l| l.strip_prefix("cm: next_argv "));
    if stderr.contains("cm: thread agent session ") {
        assert!(hint.is_some(), "missing recovery arguments: {stderr}");
    }
    if let Some(hint) = hint {
        let next: Value = serde_json::from_str(hint).unwrap();
        let record = records
            .iter()
            .find(|r| r["record"] == "thread_dialogue")
            .unwrap();
        assert_eq!(next, record["next_argv"]);
        assert_eq!(next[2], "--dir");
        assert_eq!(
            Path::new(next[3].as_str().unwrap()).canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }
    records
}
fn agent(root: &Path, args: &[&str]) -> Value {
    let mut command = args.to_vec();
    if matches!(command[0], "get" | "status") {
        command[0] = "read";
    }
    records(root, &command)
        .into_iter()
        .find(|r| {
            r["record"] == "thread_agent"
                || r["record"] == "thread_dialogue"
                || r["record"] == "thread_agent_summary"
        })
        .unwrap()
}
fn fixture(server: &Server, adapter: &str) -> TempDir {
    let temp = tempfile::Builder::new()
        .prefix("cm dialogue ' ")
        .tempdir()
        .unwrap();
    // Thread dialogues must work with the default minimal memory setup.
    common::run_raw(&["init", temp.path().to_str().unwrap()], "").success();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("src/app.rs"),
        "fn main() {}\n// fixture boundary\n",
    )
    .unwrap();
    let path = temp.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = Value::Null;
    config["agent"]["providers"] = json!({"memory-test":{"adapter":adapter,"endpoint":server.endpoint,"default_model":"must-not-be-used"}});
    config["agent"]["profiles"]["agent_medium"] =
        json!({"provider":"memory-test","model":"fixture","reasoning_effort":null});
    fs::write(path, config.to_string()).unwrap();
    for slug in ["idea", "application", "settings"] {
        common::run(temp.path(), &["create", slug], "").success();
    }
    temp
}
fn bind(root: &Path, slug: &str, parent: Option<&str>) -> Value {
    let mut args = vec!["bind", slug, "--agent", "agent_medium"];
    if let Some(parent) = parent {
        args.extend(["--parent", parent]);
    }
    agent(root, &args)
}

fn document_answer(input: &Value) -> Option<Value> {
    if input["phase"] == "document_issue_scope" {
        let count = input["verified_requirements"]["issues"]
            .as_array()
            .unwrap()
            .len();
        return Some(
            json!({"action":"context","text":"","memory":{"issue_links":(1..=count).map(|id|json!({"id":id,"rule_ids":[]})).collect::<Vec<_>>()}}),
        );
    }
    if matches!(
        input["phase"].as_str(),
        Some("document_index" | "document_review")
    ) {
        assert_eq!(input["thread"]["slug"], "documents");
        let doc = &input["user_documents"][0];
        assert_eq!(doc["read_only"], true);
        if input["phase"] == "document_review" {
            let mut memory = input["document_review"]["previous_requirements"].clone();
            for doc in input["user_documents"].as_array().unwrap() {
                memory["rules"].as_array_mut().unwrap().push(json!({"rule":doc["text"].as_str().unwrap().split_whitespace().collect::<Vec<_>>().join(" "),"when":"","sources":[{"path":doc["path"],"start_line":doc["start_line"],"end_line":doc["end_line"]}]}));
            }
            return Some(json!({"action":"context","text":"","memory":memory}));
        }
        return Some(
            json!({"action":"context","memory":null,"text":format!("{} {}:{}",doc["text"].as_str().unwrap(),doc["path"].as_str().unwrap(),doc["start_line"])}),
        );
    }
    None
}

#[test]
fn dialogue_clarifies_reports_and_remembers_across_processes() {
    let server = Server::start(|input| {
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        assert!(input["user_documents"].as_array().unwrap().is_empty());
        assert!(input["document_requirements"]["text"]
            .as_str()
            .unwrap()
            .contains(if input["phase"] == "report" {
                "Blue Save"
            } else {
                "Explicit Save"
            }));
        match input["phase"].as_str().unwrap() {
            "report" => {
                assert!(input["report"].as_str().unwrap().contains("blue"));
                json!({"action":"remember","text":"Saved.","memory":{
                "why":"Persist edited values.","changes":"Blue Save button below general settings.",
                "constraints":"Explicit save only.","validation":"Saving tests pass."}})
            }
            _ if input["history"].as_array().unwrap().is_empty() => {
                json!({"action":"question","text":"General or project settings?"})
            }
            _ => {
                assert!(input["history"].to_string().contains("General settings"));
                json!({"action":"context","text":"General settings contain editable values. Add explicit saving below the fields."})
            }
        }
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let user_doc = root.join("memory/docs/ui.md");
    fs::write(&user_doc, "Explicit Save button.").unwrap();
    let binding = bind(root, "settings", None);
    let id = binding["thread_id"].as_str().unwrap();
    let source = common::thread_path(root, id);
    let original = fs::read(&source).unwrap();
    let first = agent(root, &["ask", "settings", "Add a Save button"]);
    assert_eq!(first["status"], "question");
    let session = first["session"].as_str().unwrap();
    assert_eq!(agent(root, &["pending"])["count"], 1);
    assert_eq!(
        agent(root, &["status", session])["question"],
        "General or project settings?"
    );
    let reply = agent(
        root,
        &[
            "reply",
            session,
            "General settings, to persist edited values",
        ],
    );
    assert_eq!(reply["status"], "awaiting_report");
    assert_eq!(reply["report_required"], true);
    let report = "Added a blue Save button below the fields; saving tests pass.";
    fs::write(&user_doc, "Blue Save below the fields.").unwrap();
    assert_eq!(
        agent(root, &["report", session, report])["status"],
        "complete"
    );
    assert_eq!(
        agent(root, &["report", session, report])["status"],
        "complete"
    );
    assert_eq!(server.calls.load(Ordering::Relaxed), 11);
    assert!(agent(root, &["get", "settings"])["memory"]
        .as_str()
        .unwrap()
        .contains("Source: Reported by primary model;"));
    assert_eq!(agent(root, &["pending"])["count"], 0);
    assert_eq!(fs::read(source).unwrap(), original);
    assert_eq!(
        fs::read_to_string(user_doc).unwrap(),
        "Blue Save below the fields."
    );
}

#[test]
fn parent_chain_clarifies_and_returns_to_owning_agent() {
    let server = Server::start(|input| {
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        assert!(input["document_requirements"]["text"]
            .as_str()
            .unwrap()
            .contains("Persist preferences locally."));
        let slug = input["thread"]["slug"].as_str().unwrap();
        let history = input["history"].as_array().unwrap();
        if slug == "idea" {
            if history.is_empty() {
                return json!({"action":"question","text":"Should settings persist across visits?"});
            }
            assert!(input["history"].to_string().contains("Yes"));
            return json!({"action":"context","text":"Persist across visits; the primary model confirmed this."});
        }
        if history.is_empty() {
            return json!({"action":"consult","text":"What persistence behavior is intended?"});
        }
        assert!(history.iter().any(|h| h["speaker"] == "parent_agent"));
        json!({"action":"context","text":"Use explicit saving and persist across visits."})
    });
    let temp = fixture(&server, "openai-compatible");
    let root = temp.path();
    fs::write(
        root.join("memory/docs/requirements.txt"),
        "Persist preferences locally.",
    )
    .unwrap();
    let idea = bind(root, "idea", None);
    bind(root, "application", Some("idea"));
    bind(root, "settings", Some("application"));
    common::run(root, &["bind", "idea", "--parent", "settings"], "").failure();
    let first = agent(root, &["ask", "settings", "Add Save"]);
    let session = first["session"].as_str().unwrap();
    assert_eq!(first["status"], "question");
    assert_eq!(first["speaking_thread"], idea["thread_id"]);
    assert_eq!(
        agent(root, &["reply", session, "Yes, across visits"])["status"],
        "awaiting_report"
    );
    assert_eq!(server.calls.load(Ordering::Relaxed), 17);
    assert_eq!(agent(root, &["get", "idea"])["memory"], "");
    assert_eq!(agent(root, &["cancel", session])["status"], "cancelled");
}

#[test]
fn oversized_memory_is_corrected_without_truncation_and_keeps_history() {
    let server = Server::start_with_review(
        |input| {
            if input["phase"] != "report" {
                return json!({"action":"context","text":"Ready."});
            }
            json!({"action":"remember","text":"Save note.","memory":{
            "why":"Persist settings.","changes":"x".repeat(1300),"constraints":"Storage may fail.","validation":"Two tests pass."}})
        },
        |input| {
            assert_eq!(input["memory_candidate"]["changes"], "x".repeat(1300));
            assert_eq!(
                input["memory_candidate_budget"]["field_chars"]["changes"],
                1300
            );
            assert!(
                input["memory_candidate_budget"]["excess_chars"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            json!({"action":"remember","text":"Saved","memory":{"why":"Persist settings.","changes":"Save button","constraints":"Storage may fail.","validation":"Two tests pass."}})
        },
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    bind(root, "settings", None);
    let d = agent(root, &["ask", "settings", "Add Save"]);
    let session = d["session"].as_str().unwrap();
    assert_eq!(
        agent(root, &["report", session, "Added Save and tested storage."])["status"],
        "complete"
    );
    assert_eq!(server.calls.load(Ordering::Relaxed), 3);
    let b = agent(root, &["get", "settings"]);
    let memory = b["memory"].as_str().unwrap();
    assert!(memory.chars().count() <= 1200);
    assert!(memory.contains("Changes: Save button"));
    assert!(memory.contains(&format!("thread-dialogues/{session}.json")));
    let saved =
        fs::read_to_string(root.join(format!("memory/agent-runs/thread-dialogues/{session}.json")))
            .unwrap();
    assert!(saved.contains(&"x".repeat(1300)));
    assert!(saved.contains("memory_review_candidate"));
}

#[test]
fn invalid_memory_correction_is_bounded_and_retry_preserves_old_memory() {
    let valid = Arc::new(AtomicBool::new(false));
    let flag = valid.clone();
    let server = Server::start(move |input| {
        if input["phase"] != "report" {
            return json!({"action":"context","text":"Ready."});
        }
        if !flag.load(Ordering::Relaxed) {
            return json!({"action":"remember","text":"Unstructured old format."});
        }
        assert!(input["history"].to_string().contains("memory_validation"));
        json!({"action":"remember","text":"Saved.","memory":{
            "why":"Save settings.","changes":"Added Save.","constraints":"None reported.","validation":"Tests pass."}})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let b = bind(root, "settings", None);
    let path = root.join(format!(
        "memory/thread-agents/{}.json",
        b["thread_id"].as_str().unwrap()
    ));
    let mut legacy: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    legacy["memory"] = json!("Old advisory memory. ".repeat(100));
    fs::write(&path, legacy.to_string()).unwrap();
    let before = fs::read(&path).unwrap();
    let d = agent(root, &["ask", "settings", "Add Save"]);
    let session = d["session"].as_str().unwrap();
    let failed = agent(root, &["report", session, "Added Save."]);
    assert_eq!(failed["status"], "error");
    assert!(failed["error"]
        .as_str()
        .unwrap()
        .contains("after correction"));
    assert_eq!(server.calls.load(Ordering::Relaxed), 3);
    assert_eq!(fs::read(&path).unwrap(), before);
    valid.store(true, Ordering::Relaxed);
    assert_eq!(agent(root, &["retry", session])["status"], "complete");
    assert!(
        agent(root, &["get", "settings"])["memory"]
            .as_str()
            .unwrap()
            .len()
            < 1200
    );
}

#[test]
fn premature_memory_is_rejected_and_retry_preserves_dialogue() {
    let valid = Arc::new(AtomicBool::new(false));
    let v = valid.clone();
    let server = Server::start(move |_| {
        if v.load(Ordering::Relaxed) {
            json!({"action":"context","text":"Ready to implement."})
        } else {
            json!({"action":"remember","text":"Invented implementation."})
        }
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    bind(root, "settings", None);
    let first = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(first["status"], "error");
    assert_eq!(agent(root, &["get", "settings"])["memory"], "");
    let session = first["session"].as_str().unwrap();
    valid.store(true, Ordering::Relaxed);
    assert_eq!(
        agent(root, &["retry", session])["status"],
        "awaiting_report"
    );
    common::run(root, &["reply", session, "Late answer"], "").failure();
    common::run(root, &["read", "../../config"], "").failure();
}

#[test]
fn pending_report_commit_resumes_without_duplicate_memory_update() {
    let server = Server::start(|input| {
        document_answer(&input)
            .unwrap_or_else(|| json!({"action":"context","text":"Settings context."}))
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/ui.md"), "Save is green.").unwrap();
    let binding = bind(root, "settings", None);
    let d = agent(root, &["ask", "settings", "Add Save"]);
    let session = d["session"].as_str().unwrap();
    let path = root.join(format!("memory/agent-runs/thread-dialogues/{session}.json"));
    let mut saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    saved["phase"] = json!("report");
    saved["report"] = json!("Added Save for general settings.");
    saved["status"] = json!("running");
    saved["pending_memory"] = json!("Reported: Save persists general settings.");
    saved["pending_revision"] = binding["revision"].clone();
    fs::write(&path, saved.to_string()).unwrap();
    // Simulate a crash after binding write but before the dialogue completion write.
    let binding_path = root.join(format!(
        "memory/thread-agents/{}.json",
        binding["thread_id"].as_str().unwrap()
    ));
    let mut b: Value = serde_json::from_slice(&fs::read(&binding_path).unwrap()).unwrap();
    b["revision"] = json!(b["revision"].as_u64().unwrap() + 1);
    b["memory"] = saved["pending_memory"].clone();
    b["last_dialogue"] = json!(session);
    fs::write(&binding_path, b.to_string()).unwrap();
    common::run(root, &["cancel", session], "").failure();
    let restored = agent(root, &["retry", session]);
    assert_eq!(restored["status"], "complete");
    assert!(d["document_requirements"].is_object());
    assert_eq!(
        restored["document_requirements"],
        d["document_requirements"]
    );
    assert_eq!(agent(root, &["get", "settings"])["revision"], b["revision"]);
    assert_eq!(server.calls.load(Ordering::Relaxed), 4);
}

#[test]
fn bounded_consultation_resumes_and_rebind_keeps_memory() {
    let server = Server::start(|input| {
        if input["thread"]["slug"] == "settings" && input["history"].as_array().unwrap().is_empty()
        {
            json!({"action":"consult","text":"Why explicit saving?"})
        } else {
            json!({"action":"context","text":"Explicit saving avoids persisting accidental edits."})
        }
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    bind(root, "idea", None);
    let binding = bind(root, "settings", Some("idea"));
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(1);
    fs::write(&config_path, config.to_string()).unwrap();
    let first = agent(root, &["ask", "settings", "Add Save"]);
    config["memory"]["max_steps"] = json!(16);
    fs::write(&config_path, config.to_string()).unwrap();
    assert_eq!(first["status"], "error");
    assert!(first["error"].as_str().unwrap().contains("budget"));
    let session = first["session"].as_str().unwrap();
    assert_eq!(
        agent(root, &["retry", session])["status"],
        "awaiting_report"
    );
    let rebound = bind(root, "settings", Some("none"));
    assert_eq!(rebound["thread_id"], binding["thread_id"]);
    assert_eq!(rebound["parent"], Value::Null);
    assert_eq!(
        bind(root, "settings", None)["revision"],
        rebound["revision"]
    );
    assert_eq!(server.calls.load(Ordering::Relaxed), 3);
}

#[test]
fn shared_profile_switches_provider_and_model_for_existing_dialogues_and_threads() {
    let first = Server::start(|input| {
        assert_eq!(input["test_request_model"], "fixture");
        json!({"action":"question","text":"General settings?"})
    });
    let second = Server::start(|input| {
        assert_eq!(input["test_request_model"], "new-model");
        json!({"action":"context","text":"Context from the replacement provider."})
    });
    let temp = fixture(&first, "ollama");
    let root = temp.path();
    let binding = bind(root, "settings", None);
    bind(root, "application", None);
    let binding_path = root.join(format!(
        "memory/thread-agents/{}.json",
        binding["thread_id"].as_str().unwrap()
    ));
    let before = fs::read(&binding_path).unwrap();
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "question");
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent"]["providers"]["replacement"] = json!({"adapter":"openai-compatible","endpoint":second.endpoint,"default_model":"unused-default"});
    config["agent"]["profiles"]["agent_medium"] =
        json!({"provider":"replacement","model":"new-model","reasoning_effort":null});
    fs::write(&path, config.to_string()).unwrap();
    assert_eq!(
        agent(
            root,
            &["reply", d["session"].as_str().unwrap(), "General settings"]
        )["status"],
        "awaiting_report"
    );
    assert_eq!(
        agent(root, &["ask", "application", "Read settings context"])["status"],
        "awaiting_report"
    );
    assert_eq!(agent(root, &["get", "settings"])["provider"], "replacement");
    assert_eq!(fs::read(binding_path).unwrap(), before);
    assert_eq!(first.calls.load(Ordering::Relaxed), 1);
    assert_eq!(second.calls.load(Ordering::Relaxed), 2);
}

#[test]
fn legacy_rebind_preserves_memory_parent_and_history_but_removes_model_settings() {
    let server = Server::start(|_| json!({"action":"context","text":"Ready."}));
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let parent = bind(root, "application", None);
    let binding = bind(root, "settings", Some("application"));
    let path = root.join(format!(
        "memory/thread-agents/{}.json",
        binding["thread_id"].as_str().unwrap()
    ));
    let mut legacy: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    legacy["format"] = json!("climemory/thread-agent-1");
    legacy.as_object_mut().unwrap().remove("agent");
    legacy["provider"] = json!("memory-test");
    legacy["model"] = json!("fixture");
    legacy["memory"] = json!("Reported: Save persists theme.");
    legacy["last_dialogue"] = json!("ta-0123456789abcdef0123456789abcdef");
    fs::write(&path, legacy.to_string()).unwrap();
    assert_eq!(agent(root, &["get", "settings"])["legacy_binding"], true);
    let mut without_model = legacy.clone();
    without_model.as_object_mut().unwrap().remove("model");
    fs::write(&path, without_model.to_string()).unwrap();
    let invalid = agent(root, &["get", "settings"]);
    assert_eq!(invalid["memory"], legacy["memory"]);
    assert!(invalid["configuration_error"]
        .as_str()
        .unwrap()
        .contains("explicit model"));
    assert_eq!(
        agent(root, &["ask", "settings", "Read memory"])["status"],
        "error"
    );
    assert_eq!(server.calls.load(Ordering::Relaxed), 0);
    fs::write(&path, legacy.to_string()).unwrap();
    let d = agent(root, &["ask", "settings", "Read memory"]);
    let session_path = root.join(format!(
        "memory/agent-runs/thread-dialogues/{}.json",
        d["session"].as_str().unwrap()
    ));
    let history = fs::read(&session_path).unwrap();
    common::run(root, &["bind", "settings"], "").failure();
    let migrated = bind(root, "settings", None);
    assert_eq!(migrated["memory"], legacy["memory"]);
    assert_eq!(migrated["parent"], parent["thread_id"]);
    assert_eq!(migrated["last_dialogue"], legacy["last_dialogue"]);
    assert_eq!(migrated["legacy_binding"], false);
    assert_eq!(fs::read(session_path).unwrap(), history);
    let stored: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(stored["format"], "climemory/thread-agent-2");
    assert_eq!(stored["agent"], "agent_medium");
    assert!(stored.get("provider").is_none() && stored.get("model").is_none());
}

#[test]
fn missing_profile_does_not_fallback_and_memory_remains_readable() {
    let server = Server::start(|_| panic!("deleted profile must not call a provider"));
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    bind(root, "settings", None);
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent"]["profiles"]
        .as_object_mut()
        .unwrap()
        .remove("agent_medium");
    fs::write(&path, config.to_string()).unwrap();
    let b = agent(root, &["get", "settings"]);
    assert_eq!(b["agent"], "agent_medium");
    assert!(b["configuration_error"]
        .as_str()
        .unwrap()
        .contains("missing"));
    let d = agent(root, &["ask", "settings", "Read settings"]);
    assert_eq!(d["status"], "error");
    assert!(d["error"].as_str().unwrap().contains("agent_medium"));
    assert_eq!(server.calls.load(Ordering::Relaxed), 0);
    for section in ["profiles", "agent"] {
        if section == "profiles" {
            config["agent"].as_object_mut().unwrap().remove(section);
        } else {
            config.as_object_mut().unwrap().remove(section);
        }
        fs::write(&path, config.to_string()).unwrap();
        let b = agent(root, &["get", "settings"]);
        assert!(b["configuration_error"]
            .as_str()
            .unwrap()
            .contains("missing"));
        let d = agent(root, &["ask", "settings", "Read settings"]);
        assert_eq!(d["status"], "error");
        assert!(d["error"].as_str().unwrap().contains("agent_medium"));
    }
    common::run(root, &["bind", "idea", "--agent", "does-not-exist"], "").failure();
    common::run(root, &["bind", "idea", "--provider", "codex"], "").failure();
}

#[test]
fn changed_profile_during_response_is_rejected_and_retry_uses_new_model() {
    let target = Arc::new(std::sync::Mutex::new(None::<std::path::PathBuf>));
    let changed_path = target.clone();
    let changed = Arc::new(AtomicBool::new(false));
    let once = changed.clone();
    let server = Server::start(move |input| {
        if !once.swap(true, Ordering::SeqCst) {
            let path = changed_path.lock().unwrap().clone().unwrap();
            let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            config["agent"]["profiles"]["agent_medium"]["model"] = json!("replacement-model");
            fs::write(path, config.to_string()).unwrap();
        } else {
            assert_eq!(input["test_request_model"], "replacement-model");
        }
        json!({"action":"context","text":"Response depends on current model configuration."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    *target.lock().unwrap() = Some(root.join("memory/config.json"));
    bind(root, "settings", None);
    let d = agent(root, &["ask", "settings", "Read settings"]);
    assert_eq!(d["status"], "error");
    assert!(d["error"].as_str().unwrap().contains("changed during turn"));
    assert_eq!(d["context"], Value::Null);
    assert_eq!(
        agent(root, &["retry", d["session"].as_str().unwrap()])["status"],
        "awaiting_report"
    );
}

#[test]
fn user_document_changes_invalidate_responses_and_unreadable_docs_prevent_calls() {
    let target = Arc::new(std::sync::Mutex::new(None::<std::path::PathBuf>));
    let changed_path = target.clone();
    let once = Arc::new(AtomicBool::new(false));
    let changed = once.clone();
    let server = Server::start(move |input| {
        if !changed.swap(true, Ordering::SeqCst) {
            assert_eq!(input["user_documents"][0]["text"], "Old requirement");
            fs::write(
                changed_path.lock().unwrap().as_ref().unwrap(),
                "New requirement",
            )
            .unwrap();
        } else if let Some(answer) = document_answer(&input) {
            assert_eq!(input["user_documents"][0]["text"], "New requirement");
            return answer;
        } else {
            assert!(input["document_requirements"]["text"]
                .as_str()
                .unwrap()
                .contains("New requirement"));
        }
        json!({"action":"context","text":"Use current requirements.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let path = root.join("memory/docs/requirements.md");
    *target.lock().unwrap() = Some(path.clone());
    fs::write(&path, "Old requirement").unwrap();
    let first = agent(root, &["ask", "settings", "Read requirements"]);
    assert_eq!(first["status"], "error");
    assert!(first["error"]
        .as_str()
        .unwrap()
        .contains("user documents changed"));
    assert_eq!(first["context"], Value::Null);
    assert_eq!(
        agent(root, &["retry", first["session"].as_str().unwrap()])["status"],
        "awaiting_report"
    );
    fs::write(&path, [0xff]).unwrap();
    let bad = agent(root, &["ask", "settings", "Read requirements again"]);
    assert_eq!(bad["status"], "error");
    assert!(bad["error"].as_str().unwrap().contains("UTF-8"));
    assert_eq!(server.calls.load(Ordering::Relaxed), 5);
    assert_eq!(
        records(root, &["context", "settings"])[0]["status"],
        "incomplete"
    );
    assert_eq!(fs::read(&path).unwrap(), [0xff]);
}

#[test]
fn large_ui_kit_is_read_completely_across_retries_and_only_rules_are_returned() {
    let chunks_seen = Arc::new(AtomicUsize::new(0));
    let seen = chunks_seen.clone();
    let server = Server::start(move |input| {
        assert!(
            input["test_context_size"].as_u64().unwrap()
                > input["test_prompt_bytes"].as_u64().unwrap()
        );
        if input["phase"] == "document_index" {
            let text = input["user_documents"][0]["text"].as_str().unwrap();
            return json!({"action":"context","memory":null,"text":if text.contains("Save buttons") { "Save button requirements and disabled exceptions." } else { "Typography." }});
        }
        if input["phase"] == "document_review" {
            let index = seen.fetch_add(1, Ordering::SeqCst) + 1;
            assert_eq!(input["document_review"]["chunk"], index);
            let chunk = input["user_documents"][0]["text"].as_str().unwrap();
            let mut memory = input["document_review"]["previous_requirements"].clone();
            for (needle, rule, when, line) in [
                (
                    "Save buttons must be green.",
                    "Save buttons must be green.",
                    "active",
                    json!(1),
                ),
                (
                    "Exception: disabled Save buttons must be grey.",
                    "Save buttons must be grey.",
                    "disabled",
                    input["user_documents"][0]["end_line"].clone(),
                ),
            ] {
                if chunk.contains(needle) {
                    memory["rules"].as_array_mut().unwrap().push(json!({"rule":rule,"when":when,"sources":[{"path":"memory/docs/ui-kit.md","start_line":line,"end_line":line}]}));
                }
            }
            return json!({"action":"context","text":"","memory":memory});
        }
        if input["phase"] == "document_selection" {
            let ids: Vec<_> = input["indexes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["id"].clone())
                .collect();
            return json!({"action":"context","text":"","memory":{"selected_chunks":ids,"reuse_previous":false}});
        }
        assert!(input["user_documents"].as_array().unwrap().is_empty());
        assert_eq!(
            input["document_requirements"]["coverage"],
            "selected_sections_only"
        );
        let requirements = input["document_requirements"]["text"].as_str().unwrap();
        assert!(requirements.contains("green"));
        assert!(requirements.contains("grey"));
        assert!(!requirements.contains("Irrelevant typography"));
        json!({"action":"context","text":requirements,"memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let source = format!(
        "Save buttons must be green.\n{}Exception: disabled Save buttons must be grey.\n",
        "Irrelevant typography description.\n".repeat(6000)
    );
    assert!(source.len() > 128_000);
    let path = root.join("memory/docs/ui-kit.md");
    fs::write(&path, &source).unwrap();
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(2);
    fs::write(config_path, config.to_string()).unwrap();
    let mut dialogue = agent(root, &["ask", "settings", "Add a Save button"]);
    let session = dialogue["session"].as_str().unwrap().to_owned();
    assert_eq!(dialogue["status"], "error");
    assert_eq!(dialogue["context"], Value::Null);
    for _ in 0..10 {
        if dialogue["status"] == "awaiting_report" {
            break;
        }
        assert!(dialogue["error"]
            .as_str()
            .unwrap()
            .contains("step budget exhausted"));
        dialogue = agent(root, &["retry", &session]);
    }
    assert_eq!(dialogue["status"], "awaiting_report");
    assert!(chunks_seen.load(Ordering::SeqCst) > 2);
    assert!(dialogue["context"].as_str().unwrap().len() < 300);
    assert_eq!(fs::read_to_string(path).unwrap(), source);
}

#[test]
fn truncated_document_extraction_does_not_advance_saved_reading_progress() {
    for adapter in ["ollama", "openai-compatible"] {
        let first_call = Arc::new(AtomicBool::new(true));
        let first = first_call.clone();
        let server = Server::start(move |input| {
            assert_eq!(input["phase"], "document_index");
            assert_eq!(input["user_documents"][0]["start_line"], 1);
            if first.swap(false, Ordering::SeqCst) {
                return json!({"action":"context","text":"Green buttons; missing exceptions.","memory":null,"test_finish_reason":"length"});
            }
            assert_eq!(input["document_request"], Value::Null);
            json!({"action":"context","text":"Green active buttons; disabled buttons are grey. memory/docs/ui.md:1","memory":null,"test_finish_reason":"stop"})
        });
        let temp = fixture(&server, adapter);
        let root = temp.path();
        fs::write(
            root.join("memory/docs/ui.md"),
            "Button rules.\n".repeat(3000),
        )
        .unwrap();
        let config_path = root.join("memory/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["memory"]["max_steps"] = json!(1);
        fs::write(config_path, config.to_string()).unwrap();
        let d = agent(root, &["ask", "settings", "Add Save"]);
        assert_eq!(d["status"], "error");
        let d = agent(root, &["retry", d["session"].as_str().unwrap()]);
        assert!(d["error"].as_str().unwrap().contains("incomplete"));
        assert_eq!(d["context"], Value::Null);
        let retried = agent(root, &["retry", d["session"].as_str().unwrap()]);
        assert!(retried["error"]
            .as_str()
            .unwrap()
            .contains("step budget exhausted"));
        assert_eq!(server.calls.load(Ordering::SeqCst), 3);
    }
}

#[test]
fn document_agent_shares_index_and_answers_across_threads_and_reindexes_changes() {
    let index_calls = Arc::new(AtomicUsize::new(0));
    let query_calls = Arc::new(AtomicUsize::new(0));
    let indexing = index_calls.clone();
    let querying = query_calls.clone();
    let server = Server::start(move |input| {
        if input["thread"]["slug"] == "documents" {
            assert_eq!(input["test_request_model"], "docs-model");
            if input["phase"] == "document_index" {
                indexing.fetch_add(1, Ordering::SeqCst);
                assert_eq!(input["document_request"], Value::Null);
            } else {
                querying.fetch_add(1, Ordering::SeqCst);
                assert_eq!(input["user_documents"][0]["path"], "memory/docs/ui.md");
            }
            return document_answer(&input).unwrap();
        }
        assert_eq!(input["test_request_model"], "fixture");
        assert!(input["user_documents"].as_array().unwrap().is_empty());
        json!({"action":"context","text":"No additional decisions.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["documents_agent"] = json!("agent_documents");
    config["agent"]["profiles"]["agent_documents"] =
        json!({"provider":"memory-test","model":"docs-model"});
    fs::write(config_path, config.to_string()).unwrap();
    let ui = root.join("memory/docs/ui.md");
    fs::write(
        &ui,
        "Save buttons are green; disabled Save buttons are grey.",
    )
    .unwrap();
    fs::write(
        root.join("memory/docs/storage.md"),
        "Offline persistence and database encryption.",
    )
    .unwrap();
    for slug in ["settings", "application"] {
        let answer = agent(root, &["ask", slug, "Add Save"]);
        assert_eq!(answer["status"], "awaiting_report");
        assert_eq!(answer["context"], "No additional decisions.");
        let mut saved = agent(root, &["read", answer["session"].as_str().unwrap()]);
        assert!(saved["document_requirements"]["structured_requirements"].is_object());
        saved["document_requirements"]
            .as_object_mut()
            .unwrap()
            .remove("structured_requirements");
        assert_eq!(
            saved["document_requirements"],
            answer["document_requirements"]
        );
        assert!(answer["document_requirements"]["text"]
            .as_str()
            .unwrap()
            .contains("green"));
    }
    assert_eq!(index_calls.load(Ordering::SeqCst), 2);
    assert_eq!(query_calls.load(Ordering::SeqCst), 1);
    fs::write(
        &ui,
        "Save buttons are blue; disabled Save buttons are grey.",
    )
    .unwrap();
    let changed = agent(root, &["ask", "idea", "Add Save"]);
    assert!(changed["document_requirements"]["text"]
        .as_str()
        .unwrap()
        .contains("blue"));
    assert_eq!(index_calls.load(Ordering::SeqCst), 3);
    assert_eq!(query_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        fs::read_to_string(&ui).unwrap(),
        "Save buttons are blue; disabled Save buttons are grey."
    );
}

#[test]
fn shared_document_selection_reads_global_and_linked_sources_without_unrelated_files() {
    let paths = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
    let selected = paths.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_index" {
            let mut answer = document_answer(&input).unwrap();
            if input["user_documents"][0]["path"] == "memory/docs/global.md" {
                answer["text"] = json!(format!("[GLOBAL] {}", answer["text"].as_str().unwrap()));
            }
            return answer;
        }
        if input["phase"] == "document_selection" {
            let ids: Vec<_> = input["indexes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["path"] != "memory/docs/typography.md")
                .map(|v| v["id"].clone())
                .collect();
            return json!({"action":"context","text":"","memory":{"selected_chunks":ids,"reuse_previous":false}});
        }
        if input["phase"] == "document_review" {
            let path = input["user_documents"][0]["path"].as_str().unwrap();
            selected.lock().unwrap().insert(path.to_owned());
            return document_answer(&input).unwrap();
        }
        json!({"action":"context","memory":null,"text":input["document_requirements"]["text"]})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    for (name, text) in [
        (
            "ui.md",
            "Save buttons use the brand color from memory/docs/palette.md.",
        ),
        (
            "palette.md",
            "Brand color is green; inactive color is grey.",
        ),
        (
            "global.md",
            "All interactive elements must support keyboard navigation.",
        ),
        ("typography.md", "Paragraph line spacing is 1.5."),
    ] {
        fs::write(root.join("memory/docs").join(name), text).unwrap();
    }
    let answer = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(answer["status"], "awaiting_report");
    assert_eq!(
        *paths.lock().unwrap(),
        std::collections::BTreeSet::from([
            "memory/docs/ui.md".to_string(),
            "memory/docs/global.md".to_string(),
            "memory/docs/palette.md".to_string()
        ])
    );
}

#[test]
fn changing_document_profile_during_owner_response_rejects_old_requirements() {
    let target = Arc::new(std::sync::Mutex::new(None::<std::path::PathBuf>));
    let config_target = target.clone();
    let once = Arc::new(AtomicBool::new(false));
    let changed = once.clone();
    let server = Server::start(move |input| {
        if let Some(mut answer) = document_answer(&input) {
            if input["phase"] == "document_review" {
                answer["memory"]["rules"][0]["rule"] = json!(format!(
                    "{} {}",
                    answer["memory"]["rules"][0]["rule"].as_str().unwrap(),
                    input["test_request_model"].as_str().unwrap()
                ));
            }
            return answer;
        }
        if !changed.swap(true, Ordering::SeqCst) {
            assert!(input["document_requirements"]["text"]
                .as_str()
                .unwrap()
                .contains("docs-old"));
            let path = config_target.lock().unwrap().clone().unwrap();
            let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            config["agent"]["profiles"]["agent_documents"]["model"] = json!("docs-new");
            fs::write(path, config.to_string()).unwrap();
        } else {
            assert!(input["document_requirements"]["text"]
                .as_str()
                .unwrap()
                .contains("docs-new"));
        }
        json!({"action":"context","text":"Current requirements.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let path = root.join("memory/config.json");
    *target.lock().unwrap() = Some(path.clone());
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["documents_agent"] = json!("agent_documents");
    config["agent"]["profiles"]["agent_documents"] =
        json!({"provider":"memory-test","model":"docs-old"});
    fs::write(path, config.to_string()).unwrap();
    fs::write(root.join("memory/docs/ui.md"), "Save buttons are green.").unwrap();
    let first = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(first["status"], "error");
    assert_eq!(first["context"], Value::Null);
    assert!(first["error"]
        .as_str()
        .unwrap()
        .contains("document agent profile"));
    let next = agent(root, &["retry", first["session"].as_str().unwrap()]);
    assert_eq!(next["status"], "awaiting_report");
}

#[test]
fn init_exposes_three_profiles_and_binding_defaults_to_medium() {
    let temp = TempDir::new().unwrap();
    common::init(temp.path());
    let config: Value =
        serde_json::from_slice(&fs::read(temp.path().join("memory/config.json")).unwrap()).unwrap();
    for effort in ["high", "medium", "low"] {
        let profile = &config["agent"]["profiles"][format!("agent_{effort}")];
        assert_eq!(profile["provider"], "codex");
        assert_eq!(profile["model"], "gpt-5.5");
        assert_eq!(profile["reasoning_effort"], effort);
    }
    common::run(temp.path(), &["create", "Application"], "").success();
    let binding = agent(temp.path(), &["bind", "application"]);
    assert_eq!(binding["agent"], "agent_medium");
    assert_eq!(binding["model"], "gpt-5.5");
    assert_eq!(binding["reasoning_effort"], "medium");
}

#[test]
fn oversized_rejected_memory_does_not_poison_the_correction_prompt() {
    let server = Server::start(|input| {
        if input["phase"] != "report" {
            return json!({"action":"context","text":"Ready."});
        }
        let history = input["history"].as_array().unwrap();
        if !history
            .iter()
            .any(|entry| entry["kind"] == "memory_validation")
        {
            return json!({"action":"remember","text":"x".repeat(200_000),"memory":{
                "why":"Persist values.","changes":"x".repeat(200_000),
                "constraints":"Explicit save.","validation":"Reported tests passed."}});
        }
        assert!(input.to_string().len() < 180_000);
        assert_eq!(input["report"], "Added Save; tests passed.");
        assert!(history
            .iter()
            .any(|entry| entry["candidate_omitted"] == true));
        json!({"action":"remember","text":"Saved.","memory":{
            "why":"Persist values.","changes":"Added Save.",
            "constraints":"Explicit save.","validation":"Reported tests passed."}})
    });
    let temp = fixture(&server, "ollama");
    bind(temp.path(), "settings", None);
    let dialogue = agent(temp.path(), &["ask", "settings", "Add Save"]);
    let session = dialogue["session"].as_str().unwrap();
    let result = agent(
        temp.path(),
        &["report", session, "Added Save; tests passed."],
    );
    assert_eq!(result["status"], "complete", "{result}");
    assert!(agent(temp.path(), &["get", "settings"])["memory"]
        .as_str()
        .unwrap()
        .contains("Added Save."));
    assert_eq!(server.calls.load(Ordering::Relaxed), 4);
}

#[test]
fn local_context_reads_memory_and_parent_without_provider_calls() {
    let server = Server::start(|_| panic!("context and read must stay local"));
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let parent = bind(root, "settings", None);
    let child = records(
        root,
        &[
            "create",
            "Binary saving",
            "--parent",
            "settings",
            "--note",
            "Binary format preserves edited values.",
        ],
    )[0]
    .clone();
    let packet = records(root, &["context", "Binary saving"]);
    assert_eq!(packet[0]["binary_profile"], "climemory-memory-v1");
    assert_eq!(packet[0]["status"], "scoped_ready");
    assert!(packet.iter().any(
        |r| r["thread"] == "binary-saving" && r["text"].as_str().unwrap().contains("preserves")
    ));
    assert!(packet[0]["thread_agents"]["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["thread_id"] == parent["thread_id"]));
    assert_eq!(child["parent"], parent["thread_id"]);
    let memory = agent(root, &["read", "binary-saving"]);
    assert!(memory["memory"].as_str().unwrap().contains("preserves"));
    assert_eq!(server.calls.load(Ordering::Relaxed), 0);
    let path = root.join(format!(
        "memory/thread-agents/{}.json",
        child["thread_id"].as_str().unwrap()
    ));
    fs::write(path, "invalid JSON").unwrap();
    let packet = records(root, &["context", "Binary saving"]);
    assert_eq!(packet[0]["status"], "incomplete");
    assert!(!packet[0]["gaps"].as_array().unwrap().is_empty());
}
#[test]
fn context_refreshes_after_report_and_pending_errors_are_disclosed() {
    let server = Server::start(|input| {
        if input["phase"] == "report" {
            json!({"action":"remember","text":"Saved","memory":{"why":"Persist preferences","changes":"Cerulean save button below settings","constraints":"Explicit action","validation":"Reported tests pass"}})
        } else {
            json!({"action":"context","text":"Use explicit saving"})
        }
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let dialogue = agent(root, &["ask", "settings", "Add Save"]);
    let session = dialogue["session"].as_str().unwrap();
    let packet = records(root, &["context", "settings"]);
    assert_eq!(packet[0]["thread_agents"]["pending"][0]["session"], session);
    agent(
        root,
        &["report", session, "Added a blue button; tests pass"],
    );
    let calls = server.calls.load(Ordering::Relaxed);
    let packet = records(root, &["context", "Cerulean"]);
    assert!(packet
        .iter()
        .any(|r| r["text"].as_str().is_some_and(|t| t.contains("Cerulean"))));
    let history = records(root, &["read", session]);
    assert!(history.iter().any(|r| r["record"] == "dialogue_event"));
    fs::write(
        root.join(format!("memory/agent-runs/thread-dialogues/{session}.json")),
        "invalid",
    )
    .unwrap();
    let packet = records(root, &["context", "settings"]);
    assert_eq!(packet[0]["status"], "incomplete");
    assert!(packet[0]["thread_agents"]["pending_error"].is_string());
    assert_eq!(server.calls.load(Ordering::Relaxed), calls);
}
#[test]
fn memory_budget_omits_whole_notes_and_exposes_read_handles() {
    let server = Server::start(|_| panic!("no provider expected"));
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    for n in 0..12 {
        records(
            root,
            &[
                "create",
                &format!("Preferences {n}"),
                "--note",
                &"Preferences saved safely. ".repeat(40),
            ],
        );
    }
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["budget_tokens"] = json!(2200);
    fs::write(path, config.to_string()).unwrap();
    let packet = records(root, &["context", "Preferences"]);
    assert!(packet[0]["omitted"].as_u64().unwrap() > 0);
    assert!(packet
        .iter()
        .skip(1)
        .all(|r| r["text"].as_str().unwrap().trim()
            == "Preferences saved safely. ".repeat(40).trim()));
    for binding in packet[0]["thread_agents"]["bindings"].as_array().unwrap() {
        assert!(agent(root, &["read", binding["slug"].as_str().unwrap()])["memory"].is_string());
    }
}

#[test]
fn invalid_document_citation_is_rejected_and_retry_reextracts() {
    let once = Arc::new(AtomicBool::new(true));
    let first = once.clone();
    let server = Server::start(move |input| {
        if let Some(mut answer) = document_answer(&input) {
            if input["phase"] == "document_review" && first.swap(false, Ordering::SeqCst) {
                answer["memory"]["rules"][0]["sources"][0]["end_line"] = json!(999);
            }
            return answer;
        }
        json!({"action":"context","text":"No additional decisions.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/ui.md"), "Save buttons are green.").unwrap();
    let failed = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(failed["status"], "error");
    assert!(failed["error"]
        .as_str()
        .unwrap()
        .contains("source was not read"));
    assert_eq!(failed["document_requirements"], Value::Null);
    let recovered = agent(root, &["retry", failed["session"].as_str().unwrap()]);
    assert_eq!(recovered["status"], "awaiting_report");
    assert!(recovered["document_requirements"]["text"]
        .as_str()
        .unwrap()
        .contains("green"));
    assert_eq!(server.calls.load(Ordering::SeqCst), 5);
}

#[test]
fn parent_document_findings_survive_retry_without_prose_repetition() {
    let server = Server::start(|input| {
        if input["phase"] == "document_index" {
            return document_answer(&input).unwrap();
        }
        if input["phase"] == "document_review" {
            let mut answer = document_answer(&input).unwrap();
            answer["memory"]["rules"][0]["rule"] = json!(if input["document_request"]["request"]
                == "Keyboard behavior?"
            {
                "Keyboard focus must be visible."
            } else {
                "Save is green."
            });
            return answer;
        }
        if input["thread"]["slug"] == "application" {
            return json!({"action":"context","text":"No additional decisions.","memory":null});
        }
        if input["history"].as_array().unwrap().is_empty() {
            return json!({"action":"consult","text":"Keyboard behavior?","memory":null});
        }
        assert!(input["document_requirements"]["consultations"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Keyboard focus"));
        json!({"action":"context","text":"No additional decisions.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(
        root.join("memory/docs/ui.md"),
        "Save is green. Keyboard focus must be visible.",
    )
    .unwrap();
    bind(root, "settings", Some("application"));
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(5);
    fs::write(path, config.to_string()).unwrap();
    let paused = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(paused["status"], "error");
    assert!(paused["error"]
        .as_str()
        .unwrap()
        .contains("step budget exhausted"));
    let done = agent(root, &["retry", paused["session"].as_str().unwrap()]);
    assert_eq!(done["status"], "awaiting_report");
    assert_eq!(done["context"], "No additional decisions.");
    assert!(done["document_requirements"]["text"]
        .as_str()
        .unwrap()
        .contains("Save is green"));
    assert!(done["document_requirements"]["consultations"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Keyboard focus"));
}

#[test]
fn presentation_corrections_survive_retry_and_cache_the_compact_answer() {
    let queries = Arc::new(AtomicUsize::new(0));
    let count = queries.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_index" {
            return document_answer(&input).unwrap();
        }
        if input["phase"] == "document_review" {
            count.fetch_add(1, Ordering::SeqCst);
            let text = if input.get("presentation_feedback").is_some() {
                assert!(input["presentation_feedback"]["candidate"]["rules"].is_array());
                "Save green when enabled; grey and inactive when disabled.".to_owned()
            } else {
                "Save green when enabled; grey and inactive when disabled. ".repeat(40)
            };
            return json!({"action":"context","text":"","memory":{"rules":[{"rule":text,"when":"","sources":[{"path":"memory/docs/ui.md","start_line":1,"end_line":1}]}],"issues":[]}});
        }
        json!({"action":"context","text":"","memory":{"decisions":[]}})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(
        root.join("memory/docs/ui.md"),
        "Save green when enabled; grey and inactive when disabled.",
    )
    .unwrap();
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(1);
    fs::write(config_path, config.to_string()).unwrap();
    let mut d = agent(root, &["ask", "settings", "Add Save"]);
    for _ in 0..4 {
        assert_eq!(d["status"], "error");
        d = agent(root, &["retry", d["session"].as_str().unwrap()]);
    }
    assert_eq!(d["status"], "awaiting_report");
    assert_eq!(d["context"], "No additional decisions.");
    assert!(d["document_requirements"]["text"].as_str().unwrap().len() < 150);
    let next = agent(root, &["ask", "application", "Add Save"]);
    let next = agent(root, &["retry", next["session"].as_str().unwrap()]);
    assert_eq!(next["status"], "awaiting_report");
    assert_eq!(next["document_requirements"], d["document_requirements"]);
    assert_eq!(queries.load(Ordering::SeqCst), 2);
}

#[test]
fn preparation_compresses_final_context_and_falls_back_on_bad_results() {
    for mode in ["valid", "omitted", "long", "truncated", "mixed"] {
        let server = Server::start(move |input| {
            if input["phase"] == "response_preparation" {
                assert_eq!(input["test_request_model"], "preparer");
                let ids: Vec<_> = input["sources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s["id"].clone())
                    .collect();
                assert!(input["sources"].to_string().contains("confirm persistence"));
                assert!(
                    input.get("task").is_none(),
                    "task facts must not leak into document prose"
                );
                let groups = |document: bool| {
                    input["sources"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|s| (s["kind"] == "document_requirement") == document)
                        .map(|s| s["id"].clone())
                        .collect::<Vec<_>>()
                };
                let mut out = json!({"action":"context","text":"","memory":{"items":[
                    {"text":"Save: green enabled, grey disabled; keyboard + focus.","covers":groups(true)},
                    {"text":"Confirm persistence.","covers":groups(false)}]}});
                if mode == "mixed" {
                    out["memory"]["items"] =
                        json!([{"text":"Green Save persists across visits.","covers":ids}]);
                }
                if mode == "omitted" {
                    out["memory"]["items"][0]["covers"] = json!([1]);
                }
                if mode == "long" {
                    out["memory"]["items"][0]["text"] = json!("Verbose context. ".repeat(300));
                }
                if mode == "truncated" {
                    out["test_finish_reason"] = json!("length");
                }
                return out;
            }
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            if input["phase"] == "report" {
                return json!({"action":"remember","text":"Saved","memory":{"why":"Test","changes":"No application edits","constraints":"Unresolved persistence","validation":"Reported checks"}});
            }
            json!({"action":"context","text":"The documents describe future button requirements, but confirm persistence before implementation; that decision remains unresolved. ".repeat(20),"memory":null})
        });
        let temp = fixture(&server, "ollama");
        let root = temp.path();
        fs::write(root.join("memory/docs/ui.md"), "Primary Save buttons must be green when enabled and grey when disabled. Keyboard operation and visible focus are required.").unwrap();
        let path = root.join("memory/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["memory"]["preparation_agent"] = json!("agent_preparation");
        config["agent"]["profiles"]["agent_preparation"] =
            json!({"provider":"memory-test","model":"preparer"});
        fs::write(path, config.to_string()).unwrap();
        let d = agent(root, &["ask", "settings", "Add Save"]);
        assert_eq!(d["status"], "awaiting_report", "{d}");
        assert_eq!(server.calls.load(Ordering::SeqCst), 5);
        if mode == "valid" {
            assert_eq!(d["preparation"]["status"], "prepared");
            assert_eq!(d["metrics"]["calls_by_phase"]["response_preparation"], 1);
            assert_eq!(d["metrics"]["instrumented_attempts"], d["steps"]);
            assert_eq!(d["document_requirements"], Value::Null);
            assert!(d["context"]
                .as_str()
                .unwrap()
                .contains("Confirm persistence"));
            let rows = records(root, &["read", d["session"].as_str().unwrap()]);
            let raw = rows
                .iter()
                .find(|r| r["record"] == "unprepared_context")
                .unwrap();
            assert!(raw["document_requirements"]["text"]
                .as_str()
                .unwrap()
                .contains("green"));
            let done = agent(
                root,
                &[
                    "report",
                    d["session"].as_str().unwrap(),
                    "No implementation; tested context delivery.",
                ],
            );
            assert_eq!(done["status"], "complete");
            assert!(done["preparation"].is_null());
            let saved: Value = serde_json::from_slice(
                &fs::read(root.join(format!(
                    "memory/agent-runs/thread-dialogues/{}.json",
                    d["session"].as_str().unwrap()
                )))
                .unwrap(),
            )
            .unwrap();
            assert!(saved["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["event"] == "preparation_input"
                    && e["sources"].to_string().contains("confirm persistence")));
        } else {
            assert_eq!(d["preparation"]["status"], "fallback", "{d}");
            assert!(d["document_requirements"]["text"].is_string());
            assert!(d["context"]
                .as_str()
                .unwrap()
                .contains("confirm persistence"));
        }
    }
}

#[test]
fn preparation_includes_primary_clarifications_and_parent_answers() {
    let server = Server::start(|input| {
        if input["phase"] == "response_preparation" {
            let sources = input["sources"].as_array().unwrap();
            assert!(sources.iter().any(|s| s["kind"] == "primary_clarification"
                && s["text"].as_str().unwrap().contains("General settings")));
            assert!(sources.iter().any(|s| s["kind"] == "parent_answer"
                && s["text"].as_str().unwrap().contains("browser storage")));
            return json!({"action":"context","text":"","memory":{"items":[{"text":"General settings: explicit Save to browser storage; persistence failure remains unresolved.","covers":sources.iter().map(|s| s["id"].clone()).collect::<Vec<_>>()}]}});
        }
        if input["phase"] == "consultation" {
            return json!({"action":"context","text":"Use browser storage for the settings; persistence failure behavior remains unresolved."});
        }
        let history = input["history"].as_array().unwrap();
        if history.is_empty() {
            return json!({"action":"question","text":"Which settings?"});
        }
        if !history.iter().any(|h| h["speaker"] == "parent_agent") {
            return json!({"action":"consult","text":"Where should settings persist?"});
        }
        json!({"action":"context","text":"Use explicit saving for the settings, with the persistence policy supplied by the parent. ".repeat(30)})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    bind(root, "settings", Some("application"));
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["preparation_agent"] = json!("agent_medium");
    fs::write(path, config.to_string()).unwrap();
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "question");
    assert!(d["preparation"].is_null());
    let d = agent(
        root,
        &[
            "reply",
            d["session"].as_str().unwrap(),
            "General settings only, excluding project settings.",
        ],
    );
    assert_eq!(d["preparation"]["status"], "prepared", "{d}");
    assert!(d["context"].as_str().unwrap().contains("unresolved"));
    assert_eq!(server.calls.load(Ordering::SeqCst), 5);
}

#[test]
fn preparation_without_step_budget_returns_original_context() {
    let server = Server::start(|input| {
        assert_ne!(input["phase"], "response_preparation");
        json!({"action":"context","text":"Original complete context."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["preparation_agent"] = json!("agent_medium");
    config["memory"]["max_steps"] = json!(1);
    fs::write(path, config.to_string()).unwrap();
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "awaiting_report");
    assert_eq!(d["preparation"]["status"], "fallback");
    assert_eq!(d["context"], "Original complete context.");
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn document_verifier_repairs_sources_marks_unsupported_and_shares_cache() {
    let calls = Arc::new(AtomicUsize::new(0));
    let verifying = calls.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_issue_scope" {
            return document_answer(&input).unwrap();
        }
        if input["phase"] == "document_index" {
            return document_answer(&input).unwrap();
        }
        if input["phase"] == "document_review" {
            return json!({"action":"context","text":"","memory":{"rules":[
                {"rule":"Disabled Save stays opaque.","when":"disabled","sources":[{"path":"memory/docs/ui.md","start_line":999,"end_line":999}]},
                {"rule":"Save must play music.","when":"","sources":[{"path":"memory/docs/ui.md","start_line":1,"end_line":1}]}],"issues":[]}});
        }
        if input["phase"] == "document_verification" {
            verifying.fetch_add(1, Ordering::SeqCst);
            assert!(["verifier", "verifier-v2"]
                .contains(&input["test_request_model"].as_str().unwrap()));
            assert_eq!(input["source_catalog"][2]["id"], "s3");
            assert_eq!(
                input["source_catalog"][2]["text"],
                "Disabled Save uses 100 percent opacity.\n"
            );
            return json!({"action":"context","text":"","memory":{"rules":[{"rule":"Save uses 100 percent opacity.","when":"disabled","source_ids":["s3"],"candidate_ids":[1]}],"issues":[]}});
        }
        let text = input["document_requirements"]["text"].as_str().unwrap();
        assert!(text.contains("[1:3]"));
        assert!(!text.contains("[1:1]"));
        assert!(text.contains("Unverified requirement (): Save must play music."));
        json!({"action":"context","text":"No additional decisions.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_verifier");
    config["agent"]["profiles"]["agent_verifier"] =
        json!({"provider":"memory-test","model":"verifier"});
    fs::write(path, config.to_string()).unwrap();
    let doc = root.join("memory/docs/ui.md");
    let original = "Catalog spacing.\nKeyboard support.\nDisabled Save uses 100 percent opacity.\n";
    fs::write(&doc, original).unwrap();
    for slug in ["settings", "application"] {
        let d = agent(root, &["ask", slug, "Style disabled Save"]);
        assert_eq!(d["status"], "awaiting_report", "{d}");
        assert_eq!(
            d["document_requirements"]["verification"]["unverified_rules"],
            1
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent"]["profiles"]["agent_verifier"]["model"] = json!("verifier-v2");
    fs::write(path, config.to_string()).unwrap();
    let changed = agent(root, &["ask", "idea", "Style disabled Save"]);
    assert_eq!(changed["status"], "awaiting_report");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(fs::read_to_string(doc).unwrap(), original);
}

#[test]
fn unknown_verification_source_id_does_not_advance_and_retry_repairs() {
    let first = Arc::new(AtomicBool::new(true));
    let invalid = first.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_verification" {
            return json!({"action":"context","text":"","memory":{"rules":[{"rule":"Save is green.","when":"","source_ids":[if invalid.swap(false,Ordering::SeqCst) {"invented"} else {"s1"}],"candidate_ids":[1]}],"issues":[]}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","text":"No additional decisions.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/ui.md"), "Save is green.").unwrap();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(path, config.to_string()).unwrap();
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "error");
    assert!(d["error"].as_str().unwrap().contains("unknown source ID"));
    assert!(d["context"].is_null());
    assert!(d["document_requirements"].is_null());
    let d = agent(root, &["retry", d["session"].as_str().unwrap()]);
    assert_eq!(d["status"], "awaiting_report");
    assert_eq!(server.calls.load(Ordering::SeqCst), 6);
}

#[test]
fn shared_clarifications_reach_parents_returning_owner_and_memory_review() {
    let server = Server::start_with_review(
        |input| {
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            let primary = input["primary_clarifications"].as_array().unwrap();
            if input["phase"] == "report" {
                assert_eq!(primary.len(), 2);
                // The report supersedes older parent document packets, even
                // though the source files themselves have not changed.
                assert!(input["document_requirements"]["consultations"].is_null());
                return json!({"action":"remember","text":"Draft","memory":{"why":"Audit Save","changes":"Reported Save-only persistence","constraints":"Persistence unresolved","validation":"Reported tests pass"}});
            }
            if input["phase"] == "consultation" {
                assert_eq!(primary[0]["question"], "Current or future?");
                assert!(primary[0]["answer"]
                    .as_str()
                    .unwrap()
                    .contains("Current behavior"));
                if primary.len() == 1 {
                    assert!(input["history"].as_array().unwrap().is_empty());
                    return json!({"action":"question","text":"Which storage key was observed?"});
                }
                assert_eq!(
                    primary[1]["answer"],
                    "Observed calc.general.theme in localStorage."
                );
                assert_eq!(primary[1]["question"], "Which storage key was observed?");
                return json!({"action":"context","text":"Save persists only on click."});
            }
            if primary.is_empty() {
                return json!({"action":"question","text":"Current or future?"});
            }
            if primary.len() == 1 {
                return json!({"action":"consult","text":"Confirm existing persistence policy."});
            }
            assert_eq!(primary.len(), 2); // Parent reply survives the popped frame.
            assert_eq!(
                input["document_requirements"]["consultations"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            json!({"action":"context","text":"No additional decisions."})
        },
        |input| {
            assert_eq!(input["primary_clarifications"].as_array().unwrap().len(), 2);
            assert_eq!(
                input["report"],
                "Verified Save-only persistence and passing tests."
            );
            assert_eq!(
                input["memory_candidate"]["constraints"],
                "Persistence unresolved"
            );
            let mut memory = input["memory_candidate"].clone();
            memory["constraints"] = json!(
                "Save-only persistence, observed key calc.general.theme; no open policy question."
            );
            json!({"action":"remember","text":"Reviewed","memory":memory})
        },
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(
        root.join("memory/docs/ui.md"),
        "Save must have a visible label.",
    )
    .unwrap();
    bind(root, "settings", Some("application"));
    let d = agent(root, &["ask", "settings", "Audit Save"]);
    let id = d["session"].as_str().unwrap();
    let d = agent(
        root,
        &["reply", id, "Current behavior, not future implementation."],
    );
    assert_eq!(d["question"], "Which storage key was observed?");
    let d = agent(
        root,
        &["reply", id, "Observed calc.general.theme in localStorage."],
    );
    assert_eq!(d["status"], "awaiting_report", "{d}");
    let d = agent(
        root,
        &[
            "report",
            id,
            "Verified Save-only persistence and passing tests.",
        ],
    );
    assert_eq!(d["status"], "complete", "{d}");
    let note = agent(root, &["read", "settings"]);
    assert!(!note["memory"]
        .as_str()
        .unwrap()
        .contains("Persistence unresolved"));
    assert!(note["memory"]
        .as_str()
        .unwrap()
        .contains("calc.general.theme"));
}

#[test]
fn verification_reassesses_issues_excludes_primary_facts_and_repairs_invalid_ranges() {
    let server = Server::start(|input| {
        if input["phase"] == "document_issue_scope" {
            return document_answer(&input).unwrap();
        }
        if input["phase"] == "document_index" {
            return document_answer(&input).unwrap();
        }
        if input["phase"] == "document_review" {
            let rules: Vec<_> = ["Enabled Save is green.","Save text is white.","Primary observed localStorage writes.","Save plays a chime."]
                .iter().map(|rule|json!({"rule":rule,"when":"","sources":[{"path":"memory/docs/ui.md","start_line":0,"end_line":0}]})).collect();
            return json!({"action":"context","text":"","memory":{"rules":rules,"issues":["Scope unresolved"]}});
        }
        if input["phase"] == "document_verification" {
            assert_eq!(
                input["candidates"][0]["claimed_sources"][0]["start_line"],
                0
            );
            assert_eq!(input["candidate_issues"][0], "Scope unresolved");
            return json!({"action":"context","text":"","memory":{"rules":[
                {"rule":"Save is green.","when":"enabled","source_ids":["s1"],"candidate_ids":[1]},
                {"rule":"Save text is white.","when":"","source_ids":["s2"],"candidate_ids":[2]}],"issues":[],"excluded_candidate_ids":[3]}});
        }
        let packet = &input["document_requirements"];
        let text = packet["text"].as_str().unwrap();
        assert!(!text.contains("Scope unresolved"));
        assert!(!text.contains("localStorage"));
        assert!(text.contains("- Save text is white. [1:2]"));
        assert_eq!(packet["verification"]["unverified_rules"], 1);
        assert_eq!(packet["verification"]["excluded_non_document_claims"], 1);
        json!({"action":"context","text":"No additional decisions."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(
        root.join("memory/docs/ui.md"),
        "Enabled Save is green.\nSave text is white.\n",
    )
    .unwrap();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(path, config.to_string()).unwrap();
    let d = agent(
        root,
        &[
            "ask",
            "settings",
            "Scope resolved: audit Save. Primary observed localStorage writes.",
        ],
    );
    assert_eq!(d["status"], "awaiting_report", "{d}");
}

#[test]
fn memory_review_survives_step_budget_without_persisting_unreviewed_note() {
    let server = Server::start_with_review(
        |input| {
            if input["phase"] == "report" {
                return json!({"action":"remember","text":"Draft","memory":{"why":"Audit","changes":"No edits","constraints":"Old unresolved policy","validation":"Reported tests"}});
            }
            json!({"action":"context","text":"Ready."})
        },
        |input| {
            assert_eq!(input["report"], "Policy resolved: Save only.");
            let mut memory = input["memory_candidate"].clone();
            memory["constraints"] = json!("Save only; policy resolved by primary.");
            json!({"action":"remember","text":"Reviewed","memory":memory})
        },
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(1);
    fs::write(path, config.to_string()).unwrap();
    let before = agent(root, &["read", "settings"]);
    let d = agent(root, &["ask", "settings", "Audit"]);
    let id = d["session"].as_str().unwrap();
    let d = agent(root, &["report", id, "Policy resolved: Save only."]);
    assert_eq!(d["status"], "error");
    assert_eq!(
        agent(root, &["read", "settings"])["memory"],
        before["memory"]
    );
    assert_eq!(agent(root, &["retry", id])["status"], "complete");
    let note = agent(root, &["read", "settings"]);
    assert!(note["memory"]
        .as_str()
        .unwrap()
        .contains("policy resolved by primary"));
    assert_eq!(server.calls.load(Ordering::SeqCst), 3);
}

#[test]
fn document_routing_reuses_verified_report_but_refreshes_scope_and_sources() {
    let queries = Arc::new(AtomicUsize::new(0));
    let seen = queries.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_selection" {
            let report = input["document_request"]["report"].as_str().unwrap_or("");
            let changed = report.contains("Also retrieve colors");
            let ids: Vec<_> = input["indexes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| {
                    v["path"]
                        == if changed {
                            "memory/docs/colors.md"
                        } else {
                            "memory/docs/font.md"
                        }
                })
                .map(|v| v["id"].clone())
                .collect();
            return json!({"action":"context","text":"","memory":{"selected_chunks":ids,
                "reuse_previous":!changed && !input["previous_verified"].is_null()}});
        }
        if input["phase"] == "document_review" {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        if input["phase"] == "document_verification" {
            return json!({"action":"context","text":"","memory":{"rules":[{
                "rule":input["source_catalog"][0]["text"].as_str().unwrap().trim(),"when":"",
                "source_ids":["s1"],"candidate_ids":[1]}],"issues":[],"excluded_candidate_ids":[]}});
        }
        if input["phase"] == "report" {
            let changed = input["report"]
                .as_str()
                .unwrap()
                .contains("Also retrieve colors");
            assert_eq!(
                input["document_requirements"]["reuse"]["status"] == "verified_packet_reused",
                !changed
            );
            assert!(input["document_requirements"]["text"]
                .as_str()
                .unwrap()
                .contains(if changed { "green" } else { "sans-serif" }));
            return json!({"action":"remember","text":"Saved.","memory":{"why":"Audit typography.","changes":"Reported tests pass.","constraints":"User references apply.","validation":"Reported tests."}});
        }
        json!({"action":"context","text":"No additional decisions.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(config_path, config.to_string()).unwrap();
    fs::write(root.join("memory/docs/font.md"), "Use system sans-serif.\n").unwrap();
    fs::write(root.join("memory/docs/colors.md"), "Save must be green.\n").unwrap();
    let first = agent(
        root,
        &[
            "ask",
            "settings",
            "Typography and font only; exclude colors and Save states",
        ],
    );
    assert_eq!(first["status"], "awaiting_report", "{first}");
    assert_eq!(first["document_requirements"]["selected_chunks"], 1);
    assert_eq!(queries.load(Ordering::SeqCst), 1);
    let report = agent(
        root,
        &[
            "report",
            first["session"].as_str().unwrap(),
            "Tests passed; implementation unchanged.",
        ],
    );
    assert_eq!(report["status"], "complete", "{report}");
    assert_eq!(queries.load(Ordering::SeqCst), 1);
    let second = agent(
        root,
        &[
            "ask",
            "application",
            "Typography and font only; exclude colors and Save states",
        ],
    );
    assert_eq!(second["status"], "awaiting_report", "{second}");
    assert_eq!(queries.load(Ordering::SeqCst), 1);
    let report = agent(
        root,
        &[
            "report",
            second["session"].as_str().unwrap(),
            "Also retrieve colors; scope changed.",
        ],
    );
    assert_eq!(report["status"], "complete", "{report}");
    assert_eq!(queries.load(Ordering::SeqCst), 2);
    fs::write(
        root.join("memory/docs/font.md"),
        "Use updated system sans-serif.\n",
    )
    .unwrap();
    let third = agent(
        root,
        &[
            "ask",
            "idea",
            "Typography and font only; exclude colors and Save states",
        ],
    );
    assert_eq!(third["status"], "awaiting_report", "{third}");
    assert!(third["document_requirements"]["text"]
        .as_str()
        .unwrap()
        .contains("updated"));
    assert_eq!(queries.load(Ordering::SeqCst), 3);
    agent(root, &["cancel", third["session"].as_str().unwrap()]);
}

#[test]
fn verifier_shares_existing_conflicts_between_chunks() {
    let server = Server::start(|input| {
        if input["phase"] == "document_selection" {
            return json!({"action":"context","text":"","memory":{"selected_chunks":[1,2],"reuse_previous":false}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        if input["phase"] == "document_verification" {
            let i = input["verification"]["chunk"].as_u64().unwrap();
            let issues = if i == 1 {
                json!(["Enabled Save color conflicts between sources."])
            } else {
                assert_eq!(
                    input["previous_issues"],
                    json!(["Enabled Save color conflicts between sources."])
                );
                json!([])
            };
            return json!({"action":"context","text":"","memory":{"rules":[{"rule":input["source_catalog"][0]["text"].as_str().unwrap().trim(),"when":"","source_ids":["s1"],"candidate_ids":[i]}],"issues":issues,"excluded_candidate_ids":[]}});
        }
        json!({"action":"context","text":"No additional decisions.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(path, config.to_string()).unwrap();
    // A full 256-entry source catalog prevents merging the next chunk.
    fs::write(
        root.join("memory/docs/a.md"),
        format!("Enabled Save is green.\n{}", "\n".repeat(255)),
    )
    .unwrap();
    fs::write(root.join("memory/docs/b.md"), "Enabled Save is purple.\n").unwrap();
    let result = agent(root, &["ask", "settings", "All Save requirements"]);
    assert_eq!(result["status"], "awaiting_report", "{result}");
    assert_eq!(
        result["document_requirements"]["text"]
            .as_str()
            .unwrap()
            .matches("Unresolved:")
            .count(),
        1
    );
    agent(root, &["cancel", result["session"].as_str().unwrap()]);
}

#[test]
fn invalid_document_selection_is_not_cached_and_retry_can_repair_it() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = attempts.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_selection" {
            let first = seen.fetch_add(1, Ordering::SeqCst) == 0;
            return json!({"action":"context","text":"","memory":{"selected_chunks":if first {vec![0,99]} else {vec![1,2]},"reuse_previous":false}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","text":"No additional decisions.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/a.md"), "Save is green.\n").unwrap();
    fs::write(root.join("memory/docs/b.md"), "Save supports keyboard.\n").unwrap();
    let first = agent(root, &["ask", "settings", "All Save requirements"]);
    assert_eq!(first["status"], "error");
    assert!(first["error"]
        .as_str()
        .unwrap()
        .contains("invalid document selection"));
    assert_eq!(first["context"], Value::Null);
    let repaired = agent(root, &["retry", first["session"].as_str().unwrap()]);
    assert_eq!(repaired["status"], "awaiting_report", "{repaired}");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    agent(root, &["cancel", repaired["session"].as_str().unwrap()]);
}

#[test]
fn scope_questions_precede_document_reads_and_ready_decisions_are_cached() {
    let queries = Arc::new(AtomicUsize::new(0));
    let count = queries.clone();
    let server = Server::start_with_scope(
        move |input| {
            if input["phase"] == "document_review" {
                count.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            json!({"action":"context","text":"","memory":{"decisions":[]}})
        },
        |input| {
            assert_eq!(input["user_documents"], json!([]));
            if input["task"] == "Unclear save"
                && input["primary_clarifications"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            {
                json!({"action":"question","text":"General settings?","memory":null})
            } else {
                json!({"action":"documents","text":"","memory":null})
            }
        },
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/ui.md"), "Save is green.").unwrap();
    let first = agent(root, &["ask", "settings", "Unclear save"]);
    assert_eq!(first["status"], "question");
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    assert_eq!(queries.load(Ordering::SeqCst), 0);
    let done = agent(
        root,
        &[
            "reply",
            first["session"].as_str().unwrap(),
            "General settings",
        ],
    );
    assert_eq!(done["status"], "awaiting_report", "{done}");
    let a = agent(root, &["ask", "settings", "Clear save"]);
    assert_eq!(a["status"], "awaiting_report");
    let before = server.calls.load(Ordering::SeqCst);
    let b = agent(root, &["ask", "settings", "Clear save"]);
    assert_eq!(b["status"], "awaiting_report");
    assert_eq!(server.calls.load(Ordering::SeqCst), before + 1);
}

#[test]
fn parent_scope_consultation_shares_one_document_packet() {
    let queries = Arc::new(AtomicUsize::new(0));
    let count = queries.clone();
    let server = Server::start_with_scope(
        move |input| {
            if input["phase"] == "document_review" {
                count.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            json!({"action":"context","text":"","memory":{"decisions":[]}})
        },
        |input| {
            if input["thread"]["slug"] == "settings"
                && input["history"].as_array().unwrap().is_empty()
            {
                json!({"action":"consult","text":"Explain application save policy.","memory":null})
            } else {
                json!({"action":"documents","text":"","memory":null})
            }
        },
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/ui.md"), "Save is green.").unwrap();
    bind(root, "settings", Some("application"));
    let done = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(done["status"], "awaiting_report", "{done}");
    assert_eq!(queries.load(Ordering::SeqCst), 1);
    assert!(done["document_requirements"]["consultations"].is_null());
    let saved: Value = serde_json::from_slice(
        &fs::read(root.join(format!(
            "memory/agent-runs/thread-dialogues/{}.json",
            done["session"].as_str().unwrap()
        )))
        .unwrap(),
    )
    .unwrap();
    let calls: Vec<_> = saved["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "model_call")
        .collect();
    assert!(calls
        .iter()
        .all(|e| e["elapsed_ms"].is_number() && e["status"] == "completed"));
}

#[test]
fn returned_parent_knowledge_refreshes_document_scope() {
    let server = Server::start_with_scope(
        |input| {
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            if input["thread"]["slug"] == "application" {
                return json!({"action":"context","text":"Check keyboard behavior too.","memory":null});
            }
            if !input["history"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["speaker"] == "parent_agent")
            {
                return json!({"action":"consult","text":"Any extra requirements?","memory":null});
            }
            assert_eq!(input["document_query"], "Keyboard behavior?");
            json!({"action":"context","text":"Ready.","memory":null})
        },
        |input| {
            let has_parent = input["history"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["speaker"] == "parent_agent");
            json!({"action":"documents","text":if has_parent {"Keyboard behavior?"} else {""},"memory":null})
        },
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(
        root.join("memory/docs/ui.md"),
        "Save green. Keyboard focus visible.",
    )
    .unwrap();
    bind(root, "settings", Some("application"));
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "awaiting_report", "{d}");
}

#[test]
fn report_parent_documents_use_the_consultation_question() {
    let server = Server::start(|input| {
        if input["thread"]["slug"] == "application" {
            assert_eq!(input["document_query"], "Keyboard behavior?");
            return json!({"action":"context","text":"Keyboard checked.","memory":null});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        if input["phase"] == "report" {
            if !input["history"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["speaker"] == "parent_agent")
            {
                return json!({"action":"consult","text":"Keyboard behavior?","memory":null});
            }
            return json!({"action":"remember","text":"Saved.","memory":{"why":"Save","changes":"Tested","constraints":"Keyboard support","validation":"Test passed"}});
        }
        json!({"action":"context","text":"Ready.","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(
        root.join("memory/docs/ui.md"),
        "Save green. Keyboard focus visible.",
    )
    .unwrap();
    bind(root, "settings", Some("application"));
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "awaiting_report");
    let done = agent(
        root,
        &[
            "report",
            d["session"].as_str().unwrap(),
            "Check keyboard behavior before recording outcome.",
        ],
    );
    assert_eq!(done["status"], "complete", "{done}");
}

#[test]
fn parent_document_hint_skips_returning_scope_and_exposes_cache_metrics() {
    let scope_calls = Arc::new(AtomicUsize::new(0));
    let scopes = scope_calls.clone();
    let server = Server::start_with_scope(
        |input| {
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            assert!(input["memory_budget"]["fields_chars"].as_u64().unwrap() < 1200);
            json!({"action":"context","text":"","memory":{"decisions":[],"document_query":""}})
        },
        move |input| {
            scopes.fetch_add(1, Ordering::SeqCst);
            if input["thread"]["slug"] == "settings" {
                json!({"action":"consult","text":"Application policy?","memory":null})
            } else {
                json!({"action":"documents","text":"","memory":null})
            }
        },
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/ui.md"), "Save is green.").unwrap();
    bind(root, "settings", Some("application"));
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "awaiting_report", "{d}");
    assert_eq!(scope_calls.load(Ordering::SeqCst), 2);
    assert_eq!(d["metrics"]["cache_reuses"], 1);
    assert_eq!(d["metrics"]["instrumented_attempts"], d["steps"]);
}

#[test]
fn new_primary_answer_invalidates_a_parent_document_hint() {
    let scopes = Arc::new(AtomicUsize::new(0));
    let count = scopes.clone();
    let server = Server::start_with_scope(
        |input| {
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            if input["thread"]["slug"] == "settings"
                && input["primary_clarifications"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            {
                return json!({"action":"question","text":"Include keyboard?","memory":null});
            }
            json!({"action":"context","text":"","memory":{"decisions":[],"document_query":""}})
        },
        move |input| {
            count.fetch_add(1, Ordering::SeqCst);
            if input["thread"]["slug"] == "settings"
                && input["history"].as_array().unwrap().is_empty()
            {
                json!({"action":"consult","text":"Application policy?","memory":null})
            } else {
                json!({"action":"documents","text":"","memory":null})
            }
        },
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(
        root.join("memory/docs/ui.md"),
        "Save green; keyboard focus visible.",
    )
    .unwrap();
    bind(root, "settings", Some("application"));
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "question");
    assert_eq!(scopes.load(Ordering::SeqCst), 2);
    let done = agent(
        root,
        &["reply", d["session"].as_str().unwrap(), "Include keyboard"],
    );
    assert_eq!(done["status"], "awaiting_report", "{done}");
    assert_eq!(scopes.load(Ordering::SeqCst), 3);
}

#[test]
fn report_rechecks_packets_with_unresolved_issues() {
    for resolves in [false, true] {
        check_issue_report(resolves);
    }
}
fn check_issue_report(resolves: bool) {
    let verifies = Arc::new(AtomicUsize::new(0));
    let count = verifies.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_selection" {
            return json!({"action":"context","text":"","memory":{"selected_chunks":[1],"reuse_previous":!resolves,"rule_ids":[],"reason":"Assess whether report resolves policy"}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        if input["phase"] == "document_verification" {
            let n = count.fetch_add(1, Ordering::SeqCst);
            return json!({"action":"context","text":"","memory":{"rules":[{"rule":"Save green","when":"","source_ids":["s1"],"candidate_ids":[1]}],"issues":if n==0 {vec!["Missing policy"]} else {vec![]},"excluded_candidate_ids":[],"issue_updates":[]}});
        }
        if input["phase"] == "report" {
            assert_eq!(
                input["document_requirements"]["unresolved_issues"],
                if resolves {
                    json!([])
                } else {
                    json!(["Missing policy"])
                }
            );
            return json!({"action":"remember","text":"Saved","memory":{"why":"Audit","changes":"No code changes","constraints":if resolves {"Policy decided"} else {"Policy unresolved"},"validation":"Checked"}});
        }
        json!({"action":"context","text":"Ready","memory":null})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let p = root.join("memory/config.json");
    let mut c: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    c["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(p, c.to_string()).unwrap();
    fs::write(root.join("memory/docs/ui.md"), "Save green.").unwrap();
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "awaiting_report");
    let done = agent(
        root,
        &[
            "report",
            d["session"].as_str().unwrap(),
            if resolves {
                "Policy is decided; report outcome"
            } else {
                "No code or documentary changes; policy remains unresolved"
            },
        ],
    );
    assert_eq!(done["status"], "complete", "{done}");
    assert_eq!(
        verifies.load(Ordering::SeqCst),
        if resolves { 2 } else { 1 }
    );
}

#[test]
fn short_context_skips_preparation_without_provider_call() {
    let server = Server::start(|input| {
        assert_ne!(input["phase"], "response_preparation");
        json!({"action":"context","text":"Save locally."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["preparation_agent"] = json!("agent_medium");
    fs::write(path, config.to_string()).unwrap();
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "awaiting_report");
    assert_eq!(d["preparation"]["status"], "skipped");
    assert_eq!(d["context"], "Save locally.");
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn parent_packet_reuse_requires_scope_assessment_and_keeps_additional_queries() {
    for reuse in [true, false] {
        let server = Server::start_with_scope(
            move |input| {
                if input["phase"] == "document_selection" {
                    assert_eq!(input["previous_request"]["request"], "Parent button rules");
                    assert_eq!(input["document_request"]["request"], "Add Save");
                    return json!({"action":"context","text":"","memory":{"selected_chunks":[1],"reuse_previous":reuse,"reason":"Compared scope"}});
                }
                if let Some(answer) = document_answer(&input) {
                    return answer;
                }
                if input["phase"] == "document_verification" {
                    return json!({"action":"context","text":"","memory":{"rules":[{"rule":"Save is green.","when":"","source_ids":["s1"],"candidate_ids":[1]}],"issues":[],"excluded_candidate_ids":[]}});
                }
                json!({"action":"context","text":"","memory":{"decisions":[],"document_query":""}})
            },
            |input| {
                if input["thread"]["slug"] != "idea" {
                    json!({"action":"consult","text":"Parent knowledge?","memory":null})
                } else {
                    json!({"action":"documents","text":"Parent button rules","memory":null})
                }
            },
        );
        let temp = fixture(&server, "ollama");
        let root = temp.path();
        bind(root, "settings", Some("application"));
        bind(root, "application", Some("idea"));
        fs::write(root.join("memory/docs/ui.md"), "Save is green.").unwrap();
        let path = root.join("memory/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["memory"]["verification_agent"] = json!("agent_medium");
        fs::write(path, config.to_string()).unwrap();
        let d = agent(root, &["ask", "settings", "Add Save"]);
        assert_eq!(d["status"], "awaiting_report", "{d}");
        assert_eq!(d["metrics"]["calls_by_phase"]["document_selection"], 1);
        assert_eq!(
            d["metrics"]["calls_by_phase"]["document_review"],
            if reuse { 1 } else { 2 }
        );
        assert_eq!(
            d["metrics"]["calls_by_phase"]["document_verification"],
            if reuse { 1 } else { 2 }
        );
        if reuse {
            assert!(d["document_requirements"]["consultations"].is_null());
        }
    }
}

#[test]
fn step_budget_exposes_continuation_and_retry_clears_it() {
    let server = Server::start(|input| {
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","text":"No additional decisions."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/ui.md"), "Save is green.").unwrap();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(3);
    fs::write(&path, config.to_string()).unwrap();
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "error", "{d}");
    assert_eq!(d["continuation"]["reason"], "command_step_budget");
    assert_eq!(
        d["continuation"]["checkpoint"]["stage_status"],
        "checkpoint_saved"
    );
    assert_eq!(d["continuation"]["checkpoint"]["completed_chunks"], 1);
    assert_eq!(d["next_argv"][0], "retry");
    let done = agent(root, &["retry", d["session"].as_str().unwrap()]);
    assert_eq!(done["status"], "awaiting_report", "{done}");
    assert!(done["continuation"].is_null());
    assert_eq!(done["metrics"]["calls_by_phase"]["document_review"], 1);
}

#[test]
fn incomplete_resolved_cache_is_rebuilt_without_losing_verified_packet() {
    let server = Server::start(|input| {
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        if input["phase"] == "document_verification" {
            return json!({"action":"context","text":"","memory":{"rules":[{"rule":"Save is green.","when":"","source_ids":["s1"],"candidate_ids":[1]}],"issues":[],"excluded_candidate_ids":[]}});
        }
        json!({"action":"context","text":"No additional decisions."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/ui.md"), "Save is green.").unwrap();
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(config_path, config.to_string()).unwrap();
    let first = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(first["status"], "awaiting_report", "{first}");
    let path = fs::read_dir(root.join("memory/runtime/documents"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("resolved-")
        })
        .unwrap();
    for field in ["verification", "packet_id", "primary_revision"] {
        let mut cached: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let packet = if cached.get("packet").is_some() {
            &mut cached["packet"]
        } else {
            &mut cached
        };
        packet.as_object_mut().unwrap().remove(field);
        fs::write(&path, cached.to_string()).unwrap();
        let next = agent(root, &["ask", "settings", "Add Save"]);
        assert_eq!(next["status"], "awaiting_report", "{next}");
        assert_eq!(
            next["document_requirements"], first["document_requirements"],
            "missing {field}"
        );
        assert_eq!(next["metrics"]["model_attempts"], 1);
    }
}

#[test]
fn verified_subsets_copy_rules_and_preserve_issues_but_new_facts_refresh() {
    for scoped in [false, true] {
        check_verified_subsets(scoped);
    }
}

fn check_verified_subsets(scoped: bool) {
    let selections = Arc::new(AtomicUsize::new(0));
    let seen = selections.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_issue_scope" {
            return json!({"action":"context","text":"","memory":{"issue_links":[{"id":1,"rule_ids":if scoped {vec![1]} else {vec![]}}]}});
        }
        if input["phase"] == "document_selection" {
            let first = seen.fetch_add(1, Ordering::SeqCst) == 0;
            let narrow = input["document_request"]["task"]
                .as_str()
                .unwrap()
                .contains("Typography");
            assert_eq!(
                input["verified_candidate"]["structured_requirements"]["rules"]
                    .as_array()
                    .unwrap()
                    .len(),
                3
            );
            return json!({"action":"context","text":"","memory":{"selected_chunks":if narrow {vec![]} else {vec![1]},"reuse_previous":false,
                "rule_ids":if first {vec![99]} else if narrow {vec![3,2]} else {vec![]},"reason":"Scope assessment", "issue_links": []}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        if input["phase"] == "document_verification" {
            return json!({"action":"context","text":"","memory":{"rules":[
                {"rule":"Save is green","when":"enabled","source_ids":["s1"],"candidate_ids":[1]},
                {"rule":"Use system sans","when":"","source_ids":["s2"],"candidate_ids":[1]},
                {"rule":"Text at least 16px","when":"","source_ids":["s3"],"candidate_ids":[1]}],
                "issues":["Save color conflict remains unresolved"],"excluded_candidate_ids":[]}});
        }
        json!({"action":"context","text":"No additional decisions."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let docs = root.join("memory/docs/ui.md");
    fs::write(
        &docs,
        "Enabled Save green.\nSystem sans.\nText at least 16px.\n",
    )
    .unwrap();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(&path, config.to_string()).unwrap();
    let broad = agent(root, &["ask", "settings", "All UI rules"]);
    assert_eq!(broad["status"], "awaiting_report", "{broad}");
    let invalid = agent(root, &["ask", "settings", "Typography only"]);
    assert_eq!(invalid["status"], "error", "{invalid}");
    let narrow = agent(root, &["retry", invalid["session"].as_str().unwrap()]);
    assert_eq!(narrow["status"], "awaiting_report", "{narrow}");
    assert!(narrow["document_requirements"]
        .get("structured_requirements")
        .is_none());
    let broad = records(root, &["read", broad["session"].as_str().unwrap()])[0].clone();
    let details = records(root, &["read", narrow["session"].as_str().unwrap()]);
    let packet = &details[0]["document_requirements"];
    assert_eq!(packet["reuse"]["status"], "verified_rule_subset");
    assert_eq!(packet["reuse"]["rule_ids"], json!([2, 3]));
    assert_eq!(
        packet["structured_requirements"]["rules"],
        json!([
            broad["document_requirements"]["structured_requirements"]["rules"][1],
            broad["document_requirements"]["structured_requirements"]["rules"][2]
        ])
    );
    assert_eq!(
        packet["unresolved_issues"],
        if scoped {
            json!([])
        } else {
            broad["document_requirements"]["unresolved_issues"].clone()
        }
    );
    assert!(narrow["metrics"]["calls_by_phase"]["document_review"].is_null());
    assert!(narrow["metrics"]["calls_by_phase"]["document_verification"].is_null());
    let again = agent(
        root,
        &["ask", "settings", "Typography with parent workflow wording"],
    );
    assert_eq!(
        again["document_requirements"]["unresolved_issues"],
        packet["unresolved_issues"]
    );
    assert!(again["metrics"]["calls_by_phase"]["document_issue_scope"].is_null());
    let new = agent(root, &["ask", "settings", "Storage requirements"]);
    assert_eq!(new["status"], "awaiting_report", "{new}");
    assert_eq!(new["metrics"]["calls_by_phase"]["document_verification"], 1);
    fs::write(
        &docs,
        "Enabled Save green.\nSystem sans.\nText at least 18px.\n",
    )
    .unwrap();
    let changed = agent(root, &["ask", "settings", "Typography after source change"]);
    assert_eq!(changed["status"], "awaiting_report", "{changed}");
    assert!(changed["metrics"]["calls_by_phase"]["document_selection"].is_null());
    assert_eq!(
        changed["metrics"]["calls_by_phase"]["document_verification"],
        1
    );
    config["agent"]["profiles"]["agent_medium"]["model"] = json!("new-model");
    fs::write(&path, config.to_string()).unwrap();
    let changed = agent(
        root,
        &["ask", "settings", "Typography after profile change"],
    );
    assert_eq!(changed["status"], "awaiting_report", "{changed}");
    assert!(changed["metrics"]["calls_by_phase"]["document_selection"].is_null());
    assert_eq!(
        changed["metrics"]["calls_by_phase"]["document_verification"],
        1
    );
}

#[test]
fn slow_optional_preparation_falls_back_before_command_budget() {
    let server = Server::start(|input| {
        if input["phase"] == "response_preparation" {
            return json!({"action":"context","text":"","memory":{"items":[]},"test_delay_ms":4000});
        }
        json!({"action":"context","text":"Keep independently confirmed context. ".repeat(80)})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["preparation_agent"] = json!("agent_medium");
    config["memory"]["timeout_seconds"] = json!(4);
    fs::write(path, config.to_string()).unwrap();
    let d = agent(
        root,
        &["ask", "settings", "Context with slow optional preparation"],
    );
    assert_eq!(d["status"], "awaiting_report", "{d}");
    assert_eq!(d["preparation"]["status"], "fallback", "{d}");
    let preparation = &d["preparation"];
    assert!(preparation["input_bytes"].as_u64().unwrap() > 0);
    assert!(preparation["source_chars"].as_u64().unwrap() >= 2000);
    assert!(
        preparation["call_limit_ms"].as_u64().unwrap()
            <= preparation["budget_ms"].as_u64().unwrap()
    );
    assert!(
        preparation["budget_ms"].as_u64().unwrap()
            <= preparation["command_remaining_ms"].as_u64().unwrap() / 2
    );
    assert!(
        d["preparation"]["elapsed_ms"].as_u64().unwrap() < 3000,
        "{d}"
    );
    assert!(
        d["preparation"]["reason"].as_str().unwrap().contains("tim"),
        "{d}"
    );
    assert_eq!(
        d["context"],
        "Keep independently confirmed context. ".repeat(80).trim()
    );
    assert_eq!(server.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn phase_reserve_stops_before_call_and_retry_resumes_without_repeating_scope() {
    let server = Server::start_with_scope(
        |input| {
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            json!({"action":"context","text":"No additional decisions."})
        },
        |input| json!({"action":"documents","text":input["request"],"memory":null,"test_delay_ms":3500}),
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/ui.md"), "Save is green.").unwrap();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["timeout_seconds"] = json!(6);
    fs::write(path, config.to_string()).unwrap();
    let d = agent(root, &["ask", "settings", "Add Save"]);
    assert_eq!(d["status"], "error", "{d}");
    assert_eq!(d["continuation"]["reason"], "command_time_budget");
    assert_eq!(d["continuation"]["checkpoint"]["phase"], "document_index");
    assert_eq!(d["continuation"]["checkpoint"]["required_reserve_ms"], 3000);
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    let done = agent(root, &["retry", d["session"].as_str().unwrap()]);
    assert_eq!(done["status"], "awaiting_report", "{done}");
    assert!(done["continuation"].is_null());
    assert_eq!(done["metrics"]["calls_by_phase"]["document_scope"], 1);
}

#[test]
fn indexed_sections_keep_original_citations_and_refresh_when_sources_change() {
    let selections = Arc::new(AtomicUsize::new(0));
    let calls = selections.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_selection" {
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            return json!({"action":"context","text":"","memory":{"selected_chunks":[1],"reuse_previous":false,
                "sections":[{"chunk":1,"start_line":2,"end_line":if first {99} else {2}}]}});
        }
        if input["phase"] == "document_review" {
            let doc = &input["user_documents"][0];
            assert_eq!(doc["start_line"], 2);
            assert_eq!(doc["end_line"], 2);
            assert_eq!(doc["start_byte"], 9);
            assert!(!doc["text"].as_str().unwrap().contains("Heading"));
            assert!(!doc["text"].as_str().unwrap().contains("duplicate"));
        }
        if input["phase"] == "document_verification" {
            let catalog = input["source_catalog"].as_array().unwrap();
            assert_eq!(
                catalog.len(),
                3,
                "verifier must see the full original, including omitted lines"
            );
            return json!({"action":"context","text":"","memory":{"rules":[
                {"rule":catalog[1]["text"],"when":"","source_ids":["s2"],"candidate_ids":[1]},
                {"rule":"Do not duplicate writes","when":"saving","source_ids":["s3"],"candidate_ids":[]}
            ],"issues":[],"excluded_candidate_ids":[]}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","text":"No extra facts."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(config_path, config.to_string()).unwrap();
    let path = root.join("memory/docs/a-ui.md");
    fs::write(
        &path,
        "Heading\r\nText at least 16px.\r\nDo not duplicate writes.\r\n",
    )
    .unwrap();
    fs::write(root.join("memory/docs/b-other.md"), "Unrelated topic").unwrap();
    let invalid = agent(root, &["ask", "settings", "Font size"]);
    assert_eq!(invalid["status"], "error", "{invalid}");
    let result = agent(root, &["retry", invalid["session"].as_str().unwrap()]);
    assert_eq!(result["status"], "awaiting_report", "{result}");
    assert_eq!(
        result["document_requirements"]["original_bytes_selected"],
        "Text at least 16px.\r\n".len()
    );
    let details = records(root, &["read", result["session"].as_str().unwrap()]);
    assert_eq!(
        details[0]["document_requirements"]["structured_requirements"]["rules"][0]["sources"][0],
        json!({"path":"memory/docs/a-ui.md","start_line":2,"end_line":2})
    );
    assert_eq!(
        details[0]["document_requirements"]["structured_requirements"]["rules"][1]["sources"][0]
            ["start_line"],
        3
    );
    let warm = agent(root, &["ask", "settings", "Font size"]);
    assert!(warm["metrics"]["calls_by_phase"]["document_selection"].is_null());
    fs::write(
        &path,
        "Heading\r\nText at least 18px.\r\nDo not duplicate writes.\r\n",
    )
    .unwrap();
    let changed = agent(root, &["ask", "settings", "Font size"]);
    assert_eq!(changed["status"], "awaiting_report", "{changed}");
    assert!(changed["document_requirements"]["text"]
        .as_str()
        .unwrap()
        .contains("18px"));
    assert_eq!(
        changed["metrics"]["calls_by_phase"]["document_selection"],
        1
    );
}

#[test]
fn preparation_skips_context_between_old_and_new_threshold() {
    let server = Server::start(|input| {
        assert_ne!(input["phase"], "response_preparation");
        json!({"action":"context","text":"Confirmed context. ".repeat(80)})
    });
    let temp = fixture(&server, "ollama");
    let path = temp.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["preparation_agent"] = json!("agent_medium");
    fs::write(path, config.to_string()).unwrap();
    let result = agent(temp.path(), &["ask", "settings", "Known facts"]);
    assert_eq!(result["status"], "awaiting_report", "{result}");
    assert_eq!(result["preparation"]["status"], "skipped", "{result}");
    assert_eq!(result["context"], "Confirmed context. ".repeat(80).trim());
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn section_selection_without_verifier_reads_full_original_chunks() {
    let server = Server::start(|input| {
        if input["phase"] == "document_selection" {
            return json!({"action":"context","text":"","memory":{"selected_chunks":[1],"reuse_previous":false,"sections":[{"chunk":1,"start_line":1,"end_line":1}]}});
        }
        if input["phase"] == "document_review" {
            assert_eq!(
                input["user_documents"][0]["text"],
                "Save is green.\nDo not duplicate writes.\n"
            );
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","text":"No extra facts."})
    });
    let temp = fixture(&server, "ollama");
    fs::write(
        temp.path().join("memory/docs/a-ui.md"),
        "Save is green.\nDo not duplicate writes.\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("memory/docs/b-other.md"),
        "Save other requirements",
    )
    .unwrap();
    let result = agent(temp.path(), &["ask", "settings", "Save requirements"]);
    assert_eq!(result["status"], "awaiting_report", "{result}");
    assert_eq!(
        result["document_requirements"]["routing"]["section_status"],
        "full_chunks_without_verifier"
    );
    assert_eq!(
        result["document_requirements"]["routing"]["sections"],
        json!([])
    );
    assert!(result["document_requirements"]["text"]
        .as_str()
        .unwrap()
        .contains("duplicate"));
}

#[test]
fn length_only_report_errors_go_directly_to_review_with_exact_counts() {
    for excess in [5, 108] {
        let server = Server::start_with_review(
            move |input| {
                if input["phase"] != "report" {
                    return json!({"action":"context","text":"Ready"});
                }
                let budget = input["memory_budget"]["fields_chars"].as_u64().unwrap() as usize;
                json!({"action":"remember","text":"","memory":{"why":"Why","changes":"x".repeat(budget-3-7-7+excess),"constraints":"Unknown","validation":"Checked"}})
            },
            move |input| {
                assert_eq!(
                    input["memory_candidate_budget"]["rendered_chars"],
                    1200 + excess
                );
                assert_eq!(input["memory_candidate_budget"]["excess_chars"], excess);
                json!({"action":"remember","text":"","memory":{"why":"Why","changes":"Saved theme explicitly","constraints":"Unknown","validation":"Checked"}})
            },
        );
        let temp = fixture(&server, "ollama");
        let d = agent(temp.path(), &["ask", "settings", "Save"]);
        let done = agent(
            temp.path(),
            &[
                "report",
                d["session"].as_str().unwrap(),
                "Saved theme; unresolved policy remains",
            ],
        );
        assert_eq!(done["status"], "complete", "{done}");
        assert_eq!(done["metrics"]["calls_by_phase"]["report"], 1);
        assert_eq!(done["metrics"]["calls_by_phase"]["memory_review"], 1);
        let saved: Value = serde_json::from_slice(
            &fs::read(temp.path().join(format!(
                "memory/agent-runs/thread-dialogues/{}.json",
                d["session"].as_str().unwrap()
            )))
            .unwrap(),
        )
        .unwrap();
        for event in saved["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event"] == "model_call")
        {
            assert!(event["input_bytes"].as_u64().unwrap() > 0);
            let limit = event["call_limit_ms"].as_u64().unwrap();
            assert!(limit > 0 && limit <= event["remaining_ms"].as_u64().unwrap());
        }
    }
}

#[test]
fn rejected_review_resumes_the_reviewer_without_repeating_the_owner_report() {
    let review_calls = Arc::new(AtomicUsize::new(0));
    let seen = review_calls.clone();
    let server = Server::start_with_review(
        |input| {
            if input["phase"] == "report" {
                return json!({"action":"remember","text":"","memory":{"why":"Why","changes":"Saved theme","constraints":"Unknown","validation":"Checked"}});
            }
            json!({"action":"context","text":"Ready"})
        },
        move |input| {
            let first = seen.fetch_add(1, Ordering::SeqCst) == 0;
            if !first {
                assert!(input["history"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["kind"] == "memory_validation"));
                assert!(
                    input["memory_candidate_budget"]["excess_chars"]
                        .as_u64()
                        .unwrap()
                        > 0
                );
            }
            json!({"action":"remember","text":"","memory":{"why":"Why","changes":if first {"x".repeat(1300)} else {"Saved theme".into()},"constraints":"Unknown","validation":"Checked"}})
        },
    );
    let temp = fixture(&server, "ollama");
    let d = agent(temp.path(), &["ask", "settings", "Save"]);
    let done = agent(
        temp.path(),
        &[
            "report",
            d["session"].as_str().unwrap(),
            "Saved theme; unresolved policy remains",
        ],
    );
    assert_eq!(done["status"], "complete", "{done}");
    assert_eq!(done["metrics"]["calls_by_phase"]["report"], 1);
    assert_eq!(done["metrics"]["calls_by_phase"]["memory_review"], 2);
}

#[test]
fn corrupt_extraction_cache_cannot_resurrect_rejected_progress_or_reset_repair_limit() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_review" {
            seen.fetch_add(1, Ordering::SeqCst);
            assert_eq!(input["document_review"]["chunk"], 1);
            assert_eq!(
                input["document_review"]["previous_requirements"]["issues"],
                json!([])
            );
            return json!({"action":"context","text":"","memory":{"rules":[],"issues":["x".repeat(16000)]}});
        }
        document_answer(&input).unwrap()
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(root.join("memory/docs/a.md"), "Save is green.").unwrap();
    let first = agent(root, &["ask", "settings", "Save requirements"]);
    assert!(
        first["error"].as_str().unwrap().contains("repair limit"),
        "{first}"
    );
    let cache = fs::read_dir(root.join("memory/runtime/documents"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("query-")
        })
        .unwrap();
    fs::write(&cache, json!({"next_chunk":999,"requirements":{"rules":[],"issues":["invalid\nissue"]},"failures":0}).to_string()).unwrap();
    let retry = agent(root, &["retry", first["session"].as_str().unwrap()]);
    assert!(
        retry["error"].as_str().unwrap().contains("repair limit"),
        "{retry}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    let saved: Value = serde_json::from_slice(&fs::read(cache).unwrap()).unwrap();
    assert_eq!(saved["next_chunk"], 0);
    assert_eq!(saved["requirements"]["issues"], json!([]));
    assert_eq!(saved["failures"], 3);
}

#[test]
fn oversized_extraction_repairs_split_batches_and_persist_retry_limits() {
    for recover in [true, false] {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let server = Server::start(move |input| {
            if input["phase"] == "document_selection" {
                return json!({"action":"context","text":"","memory":{"selected_chunks":[1,2],"reuse_previous":false}});
            }
            if input["phase"] == "document_review" {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    input["document_review"]["batch_chunks"],
                    if n < 2 { 2 } else { 1 }
                );
                if n == 1 || n == 2 {
                    assert_eq!(input["validation_feedback"]["failed_attempts"], n);
                    assert!(
                        input["validation_feedback"]["rendered_chars"]
                            .as_u64()
                            .unwrap()
                            > 16000
                    );
                    assert_eq!(
                        input["document_review"]["previous_requirements"]["rules"],
                        json!([])
                    );
                }
                if n == 3 {
                    assert!(input.get("validation_feedback").is_none());
                    assert_eq!(
                        input["document_review"]["previous_requirements"]["rules"]
                            .as_array()
                            .unwrap()
                            .len(),
                        1
                    );
                }
                if n < 2 || !recover {
                    return json!({"action":"context","text":"","memory":{"rules":[],"issues":["x".repeat(16000)]}});
                }
            }
            if input["phase"] == "document_verification" {
                let rules: Vec<_> = input["source_catalog"].as_array().unwrap().iter().enumerate()
                    .map(|(i,s)|json!({"rule":s["text"].as_str().unwrap().trim(),"when":"","source_ids":[s["id"]],"candidate_ids":[i+1]})).collect();
                return json!({"action":"context","text":"","memory":{"rules":rules,"issues":[],"excluded_candidate_ids":[]}});
            }
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            json!({"action":"context","text":"Ready"})
        });
        let temp = fixture(&server, "ollama");
        let root = temp.path();
        let path = root.join("memory/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["memory"]["verification_agent"] = json!("agent_medium");
        config["memory"]["max_steps"] = json!(1);
        fs::write(path, config.to_string()).unwrap();
        fs::write(
            root.join("memory/docs/a.md"),
            "Save is green when enabled.\n",
        )
        .unwrap();
        fs::write(
            root.join("memory/docs/b.md"),
            "Save is grey when disabled.\n",
        )
        .unwrap();
        let mut d = agent(root, &["ask", "settings", "All Save requirements"]);
        for _ in 0..14 {
            if d["status"] != "error" || d["error"].as_str().unwrap().contains("repair limit") {
                break;
            }
            d = agent(root, &["retry", d["session"].as_str().unwrap()]);
        }
        if recover {
            assert_eq!(d["status"], "awaiting_report", "{d}");
            let text = d["document_requirements"]["text"].as_str().unwrap();
            assert!(
                text.contains("green")
                    && text.contains("grey")
                    && text.contains("a.md")
                    && text.contains("b.md")
            );
            assert_eq!(
                d["document_requirements"]["verification"]["unverified_rules"],
                0
            );
            assert_eq!(calls.load(Ordering::SeqCst), 4);
        } else {
            assert!(d["error"].as_str().unwrap().contains("repair limit"), "{d}");
            assert_eq!(d["continuation"]["reason"], "extraction_repair_exhausted");
            assert_eq!(d["continuation"]["action"], "cancel");
            assert_eq!(d["continuation"]["requires_change"], true);
            assert_eq!(d["next_argv"][0], "cancel");
            let before = server.calls.load(Ordering::SeqCst);
            let retry = agent(root, &["retry", d["session"].as_str().unwrap()]);
            assert!(retry["error"].as_str().unwrap().contains("repair limit"));
            assert_eq!(retry["next_argv"][0], "cancel");
            let saved = agent(root, &["read", d["session"].as_str().unwrap()]);
            assert_eq!(saved["continuation"]["action"], "cancel");
            assert_eq!(server.calls.load(Ordering::SeqCst), before);
            assert_eq!(calls.load(Ordering::SeqCst), 3);
        }
        agent(root, &["cancel", d["session"].as_str().unwrap()]);
    }
}

#[test]
fn batched_verification_preserves_paths_and_retries_the_whole_uncommitted_batch() {
    let first = Arc::new(AtomicBool::new(true));
    let attempt = first.clone();
    let server = Server::start(move |input| {
        if input["phase"] == "document_selection" {
            return json!({"action":"context","text":"","memory":{"selected_chunks":[1,2],"reuse_previous":false}});
        }
        if input["phase"] == "document_verification" {
            assert_eq!(input["verification"]["batch_chunks"], 2);
            assert_eq!(input["verification"]["completed_after"], 2);
            assert_eq!(input["source_catalog"][0]["path"], "memory/docs/a.md");
            assert_eq!(input["source_catalog"][1]["path"], "memory/docs/b.md");
            return json!({"action":"context","text":"","memory":{"rules":[
                {"rule":"Green","when":"enabled","source_ids":["s1"],"candidate_ids":[1]},
                {"rule":"Purple","when":"enabled","source_ids":[if attempt.swap(false,Ordering::SeqCst){"invalid"}else{"s2"}],"candidate_ids":[2]}],
                "issues":["Color conflict"],"excluded_candidate_ids":[]}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","text":"Ready"})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let path = root.join("memory/config.json");
    let mut c: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    c["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(path, c.to_string()).unwrap();
    fs::write(root.join("memory/docs/a.md"), "Enabled Save is green.\n").unwrap();
    fs::write(root.join("memory/docs/b.md"), "Enabled Save is purple.\n").unwrap();
    let bad = agent(root, &["ask", "settings", "All Save requirements"]);
    assert_eq!(bad["status"], "error", "{bad}");
    let done = agent(root, &["retry", bad["session"].as_str().unwrap()]);
    assert_eq!(done["status"], "awaiting_report", "{done}");
    assert_eq!(done["metrics"]["calls_by_phase"]["document_review"], 1);
    assert_eq!(
        done["metrics"]["calls_by_phase"]["document_verification"],
        2
    );
    assert_eq!(
        done["document_requirements"]["verification"]["unverified_rules"],
        0
    );
    let saved: Value = serde_json::from_slice(
        &fs::read(root.join(format!(
            "memory/agent-runs/thread-dialogues/{}.json",
            bad["session"].as_str().unwrap()
        )))
        .unwrap(),
    )
    .unwrap();
    let checkpoints: Vec<_> = saved["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "work_checkpoint" && e["phase"] == "document_verification")
        .collect();
    assert_eq!(checkpoints[0]["completed_chunks"], 0);
    assert_eq!(checkpoints[1]["completed_chunks"], 2);
    assert_eq!(checkpoints[0]["remaining_chunks"], 2);
    assert_eq!(checkpoints[0]["remaining_work"]["verification_chunks"], 2);
    assert_eq!(checkpoints[1]["remaining_chunks"], 0);
    assert_eq!(checkpoints[1]["remaining_work"]["verification_chunks"], 0);
    let extraction: Vec<_> = saved["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "work_checkpoint" && e["phase"] == "document_review")
        .collect();
    assert_eq!(extraction.len(), 1);
    for event in extraction {
        assert_eq!(event["remaining_chunks"], 0);
        assert_eq!(event["remaining_work"]["extraction_chunks"], 0);
        assert_eq!(event["remaining_work"]["verification_chunks"], 2);
    }
}

#[test]
fn codex_usage_is_persisted_and_missing_usage_is_explicit() {
    let server = Server::start(|_| panic!("fake Codex handles this test"));
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    let exe = assert_cmd::cargo::cargo_bin("cm-internal-tests");
    config["agent"]["providers"]["memory-test"] = json!({"adapter":"codex","executable":exe});
    fs::write(config_path, config.to_string()).unwrap();
    let scenario = root.join("scenario.json");
    fs::write(&scenario,json!({"state_file":root.join("counter.json"),"calls":[
        {"events":[{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":20,"reasoning_output_tokens":5}}],"final_message":"{\"action\":\"question\",\"text\":\"Which setting?\",\"memory\":null}"},
        {"final_message":"{\"action\":\"context\",\"text\":\"Theme context.\",\"memory\":null}"}
    ]}).to_string()).unwrap();
    let run = |args: &[&str]| -> Value {
        let output = assert_cmd::Command::cargo_bin("cm-internal-tests")
            .unwrap()
            .args(args)
            .arg("--dir")
            .arg(root)
            .env("CM_FAKE_CODEX_SCENARIO", &scenario)
            .assert()
            .success();
        String::from_utf8_lossy(&output.get_output().stdout)
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|r| r["record"] == "thread_dialogue")
            .unwrap()
    };
    let question = run(&["ask", "settings", "Get theme context"]);
    assert_eq!(question["status"], "question");
    let sid = question["session"].as_str().unwrap();
    let done = run(&["reply", sid, "Theme"]);
    let usage = &done["metrics"]["provider_usage"][0];
    assert_eq!(usage["provider"], "memory-test");
    assert_eq!(usage["model"], "fixture");
    assert_eq!(usage["attempts"], 2);
    assert_eq!(usage["counters"]["input_tokens"]["reported"], 100);
    assert_eq!(usage["counters"]["input_tokens"]["missing_calls"], 1);
    assert_eq!(usage["counters"]["output_tokens"]["reported"], 20);
    assert_eq!(
        agent(root, &["read", sid])["metrics"]["provider_usage"],
        done["metrics"]["provider_usage"]
    );
    agent(root, &["cancel", sid]);
}

#[test]
fn read_only_agents_clarify_consult_and_complete_without_memory_writes() {
    let server = Server::start(|input| {
        assert_eq!(input["read_only"], true);
        assert!(input["instructions"]
            .as_str()
            .unwrap()
            .contains("Context delivery completes the task"));
        let history = input["history"].as_array().unwrap();
        if input["thread"]["slug"] == "idea" {
            if history.is_empty() {
                return json!({"action":"question","text":"Persist across visits?"});
            }
            return json!({"action":"context","text":"Persist across visits."});
        }
        if history.is_empty() {
            return json!({"action":"consult","text":"Persistence policy?"});
        }
        json!({"action":"context","text":"Use explicit saving with persistence."})
    });
    let temp = fixture(&server, "openai-compatible");
    let root = temp.path();
    bind(root, "settings", Some("idea"));
    fn collect(path: &Path, result: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, result);
            } else {
                result.push((path.clone(), fs::read(path).unwrap()));
            }
        }
    }
    let snapshot = |dir: &str| {
        let mut files = Vec::new();
        collect(&root.join(dir), &mut files);
        files.sort();
        files
    };
    let notes = snapshot("memory/thread-agents");
    let threads = snapshot("memory/threads");
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["mode"] = json!("read_only");
    fs::write(&path, config.to_string()).unwrap();
    let question = agent(root, &["ask", "settings", "Save settings"]);
    assert_eq!(question["status"], "question");
    assert_eq!(question["report_required"], false);
    let id = question["session"].as_str().unwrap();
    assert_eq!(agent(root, &["pending"])["count"], 1);
    let answer = agent(root, &["reply", id, "Yes"]);
    assert_eq!(answer["status"], "complete");
    assert_eq!(answer["report_required"], false);
    assert_eq!(agent(root, &["pending"])["count"], 0);
    for args in [
        vec!["create", "new"],
        vec!["bind", "settings", "--agent", "agent_medium"],
        vec!["report", id, "Done"],
    ] {
        common::run(root, &args, "").failure();
    }
    records(root, &["context", "settings"]);
    agent(root, &["get", "settings"]);
    let help = records(root, &["help"]);
    assert!(help[0]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["command"] == "ask"));
    assert!(!help[0]["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["command"] == "docs"));
    config["memory"]["mode"] = json!("threads");
    fs::write(&path, config.to_string()).unwrap();
    common::run(root, &["report", id, "Done"], "").failure();
    assert_eq!(notes, snapshot("memory/thread-agents"));
    assert_eq!(threads, snapshot("memory/threads"));
}

#[test]
fn read_only_mode_rejects_continuation_of_old_writable_dialogue() {
    let server = Server::start(|_| json!({"action":"question","text":"Which settings?"}));
    let temp = fixture(&server, "openai-compatible");
    let root = temp.path();
    let response = agent(root, &["ask", "settings", "Save"]);
    let id = response["session"].as_str().unwrap();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["mode"] = json!("read_only");
    fs::write(path, config.to_string()).unwrap();
    common::run(root, &["reply", id, "General"], "").failure();
    common::run(root, &["retry", id], "").failure();
    assert_eq!(agent(root, &["pending"])["count"], 0);
}
