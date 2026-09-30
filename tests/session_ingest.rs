use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const SID: &str = "11111111-2222-3333-4444-555555555555";

#[test]
fn http_ingestion_receives_schema_and_commits_incrementally() {
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    let f = Fixture::new();
    fs::create_dir_all(f.root().join("memory/docs")).unwrap();
    fs::write(
        f.root().join("memory/docs/existing.md"),
        "Existing rule: enabled Save is green.\n",
    )
    .unwrap();
    f.message(
        "request",
        "Save tooltip: No changes to save; requested, not implemented.",
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let worker = std::thread::spawn(move || {
        let start = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(start.elapsed() < Duration::from_secs(15));
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("{e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
                length = n.trim().parse::<usize>().unwrap();
            }
        }
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("task_instructions"));
        let prompt: Value =
            serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert!(prompt["task_instructions"]
            .as_str()
            .unwrap()
            .contains("NEW events"));
        let instructions = prompt["task_instructions"].as_str().unwrap();
        assert!(instructions.contains("Missing information is NOT evidence of absence"));
        // Exercise the exact problematic boundary: a fresh session receives
        // no project inventory, even though the fixture is an existing project.
        let input: Value = serde_json::from_str(
            instructions
                .lines()
                .find(|line| line.starts_with('{'))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(input["topics"], json!({}));
        assert_eq!(
            input["evidence_scope"]["memory_scope"],
            "current_session_only"
        );
        assert_eq!(input["evidence_scope"]["project_inventory_provided"], false);
        assert_eq!(input["evidence_scope"]["conversation_complete"], false);
        assert!(!instructions.contains("Existing rule: enabled Save is green."));
        assert!(prompt["response_schema"]["properties"]["updates"].is_object());
        assert!(prompt["response_schema"]
            .to_string()
            .contains(&eid("request")));
        let content = reply(
            "request",
            "Save tooltip: No changes to save; requested, not implemented.",
        )["final_message"]
            .clone();
        let response = json!({"choices":[{"message":{"content":content},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":50}}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).unwrap();
    });
    fs::write(f.root().join("memory/config.json"), json!({"memory":{"chat_agent":"cheap","timeout_seconds":10},"agent":{"providers":{"http":{"adapter":"openai-compatible","endpoint":format!("http://{address}/v1/chat/completions")}},"profiles":{"cheap":{"provider":"http","model":"test"}}}}).to_string()).unwrap();
    let out = f.run();
    worker.join().unwrap();
    assert_eq!(success(out)["model_calls"], 1);
    assert!(f.state()["summary"]
        .as_str()
        .unwrap()
        .contains("No changes to save"));
    assert_eq!(success(f.run())["model_calls"], 0);
    assert_eq!(
        fs::read_to_string(f.root().join("memory/docs/existing.md")).unwrap(),
        "Existing rule: enabled Save is green.\n"
    );
}

struct Fixture {
    dir: tempfile::TempDir,
    log: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_cm"))
            .current_dir(dir.path())
            .arg("init")
            .output()
            .unwrap();
        assert!(out.status.success());
        fs::write(dir.path().join("memory/config.json"),json!({"memory":{"mode":"read_only","chat_agent":"cheap","documents_agent":"cheap","verification_agent":null,"timeout_seconds":5},"agent":{"profiles":{"cheap":{"provider":"codex","model":"test-cheap","reasoning_effort":"low"}}}}).to_string()).unwrap();
        let sessions = dir.path().join("codex/sessions/2026");
        fs::create_dir_all(&sessions).unwrap();
        let log = sessions.join(format!("rollout-{SID}.jsonl"));
        fs::write(
            &log,
            format!("{}\n", json!({"type":"session_meta","payload":{"id":SID}})),
        )
        .unwrap();
        Self { dir, log }
    }
    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn append(&self, v: Value) {
        let mut f = fs::OpenOptions::new().append(true).open(&self.log).unwrap();
        writeln!(f, "{v}").unwrap();
    }
    fn message(&self, id: &str, text: &str) {
        self.append(json!({"type":"response_item","timestamp":id,"payload":{"id":id,"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}));
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_cm"));
        c.current_dir(self.root())
            .arg("ingest-session")
            .env("CODEX_HOME", self.root().join("codex"))
            .env("CODEX_THREAD_ID", SID)
            .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
            .env("CM_FAKE_CODEX_SCENARIO", self.root().join("scenario.json"))
            .env_remove("CM_CHAT_INTERNAL")
            .env_remove("CM_CONTEXT_INTERNAL");
        c
    }
    fn run(&self) -> Output {
        self.command().output().unwrap()
    }
    fn scenario(&self, calls: Vec<Value>) {
        for e in fs::read_dir(self.root()).unwrap() {
            let e = e.unwrap();
            if e.file_name().to_string_lossy().starts_with("calls.json") {
                fs::remove_file(e.path()).unwrap();
            }
        }
        fs::write(
            self.root().join("scenario.json"),
            json!({"state_file":self.root().join("calls.json"),"calls":calls}).to_string(),
        )
        .unwrap();
    }
    fn state_path(&self) -> PathBuf {
        let root = self.root().join("memory/runtime/session-ingest");
        fs::read_dir(root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("state.json")
    }
    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.state_path()).unwrap()).unwrap()
    }
}
fn eid(id: &str) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "e-{}",
        &format!("{:x}", Sha256::digest(id.as_bytes()))[..20]
    )
}
fn reply(id: &str, text: &str) -> Value {
    json!({"final_message":json!({"summary":text,"updates":[{"key":"settings","title":"Settings","memory":text,"claims":[{"id":"save-rule","change_reason":"User updated the Save rule","kind":"requirement","status":"requested","text":text,"sources":[eid(id)]}],"related":[]}]}).to_string(),"expect_sandbox":"read-only","expect_output_schema":true,"expect_no_native_tools":true})
}
fn success(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn node_test_summaries_survive_import_and_incremental_failure() {
    let f = Fixture::new();
    f.message("request", "Verify Save");
    let report = |id: &str, output: String| json!({"type":"response_item","timestamp":id,"payload":{"id":id,"type":"function_call_output","output":output}});
    f.append(report("pass", json!({"output":"TAP version 13\n# arbitrary private comment\n# tests 8\n# pass 8\n# fail 0\n# tests not-a-number\n# pass 8 extra-text"}).to_string()));
    f.scenario(vec![
        reply("request", "Verify Save"),
        reply("request", "Verify Save"),
    ]);
    assert_eq!(success(f.run())["new_events"], 2);
    let state = f.state();
    let event = state["recent"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "test_report")
        .unwrap();
    assert_eq!(event["text"], "# tests 8\n# pass 8\n# fail 0");
    assert_eq!(success(f.run())["model_calls"], 0);
    f.append(report(
        "fail",
        "ℹ tests 8\nℹ pass 7\nℹ fail 1\nℹ cancelled 0\nℹ skipped 0\nℹ todo 0".into(),
    ));
    assert_eq!(success(f.run())["new_events"], 1);
    let state = f.state();
    assert!(state["recent"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "test_report" && e["text"].as_str().unwrap().contains("ℹ fail 1")));
}

#[test]
fn incremental_import_noop_updates_and_native_root_bindings() {
    let f = Fixture::new();
    f.message("e1", "Save should be green");
    let mut first = reply("e1", "Save should be green");
    first["expect_prompt_contains"] = json!(["Save should be green", "new_events"]);
    f.scenario(vec![first, reply("e2", "Save should now be blue")]);
    let r = success(f.run());
    assert_eq!(r["new_events"], 1);
    assert_eq!(r["model_calls"], 1);
    let state = f.state();
    let cursor = state["cursors"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(cursor["offset"], fs::metadata(&f.log).unwrap().len());
    let root = r["root_thread"].as_str().unwrap();
    let binding: Value = serde_json::from_slice(
        &fs::read(f.root().join(format!("memory/thread-agents/{root}.json"))).unwrap(),
    )
    .unwrap();
    assert!(binding["parent"].is_null());
    assert_eq!(binding["agent"], "cheap");
    let bindings = fs::read_dir(f.root().join("memory/thread-agents"))
        .unwrap()
        .map(|e| serde_json::from_slice::<Value>(&fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(bindings.len(), 2);
    assert!(bindings.iter().any(|b| b["parent"] == root));
    let noop = success(f.run());
    assert_eq!(noop["new_events"], 0);
    assert_eq!(noop["model_calls"], 0);
    assert_eq!(f.state(), state);
    f.message("e2", "Save should now be blue");
    let next = success(f.run());
    assert_eq!(next["new_events"], 1);
    assert_eq!(next["root_thread"], root);
    assert_eq!(next["topics"], 1);
    assert_eq!(
        f.state()["topics"]["settings"]["memory"],
        "Save should now be blue"
    );
    // Published child bindings carry the latest source memory; unified claim
    // indexing is tested separately below.
    let updated: Vec<Value> = fs::read_dir(f.root().join("memory/thread-agents"))
        .unwrap()
        .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect();
    assert!(updated.iter().any(|b| b["memory"]
        .as_str()
        .is_some_and(|v| v.contains("Save should now be blue"))));
}
#[test]
fn incomplete_tail_waits_and_hidden_data_never_reaches_agent() {
    let f = Fixture::new();
    f.append(json!({"type":"response_item","payload":{"type":"message","role":"assistant","channel":"analysis","content":[{"text":"PRIVATE_REASONING"}]}}));
    f.append(json!({"type":"response_item","payload":{"type":"message","role":"developer","content":[{"text":"PRIVATE_SYSTEM"}]}}));
    f.append(json!({"type":"response_item","payload":{"type":"function_call","arguments":"PRIVATE_TOOL_INPUT"}}));
    let offset = fs::metadata(&f.log).unwrap().len();
    let tail=json!({"type":"response_item","timestamp":"tail","payload":{"id":"tail","type":"message","role":"user","content":[{"text":"Save green"}]}}).to_string();
    fs::OpenOptions::new()
        .append(true)
        .open(&f.log)
        .unwrap()
        .write_all(tail.as_bytes())
        .unwrap();
    let r = success(f.run());
    assert_eq!(r["model_calls"], 0);
    assert_eq!(
        f.state()["cursors"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["offset"],
        offset
    );
    let mut call = reply("tail", "Save green");
    call["save_prompt_to"] = json!(f.root().join("prompt.txt"));
    f.scenario(vec![call]);
    fs::OpenOptions::new()
        .append(true)
        .open(&f.log)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    assert_eq!(success(f.run())["new_events"], 1);
    let prompt = fs::read_to_string(f.root().join("prompt.txt")).unwrap();
    for hidden in ["PRIVATE_REASONING", "PRIVATE_SYSTEM", "PRIVATE_TOOL_INPUT"] {
        assert!(!prompt.contains(hidden));
    }
}
#[test]
fn failed_validation_retains_checkpoint_and_pending_commit_recovers() {
    let f = Fixture::new();
    f.message("a", "Save green");
    f.scenario(vec![
        reply("missing", "Invalid"),
        reply("missing", "Invalid"),
    ]);
    let out = f.run();
    assert!(!out.status.success());
    assert!(!f.state_path().exists());
    f.scenario(vec![reply("a", "Save green")]);
    success(f.run());
    let before = f.state();
    let parent = f.state_path().parent().unwrap().to_path_buf();
    let revision = before["revision"].as_u64().unwrap();
    let bytes = fs::read(parent.join(format!("revisions/{revision:08}.json"))).unwrap();
    fs::write(parent.join("pending.json"), bytes).unwrap();
    fs::remove_file(f.state_path()).unwrap();
    let r = success(f.run());
    assert_eq!(r["model_calls"], 0);
    assert_eq!(f.state(), before);
    assert!(!parent.join("pending.json").exists());
    f.message("b", "Save blue");
    f.scenario(vec![reply("b", "Save blue")]);
    success(f.run());
    let current = fs::read(f.state_path()).unwrap();
    // A stale prepared commit must never roll newer durable memory backwards.
    let old = fs::read(parent.join(format!("revisions/{revision:08}.json"))).unwrap();
    fs::write(parent.join("pending.json"), old).unwrap();
    assert!(!f.run().status.success());
    assert_eq!(fs::read(f.state_path()).unwrap(), current);
}
#[test]
fn copied_rollout_deduplicates_and_truncation_does_not_reset() {
    let f = Fixture::new();
    f.message("a", "Save green");
    f.scenario(vec![reply("a", "Save green")]);
    success(f.run());
    fs::copy(
        &f.log,
        f.log.parent().unwrap().join(format!("z-copy-{SID}.jsonl")),
    )
    .unwrap();
    let r = success(f.run());
    assert_eq!(r["new_events"], 0);
    assert_eq!(r["model_calls"], 0);
    let before = fs::read(f.state_path()).unwrap();
    fs::write(
        &f.log,
        format!("{}\n", json!({"type":"session_meta","payload":{"id":SID}})),
    )
    .unwrap();
    let r = f.run();
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("truncated or replaced"));
    assert_eq!(fs::read(f.state_path()).unwrap(), before);
}
#[test]
fn exact_session_and_nested_worker_guard() {
    let f = Fixture::new();
    fs::write(
        &f.log,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"other-session"}})
        ),
    )
    .unwrap();
    let r = f.run();
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("matching session_meta.id"));
    let r = f.command().env("CM_CHAT_INTERNAL", "1").output().unwrap();
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("cannot ingest"));
}

#[test]
fn new_batch_does_not_resend_old_history_and_secrets_are_masked() {
    let f = Fixture::new();
    f.message(
        "old",
        &format!("Save green. {} OLD_HISTORY_TAIL", "x".repeat(9000)),
    );
    f.scenario(vec![reply("old", "Save green")]);
    success(f.run());
    f.message(
        "new",
        "Save blue. api_key=\"sk-abcdefghijklmnopqrstuvwxyz012345\"",
    );
    let mut call = reply("new", "Save blue");
    call["save_prompt_to"] = json!(f.root().join("new-prompt.txt"));
    f.scenario(vec![call]);
    assert_eq!(success(f.run())["new_events"], 1);
    let prompt = fs::read_to_string(f.root().join("new-prompt.txt")).unwrap();
    assert!(!prompt.contains("OLD_HISTORY_TAIL"));
    assert!(!prompt.contains("sk-abcdefghijklmnopqrstuvwxyz012345"));
    assert!(prompt.contains("REDACTED"));
}

#[test]
fn native_tool_violation_and_timeout_do_not_advance_checkpoint() {
    let f = Fixture::new();
    f.message("a", "Save green");
    f.scenario(vec![reply("a", "Save green")]);
    success(f.run());
    let before = fs::read(f.state_path()).unwrap();
    f.message("b", "Save blue");
    let mut call = reply("b", "Save blue");
    call["events"] =
        json!([{"type":"item.completed","item":{"type":"command_execution","command":"bad"}}]);
    f.scenario(vec![call]);
    let r = f.run();
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("forbidden native tools"));
    assert_eq!(fs::read(f.state_path()).unwrap(), before);
    let p = f.root().join("memory/config.json");
    let mut cfg: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    cfg["memory"]["timeout_seconds"] = json!(1);
    fs::write(&p, cfg.to_string()).unwrap();
    let mut call = reply("b", "Save blue");
    call["delay_ms"] = json!(5000);
    f.scenario(vec![call]);
    assert!(!f.run().status.success());
    assert_eq!(fs::read(f.state_path()).unwrap(), before);
    f.scenario(vec![reply("b", "Save blue")]);
    assert_eq!(success(f.run())["new_events"], 1);
}

