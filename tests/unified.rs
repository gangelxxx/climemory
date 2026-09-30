use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[path = "../src/unified/compact_wire.rs"]
mod compact_wire;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))[..16].to_owned()
}
fn source() -> String {
    format!("doc-{}", hash("memory/docs/ui.md"))
}
fn fid(n: usize) -> String {
    format!("{}:L{n}", source())
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("memory/docs")).unwrap();
    fs::write(dir.path().join("memory/config.json"),json!({"memory":{"documents_as_threads":true,"unified":{"concurrency":1},"documents_agent":"cheap","chat_agent":"cheap","verification_agent":"cheap","timeout_seconds":20,"max_steps":12,"agent_retries":{"max_attempts":1}},"agent":{"profiles":{"cheap":{"provider":"codex","model":"test-model","reasoning_effort":"low"}}}}).to_string()).unwrap();
    fs::write(
        dir.path().join("memory/docs/ui.md"),
        "Buttons blue.\nDeletion buttons red.",
    )
    .unwrap();
    dir
}
fn selected(ids: Vec<String>) -> Value {
    json!({"select":ids,"elements":[],"summary":"Button colors","questions":["What color are buttons?"],"links":[],"checked":[],"need":[],"gaps":[]})
}
fn assembled(ids: Vec<String>) -> Value {
    json!({"answer":"Buttons are blue; deletion buttons are red.","select":ids,"aspects":[{"question":"Button colors and exceptions","status":"found","evidence":ids}],"need":[],"conflicts":[]})
}
fn assembled_aspects(ids: Vec<String>, questions: &[&str]) -> Value {
    let mut answer = assembled(ids.clone());
    answer["aspects"] = json!(questions
        .iter()
        .map(|question| json!({"question":question,"status":"found","evidence":ids}))
        .collect::<Vec<_>>());
    answer
}
fn route(ids: Vec<String>) -> Value {
    let mut value = selected(vec![]);
    value["need"] = json!(ids);
    value
}
fn branch(ids: Vec<String>) -> Value {
    json!({"summary":"Requested rules with exceptions.","evidence":ids,"gaps":[],"links":[]})
}
fn ordered_workers(mut workers: Vec<(String, Value)>) -> Vec<Value> {
    workers.sort_by(|a, b| a.0.cmp(&b.0));
    workers.into_iter().map(|(_, v)| v).collect()
}
fn scenario(root: &Path, values: Vec<Value>) {
    fs::write(root.join("scenario.json"),json!({"state_file":root.join(format!("calls-{}.json",hash(&serde_json::to_string(&values).unwrap()))),"operation_calls":{"unified_grounding":{"indexed_aspect_reply":{"supported":true,"answer":"","self_contained":false},"copy_aspect_answer":true,"expect_no_native_tools":true,"expect_output_schema":true,"save_prompt_to":root.join("grounding-prompts.txt")}},"calls":values.into_iter().map(|v|json!({"final_message":v.to_string(),"expect_no_native_tools":true,"expect_output_schema":true})).collect::<Vec<_>>()} ).to_string()).unwrap();
}
fn run(root: &Path, msg: &str) -> Output {
    let path = root.join("scenario.json");
    if let Ok(bytes) = fs::read(&path) {
        let mut script: Value = serde_json::from_slice(&bytes).unwrap();
        if script.get("operation_calls").is_none() {
            script["operation_calls"] = json!({"unified_grounding":{"indexed_aspect_reply":{"supported":true,"answer":"","self_contained":false},"copy_aspect_answer":true,"expect_no_native_tools":true,"expect_output_schema":true,"save_prompt_to":root.join("grounding-prompts.txt")}});
            fs::write(path, script.to_string()).unwrap();
        }
    }
    prepare_plan(root, msg);
    Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(root)
        .arg(msg)
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", root.join("scenario.json"))
        .output()
        .unwrap()
}
// Hierarchy fixtures explicitly prepare document threads before exercising retrieval.
fn prepare_documents(root: &Path) {
    fn has_large_documents(path: &Path) -> bool {
        fs::read_dir(path).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    has_large_documents(&path)
                } else {
                    fs::metadata(path).is_ok_and(|meta| meta.len() > 2048)
                }
            })
        })
    }
    if !has_large_documents(&root.join("memory/docs")) {
        return;
    }
    let config: Value =
        serde_json::from_slice(&fs::read(root.join("memory/config.json")).unwrap()).unwrap();
    if config["memory"]["documents_as_threads"] != true {
        return;
    }
    let script = root.join("docs-scenario.json");
    fs::write(&script, json!({"state_file":root.join("docs-calls.json"),"calls":[],"operation_calls":{"docs_build":{"final_message":json!({"summary":"","groups":[]}).to_string(),"expect_no_native_tools":true,"expect_output_schema":true}}}).to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(root)
        .args(["docs", "build"])
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", &script)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn prepare_plan(root: &Path, msg: &str) {
    prepare_documents(root);
    if msg.starts_with("@context:") && msg.ends_with(" @details") {
        return;
    }
    let _question = if msg.starts_with("@context:") {
        let (id, q) = msg.split_once(' ').unwrap();
        if let Ok(bytes) =
            fs::read(root.join(format!("memory/runtime/unified/sessions/{}.json", &id[9..])))
        {
            let c: Value = serde_json::from_slice(&bytes).unwrap();
            if c["question"] == q
                && c["requested_aspects"]
                    .as_array()
                    .is_some_and(|v| !v.is_empty())
                && c["requested_intents"]
                    .as_array()
                    .is_some_and(|v| !v.is_empty())
            {
                return;
            }
        }
        q
    } else {
        msg
    };
    let path = root.join("scenario.json");
    let Ok(bytes) = fs::read(&path) else { return };
    let mut script: Value = serde_json::from_slice(&bytes).unwrap();
    let seen = fs::read(script["state_file"].as_str().unwrap())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["calls_seen"].as_u64())
        .unwrap_or(0) as usize;
    let calls = script["calls"].as_array_mut().unwrap();
    let aspects = calls
        .iter()
        .skip(seen)
        .filter_map(|c| c["final_message"].as_str())
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .find_map(|v| {
            v["aspects"].as_array().map(|a| {
                a.iter()
                    .filter_map(|a| a["question"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
        })
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| vec!["Button colors and exceptions".into()]);
    let aspects: Vec<String> = fs::read(root.join("planned-aspects.json"))
        .ok()
        .map(|b| serde_json::from_slice(&b).unwrap())
        .unwrap_or(aspects);
    let requirements: Vec<String> = fs::read(root.join("planned-sources.json"))
        .ok()
        .map(|b| serde_json::from_slice(&b).unwrap())
        .unwrap_or_else(|| vec!["any".into(); aspects.len()]);
    let intents: Vec<String> = fs::read(root.join("planned-intents.json"))
        .ok()
        .map(|b| serde_json::from_slice(&b).unwrap())
        .unwrap_or_else(|| {
            requirements
                .into_iter()
                .map(|r| match r.as_str() {
                    "user_document" => "original_requirement".into(),
                    "any" => "factual_question".into(),
                    _ => r,
                })
                .collect()
        });
    let mut plan = json!({"aspects":aspects,"intents":intents});
    if let Ok(bytes) = fs::read(root.join("planned-presentation.json")) {
        plan["presentation_requirements"] = serde_json::from_slice(&bytes).unwrap();
    }
    calls.insert(seen.min(calls.len()),json!({"final_message":plan.to_string(),"expect_no_native_tools":true,"save_prompt_to":root.join("plan-prompt.txt")}));
    fs::write(path, script.to_string()).unwrap();
}
fn answer(root: &Path, msg: &str) -> Value {
    let out = run(root, msg);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    compact_wire::expand(serde_json::from_slice(&out.stdout).unwrap())
}
fn diagnostics(root: &Path, public: &Value) -> Value {
    let session: Value = serde_json::from_slice(
        &fs::read(root.join(format!(
            "memory/runtime/unified/sessions/{}.json",
            public["context_session"].as_str().unwrap()
        )))
        .unwrap(),
    )
    .unwrap();
    let mut value = session["response"].clone();
    value["unprocessed_thread_count"] =
        json!(value["unprocessed_threads"].as_array().map_or(0, Vec::len));
    if public["cache"] == "details" {
        value["calls_scheduled"] = json!(0);
    }
    value
}

#[test]
fn compact_wire_keeps_exact_quotes_claim_conditions_and_expanded_details() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    let a = json!({"answer":"Button colors.","select":ids,"need":[],"conflicts":[],"aspects":[
        {"question":"What colors must ordinary buttons use?","answer":"Ordinary buttons must be blue.","status":"found","evidence":[fid(1)],"self_contained":true},
        {"question":"What colors must deletion buttons use?","answer":"Deletion buttons must be red; this is the exception to ordinary blue buttons.","status":"found","evidence":[fid(2)],"self_contained":true}
    ]});
    scenario(d.path(), vec![selected(ids), a]);
    grounding_reply(
        d.path(),
        json!({"aspects":[{"index":0,"supported":true,"answer":"Ordinary buttons must be blue.","self_contained":true},{"index":1,"supported":true,"answer":"Deletion buttons must be red; this is the exception to ordinary blue buttons.","self_contained":true}]}),
    );
    let out = run(
        d.path(),
        "What are the ordinary and deletion button colors?",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let raw: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(raw["format"], "cm/compact-1");
    assert!(raw["aspects"].is_array());
    assert!(raw["aspects"][0].get("status").is_none());
    assert_eq!(
        raw["aspects"][1]["answer"],
        "Deletion buttons must be red; this is the exception to ordinary blue buttons."
    );
    assert!(raw.get("details").is_none());
    assert!(raw.get("citation_rules").is_none());
    assert!(raw["source_blocks"]["b1"].get("review").is_none());
    assert_eq!(
        raw["source_blocks"]["b1"]["numbered_lines"],
        json!([[1, "Buttons blue."], [2, "Deletion buttons red."]])
    );
    assert!(raw["evidence"]["columns"]
        .as_array()
        .unwrap()
        .contains(&json!("source_block")));
    let expanded = compact_wire::expand(raw.clone());
    assert_eq!(expanded["evidence"][0]["quote"], "Buttons blue.");
    assert_eq!(expanded["evidence"][1]["quote"], "Deletion buttons red.");
    assert!(expanded["aspects"][0].get("question").is_none());
    let details = answer(
        d.path(),
        &format!(
            "@context:{} @details",
            raw["context_session"].as_str().unwrap()
        ),
    );
    assert!(details.get("format").is_none());
    assert!(details["aspects"].is_array());
    assert_eq!(
        details["aspects"][0]["question"],
        "What colors must ordinary buttons use?"
    );
    assert!(details["aspects"][0].get("self_contained").is_none());
    assert_eq!(details["evidence"][0]["source"], "memory/docs/ui.md");
}

#[test]
fn source_context_receipts_survive_restart_update_revision_and_reject_corruption() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(
        d.path(),
        vec![selected(ids.clone()), assembled(ids.clone())],
    );
    let first = answer(d.path(), "Button requirements?");
    assert!(first["source_blocks"].is_object());
    let id = first["context_session"].as_str().unwrap();
    let path = d
        .path()
        .join(format!("memory/runtime/unified/sessions/{id}.json"));
    let before = fs::read(&path).unwrap();
    let initial: Value = serde_json::from_slice(&before).unwrap();
    let details = answer(d.path(), &format!("@context:{id} @details"));
    assert!(details.get("source_blocks").is_none());
    assert_eq!(fs::read(&path).unwrap(), before);
    let repeated = answer(d.path(), &format!("@context:{id} Button requirements?"));
    assert!(repeated.get("source_blocks").is_none());
    fs::write(
        d.path().join("memory/docs/ui.md"),
        "Buttons green.\nDeletion buttons red.",
    )
    .unwrap();
    let mut changed = assembled(ids.clone());
    changed["answer"] = json!("Buttons green; deletion red.");
    scenario(d.path(), vec![selected(ids), changed]);
    let updated = answer(
        d.path(),
        &format!("@context:{id} Updated button requirements?"),
    );
    assert_eq!(
        updated["source_blocks"]["b1"]["numbered_lines"][0][1],
        "Buttons green."
    );
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_ne!(
        state["delivered_source_context"],
        initial["delivered_source_context"]
    );
    state["delivered_source_context"]["memory/docs/ui.md"] = json!("broken");
    fs::write(path, state.to_string()).unwrap();
    let invalid = run(d.path(), &format!("@context:{id} @details"));
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid delivered source context"));
}

#[test]
fn aliases_reserve_hidden_details_and_survive_process_config_and_source_changes() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        "# Accessibility\nAll controls must support keyboard operation.",
    )
    .unwrap();
    let ids = vec![fid(1), fid(2)];
    let mut a = assembled(ids.clone());
    a["aspects"][0]["answer"] = json!("All controls must support keyboard operation.");
    scenario(d.path(), vec![selected(ids.clone()), a.clone()]);
    let first = answer(d.path(), "Keyboard requirements?");
    assert_eq!(first["evidence"].as_array().unwrap().len(), 1);
    let id = first["context_session"].as_str().unwrap();
    let path = d
        .path()
        .join(format!("memory/runtime/unified/sessions/{id}.json"));
    let before = fs::read(&path).unwrap();
    let state: Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(state["format"], 2);
    assert_eq!(
        state["evidence_aliases"]["entries"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    assert!(state["delivered_evidence"]
        .as_object()
        .unwrap()
        .keys()
        .all(|k| k.len() == 17));
    let details = answer(d.path(), &format!("@context:{id} @details"));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(details["evidence"].as_array().unwrap().len(), 2);
    assert!(details["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["ref"].as_str().unwrap().len() < 5));
    assert_eq!(details["evidence"][1]["ref"], first["evidence"][0]["ref"]);
    let repeat = answer(d.path(), &format!("@context:{id} Keyboard requirements?"));
    assert_eq!(
        repeat["reused_evidence"],
        json!([first["evidence"][0]["ref"]])
    );
    let config_path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["timeout_seconds"] = json!(21);
    fs::write(config_path, config.to_string()).unwrap();
    let mut worker = selected(ids.clone());
    worker["summary"] = json!("Configuration refresh.");
    scenario(d.path(), vec![worker, a.clone()]);
    let refreshed = answer(
        d.path(),
        &format!("@context:{id} Current keyboard requirements?"),
    );
    assert_eq!(refreshed["evidence"][0]["ref"], first["evidence"][0]["ref"]);
    fs::write(
        d.path().join("memory/docs/ui.md"),
        "# Accessibility\nAll controls must support keyboard operation and visible focus.",
    )
    .unwrap();
    let mut worker = selected(ids);
    worker["summary"] = json!("Source refresh.");
    a["aspects"][0]["answer"] =
        json!("All controls must support keyboard operation and visible focus.");
    scenario(d.path(), vec![worker, a]);
    let changed = answer(
        d.path(),
        &format!("@context:{id} Updated keyboard requirements?"),
    );
    assert_ne!(changed["evidence"][0]["ref"], first["evidence"][0]["ref"]);
    let later: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    for (key, value) in state["evidence_aliases"]["entries"].as_object().unwrap() {
        assert_eq!(&later["evidence_aliases"]["entries"][key], value);
    }
}

#[test]
fn legacy_contexts_keep_canonical_refs_but_missing_new_alias_state_fails_closed() {
    for corrupt in [false, true] {
        let d = fixture();
        let ids = vec![fid(1), fid(2)];
        scenario(d.path(), vec![selected(ids.clone()), assembled(ids)]);
        let first = answer(d.path(), "Button colors?");
        let id = first["context_session"].as_str().unwrap();
        let path = d
            .path()
            .join(format!("memory/runtime/unified/sessions/{id}.json"));
        let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state.as_object_mut().unwrap().remove("evidence_aliases");
        if !corrupt {
            state["format"] = json!(1);
        }
        fs::write(&path, state.to_string()).unwrap();
        let before = fs::read(&path).unwrap();
        let out = run(d.path(), &format!("@context:{id} @details"));
        assert_eq!(fs::read(&path).unwrap(), before);
        if corrupt {
            assert!(!out.status.success());
            assert!(out.stdout.is_empty());
            assert!(String::from_utf8_lossy(&out.stderr)
                .contains("invalid context evidence alias mode"));
        } else {
            assert!(out.status.success());
            let details: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert!(details["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["ref"].as_str().unwrap().len() == 17));
            let repeat = answer(d.path(), &format!("@context:{id} Button colors?"));
            assert!(repeat["reused_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r.as_str().unwrap().len() == 17));
            let after: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert!(after.get("evidence_aliases").is_none());
        }
    }
}

#[test]
fn alias_session_persistence_failure_never_emits_uncommitted_public_refs() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(d.path(), vec![selected(ids.clone()), assembled(ids)]);
    fs::create_dir_all(d.path().join("memory/runtime/unified")).unwrap();
    // A file blocks creation of the session directory after retrieval completes.
    fs::write(d.path().join("memory/runtime/unified/sessions"), "blocked").unwrap();
    let out = run(d.path(), "Button colors?");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(d.path().join("memory/runtime/unified/sessions")).unwrap(),
        "blocked"
    );
    let script: Value =
        serde_json::from_slice(&fs::read(d.path().join("scenario.json")).unwrap()).unwrap();
    assert!(Path::new(script["state_file"].as_str().unwrap()).exists());
}

#[test]
fn unified_original_evidence_context_cache_and_invalidation() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(
        d.path(),
        vec![
            selected(ids.clone()),
            assembled(ids.clone()),
            selected(ids.clone()),
            assembled(ids.clone()),
        ],
    );
    let first = answer(d.path(), "Button colors and exceptions?");
    let first_revision = diagnostics(d.path(), &first)["coverage"]["index_revision"].clone();
    assert_eq!(first["status"], "complete");
    assert_eq!(first["evidence"][1]["quote"], "Deletion buttons red.");
    assert_eq!(
        answer(
            d.path(),
            &format!(
                "@context:{} @details",
                first["context_session"].as_str().unwrap()
            )
        )["evidence"][1]["quote"],
        "Deletion buttons red."
    );
    let follow = format!(
        "@context:{} Button colors and exceptions?",
        first["context_session"].as_str().unwrap()
    );
    assert_eq!(answer(d.path(), &follow)["cache"], "hit");
    fs::write(
        d.path().join("memory/docs/ui.md"),
        "Buttons green.\nDeletion buttons red.",
    )
    .unwrap();
    let changed = answer(d.path(), &follow);
    assert_eq!(changed["cache"], "miss");
    assert_eq!(changed["response_mode"], "full");
    assert_eq!(
        answer(
            d.path(),
            &format!(
                "@context:{} @details",
                changed["context_session"].as_str().unwrap()
            )
        )["evidence"][0]["quote"],
        "Buttons green."
    );
    assert_ne!(
        first_revision,
        diagnostics(d.path(), &changed)["coverage"]["index_revision"]
    );
    // Original sources were never rewritten by agents.
    assert_eq!(
        fs::read_to_string(d.path().join("memory/docs/ui.md")).unwrap(),
        "Buttons green.\nDeletion buttons red."
    );
}
#[test]
fn rejects_fabricated_worker_citations_and_preserves_partial_status() {
    let d = fixture();
    scenario(
        d.path(),
        vec![
            selected(vec!["fake:L999".into()]),
            json!({"answer":"Unknown","select":[],"aspects":[{"question":"Colors","status":"missing","evidence":[]}],"need":[],"conflicts":[]}),
        ],
    );
    let a = answer(d.path(), "Button colors?");
    assert_eq!(a["status"], "partial");
    assert!(!diagnostics(d.path(), &a)["errors"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(a["evidence"].as_array().unwrap().is_empty());
}
#[test]
fn verifier_recovers_exception_omitted_by_worker() {
    let d = fixture();
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1), fid(2)])],
    );
    let a = answer(d.path(), "Button colors and exceptions?");
    assert_eq!(a["status"], "complete");
    assert_eq!(a["evidence"].as_array().unwrap().len(), 2);
}
#[test]
fn new_document_invalidates_session_and_calls_its_agent() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(
        d.path(),
        vec![selected(ids.clone()), assembled(ids.clone())],
    );
    let first = answer(d.path(), "Button colors?");
    fs::write(
        d.path().join("memory/docs/new.md"),
        "Buttons disabled are grey.",
    )
    .unwrap();
    let new_id = format!("doc-{}:L1", hash("memory/docs/new.md"));
    let all = vec![fid(1), fid(2), new_id.clone()];
    // Unchanged workers are retained; the verifier first discovers the added document before its agent is consulted.
    let mut discover = assembled(vec![fid(1), fid(2)]);
    discover["need"] = json!([format!("doc-{}", hash("memory/docs/new.md"))]);
    scenario(
        d.path(),
        vec![discover, selected(vec![new_id]), assembled(all)],
    );

    let follow = format!(
        "@context:{} Button colors?",
        first["context_session"].as_str().unwrap()
    );
    let second = answer(d.path(), &follow);
    assert_eq!(second["status"], "complete", "{second}");
    assert_eq!(diagnostics(d.path(), &second)["calls_scheduled"], 4);
    assert_eq!(
        diagnostics(d.path(), &second)["coverage"]["reviewed_threads"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
#[test]
fn session_paths_are_validated_before_io() {
    let d = fixture();
    let out = run(d.path(), "@context:../../config question");
    assert!(!out.status.success());
    assert!(!d.path().join("memory/runtime/unified").exists());
}

#[test]
fn discovered_links_and_semantic_elements_are_version_bound() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        hierarchy_document("Buttons blue.\n# Settings\nSettings have a Save button."),
    )
    .unwrap();
    let settings = section(&source(), "Settings");
    let mut ui = selected(vec![fid(1)]);
    ui["need"] = json!([settings]);
    ui["elements"] = json!([{"kind":"requirement","status":"documented","text":"Buttons blue.","evidence":[fid(1)]}]);
    let mut reply = branch(vec![fid(1), fid(3)]);
    reply["links"] = json!([{"from":source(),"target":settings,"kind":"applies_to","evidence":[fid(1),fid(3)]},{"from":"cm-routing-root","target":settings,"kind":"applies_to","evidence":[fid(1),fid(3)]}]);
    scenario(
        d.path(),
        vec![
            ui,
            selected(vec![fid(3)]),
            reply,
            assembled(vec![fid(1), fid(3)]),
        ],
    );
    let a = answer(d.path(), "docs");
    assert_eq!(a["status"], "complete", "{a}");
    let read_index = || -> Value {
        serde_json::from_slice(
            &fs::read(d.path().join("memory/runtime/unified/index.json")).unwrap(),
        )
        .unwrap()
    };
    let index = read_index();
    assert_eq!(index["links"].as_array().unwrap().len(), 1);
    assert_eq!(index["links"][0]["confirmed"], true);
    assert!(index["threads"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["elements"].as_array().is_some_and(|e| !e.is_empty())));
    fs::write(d.path().join("memory/docs/ui.md"), "Buttons blue.").unwrap();
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
    );
    let follow = format!("@context:{} docs", a["context_session"].as_str().unwrap());
    assert_eq!(answer(d.path(), &follow)["status"], "complete");
    let index = read_index();
    assert!(index["links"].as_array().unwrap().is_empty());
    assert_eq!(index["threads"].as_array().unwrap().len(), 1);
}

