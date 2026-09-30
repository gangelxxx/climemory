use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir_all(d.path().join("memory/docs")).unwrap();
    fs::write(d.path().join("memory/config.json"), json!({"memory":{"documents_as_threads":true,"documents_agent":"cheap","chat_agent":"cheap","verification_agent":null,"timeout_seconds":5,"agent_retries":{"max_attempts":1}},"agent":{"profiles":{"cheap":{"provider":"codex","model":"test","reasoning_effort":"low"}}}}).to_string()).unwrap();
    script(d.path(), vec![]);
    d
}
fn script(root: &Path, calls: Vec<Value>) {
    let mut value = if calls.is_empty() {
        json!({"operation_calls":{"docs_build":{"final_message":json!({"summary":"Source topics","groups":[]}).to_string(),"expect_no_native_tools":true,"expect_output_schema":true}}})
    } else {
        json!({"calls":calls})
    };
    if value.get("calls").is_none() {
        value["calls"] = json!([]);
    }
    value["state_file"] = json!(root.join(format!(
        "calls-{}.json",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    fs::write(root.join("scenario.json"), value.to_string()).unwrap();
}
fn command(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_cm"));
    c.current_dir(root)
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", root.join("scenario.json"));
    c
}
fn run(root: &Path, args: &[&str]) -> Output {
    command(root).args(args).output().unwrap()
}
fn value(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn status(root: &Path) -> Value {
    value(run(root, &["docs", "status"]))
}
fn build(root: &Path) -> Value {
    value(run(root, &["docs", "build"]))
}
fn doc(root: &Path, name: &str, text: &str) {
    fs::write(root.join("memory/docs").join(name), text).unwrap();
}

fn native_files(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, String> {
    fn visit(dir: &Path, files: &mut std::collections::BTreeMap<std::path::PathBuf, String>) {
        if !dir.exists() {
            return;
        }
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().is_some_and(|s| s == "md") {
                files.insert(path.clone(), fs::read_to_string(path).unwrap());
            }
        }
    }
    let mut files = Default::default();
    visit(&root.join("memory/threads"), &mut files);
    files
}

#[test]
fn native_threads_migrate_repair_and_prune_without_touching_user_threads() {
    let d = fixture();
    let root = d.path();
    doc(
        root,
        "rules.md",
        &format!(
            "# First\nOne rule.\n## Second\nAnother rule.\n{}",
            " ".repeat(2100)
        ),
    );
    let initial = build(root);
    let files = native_files(root);
    assert_eq!(files.len() as u64, initial["threads"].as_u64().unwrap());
    assert_eq!(files.len(), 3);
    for (path, text) in &files {
        let id = path.file_stem().unwrap().to_str().unwrap();
        assert!(text.starts_with("---\nformat: climemory-memory-thread/1\n"));
        assert!(text.contains(&format!("id: {id}\n")));
        assert!(text.contains("Source: memory/docs/rules.md\nSource SHA-256: "));
        let binding: Value = serde_json::from_slice(
            &fs::read(root.join(format!("memory/thread-agents/{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(binding["agent"], "cheap");
        assert_eq!(binding["document"][0], "memory/docs/rules.md");
        if let Some(parent) = binding["parent"].as_str() {
            assert!(text.contains(&format!("Parent: thread:{parent}")));
            assert!(files.keys().any(|p| p.file_stem().unwrap() == parent));
        }
    }
    // Simulate the old state-only layout; migration must use saved model work.
    for file in files.keys() {
        fs::remove_file(file).unwrap();
    }
    for file in fs::read_dir(root.join("memory/thread-agents")).unwrap() {
        fs::remove_file(file.unwrap().path()).unwrap();
    }
    let state_path = root.join("memory/runtime/docs/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    for document in state["documents"].as_object_mut().unwrap().values_mut() {
        document.as_object_mut().unwrap().remove("updated");
        document.as_object_mut().unwrap().remove("generation");
    }
    fs::write(state_path, state.to_string()).unwrap();
    script(root, vec![json!({"final_message":"must not call model"})]);
    assert_eq!(status(root)["status"], "outdated");
    let migrated = build(root);
    assert_eq!(migrated["processed"], 0);
    assert_eq!(migrated["skipped"], 1);
    assert_eq!(migrated["status"], "up_to_date");
    assert_eq!(native_files(root), files);
    let missing = files.keys().next().unwrap();
    let binding = root.join(format!(
        "memory/thread-agents/{}.json",
        missing.file_stem().unwrap().to_str().unwrap()
    ));
    fs::remove_file(&binding).unwrap();
    assert_eq!(status(root)["ready_documents"], 0);
    assert_eq!(build(root)["status"], "up_to_date");
    fs::remove_file(missing).unwrap();
    assert_eq!(status(root)["ready_documents"], 0);
    assert_eq!(build(root)["status"], "up_to_date");
    let user_file = root.join("memory/threads/user.md");
    let user = "---\nformat: climemory-memory-thread/1\nid: abcdef1234567890\nslug: user-note\ntitle: User note\narea: memory\n---\nKeep this note.\n";
    fs::write(&user_file, user).unwrap();
    doc(root, "rules.md", "A replacement rule.");
    script(root, vec![]);
    assert_eq!(build(root)["threads"], 1);
    assert_eq!(native_files(root).len(), 2);
    assert_eq!(
        fs::read_dir(root.join("memory/thread-agents"))
            .unwrap()
            .count(),
        1
    );
    // A deleted Markdown must not leave a dangling agent after source removal.
    for path in native_files(root).keys().filter(|p| *p != &user_file) {
        fs::remove_file(path).unwrap();
    }
    fs::remove_file(root.join("memory/docs/rules.md")).unwrap();
    assert_eq!(build(root)["status"], "up_to_date");
    assert_eq!(
        native_files(root),
        std::collections::BTreeMap::from([(user_file, user.into())])
    );
    assert_eq!(
        fs::read_dir(root.join("memory/thread-agents"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn native_publication_preserves_foreign_thread_identities() {
    let d = fixture();
    let root = d.path();
    doc(root, "rule.md", "A rule.");
    build(root);
    let (path, text) = native_files(root).into_iter().next().unwrap();
    let foreign = text.replace("area: document-import", "area: memory");
    fs::write(&path, &foreign).unwrap();
    let output = run(root, &["docs", "build"]);
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["errors"]["memory/docs/rule.md"]
        .as_str()
        .unwrap()
        .contains("identity collision"));
    assert_eq!(result["status"], "outdated");
    assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
    let binding = root.join(format!(
        "memory/thread-agents/{}.json",
        path.file_stem().unwrap().to_str().unwrap()
    ));
    let saved_binding = fs::read(&binding).unwrap();
    fs::remove_file(root.join("memory/docs/rule.md")).unwrap();
    assert!(
        !run(root, &["docs", "build"]).status.success(),
        "pruning must check ownership before removing the associated binding"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), foreign);
    assert_eq!(fs::read(binding).unwrap(), saved_binding);
}

#[test]
fn hashes_track_content_paths_additions_deletions_and_incremental_rebuilds() {
    let d = fixture();
    let root = d.path();
    doc(root, "a.md", "First rule.");
    doc(root, "b.md", "Second rule.");
    let before = status(root);
    assert_eq!(before["status"], "not_built");
    assert!(before["threads_docs_hash"].is_null());
    let dry = value(run(root, &["docs", "build", "--dry-run"]));
    assert_eq!(dry["dry_run"], true);
    assert!(!root.join("memory/runtime/docs/state.json").exists());
    let initial = build(root);
    assert_eq!(initial["status"], "up_to_date");
    assert_eq!(initial["processed"], 2);
    assert_eq!(initial["docs_hash"], initial["threads_docs_hash"]);
    // Same bytes and paths preserve the hash regardless of write time.
    doc(root, "a.md", "First rule.");
    assert_eq!(status(root)["docs_hash"], initial["docs_hash"]);
    let unchanged = build(root);
    assert_eq!(unchanged["processed"], 0);
    assert_eq!(unchanged["skipped"], 2);
    doc(root, "a.md", "Updated rule.");
    fs::remove_file(root.join("memory/docs/b.md")).unwrap();
    doc(root, "c.md", "Third rule.");
    let stale = status(root);
    assert_eq!(stale["status"], "outdated");
    assert_eq!(stale["threads_docs_hash"], initial["docs_hash"]);
    assert_ne!(stale["docs_hash"], initial["docs_hash"]);
    assert_eq!(stale["added"], json!(["memory/docs/c.md"]));
    assert_eq!(stale["changed"], json!(["memory/docs/a.md"]));
    assert_eq!(stale["deleted"], json!(["memory/docs/b.md"]));
    let rebuilt = build(root);
    assert_eq!(rebuilt["status"], "up_to_date");
    assert_eq!(rebuilt["removed"], 1);
    fs::rename(
        root.join("memory/docs/c.md"),
        root.join("memory/docs/renamed.md"),
    )
    .unwrap();
    assert_ne!(status(root)["docs_hash"], rebuilt["docs_hash"]);
    let renamed = build(root);
    assert_eq!(renamed["processed"], 1);
    assert_eq!(renamed["skipped"], 1);
    fs::remove_file(root.join("memory/docs/a.md")).unwrap();
    fs::remove_file(root.join("memory/docs/renamed.md")).unwrap();
    let empty = build(root);
    assert_eq!(empty["status"], "up_to_date");
    assert_eq!(empty["threads"], 0);
    assert_eq!(empty["docs_hash"], empty["threads_docs_hash"]);
}

#[test]
fn partial_build_keeps_checkpoints_and_resume_skips_completed_documents() {
    let d = fixture();
    let root = d.path();
    doc(root, "a.md", "First rule.");
    doc(root, "b.md", "Second rule.");
    script(
        root,
        vec![
            json!({"final_message":"{\"summary\":\"First\",\"groups\":[]}"}),
            json!({"final_message":"not JSON"}),
        ],
    );
    let out = run(root, &["docs", "build"]);
    assert!(!out.status.success());
    let partial: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(partial["processed"], 1);
    assert_eq!(partial["incomplete_build"], true);
    assert!(partial["threads_docs_hash"].is_null());
    assert_eq!(status(root)["ready_documents"], 1);
    script(root, vec![]);
    let complete = build(root);
    assert_eq!(complete["skipped"], 1);
    assert_eq!(complete["processed"], 1);
    assert_eq!(complete["status"], "up_to_date");
    // A later failure must preserve the previous complete snapshot hash.
    doc(root, "b.md", "Changed again.");
    script(root, vec![json!({"final_message":"not JSON"})]);
    let out = run(root, &["docs", "build"]);
    assert!(!out.status.success());
    assert_eq!(
        status(root)["threads_docs_hash"],
        complete["threads_docs_hash"]
    );
}

#[test]
fn changes_during_build_do_not_label_new_content_as_built() {
    let d = fixture();
    let root = d.path();
    doc(root, "a.md", "Original rule.");
    let original = status(root)["docs_hash"].clone();
    let marker = root.join("building.txt");
    script(
        root,
        vec![
            json!({"final_message":json!({"summary":"Original","groups":[]}).to_string(),"delay_ms":2000,"save_prompt_to":marker}),
        ],
    );
    let child = command(root)
        .args(["docs", "build"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "builder did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(status(root)["incomplete_build"], true);
    assert!(
        !run(root, &["docs", "build"]).status.success(),
        "concurrent builders must not overwrite checkpoints"
    );
    doc(root, "a.md", "Edited during build.");
    let result = value(child.wait_with_output().unwrap());
    assert_eq!(result["status"], "outdated");
    assert_eq!(result["threads_docs_hash"], original);
    assert_ne!(result["docs_hash"], original);
    assert_eq!(result["ready_documents"], 0);
    script(root, vec![]);
    assert_eq!(build(root)["status"], "up_to_date");
}

#[test]
fn semantic_groups_are_explicit_validated_and_bound_to_originals() {
    let d = fixture();
    let root = d.path();
    let original = format!(
        "One.\nTwo.\nThree.\nFour.\nFive.\nSix.\nSeven.\nEight.\n{}",
        " ".repeat(2100)
    );
    doc(root, "a.md", &original);
    let reply = json!({"summary":"Two topics","groups":[{"title":"First","fragments":["1","2","3","4"]},{"title":"Second","fragments":["5","6","7","8"]}]});
    script(
        root,
        vec![
            json!({"final_message":reply.to_string(),"expect_no_native_tools":true,"expect_output_schema":true}),
        ],
    );
    assert_eq!(build(root)["threads"], 3);
    let state: Value =
        serde_json::from_slice(&fs::read(root.join("memory/runtime/docs/state.json")).unwrap())
            .unwrap();
    let threads = state["documents"]["memory/docs/a.md"]["threads"]
        .as_array()
        .unwrap();
    assert_eq!(
        threads
            .iter()
            .map(|t| t["fragments"].as_array().unwrap().len())
            .sum::<usize>(),
        8
    );
    assert_eq!(
        fs::read_to_string(root.join("memory/docs/a.md")).unwrap(),
        original
    );
    // Corruption cannot remain current just because the source hash matches.
    let mut broken = state;
    broken["documents"]["memory/docs/a.md"]["threads"][1]["fragments"][0]["text"] =
        json!("fabricated");
    fs::write(
        root.join("memory/runtime/docs/state.json"),
        broken.to_string(),
    )
    .unwrap();
    assert_eq!(status(root)["status"], "outdated");
    let invalid = json!({"summary":"Invalid","groups":[{"title":"A","fragments":["1","2"]},{"title":"B","fragments":["2","3"]}]});
    script(root, vec![json!({"final_message":invalid.to_string()})]);
    assert!(!run(root, &["docs", "build"]).status.success());
    assert_eq!(status(root)["ready_documents"], 0);
    script(root, vec![]);
    assert_eq!(build(root)["status"], "up_to_date");
}

#[test]
fn build_does_not_enable_threads_and_internal_workers_cannot_mutate() {
    let d = fixture();
    let root = d.path();
    doc(root, "a.md", "A rule.");
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["documents_as_threads"] = json!(false);
    fs::write(&config_path, config.to_string()).unwrap();
    let original = fs::read(&config_path).unwrap();
    let blocked = command(root)
        .args(["docs", "build"])
        .env("CM_CONTEXT_INTERNAL", root)
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(!root.join("memory/runtime/docs/state.json").exists());
    let result = build(root);
    assert_eq!(result["search_uses_threads"], false);
    assert_eq!(fs::read(&config_path).unwrap(), original);
    let pretty = run(root, &["docs", "status", "-pretty", "-ru"]);
    assert!(pretty.status.success());
    assert!(String::from_utf8_lossy(&pretty.stdout).contains("актуальны"));
    assert!(!run(root, &["docs", "status", "--dry-run"]).status.success());
}

#[test]
fn single_fragment_sections_have_consistent_provider_schema_bounds() {
    let d = fixture();
    let root = d.path();
    let config_path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["agent_logs"] = json!({"enabled":true});
    fs::write(config_path, config.to_string()).unwrap();
    // The parent owns only its heading; the child is large enough for grouping.
    doc(
        root,
        "sections.md",
        &format!(
            "# Title\n## Body\nOne.\nTwo.\nThree.\nFour.\nFive.\nSix.\nSeven.\nEight.\n{}",
            " ".repeat(2100)
        ),
    );
    assert_eq!(build(root)["status"], "up_to_date");
    let mut saw_single = false;
    let mut saw_partition = false;
    for file in fs::read_dir(root.join("memory/runtime/agent-logs")).unwrap() {
        let log = fs::read_to_string(file.unwrap().path()).unwrap();
        for line in log.lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            if row["event"] != "call_started" {
                continue;
            }
            let groups = &row["data"]["schema"]["properties"]["groups"];
            let fragments = &groups["items"]["properties"]["fragments"];
            let minimum = fragments["minItems"].as_u64().unwrap_or(0);
            let maximum = fragments["maxItems"].as_u64().unwrap();
            assert!(
                minimum <= maximum,
                "provider cannot compile contradictory bounds: {fragments}"
            );
            if maximum == 1 {
                saw_single = true;
                assert_eq!(groups["maxItems"], 0);
            }
            if groups["maxItems"] == 8 {
                saw_partition = true;
                assert_eq!(minimum, 2);
            }
        }
    }
    assert!(saw_single && saw_partition);
}

#[test]
fn invalid_partition_gets_one_correction_without_losing_originals() {
    for repaired in [true, false] {
        let d = fixture();
        let root = d.path();
        let original = format!(
            "One.\nTwo.\nThree.\nFour.\nFive.\nSix.\nSeven.\nEight.\n{}",
            " ".repeat(2100)
        );
        doc(root, "a.md", &original);
        let correct = json!({"summary":"Two topics","groups":[{"title":"First","fragments":["1","2","3","4"]},{"title":"Second","fragments":["5","6","7","8"]}]});
        let mut invalid = correct.clone();
        invalid["groups"][1]["fragments"] = json!(["5", "6", "7", "8", "1"]);
        script(
            root,
            vec![
                json!({"final_message":invalid.to_string()}),
                json!({"final_message":if repaired {correct.to_string()} else {invalid.to_string()},"expect_prompt_contains":["previous_validation_error","duplicate_ids","previous_response"]}),
                json!({"final_message":correct.to_string()}),
            ],
        );
        let out = run(root, &["docs", "build"]);
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(out.status.success(), repaired, "{result}");
        assert_eq!(result["ready_documents"], if repaired { 1 } else { 0 });
        if repaired {
            assert_eq!(result["threads"], 3);
        } else {
            assert!(result["errors"]["memory/docs/a.md"]
                .as_str()
                .unwrap()
                .contains("duplicate_ids"));
        }
        assert_eq!(
            fs::read_to_string(root.join("memory/docs/a.md")).unwrap(),
            original
        );
    }
}
