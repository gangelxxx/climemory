use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    time::Instant,
};

fn init(root: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cm"))
        .arg("init")
        .current_dir(root)
        .output()
        .unwrap()
}
#[test]
fn init_registers_native_mcp_idempotently_and_preserves_user_configuration() {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir(d.path().join(".codex")).unwrap();
    let path = d.path().join(".codex/config.toml");
    fs::write(
        &path,
        "# user comment\nmodel = 'custom'\n[mcp_servers.other]\ncommand = 'other'\n",
    )
    .unwrap();
    assert!(init(d.path()).status.success());
    let first = fs::read_to_string(&path).unwrap();
    let doc = first.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(doc["mcp_servers"]["cm"]["args"][0].as_str(), Some("--mcp"));
    assert_eq!(
        doc["mcp_servers"]["other"]["command"].as_str(),
        Some("other")
    );
    assert!(first.contains("# user comment"));
    assert!(init(d.path()).status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), first);
    fs::write(&path, first.replace("enabled = true", "enabled = false")).unwrap();
    assert!(init(d.path()).status.success());
    assert!(fs::read_to_string(path)
        .unwrap()
        .contains("enabled = false"));
    assert!(fs::read_to_string(d.path().join("AGENTS.md"))
        .unwrap()
        .contains("MCP `ask`"));
}
#[test]
fn conflicting_or_invalid_configuration_is_preserved_before_memory_creation() {
    for text in ["[mcp_servers.cm]\ncommand='foreign'\n", "broken [toml"] {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join(".codex")).unwrap();
        let path = d.path().join(".codex/config.toml");
        fs::write(&path, text).unwrap();
        assert!(!init(d.path()).status.success());
        assert_eq!(fs::read_to_string(path).unwrap(), text);
        assert!(!d.path().join("memory").exists());
    }
}
#[test]
fn legacy_python_bridge_is_migrated_without_enabling_disabled_server() {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir(d.path().join(".codex")).unwrap();
    let path = d.path().join(".codex/config.toml");
    let args = json!([
        d.path().join("memory/runtime/integration/cm_mcp.py"),
        "--project",
        d.path(),
        "--timeout",
        "330"
    ]);
    fs::write(
        &path,
        format!("[mcp_servers.cm]\ncommand='python'\nargs={args}\nenabled=false\n"),
    )
    .unwrap();
    assert!(init(d.path()).status.success());
    let text = fs::read_to_string(path).unwrap();
    assert!(!text.contains("cm_mcp.py"));
    assert!(text.contains("--mcp"));
    assert!(text.contains("enabled=false"));
}
#[test]
fn stdio_protocol_rejects_writes_and_cancels_without_model_polling() {
    let d = tempfile::tempdir().unwrap();
    assert!(init(d.path()).status.success());
    fs::write(
        d.path().join("scenario.json"),
        json!({"calls":[{"delay_ms":5000,"final_message":"{}"}]}).to_string(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cm"))
        .arg("--mcp")
        .current_dir(d.path())
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", d.path().join("scenario.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    for (id, method, params) in [
        (1, "initialize", json!({})),
        (2, "tools/list", json!({})),
        (
            3,
            "tools/call",
            json!({"name":"ask","arguments":{"question":"init"}}),
        ),
    ] {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id);
        if id == 3 {
            assert_eq!(response["error"]["code"], -32602);
        }
    }
    let start = Instant::now();
    writeln!(input,"{}",json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ask","arguments":{"question":"Button color?"}}})).unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4}})
    )
    .unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let result: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["result"]["isError"], true);
    assert!(start.elapsed().as_secs() < 4);
    drop(input);
    assert!(child.wait().unwrap().success());
}