#[test]
fn successful_batches_survive_later_failure_without_reanalysis() {
    let f = Fixture::new();
    f.message(
        "first",
        &format!("Save green {} OLD_BATCH_TAIL", "x".repeat(60_000)),
    );
    f.message("second", "Save blue");
    f.scenario(vec![
        reply("first", "Save green"),
        reply("absent", "bad"),
        reply("absent", "bad"),
    ]);
    let failed = f.run();
    assert!(!failed.status.success());
    let receipt: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(receipt["write_receipt"]["status"], "partially_saved");
    assert_eq!(receipt["write_receipt"]["revision"], 1);
    assert_eq!(receipt["write_receipt"]["added"], 1);
    assert_eq!(f.state()["revision"], 1);
    assert_eq!(f.state()["topics"]["settings"]["memory"], "Save green");
    let mut call = reply("second", "Save blue");
    call["save_prompt_to"] = json!(f.root().join("retry-prompt.txt"));
    f.scenario(vec![call]);
    let r = success(f.run());
    assert_eq!(r["new_events"], 1);
    assert_eq!(r["model_calls"], 1);
    assert_eq!(r["revision"], 2);
    assert!(!fs::read_to_string(f.root().join("retry-prompt.txt"))
        .unwrap()
        .contains("OLD_BATCH_TAIL"));
}

