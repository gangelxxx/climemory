use super::*;

fn configure(root: &Path, endpoint: &str) {
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent"]["classifier"] = json!({"enabled":true,"source_blocks":false,"endpoint":endpoint,"api_key":"fixture-secret","timeout_ms":1000});
    fs::write(path, config.to_string()).unwrap();
}

fn decisions(input: &Value, relevant: impl Fn(&Value) -> bool) -> Value {
    assert_eq!(input["model"], "typesafe/jev-1.13");
    assert!(!input.to_string().contains("fixture-secret"));
    let candidates: Vec<Value> = if let Some(rows) = input["state"]["candidates"].as_array() {
        rows.iter().map(|v| v["candidate"].clone()).collect()
    } else {
        (0..input["questions"].as_object().unwrap().len())
            .map(|i| {
                if input["state"].get(format!("thread_{i}")).is_some() {
                    assert!(input["questions"][format!("candidate_{i}")]["instructions"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("state.thread_{i}")));
                    return input["state"][format!("thread_{i}")].clone();
                }
                assert!(input["questions"][format!("candidate_{i}")]["instructions"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("state.chunk_{i}")));
                input["state"][format!("chunk_{i}")].clone()
            })
            .collect()
    };
    let answers: serde_json::Map<String,Value> = candidates.iter().enumerate().map(|(i,c)| {
        let keep = relevant(c);
        (format!("candidate_{i}"),json!({"type":"choice","choice":if keep {"relevant"} else {"irrelevant"},
            "confidence":1.0,"probabilities":{"relevant":if keep {1.0} else {0.0},"uncertain":0.0,"irrelevant":if keep {0.0} else {1.0}}}))
    }).collect();
    json!({"answers":answers})
}

