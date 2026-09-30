mod common;
use serde_json::Value;
use std::{fs, path::Path};

fn records(root: &Path, args: &[&str]) -> Vec<Value> {
    let output = common::run(root, args, "").success();
    String::from_utf8_lossy(&output.get_output().stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn minimal_init_preserves_user_text_and_rejects_removed_commands() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("AGENTS.md"), "User instructions.\n").unwrap();
    fs::create_dir_all(root.join("memory/docs")).unwrap();
    fs::write(root.join("memory/docs/ui.md"), "User UI requirements").unwrap();
    common::init(root);
    assert_eq!(
        records(root, &["--version"])[0]["binary_profile"],
        "climemory-memory-v1"
    );
    common::run(root, &["version", "unexpected"], "").failure();
    let instructions = fs::read(root.join("AGENTS.md")).unwrap();
    let config = fs::read(root.join("memory/config.json")).unwrap();
    common::init(root);
    assert_eq!(instructions, fs::read(root.join("AGENTS.md")).unwrap());
    assert_eq!(config, fs::read(root.join("memory/config.json")).unwrap());
    assert_eq!(
        fs::read_to_string(root.join("memory/docs/ui.md")).unwrap(),
        "User UI requirements"
    );
    assert_eq!(
        records(root, &["context", "settings"])[0]["user_documents"]["count"],
        1
    );
    assert!(!root.join("memory/states").exists());
    assert!(!root.join("memory/thread-health").exists());
    for args in [
        vec!["state"],
        vec!["agent"],
        vec!["handoff"],
        vec!["thread", "verify"],
        vec!["context", "prepare", "task"],
        vec!["context", "task", "--view", "current"],
    ] {
        common::run(root, &args, "").failure();
    }
    assert_eq!(
        records(root, &["context", "settings"])[0]["status"],
        "empty"
    );
}

#[test]
fn historical_threads_are_readable_without_rewriting_source() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    common::init(root);
    let id = "0123456789abcdef0123456789abcdef";
    let path = common::thread_path(root, id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let source=format!("---\nformat: climemory-thread/1\nid: {id}\nslug: settings\ntitle: Settings\ncurrent_state: verified\n---\n## Current\nOld saved claim.\n## Proposal\nAn old idea.\n");
    fs::write(&path, &source).unwrap();
    let read = records(root, &["read", "settings"]);
    assert_eq!(read[0]["authority"], "historical_reference_only");
    assert!(read[0]["text"].as_str().unwrap().contains("An old idea"));
    records(root, &["bind", "settings", "--agent", "agent_medium"]);
    assert_eq!(fs::read_to_string(path).unwrap(), source);
    assert_eq!(
        records(root, &["read", "settings"])[0]["agent"],
        "agent_medium"
    );
    let note = records(root, &["read", "settings"]);
    let archive = records(root, &["read", note[0]["archive_handle"].as_str().unwrap()]);
    assert_eq!(archive[0]["text"], read[0]["text"]);
}

#[test]
fn code_search_continuations_are_executable_and_keep_project_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    common::init(root);
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("src/app.rs"),
        "fn saved() {}\nfn caller() { saved(); }\n// saved\n",
    )
    .unwrap();
    let first = records(
        root,
        &["code", "grep", "saved", "--path", "src", "--limit", "1"],
    );
    let argv = first[0]["continuation"]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect::<Vec<_>>();
    let output = common::run_raw(&argv, "").success();
    let next: Value = serde_json::from_str(
        String::from_utf8_lossy(&output.get_output().stdout)
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(next["offset"], 1);
    #[cfg(feature = "code-index")]
    {
        let found = records(root, &["code", "find", "saved", "--limit", "1"]);
        assert_eq!(found[0]["definitions"], 1);
        let argv = found[0]["continuation"]["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect::<Vec<_>>();
        common::run_raw(&argv, "").success();
    }
}

#[test]
fn explicit_model_and_unique_thread_identity_are_required() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    common::init(root);
    let created = records(root, &["create", "Settings"]);
    let id = created[0]["thread_id"].as_str().unwrap();
    common::run(root, &["create", "Collision", "--slug", id], "").failure();
    assert_eq!(records(root, &["read", id])[0]["slug"], "settings");
    common::run(root, &["create", "Settings"], "").failure();
    common::run(root, &["create", "Title", "--slug", "../escape"], "").failure();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent"]["profiles"]["agent_medium"]["model"] = Value::Null;
    fs::write(path, config.to_string()).unwrap();
    common::run(root, &["ask", "settings", "Read preferences"], "").failure();
}

#[test]
fn option_like_search_text_and_historical_pages_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    common::init(root);
    fs::write(root.join("sample.md"), "--literal\n--literal\n").unwrap();
    let first = records(
        root,
        &[
            "code",
            "grep",
            "--path",
            "sample.md",
            "--limit",
            "1",
            "--",
            "--literal",
        ],
    );
    let argv = first[0]["continuation"]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect::<Vec<_>>();
    common::run_raw(&argv, "").success();
    let path = common::thread_path(root, "0123456789abcdef0123456789abcdef");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path,format!("---\nformat: climemory-thread/1\nid: 0123456789abcdef0123456789abcdef\nslug: old\ntitle: Old\n---\n{}","\u{00e9}".repeat(4100))).unwrap();
    let first = records(root, &["read", "old"]);
    assert_eq!(first[0]["text"].as_str().unwrap().chars().count(), 4000);
    let argv = first[0]["next_argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect::<Vec<_>>();
    records(root, &["bind", "old", "--agent", "agent_medium"]);
    let result = common::run_raw(&argv, "").success();
    let second: Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
    assert_eq!(second["text"].as_str().unwrap().chars().count(), 100);
}

#[test]
fn thread_names_and_init_continuations_resolve_the_intended_project() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let init = records(root, &["init"]);
    let argv = init[0]["next_argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    let output = common::run_raw(&argv, "").success();
    let context: Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
    assert_eq!(context["project_root"], init[0]["project_root"]);
    records(root, &["create", "TA settings"]);
    assert_eq!(
        records(root, &["read", "ta-settings"])[0]["slug"],
        "ta-settings"
    );
    let slug = "ta-0123456789abcdef0123456789abcdef";
    records(root, &["create", "Session-shaped name", "--slug", slug]);
    assert_eq!(
        records(root, &["read", &format!("thread:{slug}")])[0]["slug"],
        slug
    );
}

#[test]
fn option_like_search_scopes_survive_pagination() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    common::init(root);
    fs::create_dir(root.join("--source")).unwrap();
    fs::write(
        root.join("--source/app.rs"),
        "fn saved() {}\nfn caller() { saved(); }\n// saved\n",
    )
    .unwrap();
    for command in ["grep", "find"] {
        if command == "find" && !cfg!(feature = "code-index") {
            continue;
        }
        let first = records(
            root,
            &["code", command, "saved", "--path=--source", "--limit", "1"],
        );
        let argv = first[0]["continuation"]["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        common::run_raw(&argv, "").success();
    }
}