#[test]
fn statistics_keep_primary_session_snapshots_separate_from_agent_usage() {
    let f = Fixture::new();
    let path = f.root().join("memory/config.json");
    let mut c: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    c["memory"]["statistics"] = json!({"enabled":true});
    fs::write(path, c.to_string()).unwrap();
    f.append(json!({"type":"event_msg","timestamp":"2026-09-23T12:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"output_tokens":100,"total_tokens":1100}}}}));
    f.message("a", "Save green");
    f.scenario(vec![reply("a", "Save green")]);
    success(f.run());
    let dir = f.root().join("memory/runtime/statistics");
    let p = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let r: Value = serde_json::from_slice(&fs::read(p).unwrap()).unwrap();
    assert_eq!(
        r["primary_session_usage"]["cumulative"]["input_tokens"],
        1000
    );
    assert_eq!(r["source_events"], 1);
    assert_eq!(r["calls"][0]["phase"], "session_ingest");
    assert!(r["totals"]["tokens"]["input_tokens"]["reported"].is_null());
    success(f.run());
    let rows = fs::read_dir(dir)
        .unwrap()
        .map(|e| serde_json::from_slice::<Value>(&fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect::<Vec<_>>();
    let noop = rows.iter().find(|r| r["totals"]["calls"] == 0).unwrap();
    assert_eq!(
        noop["primary_session_usage"]["cumulative"]["input_tokens"],
        1000
    );
    assert_eq!(noop["totals"]["tokens"]["total_tokens"]["reported"], 0);
    assert_eq!(noop["source_events"], 0);
}

#[test]
fn cm_read_evidence_is_incremental_and_updates_the_existing_topic() {
    let f = Fixture::new();
    f.message("question", "What color is Save?");
    let patch = |kind: &str, sources: Vec<String>, text: &str| json!({"final_message":json!({"summary":text,"updates":[{"key":"settings","title":"Settings","memory":text,"claims":[{"kind":kind,"status":"reported","text":text,"sources":sources}],"related":[]}]}).to_string()});
    f.scenario(vec![patch(
        "open_question",
        vec![eid("question")],
        "Save color question pending.",
    )]);
    success(f.run());
    let old = f.state();
    // Restrict this read to one real document; imported memory remains available
    // to ingestion but cannot satisfy the query by repeating the open question.
    let path = f.root().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["mode"] = json!("docs_only");
    fs::write(path, config.to_string()).unwrap();
    fs::create_dir_all(f.root().join("memory/docs")).unwrap();
    fs::write(
        f.root().join("memory/docs/ui-kit.md"),
        "Save must be green.",
    )
    .unwrap();
    use sha2::{Digest, Sha256};
    let id = format!(
        "doc-{}:L1",
        &format!("{:x}", Sha256::digest(b"memory/docs/ui-kit.md"))[..16]
    );
    f.scenario(vec![
        json!({"final_message":json!({"aspects":["Save color"],"intents":["original_requirement"]}).to_string()}),
        json!({"final_message":json!({"select":[id],"elements":[],"summary":"Save color","questions":[],"links":[],"checked":[],"need":[],"gaps":[]}).to_string()}),
        json!({"final_message":json!({"answer":"Save is green.","select":[id],"aspects":[{"question":"Save color","status":"found","evidence":[id]}],"need":[],"conflicts":[]}).to_string()})
    ]);
    let scenario_path = f.root().join("scenario.json");
    let mut scenario: Value = serde_json::from_slice(&fs::read(&scenario_path).unwrap()).unwrap();
    scenario["operation_calls"] = json!({"unified_grounding":{"indexed_aspect_reply":{"supported":true,"answer":"","self_contained":false},"copy_aspect_answer":true}});
    fs::write(scenario_path, scenario.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(f.root())
        .arg("What color is Save?")
        .env("CODEX_THREAD_ID", SID)
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", f.root().join("scenario.json"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dir = fs::read_dir(f.root().join("memory/runtime/session-reads"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let receipt = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let v: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    let evidence = eid(v["payload"]["id"].as_str().unwrap());
    // A different session's receipt must not enter this checkpoint.
    fs::create_dir_all(f.root().join("memory/runtime/session-reads/other-session")).unwrap();
    fs::write(
        f.root()
            .join("memory/runtime/session-reads/other-session/other.jsonl"),
        "invalid other session content\n",
    )
    .unwrap();
    f.scenario(vec![patch(
        "reported_result",
        vec![evidence.clone()],
        "Save color answered: CM reports green; not independently verified.",
    )]);
    let output = success(f.run());
    assert_eq!(output["new_events"], 1);
    let state = f.state();
    assert_eq!(state["topics"].as_object().unwrap().len(), 1);
    assert_eq!(
        state["topics"]["settings"]["claims"][0]["kind"],
        "reported_result"
    );
    assert_eq!(state["evidence"][&evidence]["kind"], "memory_report");
    assert!(state["evidence"][&evidence]["text"]
        .as_str()
        .unwrap()
        .contains("ui-kit.md"));
    assert_eq!(
        state["revision"].as_u64().unwrap(),
        old["revision"].as_u64().unwrap() + 1
    );
    assert_eq!(success(f.run())["model_calls"], 0);
    f.append(json!({"type":"response_item","timestamp":"later","payload":{"id":"final-answer","type":"message","role":"assistant","content":[{"type":"output_text","text":"Answered: Save must be green according to the UI kit."}]}}));
    f.scenario(vec![patch(
        "reported_result",
        vec![eid("final-answer")],
        "Answered: Save must be green according to the UI kit.",
    )]);
    assert_eq!(success(f.run())["new_events"], 1);
    assert_eq!(f.state()["topics"].as_object().unwrap().len(), 1);
    assert_eq!(success(f.run())["model_calls"], 0);
}

#[test]
fn partial_cancellation_preserves_other_topics_and_original_evidence() {
    let f = Fixture::new();
    let claim = |status: &str, text: &str, ids: Vec<&str>| json!({"replaces":if text == "Reload clears operands" { vec!["reload-rule"] } else { vec![] },"id":if text == "Reload retains operands" { "reload-rule" } else if text == "Reopening starts empty" { "reopen-rule" } else if text == "Theme persists only after Save" { "theme-rule" } else { "reload-new-rule" },"change_reason":if status == "superseded" { "User cancelled reload retention only" } else { "" },"kind":"requirement","status":status,"text":text,"sources":ids.into_iter().map(eid).collect::<Vec<_>>()});
    let topic = |key: &str, claims: Vec<Value>| json!({"key":key,"title":key,"memory":key,"claims":claims,"related":[]});
    let response = |updates: Vec<Value>| json!({"final_message":json!({"summary":"Requested only; not implemented or tested.","updates":updates}).to_string()});
    f.message("original","Binary: reopening starts empty. Theme: persists only after Save. Remember only; do not implement.");
    f.scenario(vec![response(vec![
        topic(
            "binary",
            vec![claim(
                "requested",
                "Reopening starts empty",
                vec!["original"],
            )],
        ),
        topic(
            "theme",
            vec![claim(
                "requested",
                "Theme persists only after Save",
                vec!["original"],
            )],
        ),
    ])]);
    let first = success(f.run());
    let theme = f.state()["topics"]["theme"].clone();
    f.message(
        "refine",
        "Reload retains operands; reopening stays empty; keep theme rule.",
    );
    let mut second = response(vec![topic(
        "binary",
        vec![
            claim("requested", "Reopening starts empty", vec!["original"]),
            claim("requested", "Reload retains operands", vec!["refine"]),
        ],
    )]);
    second["expect_prompt_contains"] =
        json!(["Reopening starts empty", "Theme persists only after Save"]);
    f.scenario(vec![second]);
    success(f.run());
    f.message("cancel","Cancel reload retention only: reload now clears operands. Reopening and theme rules remain.");
    let mut third = response(vec![topic(
        "binary",
        vec![
            claim("requested", "Reopening starts empty", vec!["original"]),
            claim(
                "superseded",
                "Reload retains operands",
                vec!["refine", "cancel"],
            ),
            claim("requested", "Reload clears operands", vec!["cancel"]),
        ],
    )]);
    third["expect_prompt_contains"] = json!([
        "Reload retains operands",
        "Reopening starts empty",
        "Theme persists only after Save"
    ]);
    f.scenario(vec![third]);
    let last = success(f.run());
    let state = f.state();
    assert_eq!(first["root_thread"], last["root_thread"]);
    assert_eq!(last["topics"], 2);
    assert_eq!(state["topics"]["theme"], theme);
    assert_eq!(
        state["topics"]["binary"]["claims"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(state["archive"]["binary"][0]["claim"]["id"], "reload-rule");
    assert_eq!(
        state["archive"]["binary"][0]["replaced_by"],
        json!(["reload-new-rule"])
    );
    for id in ["refine", "cancel"] {
        assert!(state["archive"]["binary"][0]["evidence"]
            .get(eid(id))
            .is_some());
    }
    assert_eq!(success(f.run())["model_calls"], 0);
    assert_eq!(f.state(), state);
}

#[test]
fn omitted_requirement_retries_without_advancing_failed_checkpoint() {
    let f = Fixture::new();
    f.message("original", "Save green");
    f.scenario(vec![reply("original", "Save green")]);
    success(f.run());
    let before = f.state();
    f.message("correction", "Save blue");
    let mut invalid = reply("correction", "Save blue");
    let mut body: Value = serde_json::from_str(invalid["final_message"].as_str().unwrap()).unwrap();
    body["updates"][0]["claims"][0]["id"] = json!("lost-old-id");
    invalid["final_message"] = json!(body.to_string());
    f.scenario(vec![invalid.clone(), invalid.clone()]);
    let failed = f.run();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("disappeared"));
    assert_eq!(f.state(), before);
    let mut corrected = reply("correction", "Save blue");
    corrected["expect_prompt_contains"] =
        json!(["requirement save-rule disappeared", "Save green"]);
    f.scenario(vec![invalid, corrected]);
    let result = success(f.run());
    assert_eq!(result["model_calls"], 2);
    assert_eq!(result["new_events"], 1);
    assert_eq!(
        f.state()["topics"]["settings"]["claims"][0]["id"],
        "save-rule"
    );
    assert_eq!(success(f.run())["model_calls"], 0);
}

#[test]
fn twenty_imports_preserve_archive_without_resending_it() {
    let f = Fixture::new();
    f.message("initial", "Save rule zero");
    f.scenario(vec![reply("initial", "Save rule zero")]);
    success(f.run());
    for n in 0..20 {
        let id = format!("revision-{n}");
        f.message(&id, &format!("Replace previous Save rule with rule {n}"));
        let mut topic = f.state()["topics"]["settings"].clone();
        let mut old = topic["claims"][0].clone();
        old["status"] = json!("superseded");
        old["change_reason"] = json!("Explicit user replacement");
        old["sources"].as_array_mut().unwrap().push(json!(eid(&id)));
        let new = json!({"id":format!("rule-{n}"),"change_reason":"","replaces":[old["id"]],"kind":"requirement","status":"requested","text":format!("Save rule {n}"),"sources":[eid(&id)]});
        topic.as_object_mut().unwrap().remove("claims");
        topic["operations"] = json!([
            {"action":"cancel","id":old["id"],"claim":null,"reason":"Explicit user replacement","sources":[eid(&id)]},
            {"action":"add","id":"","claim":new,"reason":"","sources":[]}
        ]);
        topic["memory"] = json!(format!("Save rule {n}"));
        f.scenario(vec![json!({"final_message":json!({"summary":format!("Save rule {n}"),"updates":[topic]}).to_string(),"save_prompt_to":f.root().join("archive-prompt.txt")})]);
        assert_eq!(success(f.run())["model_calls"], 1);
        assert_eq!(
            f.state()["topics"]["settings"]["claims"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            f.state()["archive"]["settings"].as_array().unwrap().len(),
            n + 1
        );
        assert_eq!(success(f.run())["model_calls"], 0);
    }
    let prompt = fs::read_to_string(f.root().join("archive-prompt.txt")).unwrap();
    let input: Value =
        serde_json::from_str(prompt.lines().rev().find(|l| l.starts_with('{')).unwrap()).unwrap();
    assert!(input.get("archive").is_none());
    assert_eq!(
        input["topics"]["settings"]["claims"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(input["previous_evidence"].as_array().unwrap().len() <= 5);
    assert!(!input.to_string().contains("Save rule zero"));
    assert_eq!(
        f.state()["archive"]["settings"].as_array().unwrap().len(),
        20
    );
    assert_eq!(
        f.state()["topics"]["settings"]["claims"][0]["text"],
        "Save rule 19"
    );
}

#[test]
fn write_receipt_is_committed_evidence_not_just_exit_success() {
    let f = Fixture::new();
    f.message("a", "Save green");
    f.scenario(vec![reply("missing", "bad"), reply("missing", "bad")]);
    let failed = f.run();
    assert!(!failed.status.success());
    let error: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(error["write_receipt"]["status"], "not_saved");
    assert!(!f.state_path().exists());
    f.scenario(vec![reply("a", "Save green")]);
    let saved = success(f.run());
    assert_eq!(saved["write_receipt"]["status"], "saved");
    assert_eq!(saved["write_receipt"]["added"], 1);
    assert_eq!(saved["write_receipt"]["revision"], f.state()["revision"]);
    assert_eq!(saved["write_receipt"]["changes"][0]["text"], "Save green");
    let noop = success(f.run());
    assert_eq!(noop["write_receipt"]["status"], "unchanged");
    assert_eq!(noop["model_calls"], 0);
}

#[test]
fn interrupted_publication_receipt_is_unknown_then_recovery_confirms_save() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    f.message("a", "Save green");
    f.scenario(vec![reply("a", "Save green")]);
    let state_path = f
        .root()
        .join("memory/runtime/session-ingest")
        .join(format!("{:x}", Sha256::digest(SID.as_bytes())))
        .join("state.json");
    let dir = state_path.parent().unwrap();
    fs::create_dir_all(dir).unwrap();
    let obstruction = dir.join("revisions");
    fs::write(&obstruction, "test obstruction").unwrap();
    let failed = f.run();
    assert!(!failed.status.success());
    let output: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(output["write_receipt"]["status"], "unknown");
    assert_eq!(output["write_receipt"]["revision"], 0);
    assert!(dir.join("pending.json").exists());
    assert!(!state_path.exists());
    fs::remove_file(obstruction).unwrap();
    let recovered = success(f.run());
    assert_eq!(recovered["write_receipt"]["status"], "saved");
    assert_eq!(recovered["write_receipt"]["added"], 1);
    assert_eq!(
        recovered["write_receipt"]["revision"],
        f.state()["revision"]
    );
    assert_eq!(recovered["model_calls"], 0);
    assert!(!dir.join("pending.json").exists());
}

#[test]
fn durable_failed_batch_replays_without_rollout_and_never_duplicates() {
    let f = Fixture::new();
    f.message("frozen", "Save green");
    let config_path = f.root().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["timeouts"] = json!({"ingest_seconds":1});
    fs::write(&config_path, config.to_string()).unwrap();
    let mut delayed = reply("frozen", "Save green");
    delayed["delay_ms"] = json!(5000);
    f.scenario(vec![delayed]);
    let start = std::time::Instant::now();
    let failed = f.run();
    assert!(!failed.status.success());
    assert!(start.elapsed() < std::time::Duration::from_secs(4));
    let output: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(output["pending_batch"], true);
    assert_eq!(output["retry"]["same_session_required"], true);
    let queue = f.state_path().with_file_name("queued.json");
    assert!(queue.exists());
    let frozen_batch = fs::read(&queue).unwrap();
    assert!(!f.state_path().exists());
    let original = fs::read(&f.log).unwrap();
    fs::remove_file(&f.log).unwrap();
    f.scenario(vec![reply("frozen", "Save green")]);
    let recovered = success(f.run());
    assert_eq!(recovered["write_receipt"]["status"], "saved");
    assert_eq!(recovered["resumed_batch"], true);
    assert_eq!(recovered["more_events_unchecked"], true);
    assert!(!queue.exists());
    fs::write(&f.log, original).unwrap();
    // Crash after commit publication but before removing the frozen input batch.
    fs::write(&queue, frozen_batch).unwrap();
    let repeat = success(f.run());
    assert_eq!(repeat["model_calls"], 0);
    assert_eq!(repeat["write_receipt"]["revision"], 1);
    assert!(!queue.exists());
}

#[test]
fn unified_index_reuses_current_claim_identity_and_original_addresses() {
    let f = Fixture::new();
    f.message("new-rule", "Save button must be green.");
    f.scenario(vec![reply("new-rule", "Save button must be green.")]);
    success(f.run());
    let before = fs::read(f.state_path()).unwrap();
    let path = f.root().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(1);
    fs::write(path, config.to_string()).unwrap();
    f.scenario(vec![json!({"final_message":json!({"aspects":["Current memory"],"intents":["original_requirement"]}).to_string()}),json!({"final_message":json!({"answer":"Incomplete","select":[],"aspects":[],"need":[],"conflicts":[]}).to_string()})]);
    let out = Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(f.root())
        .arg("Save button requirements?")
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", f.root().join("scenario.json"))
        .output()
        .unwrap();
    assert_eq!(success(out)["status"], "partial");
    let index: Value = serde_json::from_slice(
        &fs::read(f.root().join("memory/runtime/unified/index.json")).unwrap(),
    )
    .unwrap();
    let source = index["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"].as_str().unwrap().starts_with("claims-"))
        .unwrap();
    let state = f.state();
    let pointer = source["addresses"][0]["pointer"].as_str().unwrap();
    assert_eq!(state.pointer(pointer).unwrap(), &source["text"]);
    let claim = &source["claims"][0];
    assert_eq!(claim["status"], "requested");
    assert_eq!(claim["sources"], json!([eid("new-rule")]));
    let thread = index["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["source"] == source["id"])
        .unwrap();
    assert_eq!(&thread["elements"][0], claim);
    assert_eq!(fs::read(f.state_path()).unwrap(), before);
}