#[test]
fn original_blocks_replace_codex_selection_and_preserve_full_verification() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let server = Server::start(move |input| {
        if input.get("questions").is_some() {
            seen.fetch_add(1, Ordering::SeqCst);
            assert!(input["questions"].as_object().unwrap().len() <= 20);
            assert!(input["state"]["chunk_0"]["text"].is_string());
            return decisions(&input, |c| c["text"].as_str().unwrap().contains("Save"));
        }
        assert_ne!(
            input["phase"], "document_selection",
            "successful Jev replaces selector"
        );
        if input["phase"] == "document_index" {
            return json!({"action":"context","memory":null,"text":"Deliberately unhelpful index; source classification must read originals."});
        }
        if input["phase"] == "document_review" {
            let docs = input["user_documents"].as_array().unwrap();
            assert_eq!(docs.len(), 1);
            assert!(docs[0]["text"].as_str().unwrap().len() < 2000);
            assert_eq!(docs[0]["path"], "memory/docs/b-ui.md");
            return json!({"action":"context","text":"","memory":{"rules":[{"rule":"Save green.","when":"","sources":[{"path":"memory/docs/b-ui.md","start_line":1,"end_line":1}]}],"issues":[]}});
        }
        if input["phase"] == "document_verification" {
            let catalog = input["source_catalog"].as_array().unwrap();
            assert!(catalog.iter().all(|r| r["path"] == "memory/docs/b-ui.md"));
            assert!(catalog
                .iter()
                .any(|r| r["text"].as_str().unwrap().contains("Archive")));
            let source = catalog
                .iter()
                .find(|r| r["text"].as_str().unwrap().contains("Save"))
                .unwrap();
            return json!({"action":"context","text":"","memory":{"rules":[{"rule":"Save green.","when":"","source_ids":[source["id"]],"candidate_ids":[1]}],"issues":[]}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","text":"Owner context."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    configure(root, &server.endpoint);
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent"]["classifier"]["source_blocks"] = json!(true);
    config["agent"]["classifier"]["max_candidates"] = json!(2);
    config["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(path, config.to_string()).unwrap();
    fs::write(
        root.join("memory/docs/a-archive.md"),
        "Archive.\n".repeat(400),
    )
    .unwrap();
    let source = format!("Save green.\n\n{}", "Archive.\n".repeat(1000));
    fs::write(root.join("memory/docs/b-ui.md"), &source).unwrap();
    let result = agent(root, &["ask", "settings", "Save color"]);
    assert_eq!(result["status"], "awaiting_report", "{result}");
    assert!(result["metrics"]["calls_by_phase"]["document_selection"].is_null());
    assert_eq!(
        result["document_requirements"]["verification"]["unverified_rules"],
        0
    );
    assert_eq!(
        fs::read_to_string(root.join("memory/docs/b-ui.md")).unwrap(),
        source
    );
    agent(root, &["cancel", result["session"].as_str().unwrap()]);
    let first_calls = calls.load(Ordering::SeqCst);
    assert!(
        first_calls > 1,
        "multiple batches including all-negative batches"
    );
    // Lose derived packets/selections, but retain validated block decisions.
    // A resumed selection must reuse them without pretending these are new API calls.
    for entry in fs::read_dir(root.join("memory/runtime/documents")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file()
            && !entry.file_name().to_string_lossy().starts_with("blocks-")
        {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    let again = agent(root, &["ask", "settings", "Save color"]);
    assert_eq!(again["status"], "awaiting_report", "{again}");
    assert_eq!(calls.load(Ordering::SeqCst), first_calls);
    agent(root, &["cancel", again["session"].as_str().unwrap()]);
    fs::write(
        root.join("memory/docs/a-archive.md"),
        "Archive changed.\n".repeat(400),
    )
    .unwrap();
    let changed = agent(root, &["ask", "settings", "Save color"]);
    assert_eq!(changed["status"], "awaiting_report", "{changed}");
    assert!(
        calls.load(Ordering::SeqCst) > first_calls,
        "source changes invalidate decisions"
    );
    agent(root, &["cancel", changed["session"].as_str().unwrap()]);
}

#[test]
fn classifier_route_filters_local_candidates_without_scanning_unrelated_threads() {
    let server = Server::start(|input| {
        assert_eq!(input["questions"].as_object().unwrap().len(), 2);
        for i in 0..2 {
            assert!(input["state"][format!("thread_{i}")]["slug"]
                .as_str()
                .unwrap()
                .starts_with("settings"));
        }
        decisions(&input, |c| c["slug"] == "settings")
    });
    let temp = fixture(&server, "ollama");
    configure(temp.path(), &server.endpoint);
    let local = records(temp.path(), &["context", "preferences"]);
    assert_eq!(local[0]["status"], "empty");
    assert_eq!(server.calls.load(Ordering::Relaxed), 0);
    let routed = records(temp.path(), &["route", "preferences"]);
    assert_eq!(routed[0]["record"], "route_summary");
    assert_eq!(routed[0]["classifier"]["status"], "empty");
    assert_eq!(server.calls.load(Ordering::Relaxed), 0);
    common::run(temp.path(), &["create", "settings archive"], "").success();
    let routed = records(temp.path(), &["route", "Change settings"]);
    assert_eq!(routed[0]["classifier"]["status"], "selected");
    assert_eq!(routed[0]["classifier"]["excluded"], 1);
    assert_eq!(routed[1]["thread"], "settings");
    assert_eq!(routed[1]["ask_argv"][2], "Change settings");
    assert_eq!(routed.len(), 2);
    assert_eq!(server.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn classifier_route_keeps_explicit_identity_first_with_more_than_eight_matches() {
    let server = Server::start(|input| decisions(&input, |c| c["slug"] != "settings"));
    let temp = fixture(&server, "ollama");
    configure(temp.path(), &server.endpoint);
    for i in 0..9 {
        common::run(
            temp.path(),
            &["create", &format!("settings-candidate-{i}")],
            "",
        )
        .success();
    }
    let routed = records(temp.path(), &["route", "settings"]);
    assert_eq!(routed[0]["classifier"]["status"], "selected");
    assert_eq!(routed[1]["thread"], "settings");
    assert_eq!(routed[0]["omitted"], 2);
    let id = routed[1]["read_handle"]
        .as_str()
        .unwrap()
        .strip_prefix("memory:")
        .unwrap();
    let by_id = records(temp.path(), &["route", id]);
    assert_eq!(by_id[1]["thread"], "settings");
}

#[test]
fn routed_consultation_only_calls_selected_owner_and_still_reads_shared_docs() {
    let owners = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = owners.clone();
    let server = Server::start(move |input| {
        if input.get("questions").is_some() {
            return decisions(&input, |c| c["slug"] == "settings-general");
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        seen.lock()
            .unwrap()
            .push(input["thread"]["slug"].as_str().unwrap().to_owned());
        assert!(input["document_requirements"]["text"]
            .as_str()
            .unwrap()
            .contains("green"));
        json!({"action":"context","text":"General settings save locally."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    configure(root, &server.endpoint);
    common::run(
        root,
        &["create", "settings general", "--parent", "application"],
        "",
    )
    .success();
    for name in ["settings archive", "settings project"] {
        common::run(root, &["create", name], "").success();
    }
    fs::write(root.join("memory/docs/ui.md"), "Save buttons use green.").unwrap();
    let query = "Add Save to general settings";
    let local = records(root, &["context", query]);
    assert_eq!(local.iter().filter(|r| r["record"] == "memory").count(), 5);
    let routed = records(root, &["route", query]);
    assert_eq!(routed[0]["classifier"]["candidates"], 5);
    assert_eq!(routed[0]["classifier"]["selected"], 1);
    assert_eq!(routed[0]["classifier"]["excluded"], 4);
    assert_eq!(routed[1]["thread"], "settings-general");
    assert_eq!(routed.len(), 2, "excluded parent must not be added back");
    let argv: Vec<_> = routed[1]["ask_argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(argv[3], "--dir");
    // agent() supplies the fixture directory itself.
    let answer = agent(root, &argv[..3]);
    assert_eq!(answer["status"], "awaiting_report", "{answer}");
    assert_eq!(*owners.lock().unwrap(), vec!["settings-general"]);
    agent(root, &["cancel", answer["session"].as_str().unwrap()]);
}

#[test]
fn classifier_document_self_citations_do_not_expand_to_the_whole_file() {
    let read_chunks = Arc::new(AtomicUsize::new(0));
    let read_count = read_chunks.clone();
    let server = Server::start(move |input| {
        if input.get("questions").is_some() {
            return decisions(&input, |c| c["id"] == 3);
        }
        if input["phase"] == "document_selection" {
            assert_eq!(
                input["classifier_hint"]["selected_chunks"],
                json!([2, 3, 4])
            );
            assert_eq!(
                input["classifier_hint"]["expansion"]["raw_selected_chunks"],
                json!([3])
            );
            assert_eq!(
                input["classifier_hint"]["expansion"]["neighbor_added_chunks"],
                json!([2, 4])
            );
            assert_eq!(
                input["classifier_hint"]["expansion"]["reference_added_chunks"],
                json!([])
            );
            return json!({"action":"context","text":"","memory":{"selected_chunks":[2,3,4],"reuse_previous":false,"reason":"Keep uncertain neighboring boundaries"}});
        }
        if input["phase"] == "document_index" {
            let doc = &input["user_documents"][0];
            return json!({"action":"context","memory":null,"text":format!("Save requirements. {}:{}",doc["path"].as_str().unwrap(),doc["start_line"])});
        }
        if input["phase"] == "document_review" {
            read_count.fetch_add(
                input["user_documents"].as_array().unwrap().len(),
                Ordering::Relaxed,
            );
            let doc = &input["user_documents"][0];
            return json!({"action":"context","text":"","memory":{"rules":[{"rule":"Save uses green.","when":"","sources":[{"path":doc["path"],"start_line":doc["start_line"],"end_line":doc["start_line"]}]}],"issues":[]}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        assert_ne!(input["phase"], "document_selection");
        json!({"action":"context","memory":null,"text":"Owner context."})
    });
    let temp = fixture(&server, "ollama");
    configure(temp.path(), &server.endpoint);
    fs::write(
        temp.path().join("memory/docs/large.md"),
        "Save uses green.\n".repeat(9000),
    )
    .unwrap();
    let answer = agent(temp.path(), &["ask", "settings", "Add Save"]);
    assert_eq!(answer["status"], "awaiting_report", "{answer}");
    assert_eq!(read_chunks.load(Ordering::Relaxed), 3);
}

#[test]
fn classifier_expensive_expansion_is_refined_and_full_originals_are_verified() {
    let server = Server::start(|input| {
        if input.get("questions").is_some() {
            return decisions(&input, |c| c["id"] == 1);
        }
        if input["phase"] == "document_index" {
            let doc = &input["user_documents"][0];
            let note = if doc["path"] == "memory/docs/a-ui.md" {
                "Save green line 1; white text line 2. Further reading memory/docs/c-archive.md."
            } else {
                "Save context at line 1."
            };
            return json!({"action":"context","memory":null,"text":note});
        }
        if input["phase"] == "document_selection" {
            assert_eq!(input["indexes"].as_array().unwrap().len(), 3);
            assert_eq!(input["classifier_hint"]["selected_chunks"], json!([1, 3]));
            assert_eq!(
                input["classifier_hint"]["expansion"]["reference_added_chunks"],
                json!([3])
            );
            // Restore a dependency excluded by Jev; discard an unrelated reference.
            return json!({"action":"context","text":"","memory":{"selected_chunks":[1,2],"reuse_previous":false,"sections":[{"chunk":1,"start_line":1,"end_line":1}]}});
        }
        if input["phase"] == "document_review" {
            let docs = input["user_documents"].as_array().unwrap();
            assert_eq!(docs.len(), 2);
            assert_eq!(docs[0]["text"], "Save uses green.\n");
            assert_eq!(docs[1]["path"], "memory/docs/b-exception.md");
            return json!({"action":"context","text":"","memory":{"rules":[
                {"rule":"Save uses green.","when":"","sources":[{"path":"memory/docs/a-ui.md","start_line":1,"end_line":1}]},
                {"rule":"Save disabled.","when":"read-only","sources":[{"path":"memory/docs/b-exception.md","start_line":1,"end_line":1}]}
            ],"issues":[]}});
        }
        if input["phase"] == "document_verification" {
            let catalog = input["source_catalog"].as_array().unwrap();
            assert!(catalog
                .iter()
                .all(|r| r["path"] != "memory/docs/c-archive.md"));
            let rules: Vec<_> = catalog.iter().filter_map(|r| {
                let text = r["text"].as_str().unwrap();
                if r["path"] == "memory/docs/b-exception.md" {
                    Some(json!({"rule":"Save disabled.","when":"read-only","source_ids":[r["id"]],"candidate_ids":[2]}))
                } else if text.contains("Save uses green.") {
                    assert!(text.contains("White text."), "verifier recovers a rule outside the selected range");
                    Some(json!({"rule":"Save uses green. White text.","when":"","source_ids":[r["id"]],"candidate_ids":[1]}))
                } else { None }
            }).collect();
            return json!({"action":"context","text":"","memory":{"rules":rules,"issues":[]}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","text":"Owner context."})
    });
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    configure(root, &server.endpoint);
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_medium");
    fs::write(path, config.to_string()).unwrap();
    let ui = format!(
        "Save uses green.\nWhite text.\n{}",
        "Archive.\n".repeat(2000)
    );
    fs::write(root.join("memory/docs/a-ui.md"), &ui).unwrap();
    fs::write(
        root.join("memory/docs/b-exception.md"),
        "Save disabled when read-only.\n",
    )
    .unwrap();
    fs::write(
        root.join("memory/docs/c-archive.md"),
        "Archive.\n".repeat(2000),
    )
    .unwrap();
    let answer = agent(root, &["ask", "settings", "Save appearance and exceptions"]);
    assert_eq!(answer["status"], "awaiting_report", "{answer}");
    assert_eq!(answer["metrics"]["calls_by_phase"]["document_selection"], 1);
    assert_eq!(answer["metrics"]["calls_by_phase"]["document_review"], 1);
    assert_eq!(
        answer["document_requirements"]["verification"]["unverified_rules"],
        0
    );
    let saved: Value = serde_json::from_slice(
        &fs::read(root.join(format!(
            "memory/agent-runs/thread-dialogues/{}.json",
            answer["session"].as_str().unwrap()
        )))
        .unwrap(),
    )
    .unwrap();
    let events = saved["events"].as_array().unwrap();
    let selected = events
        .iter()
        .find(|e| e["event"] == "classifier_selection")
        .unwrap();
    assert_eq!(selected["decision"], "refine_with_document_agent");
    assert_eq!(selected["cost"]["read_batches"], 2);
    assert_eq!(selected["cost"]["source_bytes"], ui.len() + 18000);
    let review = events
        .iter()
        .find(|e| e["event"] == "model_call" && e["phase"] == "document_review")
        .unwrap();
    assert_eq!(
        review["source_bytes"],
        "Save uses green.\nSave disabled when read-only.\n".len()
    );
    let verification_bytes: u64 = events
        .iter()
        .filter(|e| e["event"] == "model_call" && e["phase"] == "document_verification")
        .map(|e| e["source_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(
        verification_bytes,
        (ui.len() + "Save disabled when read-only.\n".len()) as u64
    );
    assert_eq!(
        fs::read_to_string(root.join("memory/docs/a-ui.md")).unwrap(),
        ui
    );
    agent(root, &["cancel", answer["session"].as_str().unwrap()]);
}

#[test]
fn classifier_route_falls_back_on_malformed_response_and_respects_limits() {
    let server = Server::start(|_| json!({"answers":{}}));
    let temp = fixture(&server, "ollama");
    configure(temp.path(), &server.endpoint);
    common::run(temp.path(), &["create", "settings archive"], "").success();
    let routed = records(temp.path(), &["route", "settings"]);
    assert_eq!(routed[0]["classifier"]["status"], "fallback");
    assert_eq!(routed[1]["thread"], "settings");
    let path = temp.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent"]["classifier"]["max_candidates"] = json!(1);
    fs::write(path, config.to_string()).unwrap();
    let routed = records(temp.path(), &["route", "settings"]);
    assert_eq!(
        routed[0]["classifier"]["reason"],
        "classifier candidate limit"
    );
    assert_eq!(server.calls.load(Ordering::Relaxed), 1);
}

#[test]
fn route_disabled_empty_and_single_candidate_avoid_classifier_calls() {
    let server = Server::start(|_| panic!("local routing must not call a model"));
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    configure(root, &server.endpoint);
    assert_eq!(
        records(root, &["route", "unmatchedword"])[0]["classifier"]["status"],
        "empty"
    );
    assert_eq!(
        records(root, &["route", "settings"])[0]["classifier"]["status"],
        "local"
    );
    common::run(root, &["create", "settings archive"], "").success();
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent"]["classifier"]["enabled"] = json!(false);
    fs::write(path, config.to_string()).unwrap();
    let local = records(root, &["context", "settings"]);
    let route = records(root, &["route", "settings"]);
    assert_eq!(route[0]["classifier"]["status"], "fallback");
    let names = |rows: &[Value]| {
        rows.iter()
            .filter(|r| r["record"] == "memory")
            .map(|r| r["thread"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&local), names(&route));
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn classifier_document_selection_keeps_owner_and_dependencies_and_reuses_cache() {
    let classified = Arc::new(AtomicUsize::new(0));
    let calls = classified.clone();
    let server = Server::start(move |input| {
        if input.get("questions").is_some() {
            calls.fetch_add(1, Ordering::Relaxed);
            return decisions(&input, |c| c["path"] == "memory/docs/ui.md");
        }
        assert_ne!(
            input["phase"], "document_selection",
            "Jev should handle fresh selection"
        );
        assert!(!input.to_string().contains("fixture-secret"));
        if input["phase"] == "document_review" {
            assert!(input["user_documents"]
                .as_array()
                .unwrap()
                .iter()
                .all(|d| d["path"] != "memory/docs/typography.md"));
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","memory":null,"text":"Owner consulted the selected documents."})
    });
    let temp = fixture(&server, "ollama");
    configure(temp.path(), &server.endpoint);
    for (name, text) in [
        ("ui.md", "Save buttons use memory/docs/palette.md."),
        ("palette.md", "Save uses green."),
        ("typography.md", "Paragraph spacing is 1.5."),
    ] {
        fs::write(temp.path().join("memory/docs").join(name), text).unwrap();
    }
    let answer = agent(temp.path(), &["ask", "settings", "Add Save"]);
    assert_eq!(answer["status"], "awaiting_report");
    let rendered = answer.to_string();
    assert!(rendered.contains("green"));
    assert!(!rendered.contains("fixture-secret"));
    assert_eq!(classified.load(Ordering::Relaxed), 1);
    let next = agent(temp.path(), &["ask", "application", "Add Save"]);
    assert_eq!(next["status"], "awaiting_report");
    assert_eq!(classified.load(Ordering::Relaxed), 1);
}

#[test]
fn classifier_document_failure_uses_existing_document_agent() {
    check_document_failure(false);
    check_document_failure(true);
}

fn check_document_failure(source_blocks: bool) {
    let fallback = Arc::new(AtomicUsize::new(0));
    let fallback_count = fallback.clone();
    let server = Server::start(move |input| {
        if input.get("questions").is_some() {
            return json!({"error":"fixture-secret-must-not-escape"});
        }
        if input["phase"] == "document_selection" {
            fallback_count.fetch_add(1, Ordering::Relaxed);
            return json!({"action":"context","text":"","memory":{"selected_chunks":[1,2],"reuse_previous":false}});
        }
        if let Some(answer) = document_answer(&input) {
            return answer;
        }
        json!({"action":"context","memory":null,"text":"Owner answered using fallback."})
    });
    let temp = fixture(&server, "ollama");
    configure(temp.path(), &server.endpoint);
    let path = temp.path().join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["agent"]["classifier"]["source_blocks"] = json!(source_blocks);
    fs::write(path, config.to_string()).unwrap();
    for name in ["first.md", "second.md"] {
        fs::write(
            temp.path().join("memory/docs").join(name),
            "Save buttons use green.",
        )
        .unwrap();
    }
    let answer = agent(temp.path(), &["ask", "settings", "Add Save"]);
    assert_eq!(answer["status"], "awaiting_report");
    assert_eq!(fallback.load(Ordering::Relaxed), 1);
    assert!(!answer.to_string().contains("fixture-secret"));
}

#[test]
fn classifier_verified_rules_replace_selection_and_fall_back_when_unavailable() {
    for mode in [
        "success",
        "disabled",
        "missing",
        "invalid",
        "uncertain",
        "low_confidence",
        "scope_declined",
        "scope_mismatch",
        "credentials",
        "empty",
        "timeout",
        "unavailable",
        "http",
        "limit",
    ] {
        check_classifier_verified_rules(mode);
    }
}

fn check_classifier_verified_rules(mode: &'static str) {
    let classified = Arc::new(AtomicUsize::new(0));
    let seen = classified.clone();
    let classifier = Server::start(move |input| {
        seen.fetch_add(1, Ordering::SeqCst);
        assert_eq!(input["model"], "replacement-model");
        assert!(input["state"]["rule_0"].is_object());
        assert!(input["state"]["request"].is_object());
        assert!(input["questions"]["candidate_1"]["instructions"]
            .as_str()
            .unwrap()
            .contains("state.rule_1"));
        if mode == "invalid" {
            return json!({"answers":{},"usage":{"input_tokens":300,"output_tokens":25,"cost":0.002}});
        }
        if mode == "timeout" {
            return json!({"test_delay_ms":250});
        }
        if mode == "http" {
            return json!({"test_http_status":503});
        }
        let mut answers = serde_json::Map::new();
        for i in 0..input["questions"].as_object().unwrap().len() {
            let keep = mode != "empty" && i == 1;
            answers.insert(format!("candidate_{i}"),json!({"type":"choice","choice":if keep {"relevant"}else{"irrelevant"},"confidence":1.0,
                "probabilities":{"relevant":if keep {1.0}else{0.0},"irrelevant":if keep {0.0}else{1.0},"uncertain":0.0}}));
        }
        let mut answer =
            json!({"answers":answers,"usage":{"input_tokens":300,"output_tokens":25,"cost":0.002}});
        if mode == "uncertain" {
            answer["answers"]["candidate_1"] = json!({"type":"choice","choice":"uncertain","confidence":1.0,"probabilities":{"relevant":0.0,"uncertain":1.0,"irrelevant":0.0}});
        }
        if mode == "low_confidence" {
            answer["answers"]["candidate_1"] = json!({"type":"choice","choice":"irrelevant","confidence":0.5,"probabilities":{"relevant":0.0,"uncertain":0.0,"irrelevant":1.0}});
        }
        answer
    });
    let fallback = Arc::new(AtomicUsize::new(0));
    let fallback_calls = fallback.clone();
    let server = Server::start_with_scope(
        move |input| {
            if input["phase"] == "document_selection" {
                fallback_calls.fetch_add(1, Ordering::SeqCst);
                return json!({"action":"context","text":"","memory":{"selected_chunks":[],"reuse_previous":false,"rule_ids":[2],"reason":"Typography subset","issue_links":[]}});
            }
            if input["phase"] == "document_issue_scope" {
                return json!({"action":"context","text":"","memory":{"issue_links":[{"id":1,"rule_ids":[2,3]},{"id":2,"rule_ids":[]}]}});
            }
            if let Some(answer) = document_answer(&input) {
                return answer;
            }
            if input["phase"] == "document_verification" {
                return json!({"action":"context","text":"","memory":{"rules":[
                {"rule":"Save is green","when":"enabled","source_ids":["s1"],"candidate_ids":[1]},
                {"rule":"Use system sans","when":"","source_ids":["s2"],"candidate_ids":[1]},
                {"rule":"Text at least 16px","when":"","source_ids":["s3"],"candidate_ids":[1]}],
                "issues":["Typography dependency unresolved", "Unknown issue"],"excluded_candidate_ids":[]}});
            }
            json!({"action":"context","text":"Context from selected rules."})
        },
        move |input| {
            let p = &input["verified_scope_candidate"];
            let memory = if p.is_object() && mode != "scope_declined" {
                assert_eq!(
                    p["structured_requirements"]["rules"]
                        .as_array()
                        .unwrap()
                        .len(),
                    3
                );
                json!({"packet_id":if mode == "scope_mismatch" {json!("wrong-packet")}else{p["packet_id"].clone()}})
            } else {
                Value::Null
            };
            json!({"action":"documents","text":input["request"],"memory":memory})
        },
    );
    let temp = fixture(&server, "ollama");
    let root = temp.path();
    fs::write(
        root.join("memory/docs/ui.md"),
        "Enabled Save green.\nSystem sans.\nText at least 16px.\n",
    )
    .unwrap();
    configure(root, &classifier.endpoint);
    let path = root.join("memory/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["memory"]["verification_agent"] = json!("agent_medium");
    config["agent"]["classifier"]["model"] = json!("replacement-model");
    config["agent"]["classifier"]["timeout_ms"] = json!(if mode == "timeout" { 100 } else { 1000 });
    match mode {
        "disabled" => config["agent"]["classifier"]["enabled"] = json!(false),
        "missing" => config["agent"]["classifier"] = Value::Null,
        "limit" => config["agent"]["classifier"]["max_candidates"] = json!(1),
        "credentials" => config["agent"]["classifier"]["api_key"] = json!(""),
        "unavailable" => {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            config["agent"]["classifier"]["endpoint"] =
                json!(format!("http://{}", listener.local_addr().unwrap()));
            drop(listener);
        }
        _ => {}
    }
    fs::write(&path, config.to_string()).unwrap();
    let broad = agent(root, &["ask", "settings", "All UI rules"]);
    assert_eq!(broad["status"], "awaiting_report", "{mode}: {broad}");
    assert_eq!(classified.load(Ordering::SeqCst), 0);
    let narrow = agent(root, &["ask", "settings", "Typography only"]);
    assert_eq!(narrow["status"], "awaiting_report", "{mode}: {narrow}");
    assert_eq!(
        fallback.load(Ordering::SeqCst),
        usize::from(!matches!(mode, "success" | "uncertain" | "low_confidence")),
        "{mode}"
    );
    assert_eq!(
        narrow["document_requirements"]["reuse"]["rule_ids"],
        json!([2, 3]),
        "{mode}: {narrow}"
    );
    assert_eq!(
        narrow["document_requirements"]["unresolved_issues"],
        broad["document_requirements"]["unresolved_issues"]
    );
    let details = records(root, &["read", narrow["session"].as_str().unwrap()]);
    let original = records(root, &["read", broad["session"].as_str().unwrap()]);
    assert_eq!(
        details[0]["document_requirements"]["structured_requirements"]["rules"],
        json!([
            original[0]["document_requirements"]["structured_requirements"]["rules"][1],
            original[0]["document_requirements"]["structured_requirements"]["rules"][2]
        ])
    );
    assert!(narrow["metrics"]["calls_by_phase"]["document_review"].is_null());
    assert!(narrow["metrics"]["calls_by_phase"]["document_verification"].is_null());
    if matches!(
        mode,
        "success" | "invalid" | "empty" | "uncertain" | "low_confidence"
    ) {
        let groups = narrow["metrics"]["provider_usage"].as_array().unwrap();
        let usage = groups.iter().find(|g| g["role"] == "classifier").unwrap();
        assert_eq!(usage["attempts"], 1);
        assert_eq!(usage["counters"]["input_tokens"]["reported"], 300);
        assert_eq!(usage["counters"]["cost_usd"]["reported"], 0.002);
        assert_eq!(usage["counters"]["cached_input_tokens"]["missing_calls"], 1);
    }
    let before = classified.load(Ordering::SeqCst);
    let again = agent(root, &["ask", "application", "Typography only"]);
    assert_eq!(again["status"], "awaiting_report");
    assert_eq!(
        classified.load(Ordering::SeqCst),
        before,
        "cached result: {mode}"
    );
    if matches!(
        mode,
        "disabled"
            | "missing"
            | "limit"
            | "unavailable"
            | "credentials"
            | "scope_declined"
            | "scope_mismatch"
    ) {
        assert_eq!(before, 0);
    } else {
        assert_eq!(before, 1, "{mode}");
    }
    if mode == "success" {
        // Turning off a warmed classifier must not use its cached decision or call it.
        config["agent"]["classifier"]["enabled"] = json!(false);
        fs::write(&path, config.to_string()).unwrap();
        let off = agent(root, &["ask", "settings", "Typography only"]);
        assert_eq!(off["status"], "awaiting_report");
        assert_eq!(classified.load(Ordering::SeqCst), before);
        assert_eq!(off["metrics"]["calls_by_phase"]["document_verification"], 1);
        agent(root, &["cancel", off["session"].as_str().unwrap()]);
        config["agent"]["classifier"]["enabled"] = json!(true);
        fs::write(&path, config.to_string()).unwrap();
        fs::write(
            root.join("memory/docs/ui.md"),
            "Enabled Save green.\nSystem sans.\nText at least 16px.\nNew source revision.\n",
        )
        .unwrap();
        let changed = agent(root, &["ask", "settings", "Typography only"]);
        assert_eq!(changed["status"], "awaiting_report");
        assert_eq!(
            changed["metrics"]["calls_by_phase"]["document_verification"],
            1
        );
        assert_eq!(classified.load(Ordering::SeqCst), before);
        agent(root, &["cancel", changed["session"].as_str().unwrap()]);
    }
    for row in [&broad, &narrow, &again] {
        agent(root, &["cancel", row["session"].as_str().unwrap()]);
    }
}
