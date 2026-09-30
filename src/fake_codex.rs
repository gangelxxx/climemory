//! The fake-Codex test double (Proposal hosted-agent-workflow-mvp, Plan
//! step 4). When the test suite points `CM_CODEX_EXE` at cm's own binary,
//! the adapter's codex-shaped argv — a leading `exec` — lands here instead
//! of the CLI: main.rs intercepts it whenever the `CM_FAKE_CODEX_SCENARIO`
//! env var is set (detection is by env marker + argv shape, never a
//! help-visible subcommand, mirroring `__thread_worker`). The fake replays a
//! scripted scenario so provider/scheduler tests are fully deterministic:
//!
//! ```json
//! {
//!   "state_file": "<path to the invocation-counter file>",
//!   "calls": [
//!     {
//!       "delay_ms": 0,                          // sleep before emitting (cancellation tests)
//!       "session_id": "sess-1",                 // emitted as thread.started when set
//!       "events": [{ "type": "turn.started" }], // raw JSONL values, emitted as-is
//!       "final_message": "...",                 // emitted as item.completed agent_message
//!       "stderr": "...",                        // optional stderr text
//!       "exit_code": 0,
//!       "expect_resume_session": "sess-1",      // resume argv must name this id (else exit 3)
//!       "expect_output_schema": true,           // --output-schema must be present (else exit 4)
//!       "expect_sandbox": "read-only",          // sandbox value must match (else exit 6)
//!       "expect_reasoning_effort": "high",      // Codex config override must match (else exit 8)
//!       "expect_prompt_contains": ["## Task"]   // every string must occur (else exit 7)
//!     }
//!   ]
//! }
//! ```
//!
//! Invocation N replays `calls[N]`; the counter persists in `state_file`
//! (`{"calls_seen": N}`) across the separate fake processes, so a multi-step
//! test scripts call 1, call 2, ... Exhausting the script exits 5.
//! Optional `operation_calls` use independent counters and reusable explicit replies
//! for JSON envelopes, leaving the positional script cursor unchanged.

use serde::Deserialize;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;

