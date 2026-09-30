use serde_json::json;
use std::{fs, process::Command};

#[test]
fn translated_provider_view_preserves_names_and_log_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Подробные логи с данными");
    fs::create_dir(&root).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_cm"))
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(run(&["init"]).status.success());
    let name = "Отправка тестовых запросов с данными";
    fs::write(
        root.join("memory/config.json"),
        json!({
            "agent":{"profiles":{},"providers":{name:{"adapter":"codex"}}},
            "memory":{"agent_logs":{"enabled":true}}
        })
        .to_string(),
    )
    .unwrap();
    for (flag, label) in [
        ("-en", "NOT TESTED"),
        ("-ru", "НЕ ПРОВЕРЕН"),
        ("-zh", "未测试"),
    ] {
        let output = run(&["-test_providers", "-pretty", flag]);
        assert!(!output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(name), "{text}");
        assert!(text.contains("Подробные логи с данными"), "{text}");
        assert!(text.contains(label), "{text}");
    }
}
