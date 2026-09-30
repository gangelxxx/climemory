use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const SID: &str = "11111111-2222-3333-4444-555555555555";
fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn command(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_cm"));
    c.current_dir(root)
        .env_remove("CM_CHAT_INTERNAL")
        .env_remove("CM_CONTEXT_INTERNAL")
        .env_remove("CM_DOCS_INTERNAL")
        .env("CODEX_HOME", root.join("codex"))
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", root.join("scenario.json"));
    c
}
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    assert!(command(d.path())
        .arg("init")
        .output()
        .unwrap()
        .status
        .success());
    fs::write(d.path().join("memory/config.json"), json!({"memory":{"chat_agent":"cheap","documents_agent":"cheap","statistics":{"enabled":true},"timeout_seconds":5,"agent_retries":{"max_attempts":1}},"agent":{"profiles":{"cheap":{"provider":"codex","model":"test"}}}}).to_string()).unwrap();
    fs::create_dir_all(d.path().join("codex/sessions")).unwrap();
    fs::write(d.path().join("scenario.json"), json!({"state_file":d.path().join("calls.json"),"calls":[{"final_message":json!({"summary":"Feature reported complete.","updates":[{"key":"feature","title":"Feature","memory":"Feature reported complete.","claims":[{"id":"","change_reason":"","kind":"reported_result","status":"reported","text":"Feature reported complete.","sources":[format!("e-{}", &hash("answer")[..20])]}],"related":[]}]}).to_string()}]}).to_string()).unwrap();
    d
}
fn hook(root: &Path, input: Value) -> Value {
    invoke(command(root), input)
}
fn invoke(mut c: Command, input: Value) -> Value {
    let mut child = c
        .args(["hooks", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is a single hook JSON object")
}
fn input(root: &Path, event: &str) -> Value {
    json!({"session_id":SID,"cwd":root,"hook_event_name":event,"turn_id":"turn-1"})
}
fn runtime(root: &Path, prefix: &str) -> std::path::PathBuf {
    root.join(format!("memory/runtime/hooks/{prefix}-{}.json", hash(SID)))
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "worker did not reach expected state"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn installed_hooks_preserve_config_and_execute_windows_command() {
    let d = fixture();
    fs::create_dir_all(d.path().join(".codex")).unwrap();
    let path = d.path().join(".codex/hooks.json");
    fs::write(
        &path,
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo keep"}]}]}}"#,
    )
    .unwrap();
    for _ in 0..2 {
        assert!(command(d.path())
            .args(["hooks", "install", "codex"])
            .output()
            .unwrap()
            .status
            .success());
    }
    let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(config["hooks"]["Stop"].as_array().unwrap().len(), 2);
    #[cfg(windows)]
    {
        let encoded = config["hooks"]["SessionStart"][0]["hooks"][0]["commandWindows"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .last()
            .unwrap();
        let mut c = Command::new("powershell.exe");
        c.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", encoded])
            .current_dir(d.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input(d.path(), "SessionStart").to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply["hookSpecificOutput"]["hookEventName"], "SessionStart");
    }
    assert!(command(d.path())
        .args(["hooks", "uninstall", "codex"])
        .output()
        .unwrap()
        .status
        .success());
    let config: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(
        config["hooks"]["Stop"][0]["hooks"][0]["command"],
        "echo keep"
    );
    assert_eq!(config["hooks"]["Stop"].as_array().unwrap().len(), 1);
}

#[test]
fn ignores_internal_agents_and_uninitialized_cwd_and_reports_bad_identity() {
    let d = fixture();
    let mut c = command(d.path());
    c.env("CM_CHAT_INTERNAL", "1");
    assert_eq!(invoke(c, input(d.path(), "Stop")), json!({}));
    let outside = tempfile::tempdir().unwrap();
    assert_eq!(
        hook(d.path(), input(outside.path(), "SessionStart")),
        json!({})
    );
    let mut invalid = input(d.path(), "Stop");
    invalid["session_id"] = json!("../../bad");
    assert!(hook(d.path(), invalid)["systemMessage"]
        .as_str()
        .unwrap()
        .contains("invalid Codex"));
    let mut subagent = input(d.path(), "Stop");
    subagent["agent_id"] = json!("child");
    assert_eq!(hook(d.path(), subagent), json!({}));
    assert!(!d.path().join("memory/runtime/hooks").exists());
}

#[test]
fn stop_waits_for_final_flush_then_retries_and_is_incremental() {
    let d = fixture();
    let rollout = d.path().join(format!("codex/sessions/rollout-{SID}.jsonl"));
    let header = format!("{}\n", json!({"type":"session_meta","payload":{"id":SID}}));
    fs::write(&rollout, &header).unwrap();
    let mut stop = input(d.path(), "Stop");
    stop["last_assistant_message"] = json!("Finished implementing the feature.");
    assert_eq!(hook(d.path(), stop.clone()), json!({}));
    let receipt = runtime(d.path(), "receipt");
    wait_for(|| receipt.exists());
    let failed: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(failed["pending"], true);
    assert!(runtime(d.path(), "job").exists());
    assert!(!d.path().join("calls.json").exists());
    fs::write(&rollout, format!("{header}{}\n", json!({"type":"response_item","timestamp":"2026-09-29T12:00:00Z","payload":{"id":"answer","type":"message","role":"assistant","phase":"final","content":[{"text":"Finished implementing the feature."}]}}))).unwrap();
    hook(d.path(), input(d.path(), "SessionStart"));
    wait_for(|| !runtime(d.path(), "job").exists());
    let imported: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(imported["result"]["new_events"], 1);
    let reports = fs::read_dir(d.path().join("memory/runtime/statistics"))
        .unwrap()
        .map(|p| serde_json::from_slice::<Value>(&fs::read(p.unwrap().path()).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert!(reports.iter().any(|r| r["command"] == "hooks"
        && r["session_id"] == SID
        && r["status"] == "complete"
        && r["calls"].as_array().is_some_and(|c| !c.is_empty())
        && r["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["data"]["event"] == "Import")));
    let calls = fs::read(d.path().join("calls.json")).unwrap();
    hook(d.path(), stop);
    wait_for(|| !runtime(d.path(), "job").exists());
    assert_eq!(fs::read(d.path().join("calls.json")).unwrap(), calls);
}

#[test]
fn prompt_error_is_nonblocking_and_slash_commands_do_not_query_memory() {
    let d = fixture();
    let mut prompt = input(d.path(), "UserPromptSubmit");
    prompt["prompt"] = json!("/help");
    assert_eq!(hook(d.path(), prompt.clone()), json!({}));
    assert!(!d.path().join("calls.json").exists());
    // Invalid memory profile must produce a warning, not a blocking exit code.
    fs::write(
        d.path().join("memory/docs/rule.md"),
        "Buttons must be blue.",
    )
    .unwrap();
    prompt["prompt"] = json!("What color should buttons be?");
    let result = hook(d.path(), prompt);
    assert!(result["systemMessage"].is_string(), "{result}");
    assert!(result.get("decision").is_none());
}

#[test]
fn statistics_follow_payload_project_and_identity_and_count_failures() {
    let d = fixture();
    let other = tempfile::tempdir().unwrap();
    let mut c = command(d.path());
    c.current_dir(other.path())
        .env("CODEX_THREAD_ID", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let out = invoke(c, input(d.path(), "SessionStart"));
    assert!(out["hookSpecificOutput"].is_object());
    let mut prompt = input(d.path(), "UserPromptSubmit");
    prompt["prompt"] = json!("Find a requirement");
    // No matching model response in this scenario: hook remains nonblocking.
    assert!(hook(d.path(), prompt)["systemMessage"].is_string());
    let reports = fs::read_dir(d.path().join("memory/runtime/statistics"))
        .unwrap()
        .map(|p| serde_json::from_slice::<Value>(&fs::read(p.unwrap().path()).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(reports.len(), 2);
    assert!(reports.iter().all(|r| r["session_id"] == SID));
    assert!(reports
        .iter()
        .any(|r| r["status"] == "error" && r["exchange"]["errors"] == 1));
    assert!(!other.path().join("memory").exists());
}

#[test]
fn prompt_retrieves_user_document_and_reuses_duplicate_turn_without_models() {
    let d = fixture();
    fs::write(d.path().join("memory/config.json"), json!({"memory":{"chat_agent":"cheap","documents_agent":"cheap","verification_agent":"cheap","statistics":{"enabled":true},"timeout_seconds":15,"unified":{"concurrency":1},"agent_retries":{"max_attempts":1}},"agent":{"profiles":{"cheap":{"provider":"codex","model":"test"}}}}).to_string()).unwrap();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        "Buttons blue.\nDeletion buttons red.",
    )
    .unwrap();
    let doc = format!("doc-{}", &hash("memory/docs/ui.md")[..16]);
    let ids = vec![format!("{doc}:L1"), format!("{doc}:L2")];
    let plan =
        json!({"aspects":["Button colors and exceptions"],"intents":["original_requirement"]});
    let select = json!({"select":ids,"elements":[],"summary":"Button colors","questions":["What color are buttons?"],"links":[],"checked":[],"need":[],"gaps":[]});
    let assembled = json!({"answer":"Buttons are blue; deletion buttons are red.","select":ids,"aspects":[{"question":"Button colors and exceptions","status":"found","evidence":ids}],"need":[],"conflicts":[]});
    let calls = [plan, select, assembled]
        .into_iter()
        .map(|v| json!({"final_message":v.to_string(),"expect_no_native_tools":true}))
        .collect::<Vec<_>>();
    fs::write(d.path().join("scenario.json"),json!({"state_file":d.path().join("calls.json"),"calls":calls,"operation_calls":{"unified_grounding":{"indexed_aspect_reply":{"supported":true,"answer":"","self_contained":false},"copy_aspect_answer":true}}}).to_string()).unwrap();
    let mut prompt = input(d.path(), "UserPromptSubmit");
    prompt["prompt"] = json!("What colors must the buttons use?");
    let mut c = command(d.path());
    c.env("CODEX_THREAD_ID", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    let first = invoke(c, prompt.clone());
    let context = first["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect(&first.to_string());
    assert!(context.contains("blue"), "{context}");
    assert!(context.contains("red"), "{context}");
    let calls = fs::read(d.path().join("calls.json")).unwrap();
    assert_eq!(hook(d.path(), prompt.clone()), first);
    prompt["turn_id"] = json!("turn-2");
    assert_eq!(hook(d.path(), prompt.clone()), first);
    assert_eq!(fs::read(d.path().join("calls.json")).unwrap(), calls);
    assert!(d
        .path()
        .join(format!("memory/runtime/session-reads/{}", hash(SID)))
        .exists());
    assert!(!d
        .path()
        .join(format!(
            "memory/runtime/session-reads/{}",
            hash("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        ))
        .exists());
    assert_eq!(
        fs::read_to_string(d.path().join("memory/docs/ui.md")).unwrap(),
        "Buttons blue.\nDeletion buttons red."
    );
    let reports = fs::read_dir(d.path().join("memory/runtime/statistics"))
        .unwrap()
        .map(|p| serde_json::from_slice::<Value>(&fs::read(p.unwrap().path()).unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(reports.len(), 3);
    assert!(reports
        .iter()
        .all(|r| r["session_id"] == SID && r["command"] == "hooks"));
    assert_eq!(
        reports
            .iter()
            .filter(|r| r["cache"]["hook_context_hits"] == 1)
            .count(),
        2
    );
    assert!(reports
        .iter()
        .any(|r| !r["calls"].as_array().unwrap().is_empty()));
    fs::write(d.path().join("memory/docs/ui.md"), "Buttons green.").unwrap();
    assert_ne!(
        hook(d.path(), prompt),
        first,
        "changed source must invalidate reuse"
    );
}

#[test]
fn new_stop_during_import_is_not_acknowledged_by_the_older_worker_pass() {
    let d = fixture();
    let scenario_path = d.path().join("scenario.json");
    let mut scenario: Value = serde_json::from_slice(&fs::read(&scenario_path).unwrap()).unwrap();
    scenario["calls"][0]["delay_ms"] = json!(500);
    scenario["calls"].as_array_mut().unwrap().push(json!({"final_message":json!({"summary":"Feature reported complete.","updates":[]}).to_string()}));
    fs::write(scenario_path, scenario.to_string()).unwrap();
    let rollout = d.path().join(format!("codex/sessions/rollout-{SID}.jsonl"));
    let event = |id: &str, text: &str| json!({"type":"response_item","timestamp":"2026-09-29T12:00:00Z","payload":{"id":id,"type":"message","role":"assistant","phase":"final","content":[{"text":text}]}});
    fs::write(
        &rollout,
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":SID}}),
            event("answer", "Finished implementing the feature.")
        ),
    )
    .unwrap();
    hook(d.path(), input(d.path(), "Stop"));
    wait_for(|| d.path().join("calls.json").exists());
    let mut file = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
    writeln!(file, "{}", event("answer2", "Second turn completed.")).unwrap();
    drop(file);
    hook(d.path(), input(d.path(), "Stop"));
    wait_for(|| !runtime(d.path(), "job").exists());
    let state_path = d.path().join(format!(
        "memory/runtime/session-ingest/{}/state.json",
        hash(SID)
    ));
    let state: Value = serde_json::from_slice(&fs::read(state_path).unwrap()).unwrap();
    assert!(state["recent"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["text"] == "Second turn completed."));
}