#[derive(Deserialize)]
struct FakeCodexScenario {
    state_file: PathBuf,
    calls: Vec<FakeCodexCall>,
    /// Optional independent scripted responders for JSON operation envelopes.
    #[serde(default)]
    operation_calls: std::collections::BTreeMap<String, FakeCodexCall>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // a typo'd fixture key must fail, not be ignored
struct FakeCodexCall {
    #[serde(default)]
    delay_ms: u64,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    events: Vec<serde_json::Value>,
    #[serde(default)]
    final_message: Option<String>,
    /// Test-only indexed reply rows; copies only each input aspect index.
    #[serde(default)]
    indexed_aspect_reply: Option<serde_json::Value>,
    /// Copy ordinary draft prose when a fixture explicitly scripts indexed replies.
    #[serde(default)]
    copy_aspect_answer: bool,
    #[serde(default)]
    stderr: Option<String>,
    #[serde(default)]
    exit_code: i32,
    #[serde(default)]
    expect_resume_session: Option<String>,
    #[serde(default)]
    expect_output_schema: bool,
    #[serde(default)]
    expect_no_native_tools: bool,
    #[serde(default)]
    expect_no_console: bool,
    #[serde(default)]
    expect_sandbox: Option<String>,
    #[serde(default)]
    expect_reasoning_effort: Option<String>,
    #[serde(default)]
    expect_prompt_contains: Vec<String>,
    /// Append the received (stdin) prompt to this file, so tests can assert
    /// on the context pack a later invocation was given.
    #[serde(default)]
    save_prompt_to: Option<PathBuf>,
    /// Simulate application edits in the provider's actual working directory.
    #[serde(default)]
    write_files: std::collections::BTreeMap<String, String>,
}

/// Run the fake against the codex-shaped argv AFTER the leading `exec`
/// (`--sandbox workspace-write|read-only`, `--config model_reasoning_effort=...`,
/// `--json`, `--skip-git-repo-check`, `-C <dir>`, `--output-schema <file>`,
/// `resume <id>`, and the final prompt slot `-` for stdin). Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let mut resume_session = None;
    let mut sandbox = None;
    let mut reasoning_effort = None;
    let mut have_schema = false;
    let mut prompt_from_stdin = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" | "--full-auto" | "--skip-git-repo-check" | "--ephemeral" => {}
            "-C" | "--output-schema" | "-m" | "--model" => {
                if args[index].as_str() == "--output-schema" {
                    have_schema = true;
                }
                index += 1; // consume the flag's value
            }
            "-c" | "--config" => {
                if let Some(effort) = args
                    .get(index + 1)
                    .and_then(|value| value.strip_prefix("model_reasoning_effort="))
                {
                    reasoning_effort = Some(effort.trim_matches('"').to_string());
                }
                index += 1;
            }
            "--sandbox" => {
                sandbox = args.get(index + 1).cloned();
                index += 1;
            }
            "resume" => {
                resume_session = args.get(index + 1).cloned();
                index += 1;
            }
            "-" => prompt_from_stdin = true,
            _ => {} // a literal prompt string; ignored like any other
        }
        index += 1;
    }
    let mut received_prompt = String::new();
    if prompt_from_stdin {
        // Drain the prompt so the writer never blocks on a full pipe.
        let _ = std::io::stdin().read_to_string(&mut received_prompt);
    }

    let Some(scenario_path) = std::env::var_os("CM_FAKE_CODEX_SCENARIO") else {
        eprintln!("fake-codex: CM_FAKE_CODEX_SCENARIO is not set");
        return 2;
    };
    let scenario: FakeCodexScenario = match std::fs::read_to_string(&scenario_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
    {
        Some(scenario) => scenario,
        None => {
            eprintln!(
                "fake-codex: cannot read the scenario {}",
                scenario_path.to_string_lossy()
            );
            return 2;
        }
    };
    let envelope = received_prompt
        .lines()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok());
    let operation = envelope
        .as_ref()
        .and_then(|value| value["operation"].as_str())
        .filter(|name| scenario.operation_calls.contains_key(*name));
    let operation_call = operation.and_then(|name| scenario.operation_calls.get(name));
    let state_file = operation
        .map(|name| {
            PathBuf::from(format!(
                "{}.operation-{}",
                scenario.state_file.to_string_lossy(),
                name.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>()
            ))
        })
        .unwrap_or_else(|| scenario.state_file.clone());
    // Claim a call number with create_new so parallel fan-out processes never
    // replay the same script entry. The JSON counter remains the public test
    // observation, while claim files are the synchronization primitive.
    let calls_seen = if operation_call.is_some() {
        0
    } else {
        (0..scenario.calls.len())
            .find(|index| {
                let claim = PathBuf::from(format!("{}.call-{index}", state_file.to_string_lossy()));
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(claim)
                    .is_ok()
            })
            .unwrap_or(scenario.calls.len())
    };
    // The JSON counter is the public test observation; keep it a true call
    // count under parallel fan-out by taking a lock file around the
    // read-modify-write instead of a last-writer-wins blind write.
    let lock = PathBuf::from(format!("{}.lock", state_file.to_string_lossy()));
    let mut attempts = 0;
    let guard = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(file) => break Some(file),
            Err(_) if attempts < 2_000 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => break None,
        }
    };
    let current = std::fs::read_to_string(&state_file)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("calls_seen").and_then(|seen| seen.as_u64()))
        .unwrap_or(0);
    let _ = std::fs::write(
        &state_file,
        format!("{{\"calls_seen\": {}}}\n", current + 1),
    );
    drop(guard);
    let _ = std::fs::remove_file(&lock);
    let Some(call) = operation_call.or_else(|| scenario.calls.get(calls_seen)) else {
        eprintln!(
            "fake-codex: the scenario is exhausted ({} calls scripted)",
            scenario.calls.len()
        );
        return 5;
    };
    #[cfg(windows)]
    if call.expect_no_console {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetConsoleWindow() -> *mut std::ffi::c_void;
        }
        if !unsafe { GetConsoleWindow() }.is_null() {
            eprintln!("fake-codex: background provider has a console window");
            return 12;
        }
    }
    #[cfg(not(windows))]
    let _ = call.expect_no_console;
    if call.expect_no_native_tools {
        for required in [
            "--ignore-user-config",
            "--ephemeral",
            "features.shell_tool=false",
            "features.unified_exec=false",
            "features.multi_agent=false",
            "features.apps=false",
            "features.plugins=false",
            "web_search=\"disabled\"",
            "project_doc_max_bytes=0",
        ] {
            if !args.iter().any(|arg| arg == required) {
                eprintln!("fake-codex: missing isolated-worker argument {required}");
                return 9;
            }
        }
    }
    if let Some(expected) = &call.expect_resume_session {
        if resume_session.as_deref() != Some(expected.as_str()) {
            eprintln!(
                "fake-codex: expected `resume {expected}`, got {:?}",
                resume_session
            );
            return 3;
        }
    }
    if call.expect_output_schema && !have_schema {
        eprintln!("fake-codex: expected --output-schema on the argv");
        return 4;
    }
    if let Some(expected) = &call.expect_sandbox {
        if sandbox.as_deref() != Some(expected.as_str()) {
            eprintln!(
                "fake-codex: expected --sandbox {expected}, got {:?}",
                sandbox
            );
            return 6;
        }
    }
    if let Some(expected) = &call.expect_reasoning_effort {
        if reasoning_effort.as_deref() != Some(expected.as_str()) {
            eprintln!(
                "fake-codex: expected reasoning effort {expected}, got {:?}",
                reasoning_effort
            );
            return 8;
        }
    }
    for expected in &call.expect_prompt_contains {
        if !received_prompt.contains(expected) {
            eprintln!("fake-codex: expected prompt to contain {expected:?}");
            return 7;
        }
    }
    if let Some(path) = &call.save_prompt_to {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| writeln!(file, "{received_prompt}\n---"));
    }
    if call.delay_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(call.delay_ms));
    }
    for (path, text) in &call.write_files {
        let path = std::path::Path::new(path);
        if path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            eprintln!("fake-codex: edit path must be relative without traversal");
            return 9;
        }
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                eprintln!("fake-codex: cannot prepare edit: {error}");
                return 9;
            }
        }
        if let Err(error) = std::fs::write(path, text) {
            eprintln!("fake-codex: cannot apply edit: {error}");
            return 9;
        }
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut emit = |value: serde_json::Value| {
        let _ = writeln!(out, "{}", serde_json::to_string(&value).unwrap());
        let _ = out.flush();
    };
    if let Some(session_id) = &call.session_id {
        emit(serde_json::json!({"type": "thread.started", "thread_id": session_id}));
    }
    for event in &call.events {
        emit(event.clone());
    }
    let indexed_message = call.indexed_aspect_reply.as_ref().and_then(|template| {
        let rows = envelope.as_ref()?["aspects"].as_array()?;
        let mut replies = Vec::new();
        for row in rows {
            let index = row["index"].as_u64()?;
            let mut reply = template.as_object()?.clone();
            reply.insert("index".into(), serde_json::json!(index));
            if call.copy_aspect_answer {
                reply.insert("answer".into(), serde_json::json!(row["answer"].as_str()?));
            }
            replies.push(serde_json::Value::Object(reply));
        }
        Some(serde_json::json!({"aspects":replies}).to_string())
    });
    if call.indexed_aspect_reply.is_some() && indexed_message.is_none() {
        eprintln!("fake-codex: indexed reply requires a JSON aspects array with integer indices");
        return 7;
    }
    if let Some(final_message) = call.final_message.as_ref().or(indexed_message.as_ref()) {
        emit(serde_json::json!({
            "type": "item.completed",
            "item": {"id": "item_final", "type": "agent_message", "text": final_message}
        }));
    }
    drop(out);
    if let Some(stderr) = &call.stderr {
        eprint!("{stderr}");
    }
    call.exit_code
}
