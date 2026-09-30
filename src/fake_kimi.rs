//! Scripted Kimi Code CLI test double. It is activated only when cm is
//! launched with `CM_FAKE_KIMI_SCENARIO` and a Kimi print-mode argv.

use serde::Deserialize;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Scenario {
    state_file: PathBuf,
    calls: Vec<Call>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // a typo'd fixture key must fail, not be ignored
struct Call {
    #[serde(default)]
    delay_ms: u64,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    events: Vec<serde_json::Value>,
    #[serde(default)]
    final_message: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
    #[serde(default)]
    exit_code: i32,
    #[serde(default)]
    expect_resume_session: Option<String>,
    #[serde(default)]
    expect_model: Option<String>,
    #[serde(default)]
    expect_plan: Option<bool>,
    #[serde(default)]
    expect_prompt_contains: Vec<String>,
}

pub fn run(args: &[String]) -> i32 {
    let mut session = None;
    let mut model = None;
    let mut prompt = None;
    let mut print_mode = false;
    let mut plan_mode = false;
    let mut text_input = false;
    let mut stream_json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--session" | "--resume" | "-S" | "-r" => {
                session = args.get(index + 1).cloned();
                index += 1;
            }
            "--model" | "-m" => {
                model = args.get(index + 1).cloned();
                index += 1;
            }
            "--prompt" | "-p" => {
                prompt = args.get(index + 1).cloned();
                index += 1;
            }
            "--print" => print_mode = true,
            "--plan" => plan_mode = true,
            "--input-format" => {
                text_input = args.get(index + 1).is_some_and(|value| value == "text");
                index += 1;
            }
            "--output-format" => {
                stream_json = args
                    .get(index + 1)
                    .is_some_and(|value| value == "stream-json");
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    if !print_mode || !text_input || !stream_json {
        eprintln!("fake-kimi: expected --print --input-format text --output-format stream-json");
        return 2;
    }
    let prompt = match prompt {
        Some(prompt) => prompt,
        None => {
            let mut prompt = String::new();
            if std::io::stdin().read_to_string(&mut prompt).is_err() || prompt.is_empty() {
                eprintln!("fake-kimi: expected a prompt on stdin");
                return 2;
            }
            prompt
        }
    };
    let Some(scenario_path) = std::env::var_os("CM_FAKE_KIMI_SCENARIO") else {
        eprintln!("fake-kimi: CM_FAKE_KIMI_SCENARIO is not set");
        return 2;
    };
    let scenario: Scenario = match std::fs::read_to_string(&scenario_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
    {
        Some(scenario) => scenario,
        None => {
            eprintln!(
                "fake-kimi: cannot read the scenario {}",
                scenario_path.to_string_lossy()
            );
            return 2;
        }
    };
    let calls_seen = std::fs::read_to_string(&scenario.state_file)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("calls_seen").and_then(|seen| seen.as_u64()))
        .unwrap_or(0) as usize;
    let _ = std::fs::write(
        &scenario.state_file,
        format!("{{\"calls_seen\": {}}}\n", calls_seen + 1),
    );
    let Some(call) = scenario.calls.get(calls_seen) else {
        eprintln!(
            "fake-kimi: the scenario is exhausted ({} calls scripted)",
            scenario.calls.len()
        );
        return 5;
    };
    if let Some(expected) = &call.expect_resume_session {
        if session.as_deref() != Some(expected) {
            eprintln!("fake-kimi: expected --session {expected}, got {session:?}");
            return 3;
        }
    }
    if let Some(expected) = &call.expect_model {
        if model.as_deref() != Some(expected) {
            eprintln!("fake-kimi: expected --model {expected}, got {model:?}");
            return 4;
        }
    }
    if let Some(expected) = call.expect_plan {
        if plan_mode != expected {
            eprintln!("fake-kimi: expected --plan={expected}, got {plan_mode}");
            return 8;
        }
    }
    for expected in &call.expect_prompt_contains {
        if !prompt.contains(expected) {
            eprintln!("fake-kimi: expected prompt to contain {expected:?}");
            return 7;
        }
    }
    if call.delay_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(call.delay_ms));
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut emit = |value: serde_json::Value| {
        let _ = writeln!(out, "{}", serde_json::to_string(&value).unwrap());
        let _ = out.flush();
    };
    for event in &call.events {
        emit(event.clone());
    }
    if let Some(final_message) = &call.final_message {
        emit(serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": final_message}]
        }));
    }
    if let Some(session_id) = &call.session_id {
        emit(serde_json::json!({
            "role": "meta",
            "type": "session.resume_hint",
            "session_id": session_id,
            "resume_command": format!("kimi --session {session_id}")
        }));
    }
    drop(out);
    if let Some(stderr) = &call.stderr {
        eprint!("{stderr}");
    }
    call.exit_code
}
