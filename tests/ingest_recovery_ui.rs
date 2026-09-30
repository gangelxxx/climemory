use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, process::Command};

#[test]
fn pretty_import_explains_pending_batches_and_unchecked_events_in_every_language() {
    for (language, pending, recovered) in [
        (
            "-en",
            "A batch is pending",
            "Newer events have not been checked",
        ),
        (
            "-ru",
            "Пакет ожидает сохранения",
            "Новые события ещё не проверены",
        ),
        ("-zh", "有待保存的批次", "尚未检查较新的事件"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(Command::new(env!("CARGO_BIN_EXE_cm"))
            .current_dir(root)
            .arg("init")
            .output()
            .unwrap()
            .status
            .success());
        fs::write(root.join("memory/config.json"), json!({"memory":{"chat_agent":"cheap","timeout_seconds":5,"agent_retries":{"max_attempts":1}},"agent":{"profiles":{"cheap":{"provider":"codex","model":"test"}}}}).to_string()).unwrap();
        let home = root.join("codex");
        fs::create_dir_all(home.join("sessions")).unwrap();
        let session = "11111111-2222-3333-4444-555555555555";
        fs::write(home.join(format!("sessions/rollout-{session}.jsonl")), format!("{}\n{}\n", json!({"type":"session_meta","payload":{"id":session}}), json!({"type":"response_item","payload":{"id":"request","type":"message","role":"user","content":[{"type":"input_text","text":"Save should be green"}]}}))).unwrap();
        let scenario = root.join("scenario.json");
        fs::write(&scenario, json!({"state_file":root.join("failed-calls.json"),"calls":[{"final_message":"invalid"},{"final_message":"invalid"}]}).to_string()).unwrap();
        let run = || {
            Command::new(env!("CARGO_BIN_EXE_cm"))
                .current_dir(root)
                .args(["ingest-session", "-pretty", language])
                .env("CODEX_HOME", &home)
                .env("CODEX_THREAD_ID", session)
                .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
                .env("CM_FAKE_CODEX_SCENARIO", &scenario)
                .env_remove("CM_CHAT_INTERNAL")
                .env_remove("CM_CONTEXT_INTERNAL")
                .env_remove("CM_DOCS_INTERNAL")
                .output()
                .unwrap()
        };
        let failed = run();
        assert!(!failed.status.success());
        let text = String::from_utf8(failed.stdout).unwrap();
        assert!(text.contains(pending), "{text}");
        assert!(text.contains("cm ingest-session"));
        let id = format!("e-{}", &format!("{:x}", Sha256::digest(b"request"))[..20]);
        let reply: Value = json!({"summary":"Save should be green","updates":[{"key":"settings","title":"Settings","memory":"Save should be green","claims":[{"kind":"requirement","status":"requested","text":"Save should be green","sources":[id]}],"related":[]}]});
        fs::write(&scenario, json!({"state_file":root.join("recovery-calls.json"),"calls":[{"final_message":reply.to_string()}]}).to_string()).unwrap();
        let result = run();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.contains(recovered), "{text}");
        assert!(text.contains("cm ingest-session"));
        let repeated = run();
        assert!(repeated.status.success());
        let text = String::from_utf8(repeated.stdout).unwrap();
        assert!(
            !text.contains(pending) && !text.contains(recovered),
            "{text}"
        );
    }
}
