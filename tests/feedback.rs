use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    assert!(Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(dir.path())
        .arg("init")
        .output()
        .unwrap()
        .status
        .success());
    fs::write(dir.path().join("memory/config.json"), json!({
        "memory":{"feedback":{"background":false,"enabled":true,"agent":"cheap","timeout_seconds":5,"retry_cooldown_seconds":0},"timeout_seconds":5,"agent_retries":{"max_attempts":1}},
        "agent":{"providers":{"codex":{"api_key":"secret-test-key"}},"profiles":{"cheap":{"provider":"codex","model":"test"}}}
    }).to_string()).unwrap();
    dir
}
fn scenario(root: &Path, calls: Vec<Value>) {
    fs::write(
        root.join("scenario.json"),
        json!({"state_file":root.join("calls.json"),"calls":calls}).to_string(),
    )
    .unwrap();
}
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(root)
        .args(args)
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", root.join("scenario.json"))
        .env_remove("CM_CHAT_INTERNAL")
        .env_remove("CM_CONTEXT_INTERNAL")
        .env_remove("CM_DOCS_INTERNAL")
        .output()
        .unwrap()
}
fn state(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("memory/runtime/diagnostics/errors.json")).unwrap())
        .unwrap()
}
fn reports(root: &Path) -> Vec<Value> {
    fs::read_dir(root.join("memory/feedback"))
        .unwrap()
        .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect()
}
fn analysis() -> Value {
    json!({"final_message":json!({"summary":"Invalid CLI arguments","probable_causes":"The recorded command had unsupported flags.","suggested_fixes":"Check help.","limitations":"No provider transport error was observed."}).to_string(),"expect_no_native_tools":true,"expect_sandbox":"read-only","expect_prompt_contains":["untrusted data","incident"]})
}
fn probe() -> Value {
    json!({"final_message":"{\"status\":\"ok\"}"})
}