#[test]
fn budget_exhaustion_never_becomes_complete() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(1);
    config["memory"]["statistics"] = json!({"enabled":true});
    fs::write(&path, config.to_string()).unwrap();
    scenario(
        d.path(),
        vec![json!({"answer":"Unknown","select":[],"aspects":[],"need":[],"conflicts":[]})],
    );
    let a = answer(d.path(), "Button colors?");
    assert_eq!(a["status"], "partial");
    assert_eq!(a["unprocessed_threads"].as_array().unwrap().len(), 1);
    let stats = fs::read_dir(d.path().join("memory/runtime/statistics"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let stats: Value = serde_json::from_slice(&fs::read(stats).unwrap()).unwrap();
    assert_eq!(stats["call_counts"]["total"], 1);
}

#[test]
fn followup_reuses_only_evidence_previously_extracted_by_thread_agents() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(
        d.path(),
        vec![
            selected(ids.clone()),
            assembled(ids.clone()),
            assembled(vec![fid(2)]),
        ],
    );
    let a = answer(d.path(), "Button colors?");
    let follow = format!(
        "@context:{} What about deletion?",
        a["context_session"].as_str().unwrap()
    );
    let b = answer(d.path(), &follow);
    assert_eq!(b["status"], "complete", "{b}");
    assert_eq!(b["cache"], "reused_evidence");
    assert_eq!(diagnostics(d.path(), &b)["calls_scheduled"], 3);
    assert_eq!(b["evidence"].as_array().unwrap().len(), 0);
    assert_eq!(b["response_mode"], "delta");
    assert_eq!(b["reused_evidence"].as_array().unwrap().len(), 1);
}

#[test]
fn followup_routes_new_indexed_terms_before_one_final_verification() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/keyboard.md"),
        "Keyboard focus must be visible.",
    )
    .unwrap();
    let keyboard = format!("doc-{}:L1", hash("memory/docs/keyboard.md"));
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
    );
    let first = answer(d.path(), "Blue buttons?");
    scenario(
        d.path(),
        vec![selected(vec![keyboard.clone()]), assembled(vec![keyboard])],
    );
    let second = answer(
        d.path(),
        &format!(
            "@context:{} Keyboard focus?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(second["status"], "complete", "{second}");
    // Scope planning, owning-thread read, final verification. No preflight verifier.
    assert_eq!(diagnostics(d.path(), &second)["calls_scheduled"], 4);
    assert_eq!(
        second["evidence"][0]["quote"],
        "Keyboard focus must be visible."
    );
    let plan = fs::read_to_string(d.path().join("plan-prompt.txt")).unwrap();
    assert!(!plan.contains("Keyboard focus must be visible."));
}

#[test]
fn followup_cannot_bypass_thread_agent_for_uncached_originals() {
    let d = fixture();
    scenario(
        d.path(),
        vec![
            selected(vec![fid(1)]),
            assembled(vec![fid(1)]),
            selected(vec![fid(2)]),
            assembled(vec![fid(2)]),
        ],
    );
    let a = answer(d.path(), "Enabled button color?");
    let follow = format!(
        "@context:{} Deletion color?",
        a["context_session"].as_str().unwrap()
    );
    let b = answer(d.path(), &follow);
    assert_eq!(b["status"], "complete", "{b}");
    assert_eq!(b["cache"], "miss");
    assert_eq!(diagnostics(d.path(), &b)["calls_scheduled"], 4);
}

#[test]
fn provider_statistics_include_calls_from_worker_threads() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["statistics"] = json!({"enabled":true});
    fs::write(path, config.to_string()).unwrap();
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
    );
    assert_eq!(answer(d.path(), "Button colors?")["status"], "complete");
    let stats = fs::read_dir(d.path().join("memory/runtime/statistics"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let stats: Value = serde_json::from_slice(&fs::read(stats).unwrap()).unwrap();
    assert_eq!(stats["call_counts"]["total"], 4);
    assert_eq!(stats["totals"]["calls"], 4);
}

#[test]
fn invalid_relation_does_not_discard_valid_thread_evidence() {
    let d = fixture();
    let mut selection = selected(vec![fid(1), fid(2)]);
    selection["note"] = json!("Optional model annotation, not authoritative evidence.");
    selection["links"] =
        json!([{"target":"missing-thread","kind":"applies_to","evidence":[fid(1)]}]);
    scenario(d.path(), vec![selection, assembled(vec![fid(1), fid(2)])]);
    let a = answer(d.path(), "Button colors?");
    assert_eq!(a["status"], "complete", "{a}");
    assert_eq!(a["evidence"].as_array().unwrap().len(), 2);
    let index: Value = serde_json::from_slice(
        &fs::read(d.path().join("memory/runtime/unified/index.json")).unwrap(),
    )
    .unwrap();
    assert!(index["links"].as_array().unwrap().is_empty());
}

#[test]
fn corrupt_derived_quotes_are_rebuilt_from_unchanged_originals() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(
        d.path(),
        vec![
            selected(ids.clone()),
            assembled(ids.clone()),
            assembled(ids),
        ],
    );
    assert_eq!(answer(d.path(), "Button colors?")["status"], "complete");
    let p = d.path().join("memory/runtime/unified/index.json");
    let mut index: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    index["threads"][0]["fragments"][0]["text"] = json!("Forged color rule");
    fs::write(&p, index.to_string()).unwrap();
    let a = answer(d.path(), "Button colors?");
    assert_eq!(
        answer(
            d.path(),
            &format!(
                "@context:{} @details",
                a["context_session"].as_str().unwrap()
            )
        )["evidence"][0]["quote"],
        "Buttons blue."
    );
    let restored: Value = serde_json::from_slice(&fs::read(p).unwrap()).unwrap();
    assert_eq!(
        restored["threads"][0]["fragments"][0]["text"],
        "Buttons blue."
    );
}

#[test]
fn unresolved_requirements_and_implementation_discrepancies_have_distinct_statuses() {
    for (kind, expected) in [
        ("requirement_conflict", "partial"),
        ("implementation_discrepancy", "complete"),
    ] {
        let d = fixture();
        let ids = vec![fid(1), fid(2)];
        let mut assembled = assembled(ids.clone());
        assembled["conflicts"] =
            json!([{"kind":kind,"description":"Two differing source statements.","evidence":ids}]);
        scenario(d.path(), vec![selected(ids), assembled]);
        let a = answer(d.path(), "Button requirements?");
        assert_eq!(a["status"], expected, "{a}");
        assert_eq!(
            a["conflicts"][0]["unresolved"],
            kind == "requirement_conflict"
        );
    }
}

#[test]
fn verifier_request_for_reviewed_thread_remains_partial() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    let mut verification = assembled(ids.clone());
    verification["need"] = json!([source()]);
    scenario(d.path(), vec![selected(ids), verification]);
    let result = answer(d.path(), "Button colors?");
    assert_eq!(result["status"], "partial");
    assert_eq!(result["unprocessed_threads"].as_array().unwrap().len(), 1);
}

#[test]
fn failed_reverification_preserves_new_worker_evidence() {
    let d = fixture();
    let other = section(&source(), "Keyboard");
    let extra = fid(3);
    fs::write(
        d.path().join("memory/docs/ui.md"),
        hierarchy_document("Buttons blue.\n# Keyboard\nButtons keyboard shortcut."),
    )
    .unwrap();
    let mut first = assembled(vec![fid(1)]);
    first["need"] = json!([other]);
    scenario(
        d.path(),
        vec![
            selected(vec![fid(1)]),
            first,
            selected(vec![extra.clone()]),
            json!({"invalid":true}),
        ],
    );
    let result = answer(d.path(), "docs");
    assert_eq!(result["status"], "partial");
    let evidence = result["evidence"].as_array().unwrap();
    assert!(
        evidence
            .iter()
            .any(|r| r["quote"] == "Buttons keyboard shortcut."),
        "{result}"
    );
    assert!(result["answer"]
        .as_str()
        .unwrap()
        .starts_with("Retrieval incomplete"));
}

#[test]
fn malformed_cached_evidence_returns_error_without_panicking() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(d.path(), vec![selected(ids.clone()), assembled(ids)]);
    let first = answer(d.path(), "Button colors?");
    let id = first["context_session"].as_str().unwrap();
    let path = d
        .path()
        .join(format!("memory/runtime/unified/sessions/{id}.json"));
    let mut session: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (field, value, expected) in [
        ("evidence", json!([null]), "invalid cached context evidence"),
        (
            "aspects",
            json!([42]),
            "invalid cached context aspect or conflict",
        ),
        (
            "conflicts",
            json!(["broken"]),
            "invalid cached context aspect or conflict",
        ),
    ] {
        let old = session["response"][field].clone();
        session["response"][field] = value;
        fs::write(&path, session.to_string()).unwrap();
        let out = run(d.path(), &format!("@context:{id} Button colors?"));
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(expected), "{err}");
        assert!(!err.contains("panicked"));
        session["response"][field] = old;
    }
}

#[test]
fn exact_cache_hits_record_session_read_receipts() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(d.path(), vec![selected(ids.clone()), assembled(ids)]);
    let first = answer(d.path(), "Button colors?");
    let session = "unified-cache-receipt-session";
    let out = Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(d.path())
        .arg(format!(
            "@context:{} Button colors?",
            first["context_session"].as_str().unwrap()
        ))
        .env("CODEX_THREAD_ID", session)
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", d.path().join("scenario.json"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["cache"], "hit");
    let dir = d
        .path()
        .join("memory/runtime/session-reads")
        .join(format!("{:x}", Sha256::digest(session.as_bytes())));
    let files: Vec<_> = fs::read_dir(dir).unwrap().collect();
    assert_eq!(files.len(), 1);
    let receipt: Value =
        serde_json::from_slice(&fs::read(files[0].as_ref().unwrap().path()).unwrap()).unwrap();
    assert_eq!(receipt["payload"]["request"], "Button colors?");
}

#[test]
fn partial_session_reuses_successful_workers() {
    let d = fixture();
    scenario(
        d.path(),
        vec![
            selected(vec![fid(1), fid(2)]),
            json!({"invalid":true}),
            json!({"invalid":true}),
        ],
    );
    let first = answer(d.path(), "Button colors?");
    assert_eq!(first["status"], "partial");
    scenario(d.path(), vec![assembled(vec![fid(1), fid(2)])]);
    let second = answer(
        d.path(),
        &format!(
            "@context:{} Button colors?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(second["status"], "complete", "{second}");
    assert_eq!(diagnostics(d.path(), &second)["calls_scheduled"], 2);
}

#[test]
fn redundant_verifier_request_is_clarified_once() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    let mut request = assembled(ids.clone());
    request["need"] = json!([source()]);
    scenario(
        d.path(),
        vec![selected(ids.clone()), request, assembled(ids)],
    );
    let result = answer(d.path(), "Button colors?");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 5);
}

