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

fn startup_command(root: &Path) -> Command {
    let mut c = command(root);
    c.env("TMP", root).env("TEMP", root).env("TMPDIR", root);
    c
}

fn startup_rows(root: &Path, prefix: &str) -> Vec<Value> {
    let dir = root.join("climemory-hook-startup");
    let Ok(files) = fs::read_dir(dir) else {
        return vec![];
    };
    files
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .flat_map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn startup_trace_is_visible_while_stdin_is_still_open() {
    let d = fixture();
    let mut child = startup_command(d.path())
        .args(["hooks", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Always reap the child, including when a regression prevents the marker.
    let deadline = Instant::now() + Duration::from_secs(10);
    let visible = loop {
        if startup_rows(d.path(), "cm-")
            .iter()
            .any(|r| r["stage"] == "stdin_read")
        {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let still_running = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    child.wait().unwrap();
    assert!(visible, "stdin wait must be logged before EOF");
    assert!(still_running);
    assert!(!d.path().join("memory/runtime/hooks").exists());
}

#[test]
fn startup_trace_covers_invalid_input_and_stdout_flush() {
    let d = fixture();
    assert!(invoke(startup_command(d.path()), json!({}))["systemMessage"].is_string());
    let rows = startup_rows(d.path(), "cm-");
    assert!(rows
        .iter()
        .any(|r| r["stage"] == "handler_return" && r["success"] == false));
    assert!(rows
        .iter()
        .any(|r| r["stage"] == "stdout_flush_return" && r["success"] == true));
    assert_eq!(rows.last().unwrap()["stage"], "hook_run_return");
}

#[test]
fn startup_trace_reports_closed_output_pipe() {
    let d = fixture();
    let mut child = startup_command(d.path())
        .args(["hooks", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let rows = startup_rows(d.path(), "cm-");
    assert!(rows
        .iter()
        .any(|r| r["stage"] == "stdout_write_return" && r["success"] == false));
    assert!(!rows.iter().any(|r| r["stage"] == "hook_run_return"));
}

#[test]
fn timing_survives_disabled_statistics_and_project_open_errors_without_content() {
    for invalid_config in [false, true] {
        let d = fixture();
        fs::write(
            d.path().join("memory/config.json"),
            if invalid_config {
                "{".to_string()
            } else {
                json!({"memory":{"statistics":{"enabled":false}}}).to_string()
            },
        )
        .unwrap();
        let mut request = input(d.path(), "SessionStart");
        request["prompt"] = json!("private prompt must not appear in timing logs");
        let reply = hook(d.path(), request);
        assert_eq!(reply["systemMessage"].is_string(), invalid_config);
        let files = fs::read_dir(d.path().join("memory/runtime/hooks/timings"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        let text = fs::read_to_string(files[0].path()).unwrap();
        assert!(!text.contains("private prompt"));
        let rows = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows[0]["event"], "SessionStart");
        assert_eq!(rows[0]["session_id"], SID);
        assert_eq!(rows[1]["stage"], "input_read");
        assert_eq!(rows[2]["stage"], "project_open");
        assert_eq!(rows.last().unwrap()["stage"], "handler_return");
        assert_eq!(rows.last().unwrap()["success"], !invalid_config);
        for pair in rows[1..].windows(2) {
            assert!(
                pair[0]["elapsed_ms"].as_u64().unwrap() <= pair[1]["elapsed_ms"].as_u64().unwrap()
            );
        }
    }
}

#[test]
fn unavailable_timing_directory_does_not_break_hook() {
    let d = fixture();
    fs::create_dir_all(d.path().join("memory/runtime/hooks")).unwrap();
    fs::write(d.path().join("memory/runtime/hooks/timings"), "occupied").unwrap();
    fs::write(d.path().join("climemory-hook-startup"), "occupied").unwrap();
    assert_eq!(
        invoke(startup_command(d.path()), input(d.path(), "SessionStart"))["hookSpecificOutput"]
            ["hookEventName"],
        "SessionStart"
    );
}

#[test]
fn init_installs_all_hooks_and_preserves_existing_handlers_on_repeat() {
    let d = fixture();
    let path = d.path().join(".codex/hooks.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "PreCompact"] {
        assert_eq!(config["hooks"][event].as_array().unwrap().len(), 1);
        let handler = &config["hooks"][event][0]["hooks"][0];
        if matches!(event, "SessionStart" | "UserPromptSubmit") {
            assert_eq!(handler["additionalContextLimit"], 2500);
        } else {
            assert!(handler.get("additionalContextLimit").is_none());
            // Reinitialization must also repair configurations from older CM versions.
            config["hooks"][event][0]["hooks"][0]["additionalContextLimit"] = json!(2500);
        }
    }
    let other = json!({"type":"command","command":"echo keep"});
    config["description"] = json!("user hooks");
    config["hooks"]["Stop"][0]["hooks"]
        .as_array_mut()
        .unwrap()
        .push(other.clone());
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut previous = None;
    for _ in 0..2 {
        let output = command(d.path()).arg("init").output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        let message = String::from_utf8(output.stdout).unwrap();
        assert!(message.contains("/hooks"));
        let bytes = fs::read(&path).unwrap();
        if let Some(previous) = previous {
            assert_eq!(bytes, previous);
        }
        previous = Some(bytes.clone());
        let config: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(config["description"], "user hooks");
        assert_eq!(config["hooks"]["Stop"][0]["hooks"], json!([other]));
        for event in ["SessionStart", "UserPromptSubmit", "Stop", "PreCompact"] {
            let handlers: Vec<_> = config["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["hooks"].as_array().unwrap())
                .filter(|handler| handler["statusMessage"] == "Climemory automatic memory")
                .collect();
            assert_eq!(handlers.len(), 1);
            if matches!(event, "Stop" | "PreCompact") {
                assert!(handlers[0].get("additionalContextLimit").is_none());
            }
        }
    }
}

#[test]
fn init_rejects_invalid_hook_configuration_without_creating_memory() {
    for invalid in ["{", "[]", r#"{"hooks":[]}"#, r#"{"hooks":{"Stop":{}}}"#] {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join(".codex")).unwrap();
        let path = d.path().join(".codex/hooks.json");
        fs::write(&path, invalid).unwrap();
        fs::write(d.path().join("AGENTS.md"), "User instructions\n").unwrap();
        let output = command(d.path()).arg("init").output().unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read_to_string(path).unwrap(), invalid);
        assert!(!d.path().join("memory").exists());
        assert!(!d.path().join(".codex/config.toml").exists());
        assert_eq!(
            fs::read_to_string(d.path().join("AGENTS.md")).unwrap(),
            "User instructions\n"
        );
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
            .env("TMP", d.path())
            .env("TEMP", d.path())
            .env("TMPDIR", d.path())
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
        let wrapper = startup_rows(d.path(), "wrapper-");
        assert_eq!(
            wrapper
                .iter()
                .map(|r| r["stage"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "wrapper_enter",
                "native_start",
                "native_return",
                "wrapper_exit"
            ]
        );
        assert_eq!(wrapper.last().unwrap()["exit_code"], 0);
        let native = startup_rows(d.path(), "cm-");
        assert_eq!(native[0]["wrapper_id"], wrapper[0]["wrapper_id"]);
        assert!(native
            .iter()
            .any(|r| r["stage"] == "stdout_flush_return" && r["success"] == true));
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
        .unwrap_or_else(|| panic!("{first}"));
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