#[test]
fn two_errors_trigger_next_work_request_and_report_is_durable() {
    let dir = fixture();
    let root = dir.path();
    assert!(!run(root, &["--bad"]).status.success());
    assert_eq!(state(root)["pending"].as_array().unwrap().len(), 1);
    assert!(!run(root, &["--bad"]).status.success());
    assert_eq!(state(root)["total_errors"], 2);
    let pending_before = state(root);
    assert!(run(root, &["help"]).status.success());
    assert!(!root.join("memory/feedback").exists());
    scenario(root, vec![analysis(), probe(), probe()]);
    let output = run(root, &["-test_providers"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(state(root)["pending"], json!([]));
    assert_eq!(reports(root)[0]["kind"], "error_analysis");
    assert_eq!(
        reports(root)[0]["incident_ids"].as_array().unwrap().len(),
        2
    );
    // Simulate a crash after durable report publication but before queue acknowledgement.
    fs::write(
        root.join("memory/runtime/diagnostics/errors.json"),
        pending_before.to_string(),
    )
    .unwrap();
    assert!(run(root, &["-test_providers"]).status.success());
    assert_eq!(reports(root).len(), 1);
    let calls: Value = serde_json::from_slice(&fs::read(root.join("calls.json")).unwrap()).unwrap();
    assert_eq!(calls["calls_seen"], 3);
}

#[test]
fn failed_analysis_keeps_queue_does_not_recurse_and_main_request_runs() {
    let dir = fixture();
    let root = dir.path();
    run(root, &["--bad"]);
    run(root, &["--bad"]);
    scenario(
        root,
        vec![
            json!({"final_message":"invalid"}),
            probe(),
            analysis(),
            probe(),
        ],
    );
    assert!(run(root, &["-test_providers"]).status.success());
    assert_eq!(state(root)["pending"].as_array().unwrap().len(), 2);
    assert_eq!(state(root)["total_errors"], 2);
    assert!(run(root, &["-test_providers"]).status.success());
    assert_eq!(state(root)["pending"], json!([]));
    assert_eq!(reports(root).len(), 1);
}

#[test]
fn external_and_agent_feedback_are_saved_and_credentials_redacted() {
    let dir = fixture();
    let root = dir.path();
    assert!(
        run(root, &["feedback", "Observed failure: secret-test-key"])
            .status
            .success()
    );
    scenario(
        root,
        vec![
            json!({"final_message":"{\"status\":\"ok\",\"feedback\":\"Please improve timeout feedback\"}"}),
        ],
    );
    assert!(run(root, &["-test_providers"]).status.success());
    let records = reports(root);
    assert_eq!(records.len(), 2);
    assert!(records.iter().any(|r| r["source"] == "agent_submission"));
    assert!(records
        .iter()
        .any(|r| r["text"] == "Observed failure: [REDACTED]"));
    for entry in fs::read_dir(root.join("memory/runtime/diagnostics")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            assert!(!fs::read_to_string(path)
                .unwrap()
                .contains("secret-test-key"));
        }
    }
}

#[test]
fn provider_failure_and_its_cli_error_are_counted_once() {
    let dir = fixture();
    let root = dir.path();
    scenario(
        root,
        vec![json!({"stderr":"provider failed","exit_code":1})],
    );
    assert!(!run(root, &["-test_providers"]).status.success());
    assert_eq!(state(root)["total_errors"], 1);
    assert_eq!(state(root)["error_count"], 1);
    assert_eq!(state(root)["pending"][0]["kind"], "attempt_finished");
}

#[test]
fn disabled_diagnostics_keeps_manual_feedback_and_masks_keys() {
    let dir = fixture();
    let root = dir.path();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["feedback"]["enabled"] = json!(false);
    fs::write(path, config.to_string()).unwrap();
    assert!(run(root, &["feedback", "secret-test-key"]).status.success());
    assert_eq!(reports(root)[0]["text"], "[REDACTED]");
    assert!(!root.join("memory/runtime/diagnostics").exists());
}

#[test]
fn concurrent_command_failures_preserve_both_incidents() {
    let dir = fixture();
    let root = dir.path();
    std::thread::scope(|scope| {
        let a = scope.spawn(|| run(root, &["--bad-a"]));
        let b = scope.spawn(|| run(root, &["--bad-b"]));
        assert!(!a.join().unwrap().status.success());
        assert!(!b.join().unwrap().status.success());
    });
    assert_eq!(state(root)["error_count"], 2);
    assert_eq!(state(root)["total_errors"], 2);
}

#[test]
fn missing_logs_do_not_block_analysis_of_saved_incidents() {
    let dir = fixture();
    let root = dir.path();
    run(root, &["--bad"]);
    run(root, &["--bad"]);
    for incident in state(root)["pending"].as_array().unwrap() {
        fs::remove_file(root.join(format!(
            "memory/runtime/diagnostics/{}.jsonl",
            incident["run"].as_str().unwrap()
        )))
        .unwrap();
    }
    let mut call = analysis();
    // The prompt includes the recorded errors even when their journal files were removed.
    call["expect_prompt_contains"] = json!(["unavailable", "cm_error", "incident"]);
    scenario(root, vec![call, probe()]);
    assert!(run(root, &["-test_providers"]).status.success());
    assert_eq!(state(root)["error_count"], 0);
    assert!(reports(root)[0]["log_sources"]
        .as_array()
        .unwrap()
        .iter()
        .all(|l| l["unavailable"].is_string()));
}

#[test]
fn incomplete_existing_report_does_not_acknowledge_incidents() {
    let dir = fixture();
    let root = dir.path();
    run(root, &["--bad"]);
    run(root, &["--bad"]);
    let pending = state(root);
    scenario(root, vec![analysis(), probe(), probe()]);
    assert!(run(root, &["-test_providers"]).status.success());
    let path = fs::read_dir(root.join("memory/feedback"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let report: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fs::write(
        path,
        json!({"incident_ids":report["incident_ids"]}).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("memory/runtime/diagnostics/errors.json"),
        pending.to_string(),
    )
    .unwrap();
    assert!(run(root, &["-test_providers"]).status.success());
    assert_eq!(state(root)["error_count"], 2);
}

#[test]
fn workers_cannot_write_feedback_through_nested_cli() {
    let dir = fixture();
    for marker in [
        "CM_CHAT_INTERNAL",
        "CM_CONTEXT_INTERNAL",
        "CM_DOCS_INTERNAL",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cm"))
            .current_dir(dir.path())
            .args(["feedback", "nested feedback"])
            .env_remove("CM_CHAT_INTERNAL")
            .env_remove("CM_CONTEXT_INTERNAL")
            .env_remove("CM_DOCS_INTERNAL")
            .env(marker, "1")
            .output()
            .unwrap();
        assert!(!output.status.success(), "{marker}");
    }
    assert!(!dir.path().join("memory/feedback").exists());
}

#[test]
fn analysis_request_budget_preserves_queue_and_allows_main_request() {
    let dir = fixture();
    let root = dir.path();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["feedback"]["request_budget_seconds"] = json!(1);
    fs::write(path, config.to_string()).unwrap();
    run(root, &["--bad"]);
    run(root, &["--bad"]);
    let mut slow = analysis();
    slow["expect_no_console"] = json!(true);
    slow["delay_ms"] = json!(5000);
    scenario(root, vec![slow, probe()]);
    let start = std::time::Instant::now();
    assert!(run(root, &["-test_providers"]).status.success());
    assert!(start.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(state(root)["error_count"], 2);
    assert_eq!(state(root)["total_errors"], 2);
}

#[test]
fn analysis_cooldown_persists_between_processes_and_expires() {
    let dir = fixture();
    let root = dir.path();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["feedback"]["retry_cooldown_seconds"] = json!(300);
    config["memory"]["statistics"] = json!({"enabled":true});
    fs::write(path, config.to_string()).unwrap();
    run(root, &["--bad"]);
    run(root, &["--bad"]);
    scenario(
        root,
        vec![
            json!({"final_message":"invalid"}),
            probe(),
            probe(),
            analysis(),
            probe(),
        ],
    );
    assert!(run(root, &["-test_providers"]).status.success());
    let after = state(root);
    assert!(after["next_analysis_after"].as_u64().unwrap() > 0);
    assert_eq!(after["error_count"], 2);
    assert!(run(root, &["-test_providers"]).status.success());
    assert_eq!(state(root), after);
    let calls: Value = serde_json::from_slice(&fs::read(root.join("calls.json")).unwrap()).unwrap();
    assert_eq!(calls["calls_seen"], 3);
    let stats: Vec<Value> = fs::read_dir(root.join("memory/runtime/statistics"))
        .unwrap()
        .map(|f| serde_json::from_slice(&fs::read(f.unwrap().path()).unwrap()).unwrap())
        .collect();
    assert!(stats.iter().any(|r| r["call_counts"]["analysis"] == 0
        && r["call_counts"]["operation"] == 1
        && r["call_counts"]["total"] == 1));
    assert!(stats
        .iter()
        .any(|r| r["call_breakdown"]["analysis"]["calls"] == 1
            && r["call_breakdown"]["operation"]["calls"] == 1
            && r["call_counts"]["total"] == 2));
    let mut expired = after;
    expired["next_analysis_after"] = json!(1);
    fs::write(
        root.join("memory/runtime/diagnostics/errors.json"),
        expired.to_string(),
    )
    .unwrap();
    assert!(run(root, &["-test_providers"]).status.success());
    assert_eq!(state(root)["error_count"], 0);
    assert_eq!(state(root)["next_analysis_after"], 0);
}

#[test]
fn empty_ingestion_never_runs_analyst_even_with_two_pending_errors() {
    let dir = fixture();
    let root = dir.path();
    run(root, &["--bad"]);
    run(root, &["--bad"]);
    let before = state(root);
    scenario(root, vec![]);
    let home = root.join("codex");
    fs::create_dir_all(home.join("sessions")).unwrap();
    let id = "11111111-2222-3333-4444-555555555555";
    fs::write(
        home.join(format!("sessions/rollout-{id}.jsonl")),
        format!("{}\n", json!({"type":"session_meta","payload":{"id":id}})),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(root)
        .arg("ingest-session")
        .env("CODEX_HOME", home)
        .env("CODEX_THREAD_ID", id)
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", root.join("scenario.json"))
        .env_remove("CM_CHAT_INTERNAL")
        .env_remove("CM_CONTEXT_INTERNAL")
        .env_remove("CM_DOCS_INTERNAL")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["model_calls"], 0);
    assert_eq!(result["call_counts"]["total"], 0);
    assert_eq!(state(root), before);
    assert!(!root.join("calls.json").exists());
}

#[test]
fn background_analysis_survives_caller_and_has_separate_statistics() {
    let dir = fixture();
    let root = dir.path();
    run(root, &["--bad"]);
    run(root, &["--bad"]);
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["feedback"]["background"] = json!(true);
    config["memory"]["statistics"] = json!({"enabled":true});
    fs::write(path, config.to_string()).unwrap();
    let mut slow = analysis();
    slow["expect_no_console"] = json!(true);
    slow["delay_ms"] = json!(4000);
    scenario(root, vec![slow]);
    let start = std::time::Instant::now();
    // Invalid work command finishes immediately; only the background worker uses a model.
    assert!(!run(root, &["--bad-after-queue"]).status.success());
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let reports = fs::read_dir(root.join("memory/runtime/statistics"))
            .unwrap()
            .filter_map(|p| fs::read(p.ok()?.path()).ok())
            .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
            .collect::<Vec<_>>();
        if reports
            .iter()
            .any(|r| r["status"] == "complete" && r["call_breakdown"]["analysis"]["calls"] == 1)
        {
            assert!(reports
                .iter()
                .any(|r| r["status"] == "error" && r["totals"]["calls"] == 0));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker did not complete"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(reports(root).iter().any(|r| r["kind"] == "error_analysis"));
}