#[test]
fn semantic_validation_errors_are_repaired_once_with_specific_feedback() {
    for fabricated_id in [false, true] {
        let d = fixture();
        let config_path = d.path().join("memory/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["memory"]["feedback"] = json!({"enabled":true,"agent":"cheap"});
        fs::write(config_path, config.to_string()).unwrap();
        let mut bad = selected(vec![fid(1)]);
        if fabricated_id {
            bad["select"] = json!(["nonexistent:L99"]);
        } else {
            bad["elements"] = json!([{"kind":"constraint","status":"reported","text":"A rule","evidence":[fid(1)]}]);
        }
        scenario(
            d.path(),
            vec![bad, selected(vec![fid(1)]), assembled(vec![fid(1)])],
        );
        let path = d.path().join("scenario.json");
        let mut script: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        script["calls"][1]["save_prompt_to"] = json!(d.path().join("repair-prompt.txt"));
        fs::write(path, script.to_string()).unwrap();
        let result = answer(d.path(), "Blue buttons?");
        assert_eq!(result["status"], "complete", "{result}");
        assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 5);
        let errors: Value = serde_json::from_slice(
            &fs::read(d.path().join("memory/runtime/diagnostics/errors.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            errors["total_errors"], 1,
            "one invalid response must count once"
        );
        let prompt = fs::read_to_string(d.path().join("repair-prompt.txt")).unwrap();
        assert!(prompt.contains("previous_validation_error"));
        assert!(prompt.contains(if fabricated_id {
            "nonexistent fragment"
        } else {
            "kind must be"
        }));
        assert_eq!(result["evidence"][0]["quote"], "Buttons blue.");
    }
}

#[test]
fn initial_root_can_expand_to_a_lower_ranked_exception() {
    let d = fixture();
    let mut owners = Vec::new();
    for n in 0..6 {
        let path = format!("memory/docs/rule{n}.md");
        fs::write(d.path().join(&path), "Uniquequery exception applies.").unwrap();
        owners.push(format!("doc-{}", hash(&path)));
    }
    owners.sort();
    let initial: Vec<_> = owners.iter().take(1).map(|id| format!("{id}:L1")).collect();
    let extra = format!("{}:L1", owners[5]);
    let mut need = assembled(initial.clone());
    need["need"] = json!([owners[5]]);
    let mut calls: Vec<_> = initial
        .iter()
        .map(|id| selected(vec![id.clone()]))
        .collect();
    let mut all = initial;
    all.push(extra.clone());
    calls.extend([need, selected(vec![extra]), assembled(all)]);
    scenario(d.path(), calls);
    let result = answer(d.path(), "Uniquequery exceptions?");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 6);
    assert_eq!(
        diagnostics(d.path(), &result)["coverage"]["reviewed_threads"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn malformed_worker_json_is_retried_once_within_budget() {
    let d = fixture();
    fs::write(
        d.path().join("scenario.json"),
        json!({"state_file":d.path().join("repair-calls.json"),"calls":[
            {"final_message":"{bad JSON"},
            {"final_message":selected(vec![fid(1)]).to_string()},
            {"final_message":assembled(vec![fid(1)]).to_string()}
        ]})
        .to_string(),
    )
    .unwrap();
    let result = answer(d.path(), "Buttons?");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 5);
}

#[test]
fn followup_consults_only_missing_thread_and_retains_other_evidence() {
    let d = fixture();
    let other = format!("doc-{}", hash("memory/docs/extra.md"));
    let old = format!("{other}:L1");
    let new = format!("{other}:L2");
    fs::write(
        d.path().join("memory/docs/extra.md"),
        "Buttons grey.\nKeyboard focus visible.",
    )
    .unwrap();
    let mut workers = [
        (source(), selected(vec![fid(1)])),
        (other.clone(), selected(vec![old.clone()])),
    ];
    workers.sort_by(|a, b| a.0.cmp(&b.0));
    let questions = ["Blue buttons?", "Grey buttons?"];
    let follow_questions = ["Blue buttons?", "Grey buttons?", "Keyboard focus?"];
    let mut need = assembled_aspects(vec![fid(1), old.clone()], &follow_questions);
    need["aspects"][2]["status"] = json!("missing");
    need["need"] = json!([other]);
    scenario(
        d.path(),
        workers
            .into_iter()
            .map(|(_, v)| v)
            .chain([
                assembled_aspects(vec![fid(1), old], &questions),
                need,
                selected(vec![new.clone()]),
                assembled_aspects(vec![fid(1), new], &follow_questions),
            ])
            .collect(),
    );
    let first = answer(d.path(), "Blue and grey Buttons?");
    assert_eq!(first["status"], "complete", "{first}");
    let second = answer(
        d.path(),
        &format!(
            "@context:{} Include keyboard focus",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(second["status"], "complete", "{second}");
    assert_eq!(diagnostics(d.path(), &second)["calls_scheduled"], 5);
}

#[test]
fn moved_document_replaces_old_addresses_and_orphaned_index_is_rebuilt() {
    let d = fixture();
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
    );
    let first = answer(d.path(), "Buttons?");
    fs::rename(
        d.path().join("memory/docs/ui.md"),
        d.path().join("memory/docs/moved.md"),
    )
    .unwrap();
    fs::write(
        d.path().join("memory/runtime/unified/index.json"),
        "{interrupted",
    )
    .unwrap();
    let moved = format!("doc-{}:L1", hash("memory/docs/moved.md"));
    scenario(
        d.path(),
        vec![selected(vec![moved.clone()]), assembled(vec![moved])],
    );
    let second = answer(
        d.path(),
        &format!(
            "@context:{} Buttons?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(second["status"], "complete", "{second}");
    assert_eq!(second["evidence"][0]["source"], "memory/docs/moved.md");
    assert_eq!(
        diagnostics(d.path(), &second)["coverage"]["total_threads"],
        1
    );
}

#[test]
fn separate_topic_sessions_do_not_reuse_private_context() {
    let d = fixture();
    scenario(
        d.path(),
        vec![
            selected(vec![fid(1)]),
            assembled(vec![fid(1)]),
            selected(vec![fid(2)]),
            assembled(vec![fid(2)]),
        ],
    );
    let first = answer(d.path(), "Blue buttons in task A?");
    let second = answer(d.path(), "Deletion buttons in task B?");
    assert_ne!(first["context_session"], second["context_session"]);
    for (r, goal) in [
        (first, "Blue buttons in task A?"),
        (second, "Deletion buttons in task B?"),
    ] {
        let path = d.path().join(format!(
            "memory/runtime/unified/sessions/{}.json",
            r["context_session"].as_str().unwrap()
        ));
        let c: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(c["goal"], goal);
        assert_eq!(c["history"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn cached_followup_repairs_unasked_aspect_without_reconsulting_workers() {
    let d = fixture();
    fs::write(
        d.path().join("planned-aspects.json"),
        json!(["Button colors and exceptions"]).to_string(),
    )
    .unwrap();
    let ids = vec![fid(1), fid(2)];
    let mut extra = assembled(ids.clone());
    extra["aspects"]
        .as_array_mut()
        .unwrap()
        .push(json!({"question":"Is implementation verified?","status":"missing","evidence":[]}));
    scenario(
        d.path(),
        vec![
            selected(ids.clone()),
            assembled(ids.clone()),
            extra,
            assembled(ids),
        ],
    );
    let first = answer(d.path(), "Button requirements?");
    let second = answer(
        d.path(),
        &format!(
            "@context:{} What are the documented colors?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(second["status"], "complete", "{second}");
    assert_eq!(second["cache"], "reused_evidence");
    assert_eq!(diagnostics(d.path(), &second)["calls_scheduled"], 3);
}

#[test]
fn partial_resume_retries_failed_worker_without_repeating_successful_one() {
    let d = fixture();
    let other = format!("doc-{}", hash("memory/docs/extra.md"));
    let other_fid = format!("{other}:L1");
    fs::write(d.path().join("memory/docs/extra.md"), "Buttons grey.").unwrap();
    let questions = ["Blue buttons?", "Grey buttons?"];
    let mut partial = assembled_aspects(vec![fid(1)], &questions);
    partial["need"] = json!([other]);
    let mut calls = Vec::new();
    // The failed worker is retried before moving on to the next sibling.
    for (id, value) in [
        (source(), selected(vec![fid(1)])),
        (other.clone(), json!({"broken":true})),
    ]
    .into_iter()
    .collect::<std::collections::BTreeMap<_, _>>()
    {
        calls.push(value);
        if id == other {
            calls.push(json!({"broken":true}));
        }
    }
    calls.push(partial);
    scenario(d.path(), calls);
    let first = answer(d.path(), "Blue and grey Buttons?");
    assert_eq!(first["status"], "partial", "{first}");
    scenario(
        d.path(),
        vec![
            selected(vec![other_fid.clone()]),
            assembled_aspects(vec![fid(1), other_fid], &questions),
        ],
    );
    let second = answer(
        d.path(),
        &format!(
            "@context:{} Blue and grey Buttons?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(second["status"], "complete", "{second}");
    assert_eq!(second["response_mode"], "delta");
    assert_eq!(
        second["reused_evidence"],
        json!([first["evidence"][0]["ref"]])
    );
    assert_eq!(second["evidence"].as_array().unwrap().len(), 1);
    let details = answer(d.path(), second["details"].as_str().unwrap());
    assert_eq!(details["evidence"].as_array().unwrap().len(), 2);
    assert_eq!(diagnostics(d.path(), &second)["calls_scheduled"], 3);
}

#[test]
fn independent_scope_check_drops_only_unrequested_aspects() {
    for keep_missing in [false, true] {
        let d = fixture();
        let mut verdict = assembled(vec![fid(1)]);
        let extra = if keep_missing {
            "Deletion button requirements"
        } else {
            "Implementation verified"
        };
        verdict["aspects"]
            .as_array_mut()
            .unwrap()
            .push(json!({"question":extra,"status":"missing","evidence":[]}));
        let mut keep = vec!["Button colors and exceptions"];
        if keep_missing {
            keep.push(extra);
        }
        fs::write(
            d.path().join("planned-aspects.json"),
            json!(keep).to_string(),
        )
        .unwrap();
        scenario(
            d.path(),
            vec![
                selected(vec![fid(1)]),
                verdict.clone(),
                verdict,
                json!({"keep":keep,"unrepresented":[]}),
            ],
        );
        let result = answer(d.path(), "Button requirements and exceptions?");
        assert_eq!(
            result["status"],
            if keep_missing { "partial" } else { "complete" },
            "{result}"
        );
        assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 4);
    }
}

#[test]
fn verifier_recovered_originals_are_available_to_cached_followup() {
    let d = fixture();
    scenario(
        d.path(),
        vec![
            selected(vec![fid(1)]),
            assembled(vec![fid(1), fid(2)]),
            assembled(vec![fid(2)]),
        ],
    );
    let first = answer(d.path(), "Button requirements?");
    let next = answer(
        d.path(),
        &format!(
            "@context:{} Deletion color?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(next["status"], "complete", "{next}");
    assert_eq!(next["cache"], "reused_evidence");
    assert_eq!(diagnostics(d.path(), &next)["calls_scheduled"], 3);
}

#[test]
fn query_partition_is_ignored_and_preserves_originals_and_repeat_cache() {
    let d = fixture();
    let text=hierarchy_document("Buttons blue.\nButtons red on deletion.\nButtons disabled grey.\nButtons have labels.\nKeyboard operates controls.\nFocus is visible.\nTab moves focus.\nEscape cancels.");
    fs::write(d.path().join("memory/docs/ui.md"), &text).unwrap();
    let ids: Vec<_> = (1..=8).map(fid).collect();
    let mut selection = selected(ids.clone());
    selection["elements"] = json!([{"kind":"requirement","status":"documented","text":"Button appearance rules","evidence":[fid(1),fid(2)]}]);
    selection["groups"] = json!([{"title":"Button appearance","fragments":ids[..4]},{"title":"Keyboard and focus","fragments":ids[4..]}]);
    scenario(d.path(), vec![selection, assembled(ids)]);
    let first = answer(d.path(), "Buttons and keyboard?");
    assert_eq!(first["status"], "complete", "{first}");
    let index: Value = serde_json::from_slice(
        &fs::read(d.path().join("memory/runtime/unified/index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index["threads"].as_array().unwrap().len(), 1);
    assert_eq!(
        index["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["fragments"].as_array().unwrap().len())
            .sum::<usize>(),
        8
    );
    let repeat = answer(
        d.path(),
        &format!(
            "@context:{} Buttons and keyboard?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(repeat["cache"], "hit", "{repeat}");
    let reloaded: Value = serde_json::from_slice(
        &fs::read(d.path().join("memory/runtime/unified/index.json")).unwrap(),
    )
    .unwrap();
    let element_ids = |v: &Value| {
        v["threads"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|t| {
                t["elements"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["id"].as_str().unwrap().to_owned())
            })
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(element_ids(&index).len(), 1);
    assert_eq!(element_ids(&index), element_ids(&reloaded));
    assert_eq!(
        fs::read_to_string(d.path().join("memory/docs/ui.md")).unwrap(),
        text
    );
}

#[test]
fn retired_toggle_cannot_restore_legacy_chat_and_stale_state_is_ignored() {
    for enabled in [false, true] {
        let d = fixture();
        let path = d.path().join("memory/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["memory"]["unified"]["enabled"] = json!(enabled);
        fs::write(path, config.to_string()).unwrap();
        let old = d.path().join("memory/runtime/chat/state.json");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        let stale = b"deliberately invalid legacy conversation";
        fs::write(&old, stale).unwrap();
        let ids = vec![fid(1), fid(2)];
        scenario(d.path(), vec![selected(ids.clone()), assembled(ids)]);
        let result = answer(d.path(), "Button colors?");
        assert_eq!(result["status"], "complete", "{result}");
        assert!(result["context_session"].is_string());
        assert_eq!(fs::read(&old).unwrap(), stale);
        assert!(!d.path().join("memory/runtime/cache/answers").exists());
    }
}

#[test]
fn unified_storage_failure_never_invokes_a_legacy_provider() {
    let d = fixture();
    fs::create_dir_all(d.path().join("memory/runtime")).unwrap();
    fs::write(d.path().join("memory/runtime/unified"), "blocked storage").unwrap();
    // An old coordinator would consume this call and return a misleading answer.
    scenario(
        d.path(),
        vec![json!({"action":"answer","target":"","text":"legacy fallback"})],
    );
    let out = run(d.path(), "Button colors?");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(!d.path().join("memory/runtime/chat").exists());
    assert!(!fs::read_dir(d.path()).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("calls-")));
}

#[test]
fn recursion_guard_rejects_queries_before_creating_unified_state() {
    let d = fixture();
    for guard in [
        "CM_CHAT_INTERNAL",
        "CM_DOCS_INTERNAL",
        "CM_CONTEXT_INTERNAL",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_cm"))
            .current_dir(d.path())
            .arg("Button colors?")
            .env(guard, "1")
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("recursively"));
        assert!(!d.path().join("memory/runtime/unified").exists());
    }
}

#[test]
fn unified_usage_counts_unicode_exchange_and_zero_call_cache_hits() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["statistics"] = json!({"enabled":true});
    fs::write(path, config.to_string()).unwrap();
    let ids = vec![fid(1), fid(2)];
    let calls: Vec<_> = [selected(ids.clone()), assembled(ids)].into_iter().map(|v| json!({
        "final_message":v.to_string(),
        "events":[{"type":"turn.completed","usage":{"input_tokens":300,"cached_input_tokens":100,"output_tokens":30}}]
    })).collect();
    fs::write(
        d.path().join("scenario.json"),
        json!({"state_file":d.path().join("calls.json"),"calls":calls}).to_string(),
    )
    .unwrap();
    let query = "Button colors 🟢?";
    let out = run(d.path(), query);
    assert!(out.status.success());
    let first: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(first["status"], "complete");
    let cached = answer(
        d.path(),
        &format!(
            "@context:{} {query}",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(cached["cache"], "hit");
    let reports: Vec<Value> = fs::read_dir(d.path().join("memory/runtime/statistics"))
        .unwrap()
        .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect();
    assert_eq!(reports.len(), 2);
    let cold = reports.iter().find(|r| r["totals"]["calls"] == 4).unwrap();
    assert_eq!(cold["exchange"]["input_chars"], query.chars().count());
    assert_eq!(cold["exchange"]["input_bytes"], query.len());
    assert_eq!(
        cold["exchange"]["output_chars"],
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .chars()
            .count()
    );
    assert_eq!(cold["totals"]["tokens"]["input_tokens"]["reported"], 600);
    assert_eq!(
        cold["totals"]["tokens"]["cached_input_tokens"]["reported"],
        200
    );
    assert_eq!(cold["totals"]["tokens"]["output_tokens"]["reported"], 60);
    assert!(reports.iter().any(|r| r["totals"]["calls"] == 0));
    assert!(!serde_json::to_string(&reports).unwrap().contains(query));
    let sessions: Vec<Value> = fs::read_dir(d.path().join("memory/runtime/session-statistics"))
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension().and_then(|v| v.to_str()) == Some("json"))
                .then(|| serde_json::from_slice(&fs::read(p).unwrap()).unwrap())
        })
        .collect();
    let topic = sessions
        .iter()
        .find(|s| s["session_kind"] == "context")
        .unwrap();
    assert_eq!(topic["session_id"], first["context_session"]);
    assert_eq!(topic["request_count"], 2);
    assert_eq!(topic["totals"]["calls"], 4);
    assert_eq!(topic["totals"]["tokens"]["input_tokens"]["reported"], 600);
    assert_eq!(topic["totals"]["tokens"]["total_tokens"]["reported"], 660);
    assert_eq!(
        topic["totals"]["tokens"]["total_tokens"]["missing_calls"],
        2
    );
    assert_eq!(
        topic["totals"]["tokens"]["uncached_input_tokens"]["reported"],
        400
    );
    assert!(cold["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event"] == "indexed_candidates"));
    assert!(cold["calls"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["correlation"]["context_session"] == first["context_session"]));
}

// Hierarchy tests intentionally exceed the small-original cohesion boundary.
// Trailing blank source space preserves every existing evidence line and fact.
fn hierarchy_document(text: &str) -> String {
    format!("{text}\n{}", " ".repeat(2049))
}

fn section(parent: &str, title: &str) -> String {
    format!("section-{}", hash(&format!("{parent}/{title}/1")))
}

#[test]
fn diagnostic_files_remain_enabled_without_machine_output_paths() {
    for (pretty, language, log_prefix, statistics_prefix) in [
        (false, None, "CM agent log:", "CM statistics:"),
        (true, None, "CM agent log:", "CM statistics:"),
        (true, Some("-ru"), "CM журнал агента:", "CM статистика:"),
        (true, Some("-zh"), "CM 代理日志:", "CM 统计:"),
    ] {
        let d = fixture();
        let path = d.path().join("memory/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["memory"]["agent_logs"] = json!({"enabled":true});
        config["memory"]["statistics"] = json!({"enabled":true});
        fs::write(path, config.to_string()).unwrap();
        scenario(
            d.path(),
            vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
        );
        prepare_plan(d.path(), "Button colors?");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cm"));
        cmd.current_dir(d.path())
            .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
            .env("CM_FAKE_CODEX_SCENARIO", d.path().join("scenario.json"));
        if pretty {
            cmd.arg("-pretty");
        }
        if let Some(language) = language {
            cmd.arg(language);
        }
        let out = cmd.arg("Button colors?").output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(stderr.contains(statistics_prefix), pretty);
        assert_eq!(stderr.contains(log_prefix), pretty);
        assert_eq!(
            stderr.matches(log_prefix).count(),
            if pretty { 4 } else { 0 }
        );
        assert_eq!(
            fs::read_dir(d.path().join("memory/runtime/agent-logs"))
                .unwrap()
                .count(),
            4
        );
        assert_eq!(
            fs::read_dir(d.path().join("memory/runtime/statistics"))
                .unwrap()
                .count(),
            1
        );
        if !pretty {
            assert!(stderr.trim().is_empty(), "{stderr}");
        }
    }
}

#[test]
fn optional_shortening_reserves_remaining_parent_and_verification_calls() {
    for budget in [7, 8] {
        let d = fixture();
        let path = d.path().join("memory/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["memory"]["max_steps"] = json!(budget);
        fs::write(path, config.to_string()).unwrap();
        fs::write(
            d.path().join("memory/docs/ui.md"),
            hierarchy_document("# UI\n## Save\nSave green."),
        )
        .unwrap();
        let top = section(&source(), "UI");
        let leaf = section(&top, "Save");
        let ids = vec![fid(3)];
        let mut long = branch(ids.clone());
        long["summary"] = json!("x".repeat(1700));
        scenario(
            d.path(),
            vec![
                route(vec![top]),
                route(vec![leaf]),
                selected(ids.clone()),
                long,
                branch(ids.clone()),
                assembled(ids),
            ],
        );
        let result = answer(d.path(), "docs");
        assert_eq!(
            result["status"],
            if budget == 8 { "complete" } else { "partial" },
            "{result}"
        );
        assert!(
            diagnostics(d.path(), &result)["calls_scheduled"]
                .as_u64()
                .unwrap()
                <= budget
        );
        if budget == 8 {
            // Planner + three workers + two parent folds + verifier + grounding.
            assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 8);
            assert!(d.path().join("grounding-prompts.txt").exists());
        }
    }
}

#[test]
fn cached_followup_does_not_exceed_budget_after_planning() {
    let original = fixture();
    scenario(
        original.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
    );
    let complete = answer(original.path(), "Button color?");
    assert_eq!(complete["status"], "complete", "{complete}");
    let saved: Value = serde_json::from_slice(
        &fs::read(original.path().join(format!(
            "memory/runtime/unified/sessions/{}.json",
            complete["context_session"].as_str().unwrap()
        )))
        .unwrap(),
    )
    .unwrap();
    let d = fixture();
    let config_path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(1);
    fs::write(config_path, config.to_string()).unwrap();
    scenario(d.path(), vec![assembled(vec![fid(1)])]);
    let first = answer(d.path(), "Button color?");
    let path = d.path().join(format!(
        "memory/runtime/unified/sessions/{}.json",
        first["context_session"].as_str().unwrap()
    ));
    let mut context: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    // Seed legitimate cached excerpts into a context with the current one-call budget.
    context["workers"] = saved["workers"].clone();
    context["source_revisions"] = saved["source_revisions"].clone();
    fs::write(path, context.to_string()).unwrap();
    scenario(d.path(), vec![assembled(vec![fid(1)])]);
    let next = answer(
        d.path(),
        &format!(
            "@context:{} What color?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(diagnostics(d.path(), &next)["calls_scheduled"], 1, "{next}");
    assert_eq!(next["status"], "partial");
    assert!(!d.path().join("grounding-prompts.txt").exists());
}

#[test]
fn parent_length_tolerance_and_failed_shortening_preserve_complete_evidence() {
    for (length, repaired) in [(1308, false), (1700, true), (1700, false)] {
        let d = fixture();
        fs::write(
            d.path().join("memory/docs/ui.md"),
            hierarchy_document("# Buttons\nSave green."),
        )
        .unwrap();
        let child = section(&source(), "Buttons");
        let ids = vec![fid(2)];
        let mut reply = branch(ids.clone());
        reply["summary"] = json!("x".repeat(length));
        let mut calls = vec![route(vec![child]), selected(ids.clone()), reply];
        if length > 1600 {
            calls.push(
                json!({"summary":if repaired {"Short summary".into()} else {"x".repeat(1701)}}),
            );
        }
        calls.push(assembled(ids));
        scenario(d.path(), calls);
        let result = answer(d.path(), "docs");
        assert_eq!(result["status"], "complete", "{result}");
        assert_eq!(
            diagnostics(d.path(), &result)["calls_scheduled"],
            if length > 1600 { 7 } else { 6 }
        );
        assert_eq!(
            answer(
                d.path(),
                &format!(
                    "@context:{} @details",
                    result["context_session"].as_str().unwrap()
                )
            )["evidence"][0]["quote"],
            "Save green."
        );
        let plan = fs::read_to_string(d.path().join("plan-prompt.txt")).unwrap();
        assert!(!plan.contains("Save green."));
        assert!(!plan.contains("fragments"));
    }
}

#[test]
fn empty_router_reuses_one_child_and_still_grounds_original_evidence() {
    let d = fixture();
    let original = hierarchy_document("# UI\n## Save\nSave green.");
    fs::write(d.path().join("memory/docs/ui.md"), original).unwrap();
    let ui = section(&source(), "UI");
    let save = section(&ui, "Save");
    let mut root = route(vec![ui]);
    root["summary"] = json!("");
    let mut parent = route(vec![save]);
    parent["summary"] = json!("");
    scenario(
        d.path(),
        vec![
            root,
            parent,
            selected(vec![fid(3)]),
            assembled(vec![fid(3)]),
        ],
    );
    let result = answer(d.path(), "docs");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 6);
    assert!(fs::read_to_string(d.path().join("grounding-prompts.txt"))
        .unwrap()
        .contains("Save green."));
}

#[test]
fn documents_are_whole_by_default_and_model_cannot_split_them() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]
        .as_object_mut()
        .unwrap()
        .remove("documents_as_threads");
    fs::write(path, config.to_string()).unwrap();
    let original = format!(
        "# Intro\n{}\n# Checks\nRun checker.\n# More\nOne.\nTwo.\nThree.\n",
        "Original text. ".repeat(1000)
    );
    fs::write(d.path().join("memory/docs/ui.md"), &original).unwrap();
    let mut selection = selected(vec![fid(4)]);
    selection["groups"] = json!([
        {"title":"First","fragments":(1..=4).map(fid).collect::<Vec<_>>()},
        {"title":"Second","fragments":(5..=8).map(fid).collect::<Vec<_>>()}
    ]);
    scenario(d.path(), vec![selection, assembled(vec![fid(4)])]);
    let result = answer(d.path(), "Checks?");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(result["evidence"][0]["line"], 4);
    assert_eq!(result["evidence"][0]["quote"], "Run checker.");
    let index: Value = serde_json::from_slice(
        &fs::read(d.path().join("memory/runtime/unified/index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index["documents_as_threads"], false);
    assert_eq!(index["threads"].as_array().unwrap().len(), 1);
    assert_eq!(
        index["threads"][0]["fragments"].as_array().unwrap().len(),
        8
    );
    assert_eq!(
        fs::read_to_string(d.path().join("memory/docs/ui.md")).unwrap(),
        original
    );
}

#[test]
fn authored_relative_link_routes_to_original_in_another_document() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        "Documentation changes follow [the procedure](./checks.md#validation).\n",
    )
    .unwrap();
    fs::write(
        d.path().join("memory/docs/checks.md"),
        "Run python tools/docs/check_docs.py.\n",
    )
    .unwrap();
    let target = format!("doc-{}", hash("memory/docs/checks.md"));
    let evidence = vec![format!("{target}:L1")];
    scenario(
        d.path(),
        vec![
            route(vec![target]),
            selected(evidence.clone()),
            branch(evidence.clone()),
            assembled(evidence),
        ],
    );
    let result = answer(d.path(), "Documentation changes?");
    assert_eq!(result["status"], "complete", "{result}");
    assert!(result
        .to_string()
        .contains("python tools/docs/check_docs.py"));
    assert_eq!(result["evidence"][0]["source"], "memory/docs/checks.md");
}

#[test]
fn section_seed_can_consult_a_sibling_with_different_vocabulary() {
    let d = fixture();
    let original = hierarchy_document("# Before editing\nDocumentation changes begin here.\n# Completion\nRun python tools/docs/check_docs.py.");
    fs::write(d.path().join("memory/docs/ui.md"), &original).unwrap();
    let sibling = section(&source(), "Completion");
    let evidence = vec![fid(4)];
    let values = vec![
        route(vec![sibling]),
        selected(evidence.clone()),
        branch(evidence.clone()),
        assembled(evidence),
    ];
    let calls: Vec<_> = values
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            json!({
                "final_message":v.to_string(),
                "save_prompt_to":d.path().join(format!("sibling-{i}.txt")),
                "expect_no_native_tools":true
            })
        })
        .collect();
    fs::write(
        d.path().join("scenario.json"),
        json!({"state_file":d.path().join("calls.json"),"calls":calls}).to_string(),
    )
    .unwrap();
    let result = answer(d.path(), "Documentation changes?");
    assert_eq!(result["status"], "complete", "{result}");
    assert!(result
        .to_string()
        .contains("python tools/docs/check_docs.py"));
    let prompt = fs::read_to_string(d.path().join("sibling-0.txt")).unwrap();
    let data: Value =
        serde_json::from_str(prompt.lines().find(|l| l.starts_with('{')).unwrap()).unwrap();
    assert_eq!(data["thread"]["title"], "Before editing");
    assert_eq!(data["children"][0]["title"], "Completion");
    assert!(!data["thread"]["fragments"]
        .to_string()
        .contains("check_docs.py"));
    let child = fs::read_to_string(d.path().join("sibling-1.txt")).unwrap();
    assert!(child.contains("Run python tools/docs/check_docs.py."));
    assert_eq!(
        fs::read_to_string(d.path().join("memory/docs/ui.md")).unwrap(),
        original
    );
}

#[test]
fn recursive_agents_read_only_own_content_and_merge_selected_children_upward() {
    let d = fixture();
    let original=hierarchy_document("# UI\n## Settings\nGeneral settings.\n### Save\nSave green.\n### Exceptions\nDisabled Save grey.\n## Typography\nFont 16px.");
    fs::write(d.path().join("memory/docs/ui.md"), &original).unwrap();
    let ui = section(&source(), "UI");
    let settings = section(&ui, "Settings");
    let save = section(&settings, "Save");
    let exceptions = section(&settings, "Exceptions");
    let ids = vec![fid(5), fid(7)];
    let mut parent = route(vec![save.clone(), exceptions.clone()]);
    parent["delegations"] = json!([{"thread":save,"question":"Find enabled Save color."},{"thread":exceptions,"question":"Find disabled exceptions."}]);
    let mut values = vec![
        route(vec![ui.clone()]),
        route(vec![settings.clone()]),
        parent,
    ];
    values.extend(ordered_workers(vec![
        (save.clone(), selected(vec![fid(5)])),
        (exceptions.clone(), selected(vec![fid(7)])),
    ]));
    values.extend([
        branch(ids.clone()),
        branch(ids.clone()),
        branch(ids.clone()),
        assembled(ids),
    ]);
    let calls: Vec<_>=values.into_iter().enumerate().map(|(i,v)|json!({"final_message":v.to_string(),"save_prompt_to":d.path().join(format!("prompt-{i}.txt")),"expect_no_native_tools":true})).collect();
    fs::write(
        d.path().join("scenario.json"),
        json!({"state_file":d.path().join("calls.json"),"calls":calls}).to_string(),
    )
    .unwrap();
    let first = answer(d.path(), "docs");
    assert_eq!(first["status"], "complete", "{first}");
    assert_eq!(diagnostics(d.path(), &first)["calls_scheduled"], 11);
    assert_eq!(diagnostics(d.path(), &first)["unprocessed_thread_count"], 0);
    let prompt = |n| fs::read_to_string(d.path().join(format!("prompt-{n}.txt"))).unwrap();
    let data = |n| -> Value {
        let p = prompt(n);
        serde_json::from_str(p.lines().find(|line| line.starts_with('{')).unwrap()).unwrap()
    };
    assert!(data(0)["thread"]["fragments"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(data(0)["children"].as_array().unwrap().len(), 1);
    assert_eq!(data(1)["children"].as_array().unwrap().len(), 2);
    // Immediate sibling passports are visible too, without reading their originals.
    assert_eq!(data(2)["children"].as_array().unwrap().len(), 3);
    assert!(!prompt(0).contains("Save green."));
    assert!(!prompt(1).contains("Save green."));
    assert!(data(3)["question"]
        .as_str()
        .unwrap()
        .contains("Original question:"));
    assert!(data(5)["children"].as_array().unwrap().len() == 2);
    assert!(prompt(5).contains("Save green."));
    assert!(!prompt(5).contains("Font 16px."));
    assert_eq!(
        answer(
            d.path(),
            &format!(
                "@context:{} docs",
                first["context_session"].as_str().unwrap()
            )
        )["cache"],
        "hit"
    );
    assert_eq!(
        fs::read_to_string(d.path().join("memory/docs/ui.md")).unwrap(),
        original
    );
}

#[test]
fn hundreds_of_irrelevant_siblings_do_not_become_pending_obligations() {
    let d = fixture();
    let mut original = "# Reference\n## History\nHistory retains 240 entries.\n".to_string();
    for i in 0..650 {
        original += &format!("## Display {i}\nSpacing is configurable.\n");
    }
    original += "## Exceptions\nNever clear all history.\n";
    fs::write(d.path().join("memory/docs/ui.md"), original).unwrap();
    let top = section(&source(), "Reference");
    let history = section(&top, "History");
    let exceptions = section(&top, "Exceptions");
    let ids = vec![fid(3), fid(1305)];
    let mut calls = ordered_workers(vec![
        (history, selected(vec![ids[0].clone()])),
        (exceptions, selected(vec![ids[1].clone()])),
    ]);
    calls.push(assembled_aspects(
        ids,
        &["How many entries?", "Exceptions?"],
    ));
    scenario(d.path(), calls);
    let scenario_path = d.path().join("scenario.json");
    let mut script: Value = serde_json::from_slice(&fs::read(&scenario_path).unwrap()).unwrap();
    for (i, call) in script["calls"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        call["save_prompt_to"] = json!(d.path().join(format!("search-prompt-{i}.txt")));
    }
    fs::write(scenario_path, script.to_string()).unwrap();
    let result = answer(d.path(), "History retention and exceptions?");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 5);
    assert_eq!(
        diagnostics(d.path(), &result)["unprocessed_thread_count"],
        0
    );
    assert!(!result.to_string().contains("Display 649"));
    assert!(result.to_string().len() < 5000);
    for (step, field) in [(0, "children"), (2, "catalog")] {
        let prompt =
            fs::read_to_string(d.path().join(format!("search-prompt-{step}.txt"))).unwrap();
        let payload: Value =
            serde_json::from_str(prompt.lines().find(|l| l.starts_with('{')).unwrap()).unwrap();
        assert!(payload[field].as_array().unwrap().len() <= 12, "{payload}");
        assert!(payload["passport_search"].is_object());
        assert!(
            prompt.len() < 30000,
            "catalog leaked into prompt: {}",
            prompt.len()
        );
    }
}

#[test]
fn router_cannot_skip_ownership_and_request_an_unseen_grandchild() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        hierarchy_document("# UI\n## Save\nSave green."),
    )
    .unwrap();
    let ui = section(&source(), "UI");
    let save = section(&ui, "Save");
    scenario(
        d.path(),
        vec![
            route(vec![save.clone()]),
            route(vec![save]),
            json!({"answer":"Unknown","select":[],"aspects":[],"need":[],"conflicts":[]}),
        ],
    );
    let result = answer(d.path(), "docs");
    assert_eq!(result["status"], "partial");
    assert!(result["evidence"].as_array().unwrap().is_empty());
    assert!(diagnostics(d.path(), &result)["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e.as_str().unwrap().contains("outside visible")));
}

#[test]
fn indexed_parent_and_child_are_consulted_once_without_ancestors() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        hierarchy_document("# Settings\n## Save\nSave green."),
    )
    .unwrap();
    let parent = section(&source(), "Settings");
    let leaf = section(&parent, "Save");
    let mut calls = ordered_workers(vec![
        (parent, route(vec![leaf.clone()])),
        (leaf, selected(vec![fid(3)])),
    ]);
    calls.extend([
        branch(vec![fid(3)]),
        assembled_aspects(vec![fid(3)], &["Settings?", "Save?"]),
    ]);
    scenario(d.path(), calls);
    let result = answer(d.path(), "Settings Save");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 6);
    assert_eq!(
        diagnostics(d.path(), &result)["coverage"]["reviewed_threads"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn empty_index_search_does_not_fall_back_to_common_root() {
    let d = fixture();
    scenario(
        d.path(),
        vec![
            json!({"answer":"No matching evidence found.","select":[],"aspects":[{"question":"Button colors and exceptions","status":"missing","evidence":[]}],"need":[],"conflicts":[]});
            2
        ],
    );
    let result = answer(d.path(), "unmatchedtopicxyz");
    assert_eq!(result["status"], "partial", "{result}");
    assert_eq!(
        result["aspects"][0]["search_state"],
        "not_found_in_reviewed_sources"
    );
    assert!(
        diagnostics(d.path(), &result)["coverage"]["reviewed_threads"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(result["evidence"].as_array().unwrap().is_empty());
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 2);
}

#[test]
fn indexed_initial_candidates_obey_configured_limit() {
    let d = fixture();
    let config_path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["unified"]["max_candidates"] = json!(2);
    fs::write(config_path, config.to_string()).unwrap();
    let mut owners = Vec::new();
    for n in 0..20 {
        let path = format!("memory/docs/match{n}.md");
        fs::write(d.path().join(&path), "Uniquequery fact.").unwrap();
        owners.push(format!("doc-{}", hash(&path)));
    }
    owners.sort();
    for (id, word) in owners.iter().take(2).zip(["alpha", "beta"]) {
        for n in 0..20 {
            let path = format!("memory/docs/match{n}.md");
            if *id == format!("doc-{}", hash(&path)) {
                fs::write(d.path().join(path), format!("Uniquequery {word} fact.")).unwrap();
            }
        }
    }
    let ids: Vec<_> = owners.iter().take(2).map(|id| format!("{id}:L1")).collect();
    scenario(
        d.path(),
        vec![
            selected(vec![ids[0].clone()]),
            selected(vec![ids[1].clone()]),
            assembled_aspects(ids, &["alpha?", "beta?"]),
        ],
    );
    let result = answer(d.path(), "Uniquequery");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 5);
    assert_eq!(
        diagnostics(d.path(), &result)["coverage"]["reviewed_threads"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn verifier_can_expand_beyond_initial_candidate_limit() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["unified"]["max_candidates"] = json!(1);
    fs::write(path, config.to_string()).unwrap();
    let extra = format!("doc-{}", hash("memory/docs/extra.md"));
    fs::write(
        d.path().join("memory/docs/extra.md"),
        "Buttons have keyboard support.",
    )
    .unwrap();
    let mut first = assembled(vec![fid(1)]);
    first["need"] = json!([extra]);
    scenario(
        d.path(),
        vec![
            selected(vec![fid(1)]),
            first,
            selected(vec![format!("{extra}:L1")]),
            assembled(vec![fid(1), format!("{extra}:L1")]),
        ],
    );
    let result = answer(d.path(), "Blue buttons?");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 6);
    assert_eq!(
        diagnostics(d.path(), &result)["coverage"]["reviewed_threads"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn detailed_request_logs_link_calls_and_redact_configured_secrets() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["statistics"] = json!({"enabled":true});
    config["memory"]["agent_logs"] = json!({"enabled":true});
    config["agent"]["providers"]["codex"] =
        json!({"adapter":"codex","api_key":"SESSION-SECRET-12345678"});
    fs::write(path, config.to_string()).unwrap();
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
    );
    let result = answer(d.path(), "Button colors? SESSION-SECRET-12345678");
    assert_eq!(result["status"], "complete", "{result}");
    let read_rows = |dir: &str| -> Vec<Value> {
        fs::read_dir(d.path().join(dir))
            .unwrap()
            .flat_map(|e| {
                fs::read_to_string(e.unwrap().path())
                    .unwrap()
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    let rows = read_rows("memory/runtime/request-logs");
    assert_eq!(rows[0]["event"], "cli_request");
    assert!(!json!(rows).to_string().contains("SESSION-SECRET-12345678"));
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| r["event"] != "cli_request")
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["event"], "external_input");
    assert_eq!(rows[1]["event"], "external_output");
    assert_eq!(rows[0]["request_id"], rows[1]["request_id"]);
    assert!(!json!(rows).to_string().contains("SESSION-SECRET-12345678"));
    assert!(rows[0]["text"].as_str().unwrap().contains("[REDACTED]"));
    let report: Value = serde_json::from_slice(
        &fs::read(
            fs::read_dir(d.path().join("memory/runtime/statistics"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap(),
    )
    .unwrap();
    for call in report["calls"].as_array().unwrap() {
        assert_eq!(call["correlation"]["request_id"], rows[0]["request_id"]);
        let log = fs::read_to_string(call["agent_log"].as_str().unwrap()).unwrap();
        assert!(!log.contains("SESSION-SECRET-12345678"));
        let first: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(first["correlation"]["run_id"], report["run_id"]);
    }
}

#[test]
fn related_questions_share_one_thread_read_and_keep_every_requested_aspect() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        "Save green.\nSave disabled when unchanged.\nSave persists changed values.",
    )
    .unwrap();
    let questions = [
        "Save color?",
        "When is Save disabled?",
        "What does Save do?",
    ];
    let ids = vec![fid(1), fid(2), fid(3)];
    let verdict = json!({"answer":"Save is green, disabled when unchanged, and persists changed values.","select":ids,"aspects":questions.iter().enumerate().map(|(i,q)|json!({"question":q,"status":"found","evidence":[fid(i+1)]})).collect::<Vec<_>>(),"need":[],"conflicts":[]});
    scenario(d.path(), vec![selected(ids), verdict]);
    let query = "General settings Save: 1) color? 2) when disabled? 3) what does it do?";
    let result = answer(d.path(), query);
    assert_eq!(result["status"], "complete");
    assert_eq!(result["evidence"].as_array().unwrap().len(), 3);
    // One scope plan, one owning-thread read and one verification; no per-question loop.
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 4);
    let session: Value = serde_json::from_slice(
        &fs::read(d.path().join(format!(
            "memory/runtime/unified/sessions/{}.json",
            result["context_session"].as_str().unwrap()
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(session["requested_aspects"], json!(questions));
    assert_eq!(session["response"]["aspects"].as_array().unwrap().len(), 3);
    let prompt = fs::read_to_string(d.path().join("plan-prompt.txt")).unwrap();
    let plan: Value =
        serde_json::from_str(prompt.lines().find(|line| line.starts_with('{')).unwrap()).unwrap();
    assert_eq!(plan["question"], query);
}

#[test]
fn details_are_local_preserve_session_and_reject_changed_sources() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(d.path(), vec![selected(ids.clone()), assembled(ids)]);
    let first = answer(d.path(), "Button colors?");
    assert_eq!(first["answer_mode"], "source_context");
    assert!(first["aspects"][0].get("answer").is_none());
    assert_eq!(first["detail_level"], "summary");
    for field in ["coverage", "calls_scheduled", "errors", "continue"] {
        assert!(
            first.get(field).is_none(),
            "unexpected public diagnostic: {field}"
        );
    }
    assert!(first["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["quote"].is_string()));
    let message = first["details"].as_str().unwrap();
    let session = d.path().join(format!(
        "memory/runtime/unified/sessions/{}.json",
        first["context_session"].as_str().unwrap()
    ));
    let before = fs::read(&session).unwrap();
    // Details must not launch deferred error-analysis agents, even when the queue is due.
    let config_path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["feedback"] =
        json!({"enabled":true,"agent":"cheap","timeout_seconds":2,"retry_cooldown_seconds":0});
    config["memory"]["cache"] = json!({"enabled":false});
    fs::write(config_path, config.to_string()).unwrap();
    for _ in 0..2 {
        assert!(!run(d.path(), "--bad").status.success());
    }
    let errors_path = d.path().join("memory/runtime/diagnostics/errors.json");
    let errors_before = fs::read(&errors_path).unwrap();
    let extended = format!("{message} Resolve missing keyboard rules.");
    let extra = answer(d.path(), &extended);
    assert_eq!(extra["additional_question_processed"], false);
    assert!(extra["notice"].as_str().unwrap().contains("not processed"));
    assert_eq!(
        extra["aspects"][0]["answer"],
        "Buttons are blue; deletion buttons are red."
    );
    assert_eq!(fs::read(&session).unwrap(), before);
    let detail = answer(d.path(), message);
    assert_eq!(fs::read(&errors_path).unwrap(), errors_before);
    // The stdin chat path has its own pre-request hook.
    let mut process = Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(d.path())
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", d.path().join("scenario.json"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    writeln!(process.stdin.take().unwrap(), "{extended}").unwrap();
    let piped = process.wait_with_output().unwrap();
    assert!(
        piped.status.success(),
        "{}",
        String::from_utf8_lossy(&piped.stderr)
    );
    assert_eq!(fs::read(&errors_path).unwrap(), errors_before);
    assert_eq!(detail["detail_level"], "full");
    assert_eq!(detail["cache"], "details");
    assert_eq!(diagnostics(d.path(), &detail)["calls_scheduled"], 0);
    assert_eq!(
        detail["aspects"][0]["answer"],
        "Buttons are blue; deletion buttons are red."
    );
    assert_eq!(detail["evidence"][0]["quote"], "Buttons blue.");
    assert_eq!(fs::read(&session).unwrap(), before);
    assert_eq!(
        detail["evidence"][0]["source"],
        first["evidence"][0]["source"]
    );
    fs::write(d.path().join("memory/docs/ui.md"), "Buttons green.").unwrap();
    let rejected = run(d.path(), &extended);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("sources changed"));
    assert!(!String::from_utf8_lossy(&rejected.stdout).contains("Buttons blue"));
    assert_eq!(fs::read(&session).unwrap(), before);
    assert!(!run(d.path(), "@details").status.success());
}

#[test]
fn repeated_topic_sends_delta_and_details_restore_self_contained_answer() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(d.path(), vec![selected(ids.clone()), assembled(ids)]);
    let first = answer(d.path(), "Button colors?");
    assert_eq!(first["answer_mode"], "source_context");
    assert!(first["aspects"][0].get("answer").is_none());
    assert_eq!(first["response_mode"], "full");
    let follow = format!(
        "@context:{} Button colors?",
        first["context_session"].as_str().unwrap()
    );
    let repeated = answer(d.path(), &follow);
    assert_eq!(diagnostics(d.path(), &repeated)["calls_scheduled"], 0);
    assert_eq!(repeated["response_mode"], "delta");
    assert_eq!(
        repeated["aspects"][0]["answer"],
        "Buttons are blue; deletion buttons are red."
    );
    assert!(repeated.get("answer").is_none());
    assert!(repeated["evidence"].as_array().unwrap().is_empty());
    assert_eq!(
        repeated["reused_evidence"],
        json!(first["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["ref"].clone())
            .collect::<Vec<_>>())
    );
    let details = answer(d.path(), repeated["details"].as_str().unwrap());
    assert_eq!(details["response_mode"], "full");
    assert_eq!(
        details["aspects"][0]["answer"],
        "Buttons are blue; deletion buttons are red."
    );
    assert_eq!(details["evidence"].as_array().unwrap().len(), 2);
    let second_repeat = answer(d.path(), &follow);
    assert_eq!(
        second_repeat["aspects"][0]["answer"],
        "Buttons are blue; deletion buttons are red."
    );
    assert_eq!(
        second_repeat["reused_evidence"],
        repeated["reused_evidence"]
    );
}

#[test]
fn expanded_verification_focuses_on_missing_aspects_but_can_revise_found_ones() {
    let d = fixture();
    let config_path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["memory"]["unified"]["max_candidates"] = json!(1);
    fs::write(config_path, config.to_string()).unwrap();
    fs::write(
        d.path().join("memory/docs/extra.md"),
        "Buttons keyboard support.",
    )
    .unwrap();
    let extra = format!("doc-{}", hash("memory/docs/extra.md"));
    let eid = format!("{extra}:L1");
    let first = json!({"answer":"Blue; keyboard unknown.","select":[fid(1)],"aspects":[{"question":"Color?","status":"found","evidence":[fid(1)]},{"question":"Keyboard?","status":"missing","evidence":[]}],"need":[extra],"conflicts":[]});
    let last = json!({"answer":"Keyboard supported; color needs clarification.","select":[fid(1),eid],"aspects":[{"question":"Color?","status":"missing","evidence":[fid(1)]},{"question":"Keyboard?","status":"found","evidence":[eid]}],"need":[],"conflicts":[]});
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), first, selected(vec![eid]), last],
    );
    let path = d.path().join("scenario.json");
    let mut script: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    script["calls"][3]["save_prompt_to"] = json!(d.path().join("verify-prompt.txt"));
    fs::write(path, script.to_string()).unwrap();
    let result = answer(d.path(), "Blue buttons?");
    assert_eq!(result["status"], "partial", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 6);
    let text = fs::read_to_string(d.path().join("verify-prompt.txt")).unwrap();
    let prompt: Value =
        serde_json::from_str(text.lines().find(|l| l.starts_with('{')).unwrap()).unwrap();
    assert_eq!(
        prompt["verification_focus"],
        json!(["Keyboard?"]),
        "previous={} originals={}",
        prompt["previous_aspects"],
        prompt["originals"]
    );
    assert_eq!(prompt["previous_aspects"][0]["evidence"], json!(["1"]));
    assert_eq!(result["aspects"][0]["status"], "missing");
}

#[test]
fn omitted_requested_aspect_is_unchecked_not_a_confirmed_gap() {
    let d = fixture();
    fs::write(
        d.path().join("planned-aspects.json"),
        json!(["Button colors and exceptions", "Keyboard?"]).to_string(),
    )
    .unwrap();
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
    );
    let result = answer(d.path(), "Buttons?");
    assert_eq!(result["status"], "partial");
    assert_eq!(result["aspects"][1]["search_state"], "incomplete");
}

#[test]
fn cached_followup_with_explicit_gap_stops_without_duplicate_verification() {
    let d = fixture();
    let mut missing = assembled(vec![fid(1)]);
    missing["aspects"][0]["status"] = json!("missing");
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)]), missing],
    );
    let first = answer(d.path(), "Button colors?");
    let second = answer(
        d.path(),
        &format!(
            "@context:{} Clarify colors?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(second["status"], "partial");
    assert_eq!(second["cache"], "reused_evidence");
    assert_eq!(diagnostics(d.path(), &second)["calls_scheduled"], 2);
    assert_eq!(
        second["aspects"][0]["search_state"],
        "not_found_in_reviewed_sources"
    );
}

#[test]
fn per_aspect_answer_removes_summary_but_keeps_citations_and_details() {
    let d = fixture();
    fs::write(
        d.path().join("planned-sources.json"),
        json!(["user_document"]).to_string(),
    )
    .unwrap();
    let mut a = assembled(vec![fid(1)]);
    a["aspects"][0]["answer"] = json!("Buttons blue.");
    scenario(d.path(), vec![selected(vec![fid(1)]), a]);
    let r = answer(d.path(), "Original button requirements?");
    assert_eq!(r["status"], "complete");
    assert_eq!(r["answer_format"], "per_aspect");
    assert!(r.get("answer").is_none());
    assert_eq!(r["aspects"][0]["answer"], "Buttons blue.");
    assert_eq!(r["evidence"][0]["quote"], "Buttons blue.");
    let detail = answer(d.path(), r["details"].as_str().unwrap());
    assert!(detail["answer"].is_string());
    let repeated = answer(
        d.path(),
        &format!(
            "@context:{} Original button requirements?",
            r["context_session"].as_str().unwrap()
        ),
    );
    assert!(repeated.get("answer_unchanged").is_none());
    assert_eq!(repeated["aspects"][0]["answer"], "Buttons blue.");
}

#[test]
fn source_plan_rejects_missing_or_unknown_authority_instead_of_defaulting() {
    for required in [json!([]), json!(["unknown"])] {
        let d = fixture();
        fs::write(d.path().join("planned-sources.json"), required.to_string()).unwrap();
        scenario(d.path(), vec![assembled(vec![fid(1)])]);
        let result = run(d.path(), "Original requirements?");
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("invalid requested aspect plan"));
    }
}

#[test]
fn compact_addresses_and_question_refs_round_trip_through_details_and_cache() {
    let d = fixture();
    let path = "memory/docs/long-name-original-button-rules-and-exceptions.md";
    fs::rename(d.path().join("memory/docs/ui.md"), d.path().join(path)).unwrap();
    let id = format!("doc-{}", hash(path));
    let ids = vec![format!("{id}:L1"), format!("{id}:L2")];
    let mut a = assembled(ids.clone());
    a["aspects"][0]["question"] =
        json!("Which original colors apply to ordinary and deletion buttons?");
    a["aspects"][0]["answer"] = json!("Ordinary blue; deletion red.");
    scenario(d.path(), vec![selected(ids), a]);
    let first = answer(d.path(), "Button colors?");
    assert_eq!(first["source_blocks"]["b1"]["source"], path);
    assert_eq!(first["evidence"][0]["source"], path);
    let detail = answer(d.path(), first["details"].as_str().unwrap());
    assert_eq!(detail["evidence"][0]["source"], path);
    assert!(detail.get("sources").is_none());
    assert!(detail["aspects"][0]["question"].is_string());
    assert_eq!(
        detail["aspects"][0]["question"],
        first["aspects"][0]["question"]
    );
    let repeat = answer(
        d.path(),
        &format!(
            "@context:{} Button colors?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(
        repeat["aspects"][0]["question"],
        first["aspects"][0]["question"]
    );
    assert!(repeat["aspects"][0].get("question_ref").is_none());
    assert_eq!(repeat["reused_evidence"].as_array().unwrap().len(), 2);
    assert!(repeat["evidence"].as_array().unwrap().is_empty());
    assert!(repeat.get("sources").is_none());
    assert_eq!(diagnostics(d.path(), &repeat)["calls_scheduled"], 0);
}

#[test]
fn invalid_parent_reference_gets_one_bounded_correction() {
    for corrected in [true, false] {
        let d = fixture();
        fs::write(
            d.path().join("memory/docs/ui.md"),
            hierarchy_document("# Buttons\nSave green."),
        )
        .unwrap();
        let child = section(&source(), "Buttons");
        let ids = vec![fid(2)];
        let bad = branch(vec!["invented:L2".into()]);
        scenario(
            d.path(),
            vec![
                route(vec![child]),
                selected(ids.clone()),
                bad.clone(),
                if corrected { branch(ids.clone()) } else { bad },
                assembled(ids),
            ],
        );
        let result = answer(d.path(), "docs");
        assert_eq!(
            result["status"],
            if corrected { "complete" } else { "partial" },
            "{result}"
        );
        assert!(!result.to_string().contains("invented:L2"));
        assert!(
            diagnostics(d.path(), &result)["calls_scheduled"]
                .as_u64()
                .unwrap()
                <= 7
        );
    }
}

#[test]
fn parent_correction_reserves_final_verification_budget() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        hierarchy_document("# Buttons\nSave green."),
    )
    .unwrap();
    let config_path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    // Planner + two workers + failed parent + verifier + grounding; no retry slot.
    config["memory"]["max_steps"] = json!(6);
    fs::write(config_path, config.to_string()).unwrap();
    scenario(
        d.path(),
        vec![
            route(vec![section(&source(), "Buttons")]),
            selected(vec![fid(2)]),
            branch(vec!["invented:L2".into()]),
            assembled(vec![fid(2)]),
        ],
    );
    let result = answer(d.path(), "docs");
    assert_eq!(result["status"], "partial");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 6);
}

#[test]
fn verifier_conflict_repair_receives_error_and_is_bounded() {
    for corrected in [true, false] {
        let d = fixture();
        let ids = vec![fid(1), fid(2)];
        let mut bad = assembled(ids.clone());
        bad["conflicts"] = json!([{"kind":"requirement_conflict","description":"Conflicting rule", "evidence":["invented:L1"]}]);
        let mut fixed = assembled(ids.clone());
        fixed["conflicts"] = json!([{"kind":"implementation_discrepancy","description":"Reported difference", "evidence":ids}]);
        scenario(
            d.path(),
            vec![
                selected(vec![fid(1), fid(2)]),
                bad.clone(),
                if corrected { fixed } else { bad },
            ],
        );
        let path = d.path().join("scenario.json");
        let mut script: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        script["calls"][2]["save_prompt_to"] = json!(d.path().join("repair-prompt.txt"));
        fs::write(path, script.to_string()).unwrap();
        let result = answer(d.path(), "Button colors?");
        assert_eq!(
            result["status"],
            if corrected { "complete" } else { "partial" },
            "{result}"
        );
        assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 4);
        let prompt = fs::read_to_string(d.path().join("repair-prompt.txt")).unwrap();
        assert!(prompt.contains("validation_repair"));
        assert!(prompt.contains("invalid conflict evidence"));
        assert!(prompt.contains("invented:L1"));
        assert!(prompt.contains("allowed_evidence"));
        if corrected {
            assert_eq!(result["conflicts"].as_array().unwrap().len(), 1);
        }
    }
}

#[test]
fn duplicate_requested_answers_receive_one_correction_without_choosing_a_duplicate() {
    for corrected in [true, false] {
        let d = fixture();
        fs::write(
            d.path().join("planned-aspects.json"),
            json!(["Button colors and exceptions"]).to_string(),
        )
        .unwrap();
        let mut bad = assembled(vec![fid(1)]);
        bad["aspects"][0]["answer"] = json!("First duplicate answer");
        let mut duplicate = bad["aspects"][0].clone();
        duplicate["answer"] = json!("Different duplicate answer");
        bad["aspects"].as_array_mut().unwrap().push(duplicate);
        let mut fixed = assembled(vec![fid(1)]);
        fixed["aspects"][0]["answer"] = json!("Buttons are blue.");
        scenario(
            d.path(),
            vec![
                selected(vec![fid(1)]),
                bad.clone(),
                if corrected { fixed } else { bad },
            ],
        );
        let path = d.path().join("scenario.json");
        let mut script: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        script["calls"][2]["save_prompt_to"] = json!(d.path().join("duplicate-repair.txt"));
        fs::write(path, script.to_string()).unwrap();
        let result = answer(d.path(), "Button colors?");
        assert_eq!(
            result["status"],
            if corrected { "complete" } else { "partial" },
            "{result}"
        );
        assert_eq!(
            diagnostics(d.path(), &result)["calls_scheduled"],
            if corrected { 5 } else { 4 }
        );
        assert!(!result.to_string().contains("duplicate answer"));
        assert_eq!(result["evidence"][0]["quote"], "Buttons blue.");
        let prompt = fs::read_to_string(d.path().join("duplicate-repair.txt")).unwrap();
        assert!(prompt.contains("duplicate requested aspect answers"));
        assert!(prompt.contains("exactly one answer per requested aspect"));
        assert!(prompt.contains("Button colors and exceptions"));
        if corrected {
            assert_eq!(result["aspects"].as_array().unwrap().len(), 1);
        }
    }
}

#[test]
fn local_reference_faults_recover_inside_one_caller_request() {
    for broken in ["unknown", "malformed", "empty", "wrong_shape"] {
        let d = fixture();
        let bad = match broken {
            "unknown" => selected(vec!["999".into()]).to_string(),
            "malformed" => "{bad".into(),
            "empty" => String::new(),
            _ => "42".into(),
        };
        fs::write(d.path().join("scenario.json"),json!({"state_file":d.path().join("fault-calls.json"),"calls":[
            {"final_message":bad},
            {"final_message":selected(vec!["1".into()]).to_string(),"save_prompt_to":d.path().join("local-prompt.txt")},
            {"final_message":assembled(vec![fid(1)]).to_string()}
        ]}).to_string()).unwrap();
        let result = answer(d.path(), "Buttons?");
        assert_eq!(result["status"], "complete", "{broken}: {result}");
        assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 5);
        assert_eq!(result["evidence"][0]["quote"], "Buttons blue.");
        let prompt = fs::read_to_string(d.path().join("local-prompt.txt")).unwrap();
        assert!(
            !prompt.contains(&fid(1)),
            "canonical own fragment ID leaked"
        );
        assert!(prompt.contains("fragment_reference_rules"));
        let saved: Value = serde_json::from_slice(
            &fs::read(d.path().join(format!(
                "memory/runtime/unified/sessions/{}.json",
                result["context_session"].as_str().unwrap()
            )))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["workers"][source()]["select"], json!([fid(1)]));
        assert_eq!(
            fs::read_to_string(d.path().join("memory/docs/ui.md")).unwrap(),
            "Buttons blue.\nDeletion buttons red."
        );
    }
}

#[test]
fn local_unknown_number_stays_partial_after_one_correction() {
    let d = fixture();
    let mut missing = assembled(vec![]);
    missing["aspects"][0]["status"] = json!("missing");
    scenario(
        d.path(),
        vec![
            selected(vec!["999".into()]),
            selected(vec!["999".into()]),
            missing,
        ],
    );
    let result = answer(d.path(), "Buttons?");
    assert_eq!(result["status"], "partial");
    assert!(
        diagnostics(d.path(), &result)["calls_scheduled"]
            .as_u64()
            .unwrap()
            <= 4
    );
}

#[test]
fn timed_out_worker_requires_one_explicit_resume_without_losing_originals() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["timeout_seconds"] = json!(2);
    fs::write(&path, config.to_string()).unwrap();
    fs::write(d.path().join("scenario.json"),json!({"state_file":d.path().join("timeout-calls.json"),"calls":[{"delay_ms":5000,"final_message":selected(vec!["1".into()]).to_string()}]}).to_string()).unwrap();
    let first = answer(d.path(), "Buttons?");
    assert_eq!(first["status"], "partial");
    assert_eq!(diagnostics(d.path(), &first)["calls_scheduled"], 2);
    config["memory"]["timeout_seconds"] = json!(20);
    fs::write(path, config.to_string()).unwrap();
    scenario(
        d.path(),
        vec![
            json!({"aspects":["Button colors and exceptions"],"intents":["factual_question"]}),
            selected(vec!["1".into()]),
            assembled(vec![fid(1)]),
        ],
    );
    let resumed = answer(
        d.path(),
        &format!(
            "@context:{} Buttons?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(resumed["status"], "complete", "{resumed}");
    assert_eq!(diagnostics(d.path(), &resumed)["calls_scheduled"], 4);
    assert_eq!(resumed["evidence"][0]["quote"], "Buttons blue.");
}

#[test]
fn invalid_optional_local_annotations_do_not_discard_valid_selection() {
    let d = fixture();
    let mut selection = selected(vec!["1".into()]);
    selection["groups"] = json!([{"title":"Invalid","fragments":[999]}]);
    selection["elements"] =
        json!([{"kind":"fact","status":"documented","text":"Invalid","evidence":[999]}]);
    selection["links"] = json!([{"target":"other","kind":"applies_to","evidence":[999]}]);
    scenario(d.path(), vec![selection, assembled(vec![fid(1)])]);
    let result = answer(d.path(), "Buttons?");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 4);
}

#[test]
fn parent_and_verifier_local_references_restore_canonical_citations() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        hierarchy_document("# Buttons\nSave green."),
    )
    .unwrap();
    let child = section(&source(), "Buttons");
    scenario(
        d.path(),
        vec![
            route(vec![child]),
            selected(vec!["2".into()]),
            branch(vec!["1".into()]),
            assembled(vec!["2".into()]),
        ],
    );
    // Parent sees only selected L2 (local 1); verifier sees L1 and L2 (local 2).
    let path = d.path().join("scenario.json");
    let mut script: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    script["calls"][2]["save_prompt_to"] = json!(d.path().join("parent-prompt.txt"));
    script["calls"][3]["save_prompt_to"] = json!(d.path().join("verifier-prompt.txt"));
    fs::write(path, script.to_string()).unwrap();
    let result = answer(d.path(), "docs");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(result["evidence"][0]["line"], 2);
    assert_eq!(result["evidence"][0]["quote"], "Save green.");
    for name in ["parent-prompt.txt", "verifier-prompt.txt"] {
        let prompt = fs::read_to_string(d.path().join(name)).unwrap();
        assert!(
            !prompt.contains(&fid(2)),
            "canonical reference leaked: {name}"
        );
        assert!(prompt.contains("reference_rules"));
    }
}

#[test]
fn knowledge_status_answer_and_missing_proof_remain_distinct() {
    for (question, intent, status, expected) in [
        (
            "What is reported about buttons and was it independently verified?",
            "verification_status",
            "found",
            "complete",
        ),
        (
            "Independently prove the button behavior.",
            "independent_proof",
            "missing",
            "partial",
        ),
    ] {
        let d = fixture();
        fs::write(
            d.path().join("planned-intents.json"),
            json!([intent]).to_string(),
        )
        .unwrap();
        fs::write(
            d.path().join("memory/docs/ui.md"),
            "Buttons reported blue; not independently verified.",
        )
        .unwrap();
        let mut a = assembled(vec![fid(1)]);
        a["aspects"][0]["question"] = json!(question);
        a["aspects"][0]["status"] = json!(status);
        a["aspects"][0]["answer"] =
            json!("Reported blue; independent verification not established in reviewed sources.");
        if intent == "verification_status" {
            a["aspects"][0].as_object_mut().unwrap().remove("status");
            a["aspects"][0]["verification"] = json!({"state":"explicit_unverified", "support":[{"id":fid(1),"quote":"not independently verified."}]});
        }
        scenario(d.path(), vec![selected(vec![fid(1)]), a]);
        let path = d.path().join("scenario.json");
        let mut script: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        script["calls"][1]["save_prompt_to"] = json!(d.path().join("verify-prompt.txt"));
        fs::write(path, script.to_string()).unwrap();
        let r = answer(d.path(), question);
        assert_eq!(r["status"], expected, "{r}");
        assert_eq!(r["aspects"][0]["status"], status);
        assert!(r["aspects"][0]["answer"]
            .as_str()
            .unwrap()
            .contains("not established in reviewed sources"));
        let plan = fs::read_to_string(d.path().join("plan-prompt.txt")).unwrap();
        assert!(plan.contains("verification status is itself the requested fact"));
        assert!(plan.contains(
            "Evidence and output constraints are acceptance criteria, not extra aspects"
        ));
        let verify = fs::read_to_string(d.path().join("verify-prompt.txt")).unwrap();
        assert!(verify.contains("keep that aspect missing unless supplied evidence contains applicable independent validation"));
        assert!(verify.contains(intent));
        assert!(verify.contains("Do not repeat source file paths"));
        assert!(
            verify.contains("when it is itself a requested fact or necessary technical content")
        );
        let detail = answer(d.path(), r["details"].as_str().unwrap());
        assert_eq!(detail["status"], expected);
        assert_eq!(detail["aspects"][0]["status"], status);
    }
}

#[test]
fn requested_path_and_verbatim_evidence_survive_concise_answers() {
    let d = fixture();
    let original = "Edit src/settings.ts; endpoint https://example.test/api/settings.";
    fs::write(d.path().join("memory/docs/ui.md"), original).unwrap();
    let mut a = assembled(vec![fid(1)]);
    a["aspects"][0]["question"] = json!("Which file and endpoint implement settings?");
    a["aspects"][0]["answer"] = json!(original);
    scenario(d.path(), vec![selected(vec![fid(1)]), a]);
    let r = answer(d.path(), "Which file and endpoint implement settings? Cite sources without repeating attribution in the answer.");
    assert_eq!(r["status"], "complete");
    assert_eq!(r["aspects"].as_array().unwrap().len(), 1);
    assert_eq!(r["answer_mode"], "source_context");
    assert!(r["aspects"][0].get("answer").is_none());
    assert_eq!(r["evidence"][0]["quote"], original);
    let detail = answer(d.path(), r["details"].as_str().unwrap());
    assert_eq!(detail["aspects"][0]["answer"], original);
    assert_eq!(detail["evidence"][0]["quote"], original);
}

#[test]
fn mixed_intents_are_persisted_and_derive_only_original_requirement_authority() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        "Buttons blue. Reported only; not independently verified.",
    )
    .unwrap();
    let intents = json!([
        "original_requirement",
        "reported_state",
        "verification_status",
        "independent_proof"
    ]);
    fs::write(d.path().join("planned-intents.json"), intents.to_string()).unwrap();
    let questions = [
        "What is required?",
        "What does memory report?",
        "What verification is recorded?",
        "Prove behavior independently?",
    ];
    let mut a = assembled(vec![fid(1)]);
    a["aspects"] = json!(questions.iter().enumerate().map(|(i,q)| json!({"question":q,"status":if i == 3 {"missing"}else{"found"},"answer":"Recorded information only.","evidence":[fid(1)]})).collect::<Vec<_>>());
    a["aspects"][2].as_object_mut().unwrap().remove("status");
    a["aspects"][2]["verification"] = json!({"state":"explicit_unverified", "support":[{"id":fid(1),"quote":"not independently verified."}]});
    scenario(d.path(), vec![selected(vec![fid(1)]), a]);
    let path = d.path().join("scenario.json");
    let mut script: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    script["calls"][1]["save_prompt_to"] = json!(d.path().join("verify-prompt.txt"));
    fs::write(path, script.to_string()).unwrap();
    let r = answer(d.path(), "Compare original requirements and reported behavior, what verification is recorded, and independently prove behavior.");
    assert_eq!(r["status"], "partial");
    let session: Value = serde_json::from_slice(
        &fs::read(d.path().join(format!(
            "memory/runtime/unified/sessions/{}.json",
            r["context_session"].as_str().unwrap()
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(session["requested_intents"], intents);
    assert_eq!(
        session["source_requirements"],
        json!(["user_document", "any", "any", "any"])
    );
    let prompt = fs::read_to_string(d.path().join("verify-prompt.txt")).unwrap();
    let payload: Value =
        serde_json::from_str(prompt.lines().find(|l| l.starts_with('{')).unwrap()).unwrap();
    assert_eq!(payload["requested_intents"], intents);
    assert_eq!(
        payload["source_requirements"],
        session["source_requirements"]
    );
}

#[test]
fn legacy_context_without_intents_is_replanned_before_exact_cache_reuse() {
    let d = fixture();
    let a = assembled(vec![fid(1)]);
    scenario(d.path(), vec![selected(vec![fid(1)]), a.clone()]);
    let first = answer(d.path(), "Button colors?");
    let path = d.path().join(format!(
        "memory/runtime/unified/sessions/{}.json",
        first["context_session"].as_str().unwrap()
    ));
    let mut session: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    session.as_object_mut().unwrap().remove("requested_intents");
    fs::write(&path, session.to_string()).unwrap();
    scenario(d.path(), vec![a]);
    let second = answer(
        d.path(),
        &format!(
            "@context:{} Button colors?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert_eq!(second["status"], "complete");
    assert_eq!(diagnostics(d.path(), &second)["calls_scheduled"], 3);
    let session: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(session["requested_intents"], json!(["factual_question"]));
}

#[test]
fn original_requirement_expands_to_a_document_exception_outside_initial_roots() {
    let d = fixture();
    fs::write(
        d.path().join("planned-intents.json"),
        json!(["original_requirement"]).to_string(),
    )
    .unwrap();
    let mut owners = Vec::new();
    for n in 0..6 {
        let path = format!("memory/docs/requirement{n}.md");
        fs::write(d.path().join(&path), "Uniquequery enabled.").unwrap();
        owners.push((format!("doc-{}", hash(&path)), path));
    }
    owners.sort();
    let exception = "Uniquequery exception: disable when unchanged.";
    fs::write(d.path().join(&owners[5].1), exception).unwrap();
    let initial: Vec<_> = owners
        .iter()
        .take(1)
        .map(|(id, _)| format!("{id}:L1"))
        .collect();
    let extra = format!("{}:L1", owners[5].0);
    let question = "What Uniquequery conditions are required?";
    let mut need = assembled(initial.clone());
    need["aspects"][0]["question"] = json!(question);
    need["aspects"][0]["status"] = json!("missing");
    need["need"] = json!([owners[5].0]);
    let mut all = initial.clone();
    all.push(extra.clone());
    let mut final_answer = assembled(all);
    final_answer["aspects"][0]["question"] = json!(question);
    final_answer["aspects"][0]["answer"] = json!("Enabled except when unchanged: then disable.");
    let mut calls: Vec<_> = initial
        .iter()
        .map(|id| selected(vec![id.clone()]))
        .collect();
    calls.extend([need, selected(vec![extra]), final_answer]);
    scenario(d.path(), calls);
    let path = d.path().join("scenario.json");
    let mut script: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    script["calls"][1]["save_prompt_to"] = json!(d.path().join("initial-verification.txt"));
    fs::write(path, script.to_string()).unwrap();
    let result = answer(d.path(), "Uniquequery?");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 6);
    assert_eq!(
        diagnostics(d.path(), &result)["coverage"]["reviewed_threads"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(result["aspects"][0]["answer"]
        .as_str()
        .unwrap()
        .contains("unchanged"));
    assert!(result["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["quote"] == exception
            && e.get("authority")
                .unwrap_or(&result["evidence_defaults"]["authority"])
                == "user_document"));
    let prompt = fs::read_to_string(d.path().join("initial-verification.txt")).unwrap();
    let payload: Value =
        serde_json::from_str(prompt.lines().find(|l| l.starts_with('{')).unwrap()).unwrap();
    assert!(payload["originals"]
        .as_array()
        .unwrap()
        .iter()
        .all(|o| o["source"]["path"] != owners[5].1));
    assert!(payload["catalog"].to_string().contains(&owners[5].0));
}

#[test]
fn original_requirement_with_only_advisory_candidates_cannot_complete() {
    let d = fixture();
    fs::remove_file(d.path().join("memory/docs/ui.md")).unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fs::create_dir_all(d.path().join("memory/threads/aa")).unwrap();
    fs::create_dir_all(d.path().join("memory/thread-agents")).unwrap();
    fs::write(d.path().join(format!("memory/threads/aa/{id}.md")),format!("---\nformat: climemory-memory-thread/1\nid: {id}\nslug: buttons\ntitle: Button colors\n---\n")).unwrap();
    fs::write(d.path().join(format!("memory/thread-agents/{id}.json")),json!({"format":"climemory/thread-agent-2","thread_id":id,"agent":"cheap","parent":null,"memory":"Buttons reported blue.","revision":1,"last_dialogue":null,"updated":"2026-09-27T00:00:00Z"}).to_string()).unwrap();
    fs::write(
        d.path().join("planned-intents.json"),
        json!(["original_requirement"]).to_string(),
    )
    .unwrap();
    let mut missing = assembled(vec![]);
    missing["answer"] = json!("Original button requirement is not established.");
    missing["aspects"][0]["status"] = json!("missing");
    missing["aspects"][0]["answer"] = json!("Original button requirement is not established.");
    scenario(d.path(), vec![missing]);
    let result = answer(d.path(), "Original button requirements?");
    assert_eq!(result["status"], "partial", "{result}");
    assert_eq!(result["aspects"][0]["status"], "missing");
    assert!(result["evidence"].as_array().unwrap().is_empty());
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 2);
    assert!(
        diagnostics(d.path(), &result)["coverage"]["reviewed_threads"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let index: Value = serde_json::from_slice(
        &fs::read(d.path().join("memory/runtime/unified/index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index["sources"].as_array().unwrap().len(), 1);
    assert_eq!(index["sources"][0]["authority"], "advisory_memory");
}

#[test]
fn compact_evidence_defaults_leave_details_expanded_and_cache_refs_unchanged() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    scenario(d.path(), vec![selected(ids.clone()), assembled(ids)]);
    let first = answer(d.path(), "Button colors?");
    assert_eq!(
        first["evidence_defaults"],
        json!({"authority":"user_document"})
    );
    assert!(first["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e.get("authority").is_none()));
    let detail = answer(d.path(), first["details"].as_str().unwrap());
    assert!(detail.get("evidence_defaults").is_none());
    assert!(detail["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["authority"] == "user_document"));
    assert_eq!(
        first["evidence"][0]["quote"],
        detail["evidence"][0]["quote"]
    );
    assert_eq!(first["evidence"][0]["ref"], detail["evidence"][0]["ref"]);
    let repeated = answer(
        d.path(),
        &format!(
            "@context:{} Button colors?",
            first["context_session"].as_str().unwrap()
        ),
    );
    assert!(repeated.get("evidence_defaults").is_none());
    assert!(repeated["evidence"].as_array().unwrap().is_empty());
    assert_eq!(
        repeated["reused_evidence"],
        json!(first["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["ref"].clone())
            .collect::<Vec<_>>())
    );
    assert_eq!(diagnostics(d.path(), &repeated)["calls_scheduled"], 0);
}

#[test]
fn single_aspect_winner_does_not_consult_merely_related_initial_candidates() {
    let d = fixture();
    let related = "memory/docs/related.md";
    let specific = "memory/docs/specific.md";
    fs::write(d.path().join(related), "Uniquequery background.").unwrap();
    fs::write(
        d.path().join(specific),
        "Uniquequery exactrare requirement: enabled unless unchanged.",
    )
    .unwrap();
    fs::write(
        d.path().join("planned-intents.json"),
        json!(["original_requirement"]).to_string(),
    )
    .unwrap();
    let id = format!("doc-{}:L1", hash(specific));
    scenario(
        d.path(),
        vec![
            selected(vec![id.clone()]),
            assembled_aspects(vec![id], &["Exactrare requirement?"]),
        ],
    );
    let result = answer(d.path(), "Uniquequery exactrare requirement?");
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(diagnostics(d.path(), &result)["calls_scheduled"], 4);
    let reviewed = diagnostics(d.path(), &result)["coverage"]["reviewed_threads"].clone();
    assert_eq!(reviewed, json!([format!("doc-{}", hash(specific))]));
    assert_eq!(
        result["evidence"][0]["quote"],
        "Uniquequery exactrare requirement: enabled unless unchanged."
    );
}

#[test]
fn verified_memory_excerpt_prefix_survives_restart_and_details_keep_original() {
    for factor_exact in [true, false] {
        let d = fixture();
        fs::remove_file(d.path().join("memory/docs/ui.md")).unwrap();
        let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        fs::create_dir_all(d.path().join("memory/threads/aa")).unwrap();
        fs::create_dir_all(d.path().join("memory/thread-agents")).unwrap();
        fs::write(d.path().join(format!("memory/threads/aa/{id}.md")),format!("---\nformat: climemory-memory-thread/1\nid: {id}\nslug: binary\ntitle: Binary reported rules\n---\n")).unwrap();
        let excerpt = "Division truncates toward zero; remainder follows the dividend sign.";
        let original=format!("Reported binary constraints, not independently verified: {excerpt} Unrelated history and theme settings remain unchanged.");
        fs::write(d.path().join(format!("memory/thread-agents/{id}.json")),json!({"format":"climemory/thread-agent-2","thread_id":id,"agent":"cheap","parent":null,"memory":original,"revision":1,"last_dialogue":null,"updated":"2026-09-27T00:00:00Z"}).to_string()).unwrap();
        let fid = format!("memory-{id}:L1");
        let prefix = "Reported binary behavior; not independently verified:";
        let expected = if factor_exact {
            format!("{prefix}\n{excerpt}")
        } else {
            "Memory reports truncation toward zero and a dividend-signed remainder; this was not independently verified.".to_owned()
        };
        let a = json!({"answer":"","select":[fid],"need":[],"conflicts":[],"excerpts":[{"id":fid,"quote":excerpt}],"aspects":[{"question":"What binary arithmetic behavior was reported?","status":"found","answer":expected,"self_contained":true,"evidence":[fid]}]});
        scenario(d.path(), vec![selected(vec![fid]), a]);
        grounding_reply(
            d.path(),
            json!({"aspects":[{"index":0,"supported":true,"answer":expected,"self_contained":true}]}),
        );
        let output = run(d.path(), "What binary arithmetic behavior was reported?");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let raw: Value = serde_json::from_slice(&output.stdout).unwrap();
        if factor_exact {
            assert_eq!(raw["aspects"][0]["answer_from_evidence"], true, "{raw}");
            assert_eq!(raw["aspects"][0]["answer_prefix"], prefix);
        } else {
            let aspects = raw["aspects"].to_string();
            assert!(!aspects.contains("\"answer_from_evidence\""));
            assert!(!aspects.contains("\"answer_prefix\""));
        }
        let first = compact_wire::expand(raw);
        assert_eq!(first["aspects"][0]["answer"], expected);
        assert_eq!(first["evidence"][0]["quote"], excerpt);
        assert!(first["evidence"][0].get("summary_quote").is_none());
        let context = first["context_session"].as_str().unwrap();
        let path = d
            .path()
            .join(format!("memory/runtime/unified/sessions/{context}.json"));
        let before = fs::read(&path).unwrap();
        let state: Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(state["response"]["evidence"][0]["quote"], original);
        assert!(state["delivered_evidence"]
            .as_object()
            .unwrap()
            .values()
            .any(|e| e["quote"] == excerpt));
        let detail = answer(d.path(), &format!("@context:{context} @details"));
        assert_eq!(detail["evidence"][0]["quote"], original);
        assert!(detail["evidence"][0].get("summary_quote").is_none());
        assert_eq!(detail["aspects"][0]["answer"], expected);
        assert_eq!(fs::read(&path).unwrap(), before);
        let repeat = answer(
            d.path(),
            &format!("@context:{context} What binary arithmetic behavior was reported?"),
        );
        assert_eq!(repeat["aspects"][0]["answer"], expected);
        assert_eq!(repeat["reused_evidence"].as_array().unwrap().len(), 1);
        assert!(repeat["evidence"].as_array().unwrap().is_empty());
    }
}

#[test]
fn complete_document_aspects_without_aggregate_still_deliver_source_context() {
    let d = fixture();
    let ids = vec![fid(1), fid(2)];
    let mut a = assembled(ids.clone());
    a["answer"] = json!("");
    a["aspects"][0]["answer"] = json!("Buttons are blue; deletion buttons are red.");
    a["aspects"][0]["self_contained"] = json!(true);
    scenario(d.path(), vec![selected(ids), a]);
    let first = answer(d.path(), "Button colors?");
    assert_eq!(first["status"], "complete");
    assert_eq!(first["detail_level"], "summary");
    assert_eq!(first["answer_mode"], "source_context");
    assert!(first["aspects"][0].get("answer").is_none());
    assert!(first["source_blocks"].is_object(), "{first}");
    let details = answer(d.path(), first["details"].as_str().unwrap());
    assert_eq!(details["detail_level"], "full");
    assert_eq!(
        details["aspects"][0]["answer"],
        "Buttons are blue; deletion buttons are red."
    );
    assert_eq!(details["evidence"][0]["quote"], "Buttons blue.");
    assert!(details.get("source_blocks").is_none());
}

#[test]
fn source_first_wire_keeps_originals_questions_and_details_across_restart() {
    let d = fixture();
    let source = "# Actions\n\nPrimary buttons are blue.\nException: deletion buttons are red.\n# Accessibility\nAll controls need visible focus.";
    fs::write(d.path().join("memory/docs/ui.md"), source).unwrap();
    let question = "What button colors are required?";
    let interpretation = "The primary-button rule requires blue buttons, with a component-specific exception requiring red deletion buttons. The deletion exception applies only to deletion buttons; it does not change the blue requirement for other primary buttons. These colors are requirements from the user document, not proof of any implementation.";
    let verdict = json!({"answer":"","select":[fid(3),fid(4)],"need":[],"conflicts":[],"aspects":[{"question":question,"status":"found","answer":interpretation,"self_contained":true,"evidence":[fid(3),fid(4)]}]});
    scenario(d.path(), vec![selected(vec![fid(3), fid(4)]), verdict]);
    let output = run(d.path(), question);
    assert!(output.status.success());
    let wire: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(wire["answer_mode"], "source_context", "{wire}");
    assert!(!wire.to_string().contains(interpretation));
    let public = compact_wire::expand(wire);
    assert_eq!(public["status"], "complete");
    assert_eq!(public["aspects"][0]["question"], question);
    assert!(public["aspects"][0].get("answer").is_none());
    assert!(public["aspects"][0].get("answer_from_evidence").is_none());
    let lines = &public["source_blocks"]["b1"]["numbered_lines"];
    assert_eq!(
        lines,
        &json!([
            [1, "# Actions"],
            [2, ""],
            [3, "Primary buttons are blue."],
            [4, "Exception: deletion buttons are red."],
            [5, "# Accessibility"],
            [6, "All controls need visible focus."]
        ])
    );
    for reference in public["aspects"][0]["evidence"].as_array().unwrap() {
        assert!(public["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| &row["ref"] == reference));
    }
    let context = public["context_session"].as_str().unwrap();
    let state_path = d
        .path()
        .join(format!("memory/runtime/unified/sessions/{context}.json"));
    let saved = fs::read(&state_path).unwrap();
    let state: Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(state["response"]["aspects"][0]["answer"], interpretation);
    let details = answer(d.path(), &format!("@context:{context} @details"));
    assert!(details.get("answer_mode").is_none());
    assert_eq!(details["aspects"][0]["answer"], interpretation);
    assert_eq!(fs::read(&state_path).unwrap(), saved);
    let repeat = answer(d.path(), &format!("@context:{context} {question}"));
    assert!(repeat.get("answer_mode").is_none());
    assert_eq!(repeat["aspects"][0]["answer"], interpretation);
    assert!(repeat.get("source_blocks").is_none());
    assert_eq!(repeat["cache"], "hit");
}

#[test]
fn source_first_keeps_ordinary_wire_when_retaining_question_costs_more() {
    let d = fixture();
    fs::write(d.path().join("memory/docs/ui.md"), "Buttons are blue.").unwrap();
    let question = "For the primary buttons in the application's general settings, using the original documented requirements and preserving their precise applicability, what color is required?";
    let verdict = json!({"answer":"","select":[fid(1)],"need":[],"conflicts":[],"aspects":[{"question":question,"status":"found","answer":"Buttons are blue.","self_contained":true,"evidence":[fid(1)]}]});
    scenario(d.path(), vec![selected(vec![fid(1)]), verdict]);
    grounding_reply(
        d.path(),
        json!({"aspects":[{"index":0,"supported":true,"answer":"Buttons are blue.","self_contained":true}]}),
    );
    let output = run(d.path(), question);
    assert!(output.status.success());
    let wire: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(wire.get("answer_mode").is_none(), "{wire}");
    let public = compact_wire::expand(wire);
    assert_eq!(public["status"], "complete");
    assert_eq!(public["aspects"][0]["answer"], "Buttons are blue.");
    assert!(public["source_blocks"].is_object());
}

fn grounding_reply(root: &Path, reply: Value) {
    let path = root.join("scenario.json");
    let mut script: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    script["operation_calls"]["unified_grounding"]
        .as_object_mut()
        .unwrap()
        .remove("indexed_aspect_reply");
    script["operation_calls"]["unified_grounding"]["final_message"] = json!(reply.to_string());
    fs::write(path, script.to_string()).unwrap();
}

#[test]
fn evidence_first_originals_and_only_winning_receipts_survive_restart() {
    for choose_evidence in [true, false] {
        let d = fixture();
        fs::remove_file(d.path().join("memory/docs/ui.md")).unwrap();
        let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        fs::create_dir_all(d.path().join("memory/threads/aa")).unwrap();
        fs::create_dir_all(d.path().join("memory/thread-agents")).unwrap();
        fs::write(d.path().join(format!("memory/threads/aa/{id}.md")), format!("---\nformat: climemory-memory-thread/1\nid: {id}\nslug: arithmetic\ntitle: Reported arithmetic\n---\n")).unwrap();
        let excerpt = "Division truncates toward zero.";
        let original = format!(
            "Reported arithmetic, not independently verified: {excerpt} {}",
            if choose_evidence {
                "The report does not establish current implementation.".into()
            } else {
                "Unrelated historical context remains unverified. ".repeat(30)
            }
        );
        fs::write(d.path().join(format!("memory/thread-agents/{id}.json")), json!({"format":"climemory/thread-agent-2","thread_id":id,"agent":"cheap","parent":null,"memory":original,"revision":1,"last_dialogue":null,"updated":"2026-09-27T00:00:00Z"}).to_string()).unwrap();
        fs::write(
            d.path().join("planned-intents.json"),
            json!(["reported_state"]).to_string(),
        )
        .unwrap();
        let fid = format!("memory-{id}:L1");
        let question = "What arithmetic behavior was reported?";
        // Simulate a mistaken supported paraphrase: source-first must not seed its extra claim.
        let interpretation = if choose_evidence {
            "Reported arithmetic division truncates toward zero and the implementation uses BigInt. The source is advisory memory and the reported behavior is not independently verified. This saved interpretation describes the arithmetic implementation and attributes its behavior to the report, while distinguishing reported behavior from independent validation.".to_owned()
        } else {
            "Reported division truncates toward zero; not independently verified.".to_owned()
        };
        let verdict = json!({"answer":"","select":[fid],"need":[],"conflicts":[],"excerpts":[{"id":fid,"quote":excerpt}],"aspects":[{"question":question,"status":"found","answer":interpretation,"self_contained":true,"evidence":[fid]}]});
        scenario(d.path(), vec![selected(vec![fid]), verdict]);
        grounding_reply(
            d.path(),
            json!({"aspects":[{"index":0,"supported":true,"answer":interpretation,"self_contained":true}]}),
        );
        let output = run(d.path(), question);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let wire: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(wire["answer_mode"] == "evidence", choose_evidence, "{wire}");
        if choose_evidence {
            assert!(!wire.to_string().contains("BigInt"));
        }
        let first = compact_wire::expand(wire);
        let delivered = if choose_evidence {
            original.as_str()
        } else {
            excerpt
        };
        assert_eq!(first["evidence"][0]["quote"], delivered);
        if choose_evidence {
            assert_eq!(first["aspects"][0]["question"], question);
            assert!(first["aspects"][0].get("answer").is_none());
        } else {
            assert_eq!(first["aspects"][0]["answer"], interpretation);
        }
        let context = first["context_session"].as_str().unwrap();
        let path = d
            .path()
            .join(format!("memory/runtime/unified/sessions/{context}.json"));
        let before = fs::read(&path).unwrap();
        let state: Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(state["response"]["evidence"][0]["quote"], original);
        assert_eq!(state["response"]["aspects"][0]["answer"], interpretation);
        let receipts = state["delivered_evidence"].as_object().unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts.values().next().unwrap()["quote"], delivered);
        let details = answer(d.path(), &format!("@context:{context} @details"));
        assert_eq!(details["evidence"][0]["quote"], original);
        assert_eq!(details["aspects"][0]["answer"], interpretation);
        assert!(details.get("answer_mode").is_none());
        assert_eq!(fs::read(&path).unwrap(), before);
        let repeated = answer(d.path(), &format!("@context:{context} {question}"));
        assert_eq!(repeated["cache"], "hit");
        assert_eq!(
            repeated["answer_mode"] == "evidence",
            choose_evidence,
            "{repeated}"
        );
        assert!(repeated["evidence"].as_array().unwrap().is_empty());
        assert_eq!(
            repeated["reused_evidence"],
            json!([first["evidence"][0]["ref"]])
        );
        let state: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(
            state["delivered_evidence"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap()["quote"],
            delivered
        );
    }
}

#[test]
fn presentation_requirements_survive_restart_without_becoming_coverage_questions() {
    let d = fixture();
    fs::write(
        d.path().join("memory/docs/ui.md"),
        "Buttons blue.\nAuthor: UI team.",
    )
    .unwrap();
    let questions = json!([
        "What color are buttons?",
        "Who authored the button requirements?"
    ]);
    let presentation = json!(["Return exact source addresses.", "Use concise English."]);
    fs::write(d.path().join("planned-aspects.json"), questions.to_string()).unwrap();
    fs::write(
        d.path().join("planned-presentation.json"),
        presentation.to_string(),
    )
    .unwrap();
    let verdict = json!({"answer":"","select":[fid(1),fid(2)],"need":[],"conflicts":[],"aspects":[
        {"question":"What color are buttons?","status":"found","answer":"Buttons are blue.","evidence":[fid(1)]},
        {"question":"Who authored the button requirements?","status":"found","answer":"The UI team authored the button requirements.","evidence":[fid(2)]}
    ]});
    scenario(
        d.path(),
        vec![selected(vec![fid(1), fid(2)]), verdict.clone()],
    );
    let scenario_path = d.path().join("scenario.json");
    let mut script: Value = serde_json::from_slice(&fs::read(&scenario_path).unwrap()).unwrap();
    script["calls"][1]["save_prompt_to"] = json!(d.path().join("presentation-verifier.txt"));
    fs::write(&scenario_path, script.to_string()).unwrap();
    let query = "What color are buttons, and who authored their requirements? Return exact source addresses in concise English.";
    let result = answer(d.path(), query);
    assert_eq!(result["status"], "complete", "{result}");
    assert_eq!(result["aspects"].as_array().unwrap().len(), 2);
    assert_eq!(result["answer_mode"], "source_context");
    assert!(result["aspects"][1].get("answer").is_none());
    assert!(result["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["quote"] == "Author: UI team."));
    let prompt = fs::read_to_string(d.path().join("presentation-verifier.txt")).unwrap();
    let packet: Value =
        serde_json::from_str(prompt.lines().find(|line| line.starts_with('{')).unwrap()).unwrap();
    assert_eq!(packet["requested_aspects"], questions);
    assert_eq!(packet["presentation_requirements"], presentation);
    let context = result["context_session"].as_str().unwrap();
    let state: Value = serde_json::from_slice(
        &fs::read(
            d.path()
                .join(format!("memory/runtime/unified/sessions/{context}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["requested_aspects"], questions);
    assert_eq!(state["presentation_requirements"], presentation);
    let audit_before = fs::read_to_string(d.path().join("grounding-prompts.txt")).unwrap();
    let repeated = answer(d.path(), &format!("@context:{context} {query}"));
    assert_eq!(repeated["status"], "complete");
    assert_eq!(repeated["cache"], "hit");
    assert_eq!(repeated["aspects"].as_array().unwrap().len(), 2);
    assert_eq!(
        fs::read_to_string(d.path().join("grounding-prompts.txt")).unwrap(),
        audit_before
    );
    // Reused extracted originals still receive the current presentation plan.
    scenario(d.path(), vec![verdict]);
    let mut script: Value = serde_json::from_slice(&fs::read(&scenario_path).unwrap()).unwrap();
    script["calls"][0]["save_prompt_to"] =
        json!(d.path().join("followup-presentation-verifier.txt"));
    fs::write(&scenario_path, script.to_string()).unwrap();
    let followup = answer(d.path(), &format!("@context:{context} Confirm the button color and the requirements' author; cite exact sources concisely."));
    assert_eq!(followup["status"], "complete");
    assert_eq!(followup["aspects"].as_array().unwrap().len(), 2);
    let prompt = fs::read_to_string(d.path().join("followup-presentation-verifier.txt")).unwrap();
    let packet: Value =
        serde_json::from_str(prompt.lines().find(|line| line.starts_with('{')).unwrap()).unwrap();
    assert_eq!(packet["presentation_requirements"], presentation);
    assert_eq!(packet["requested_aspects"], questions);
}

#[test]
fn unused_valid_memory_excerpt_needs_no_retry_and_still_runs_grounding() {
    let d = fixture();
    fs::remove_file(d.path().join("memory/docs/ui.md")).unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fs::create_dir_all(d.path().join("memory/threads/aa")).unwrap();
    fs::create_dir_all(d.path().join("memory/thread-agents")).unwrap();
    fs::write(d.path().join(format!("memory/threads/aa/{id}.md")),format!("---\nformat: climemory-memory-thread/1\nid: {id}\nslug: binary\ntitle: Binary reported rules\n---\n")).unwrap();
    let rule = "Reported division truncates toward zero.";
    let provenance = "Source: reported by primary.";
    fs::write(d.path().join(format!("memory/thread-agents/{id}.json")),json!({"format":"climemory/thread-agent-2","thread_id":id,"agent":"cheap","parent":null,"memory":format!("{rule}\n{provenance}"),"revision":1,"last_dialogue":null,"updated":"2026-09-27T00:00:00Z"}).to_string()).unwrap();
    let fid = format!("memory-{id}:L1");
    let orphan = format!("memory-{id}:L2");
    let a = json!({"answer":"","select":[fid,orphan],"need":[],"conflicts":[],"excerpts":[{"id":fid,"quote":rule},{"id":orphan,"quote":provenance}],"aspects":[{"question":"What division rule was reported?","status":"found","answer":rule,"self_contained":true,"evidence":[fid]}]});
    scenario(
        d.path(),
        vec![selected(vec![fid.clone(), orphan.clone()]), a],
    );
    let first = answer(d.path(), "What division rule was reported?");
    assert_eq!(first["status"], "complete");
    let context = first["context_session"].as_str().unwrap();
    let state: Value = serde_json::from_slice(
        &fs::read(
            d.path()
                .join(format!("memory/runtime/unified/sessions/{context}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["response"]["calls_scheduled"], 4);
    assert_eq!(state["response"]["aspects"][0]["answer"], rule);
    let prompts = fs::read_to_string(d.path().join("grounding-prompts.txt")).unwrap();
    assert!(prompts.contains(rule));
    assert!(!prompts.contains(provenance));
}

#[test]
fn cited_only_grounding_removes_unasked_claim_but_keeps_original_details_and_receipts() {
    let d = fixture();
    fs::remove_file(d.path().join("memory/docs/ui.md")).unwrap();
    let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fs::create_dir_all(d.path().join("memory/threads/aa")).unwrap();
    fs::create_dir_all(d.path().join("memory/thread-agents")).unwrap();
    fs::write(d.path().join(format!("memory/threads/aa/{id}.md")),format!("---\nformat: climemory-memory-thread/1\nid: {id}\nslug: binary\ntitle: Binary reported rules\n---\n")).unwrap();
    let excerpt = "Reported division truncates toward zero.";
    let original = format!("{excerpt} Unasked history remains unchanged.");
    fs::write(d.path().join(format!("memory/thread-agents/{id}.json")),json!({"format":"climemory/thread-agent-2","thread_id":id,"agent":"cheap","parent":null,"memory":format!("Uncited engine uses BigInt.\n{original}"),"revision":1,"last_dialogue":null,"updated":"2026-09-27T00:00:00Z"}).to_string()).unwrap();
    let fid = format!("memory-{id}:L2");
    let a = json!({"answer":"Unsupported aggregate BigInt.","select":[fid],"need":[],"conflicts":[],"excerpts":[{"id":fid,"quote":excerpt}],"aspects":[{"question":"What division rule was reported?","status":"found","answer":"Reported BigInt division truncates toward zero.","self_contained":true,"evidence":[fid]}]});
    scenario(d.path(), vec![selected(vec![fid.clone()]), a]);
    grounding_reply(
        d.path(),
        json!({"aspects":[{"index":0,"supported":true,"answer":excerpt,"self_contained":true}]}),
    );
    let first = answer(d.path(), "What division rule was reported?");
    assert_eq!(first["status"], "complete");
    assert_eq!(first["aspects"][0]["answer"], excerpt);
    assert!(!first.to_string().contains("BigInt"));
    let prompts = fs::read_to_string(d.path().join("grounding-prompts.txt")).unwrap();
    let envelope = prompts
        .split_once('\n')
        .unwrap()
        .1
        .split("\n---")
        .next()
        .unwrap();
    let packet: Value = serde_json::from_str(envelope).unwrap();
    assert_eq!(packet["operation"], "unified_grounding");
    assert_eq!(
        packet["aspects"][0]["evidence"],
        json!([{"id":fid,"quote":excerpt,"authority":"advisory_memory","source":format!("memory/thread-agents/{id}.json")} ])
    );
    assert!(!prompts.contains("Uncited engine"));
    assert!(!prompts.contains("Unasked history"));
    assert!(packet.get("catalog").is_none());
    let context = first["context_session"].as_str().unwrap();
    let detail = answer(d.path(), &format!("@context:{context} @details"));
    assert_eq!(detail["evidence"][0]["quote"], original);
    assert_eq!(detail["aspects"][0]["answer"], excerpt);
    let repeat = answer(
        d.path(),
        &format!("@context:{context} What division rule was reported?"),
    );
    assert_eq!(repeat["aspects"][0]["answer"], excerpt);
    assert_eq!(repeat["reused_evidence"].as_array().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(d.path().join("grounding-prompts.txt")).unwrap(),
        prompts
    );
}

#[test]
fn unsupported_or_failed_grounding_never_delivers_or_caches_complete_draft() {
    for fail in [false, true] {
        let d = fixture();
        let ids = vec![fid(1)];
        let a = json!({"answer":"", "select":ids,"need":[],"conflicts":[],"aspects":[{"question":"Are buttons proven waterproof?","status":"found","answer":"Buttons are proven waterproof.","self_contained":true,"evidence":ids}]});
        scenario(d.path(), vec![selected(vec![fid(1)]), a]);
        grounding_reply(
            d.path(),
            if fail {
                json!({"aspects":[]})
            } else {
                json!({"aspects":[{"index":0,"supported":false,"answer":"The cited source does not establish waterproofing.","self_contained":true}]})
            },
        );
        let first = answer(d.path(), "Are buttons proven waterproof?");
        assert_eq!(first["status"], "partial");
        assert_ne!(first["aspects"][0]["status"], "found");
        assert!(!first.to_string().contains("Buttons are proven waterproof."));
        assert!(!first
            .to_string()
            .contains("The cited source does not establish waterproofing."));
        assert_eq!(first["aspects"][0]["search_state"], "incomplete");
        let context = first["context_session"].as_str().unwrap();
        let state: Value = serde_json::from_slice(
            &fs::read(
                d.path()
                    .join(format!("memory/runtime/unified/sessions/{context}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_ne!(state["response"]["status"], "complete");
        assert!(!state["response"]
            .to_string()
            .contains("Buttons are proven waterproof."));
        assert_eq!(
            fs::read_to_string(d.path().join("grounding-prompts.txt"))
                .unwrap()
                .matches("\n---")
                .count(),
            1
        );
    }
}

#[test]
fn partial_grounding_audits_only_found_indices_and_preserves_missing_coverage() {
    for outcome in ["supported", "unsupported", "malformed"] {
        let d = fixture();
        let draft = json!({"answer":"Unreviewed aggregate waterproof claim.","select":[fid(1)],"need":[],"conflicts":[],"aspects":[
            {"question":"Keyboard support?","status":"missing","answer":"","evidence":[]},
            {"question":"Button color?","status":"found","answer":"Buttons are blue and waterproof.","self_contained":true,"evidence":[fid(1)]}
        ]});
        scenario(d.path(), vec![selected(vec![fid(1)]), draft]);
        grounding_reply(
            d.path(),
            match outcome {
                "supported" => {
                    json!({"aspects":[{"index":1,"supported":true,"answer":"Buttons are blue.","self_contained":true}]})
                }
                "unsupported" => {
                    json!({"aspects":[{"index":1,"supported":false,"answer":"Unsupported draft explanation.","self_contained":true}]})
                }
                // An assessment for a missing row must not replace that row or
                // partially apply the otherwise valid finding for the found row.
                _ => {
                    json!({"aspects":[{"index":1,"supported":true,"answer":"Buttons are blue.","self_contained":true},{"index":0,"supported":true,"answer":"Keyboard supported.","self_contained":true}]})
                }
            },
        );
        let result = answer(d.path(), "Keyboard support and button color?");
        assert_eq!(result["status"], "partial", "{outcome}: {result}");
        assert_eq!(result["aspects"][0]["question"], "Keyboard support?");
        assert_eq!(result["aspects"][0]["status"], "missing");
        assert!(!result.to_string().contains("waterproof"));
        assert!(!result.to_string().contains("Keyboard supported."));
        assert!(!result
            .to_string()
            .contains("Unsupported draft explanation."));
        if outcome == "supported" {
            assert_eq!(result["aspects"][1]["status"], "found");
            assert_eq!(result["aspects"][1]["answer"], "Buttons are blue.");
        } else {
            assert_eq!(result["aspects"][1]["status"], "missing");
            assert_eq!(result["aspects"][1]["search_state"], "incomplete");
        }
        let prompts = fs::read_to_string(d.path().join("grounding-prompts.txt")).unwrap();
        assert_eq!(prompts.matches("\n---").count(), 1, "{outcome}");
        let envelope = prompts
            .split_once('\n')
            .unwrap()
            .1
            .split("\n---")
            .next()
            .unwrap();
        let packet: Value = serde_json::from_str(envelope).unwrap();
        assert_eq!(packet["aspects"].as_array().unwrap().len(), 1);
        assert_eq!(packet["aspects"][0]["index"], 1);
        assert_eq!(packet["aspects"][0]["question"], "Button color?");
        assert_eq!(
            packet["aspects"][0]["evidence"][0]["quote"],
            "Buttons blue."
        );
        assert!(!packet.to_string().contains("Keyboard support?"));
        let context = result["context_session"].as_str().unwrap();
        let state: Value = serde_json::from_slice(
            &fs::read(
                d.path()
                    .join(format!("memory/runtime/unified/sessions/{context}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(state["response"]["status"], "partial");
        assert_eq!(state["response"]["aspects"][0]["status"], "missing");
        assert!(!state["response"].to_string().contains("waterproof"));
        let details = answer(d.path(), &format!("@context:{context} @details"));
        assert_eq!(details["status"], "partial");
        assert_eq!(details["aspects"][0]["status"], "missing");
        assert_eq!(
            fs::read_to_string(d.path().join("grounding-prompts.txt")).unwrap(),
            prompts
        );
        if outcome == "malformed" {
            assert!(diagnostics(d.path(), &result)["errors"]
                .to_string()
                .contains("grounding audit"));
        }
    }
}

#[test]
fn exhausted_budget_never_runs_grounding_or_exposes_complete_draft() {
    let d = fixture();
    let path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(3);
    fs::write(path, config.to_string()).unwrap();
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
    );
    let result = answer(d.path(), "Button color?");
    assert_eq!(result["status"], "partial");
    assert!(
        diagnostics(d.path(), &result)["calls_scheduled"]
            .as_u64()
            .unwrap()
            <= 3
    );
    assert!(!d.path().join("grounding-prompts.txt").exists());
    assert!(!result
        .to_string()
        .contains("Buttons are blue; deletion buttons are red."));
}

#[test]
fn cached_followup_failed_grounding_preserves_partial_without_hidden_audit_retry() {
    let d = fixture();
    scenario(
        d.path(),
        vec![selected(vec![fid(1)]), assembled(vec![fid(1)])],
    );
    let first = answer(d.path(), "Button color?");
    assert_eq!(first["status"], "complete");
    let context = first["context_session"].as_str().unwrap();
    let mut draft = assembled(vec![fid(1)]);
    draft["aspects"][0]["answer"] = json!("Buttons are blue and waterproof.");
    scenario(d.path(), vec![draft]);
    grounding_reply(d.path(), json!({"aspects":[]}));
    let next = answer(d.path(), &format!("@context:{context} What button color?"));
    assert_eq!(next["status"], "partial");
    assert_ne!(next["cache"], "hit");
    assert!(!next
        .to_string()
        .contains("Buttons are blue and waterproof."));
    assert_eq!(diagnostics(d.path(), &next)["calls_scheduled"], 3);
    assert!(diagnostics(d.path(), &next)["errors"]
        .to_string()
        .contains("grounding audit"));
    assert_eq!(
        fs::read_to_string(d.path().join("grounding-prompts.txt"))
            .unwrap()
            .matches("\n---")
            .count(),
        2
    );
}

#[test]
fn parallel_candidates_preserve_verification_and_audit_budget_and_pending_coverage() {
    let d = fixture();
    fs::write(d.path().join("memory/docs/ui.md"), "Blue buttons.").unwrap();
    fs::write(
        d.path().join("memory/docs/keyboard.md"),
        "Keyboard support.",
    )
    .unwrap();
    let path = d.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["max_steps"] = json!(4);
    config["memory"]["unified"]["concurrency"] = json!(2);
    fs::write(path, config.to_string()).unwrap();
    let questions = json!(["Blue buttons?", "Keyboard support?"]);
    fs::write(d.path().join("planned-aspects.json"), questions.to_string()).unwrap();
    let verdict = json!({"answer":"","select":["1"],"need":[],"conflicts":[],"aspects":[{"question":"Blue buttons?","status":"found","answer":"Blue buttons.","evidence":["1"]},{"question":"Keyboard support?","status":"missing","answer":"","evidence":[]}]});
    scenario(d.path(), vec![selected(vec!["1".into()]), verdict]);
    let result = answer(d.path(), "Blue buttons and keyboard support?");
    assert_eq!(result["status"], "partial");
    let internal = diagnostics(d.path(), &result);
    assert!(internal["calls_scheduled"].as_u64().unwrap() <= 4);
    assert_eq!(
        internal["coverage"]["reviewed_threads"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(internal["unprocessed_thread_count"].as_u64().unwrap() > 0);
    assert_eq!(internal["calls_scheduled"], 4);
    assert_eq!(
        fs::read_to_string(d.path().join("grounding-prompts.txt"))
            .unwrap()
            .matches("\n---")
            .count(),
        1
    );
}

#[test]
fn restatement_uses_one_selector_and_preserves_checked_exceptions() {
    let d = fixture();
    let root = d.path();
    let ids = vec![fid(1), fid(2)];
    let mut a = assembled(ids.clone());
    a["aspects"][0]["answer"] = json!("Buttons are blue; deletion buttons are red.");
    scenario(root, vec![selected(ids), a]);
    let first = answer(root, "Button colors and exceptions");
    let id = first["context_session"].as_str().unwrap();
    scenario(root, vec![json!({"covered":true,"indices":[0]})]);
    let output = Command::new(env!("CARGO_BIN_EXE_cm"))
        .current_dir(root)
        .arg(format!(
            "@context:{id} Repeat button colors and exceptions."
        ))
        .env("CM_CODEX_EXE", env!("CARGO_BIN_EXE_cm"))
        .env("CM_FAKE_CODEX_SCENARIO", root.join("scenario.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "complete");
    assert_eq!(result["cache"], "verified_restatement");
    let saved: Value = serde_json::from_slice(
        &fs::read(root.join(format!("memory/runtime/unified/sessions/{id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["response"]["calls_scheduled"], 1);
    assert!(saved["response"]["answer"]
        .as_str()
        .unwrap()
        .contains("deletion buttons are red"));
    let script: Value =
        serde_json::from_slice(&fs::read(root.join("scenario.json")).unwrap()).unwrap();
    let state: Value =
        serde_json::from_slice(&fs::read(script["state_file"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(state["calls_seen"], 1);
}

#[test]
fn restatement_after_document_change_uses_full_retrieval() {
    let d = fixture();
    let root = d.path();
    scenario(root, vec![selected(vec![fid(1)]), assembled(vec![fid(1)])]);
    let first = answer(root, "Button colors and exceptions");
    let id = first["context_session"].as_str().unwrap();
    fs::write(
        root.join("memory/docs/ui.md"),
        "Buttons blue.\nDeletion buttons red.\nNew condition: disabled buttons grey.",
    )
    .unwrap();
    scenario(
        root,
        vec![
            selected(vec![fid(1), fid(3)]),
            assembled(vec![fid(1), fid(3)]),
        ],
    );
    let result = answer(root, &format!("@context:{id} Repeat button colors"));
    assert_ne!(result["cache"], "verified_restatement");
    assert!(
        diagnostics(root, &result)["calls_scheduled"]
            .as_u64()
            .unwrap()
            > 1
    );
}

#[test]
fn native_mcp_returns_saved_evidence_in_one_tool_reply() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    let d = fixture();
    scenario(
        d.path(),
        vec![
            selected(vec![fid(1), fid(2)]),
            assembled(vec![fid(1), fid(2)]),
        ],
    );
    let first = answer(d.path(), "Button colors and exceptions");
    let context = first["context_session"].as_str().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cm"))
        .arg("--mcp")
        .current_dir(d.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ask","arguments":{"question":format!("@context:{context} @details")}}})).unwrap();
    input.flush().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let result: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["result"]["isError"], false);
    let evidence: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(evidence["context_session"], context);
    assert!(evidence.to_string().contains("red"));
    drop(input);
    assert!(child.wait().unwrap().success());
}
