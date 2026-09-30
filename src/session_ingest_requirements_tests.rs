use super::*;

#[test]
fn no_topic_updates_cannot_persist_procedural_summary() {
    for summary in ["", "Ignored temporary directions: do not run commands."] {
        let (mut state, evidence, fresh) = setup();
        let before = serde_json::to_value(&state).unwrap();
        apply(
            &mut state,
            Patch {
                summary: summary.into(),
                updates: vec![],
            },
            &evidence,
            &fresh,
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&state).unwrap(), before);
        let mut empty = State::new("empty-session");
        apply(
            &mut empty,
            Patch {
                summary: summary.into(),
                updates: vec![],
            },
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(empty.summary.is_empty());
        assert!(empty.topics.is_empty());
    }
    assert!(schema(&BTreeSet::new())["properties"]["summary"]
        .get("minLength")
        .is_none());
}

fn event(id: &str, kind: &str) -> Event {
    Event {
        id: id.into(),
        timestamp: "now".into(),
        kind: kind.into(),
        text: id.into(),
        file: "test.jsonl".into(),
        line: 1,
    }
}
fn claim(id: &str, text: &str) -> Claim {
    serde_json::from_value(json!({"id":id,"kind":"requirement","status":"requested","text":text,"sources":["original"]})).unwrap()
}
fn setup() -> (State, BTreeMap<String, Event>, BTreeSet<String>) {
    let supplied = BTreeMap::from([
        ("original".into(), event("original", "user")),
        ("correction".into(), event("correction", "user")),
        ("report".into(), event("report", "assistant")),
    ]);
    let mut state = State::new("test");
    state.summary = "Requested rules".into();
    state
        .evidence
        .insert("original".into(), supplied["original"].clone());
    state.topics.insert(
        "settings".into(),
        Topic {
            key: "settings".into(),
            title: "Settings".into(),
            memory: "Rules".into(),
            claims: vec![
                claim("save", "Theme persists only after Save"),
                claim("reload", "Reload retains operands"),
            ],
            related: vec![],
        },
    );
    (state, supplied, BTreeSet::from(["correction".into()]))
}
fn patch(state: &State) -> Patch {
    Patch {
        summary: "Updated rules".into(),
        updates: state.topics.values().cloned().collect(),
    }
}

#[test]
fn missing_or_disguised_requirements_are_rejected_atomically() {
    for variant in 0..7 {
        let (mut state, supplied, fresh) = setup();
        let before = serde_json::to_value(&state).unwrap();
        let mut update = patch(&state);
        let claims = &mut update.updates[0].claims;
        match variant {
            0 => {
                claims.remove(0);
            }
            1 => claims[0].id = "different".into(),
            2 => claims[0].kind = "decision".into(),
            3 => claims[0].text = "Theme persists immediately".into(),
            4 => {
                claims[0].text = "Changed".into();
                claims[0].change_reason = "Because".into();
            }
            5 => {
                claims[0].text = "Changed".into();
                claims[0].change_reason = "Because".into();
                claims[0].sources = vec!["report".into()];
            }
            _ => claims[0].sources = vec!["correction".into()],
        }
        assert!(
            apply(&mut state, update, &supplied, &fresh).is_err(),
            "variant {variant}"
        );
        assert_eq!(serde_json::to_value(&state).unwrap(), before);
    }
}

#[test]
fn retention_revision_and_partial_cancellation_keep_identity_and_other_rules() {
    let (mut state, supplied, fresh) = setup();
    let unchanged = serde_json::to_value(&state.topics["settings"].claims[0]).unwrap();
    let update = patch(&state);
    apply(&mut state, update, &supplied, &fresh).unwrap();
    let mut update = patch(&state);
    let c = &mut update.updates[0].claims[1];
    c.status = "superseded".into();
    c.change_reason = "User cancelled only reload retention".into();
    c.sources.push("correction".into());
    let mut replacement = claim("", "Reload clears operands");
    replacement.sources = vec!["correction".into()];
    replacement.replaces = vec!["reload".into()];
    update.updates[0].claims.push(replacement);
    apply(&mut state, update, &supplied, &fresh).unwrap();
    let claims = &state.topics["settings"].claims;
    assert_eq!(serde_json::to_value(&claims[0]).unwrap(), unchanged);
    assert_eq!(state.archive["settings"][0].claim.id, "reload");
    assert_eq!(
        state.archive["settings"][0].claim.sources,
        ["original", "correction"]
    );
    assert!(!claims[1].id.is_empty());
    let replacement_id = claims[1].id.clone();
    let mut invalid = patch(&state);
    invalid.updates[0].claims[1].replaces.clear();
    let error = apply(&mut state, invalid, &supplied, &fresh).unwrap_err();
    assert!(error
        .msg
        .contains("must preserve its existing replaces links"));
    assert!(error.msg.contains("reload"));
    let mut update = patch(&state);
    update.updates[0].claims[1].text = "Reload clears both operands".into();
    update.updates[0].claims[1].change_reason = "User clarified both operands".into();
    apply(&mut state, update, &supplied, &fresh).unwrap();
    assert_eq!(state.topics["settings"].claims[1].id, replacement_id);
    validate_state(&state, "test").unwrap();
}

#[test]
fn cancellation_cannot_erase_original_evidence_or_rewrite_history() {
    for rewrite in [false, true] {
        let (mut state, supplied, fresh) = setup();
        let mut update = patch(&state);
        let c = &mut update.updates[0].claims[1];
        c.status = "superseded".into();
        c.change_reason = "Cancelled".into();
        c.sources.push("correction".into());
        if rewrite {
            c.text = "Different history".into();
        } else {
            c.sources.remove(0);
        }
        assert!(apply(&mut state, update, &supplied, &fresh).is_err());
    }
}

#[test]
fn historical_user_evidence_is_not_a_new_correction() {
    let (mut state, supplied, _) = setup();
    let mut update = patch(&state);
    update.updates[0].claims[0].text = "Changed".into();
    update.updates[0].claims[0].change_reason = "Old correction reused".into();
    update.updates[0].claims[0]
        .sources
        .push("correction".into());
    assert!(apply(&mut state, update, &supplied, &BTreeSet::new()).is_err());
}

#[test]
fn legacy_ids_are_deterministic_and_survive_checkpoint_validation() {
    let (mut state, supplied, fresh) = setup();
    for c in &mut state.topics.get_mut("settings").unwrap().claims {
        c.id.clear();
    }
    validate_state(&state, "test").unwrap();
    let mut second = state.clone();
    identify_claims(&mut state.topics);
    identify_claims(&mut second.topics);
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    let update = patch(&state);
    apply(&mut state, update, &supplied, &fresh).unwrap();
    validate_state(&state, "test").unwrap();
}

#[test]
fn duplicate_ids_are_rejected() {
    let (mut state, supplied, fresh) = setup();
    let mut update = patch(&state);
    update.updates[0].claims[1].id = "save".into();
    assert!(apply(&mut state, update, &supplied, &fresh).is_err());
}

#[test]
fn twenty_replacements_keep_current_context_small_and_all_history() {
    let (mut state, mut supplied, _) = setup();
    for n in 0..20 {
        let source = format!("change-{n}");
        supplied.insert(source.clone(), event(&source, "user"));
        let mut update = patch(&state);
        let old = &mut update.updates[0].claims[1];
        let old_id = old.id.clone();
        old.status = "superseded".into();
        old.change_reason = format!("User requested revision {n}");
        old.sources.push(source.clone());
        let mut next = claim(&format!("replacement-{n}"), &format!("Reload rule {n}"));
        next.sources = vec![source.clone()];
        next.replaces = vec![old_id.clone()];
        update.updates[0].claims.push(next);
        apply(&mut state, update, &supplied, &BTreeSet::from([source])).unwrap();
        state.revision += 1;
        assert_eq!(state.topics["settings"].claims.len(), 2);
        assert_eq!(state.archive["settings"].len(), n + 1);
        assert_eq!(state.archive["settings"][n].claim.id, old_id);
        assert_eq!(
            state.archive["settings"][n].replaced_by,
            [format!("replacement-{n}")]
        );
        assert!(state.evidence.len() <= 2);
        validate_state(&state, "test").unwrap();
        supplied = state.evidence.clone();
    }
    let mut update = patch(&state);
    update.updates[0].claims[1].id = "reload".into();
    assert!(apply(&mut state, update, &supplied, &BTreeSet::new()).is_err());
}

#[test]
fn legacy_superseded_claims_move_to_archive_without_losing_sources() {
    let (mut state, supplied, _) = setup();
    let old = &mut state.topics.get_mut("settings").unwrap().claims[1];
    old.id.clear();
    old.status = "superseded".into();
    identify_claims(&mut state.topics);
    let id = state.topics["settings"].claims[1].id.clone();
    archive_superseded(&mut state, &supplied);
    assert_eq!(state.topics["settings"].claims.len(), 1);
    assert_eq!(state.archive["settings"][0].claim.id, id);
    assert_eq!(
        state.archive["settings"][0].evidence["original"].text,
        "original"
    );
    archive_superseded(&mut state, &supplied);
    assert_eq!(state.archive["settings"].len(), 1);
    validate_state(&state, "test").unwrap();
}

#[test]
fn full_topic_can_replace_all_eight_requirements_in_one_patch() {
    let (mut state, supplied, fresh) = setup();
    state.topics.get_mut("settings").unwrap().claims = (0..8)
        .map(|n| claim(&format!("old-{n}"), "Old rule"))
        .collect();
    let mut update = patch(&state);
    for c in &mut update.updates[0].claims {
        c.status = "superseded".into();
        c.change_reason = "User replaced all rules".into();
        c.sources.push("correction".into());
    }
    for n in 0..8 {
        let mut c = claim(&format!("new-{n}"), "New rule");
        c.sources = vec!["correction".into()];
        c.replaces = vec![format!("old-{n}")];
        update.updates[0].claims.push(c);
    }
    apply(&mut state, update, &supplied, &fresh).unwrap();
    assert_eq!(state.topics["settings"].claims.len(), 8);
    assert_eq!(state.archive["settings"].len(), 8);
    validate_state(&state, "test").unwrap();
}

#[test]
fn archive_successor_cannot_disappear_from_a_later_update() {
    let (mut state, supplied, fresh) = setup();
    let mut update = patch(&state);
    let old = &mut update.updates[0].claims[1];
    old.kind = "open_question".into();
    // Seed a non-requirement question so its resolution is a decision.
    state.topics.get_mut("settings").unwrap().claims[1].kind = "open_question".into();
    old.status = "superseded".into();
    let mut decision = claim("answer", "User resolved the choice");
    decision.kind = "decision".into();
    decision.replaces = vec!["reload".into()];
    update.updates[0].claims.push(decision);
    apply(&mut state, update, &supplied, &fresh).unwrap();
    validate_state(&state, "test").unwrap();
    let before = serde_json::to_value(&state).unwrap();
    let mut update = patch(&state);
    update.updates[0].claims.retain(|c| c.id != "answer");
    assert!(apply(&mut state, update, &supplied, &fresh).is_err());
    assert_eq!(serde_json::to_value(&state).unwrap(), before);
    // Explicit supersession keeps the successor reachable in the archive.
    let mut update = patch(&state);
    update.updates[0]
        .claims
        .iter_mut()
        .find(|c| c.id == "answer")
        .unwrap()
        .status = "superseded".into();
    apply(&mut state, update, &supplied, &fresh).unwrap();
    validate_state(&state, "test").unwrap();
}

fn operations(ops: Value) -> String {
    json!({"summary":"Current rules","updates":[{"key":"settings","title":"Settings","memory":"Current rules","related":[],"operations":ops}]}).to_string()
}

#[test]
fn operations_preserve_omitted_claims_and_cancel_without_copying_history() {
    let (mut state, supplied, fresh) = setup();
    let original = serde_json::to_value(&state.topics["settings"].claims[0]).unwrap();
    let text = operations(json!([
        {"action":"cancel","id":"reload","claim":null,"reason":"User cancelled retention","sources":["correction"]},
        {"action":"add","id":"","reason":"","sources":[],"claim":{"id":"new-reload","change_reason":"","replaces":["reload"],"kind":"requirement","status":"requested","text":"Reload clears operands","sources":["correction"]}}
    ]));
    let patch = parse_patch(&state, &text).unwrap();
    apply(&mut state, patch, &supplied, &fresh).unwrap();
    assert_eq!(
        serde_json::to_value(&state.topics["settings"].claims[0]).unwrap(),
        original
    );
    assert_eq!(
        state.archive["settings"][0].claim.text,
        "Reload retains operands"
    );
    assert_eq!(
        state.archive["settings"][0].claim.sources,
        ["original", "correction"]
    );
    assert_eq!(state.archive["settings"][0].replaced_by, ["new-reload"]);
    let update = parse_patch(&state, &operations(json!([]))).unwrap();
    let evidence = state.evidence.clone();
    apply(&mut state, update, &evidence, &BTreeSet::new()).unwrap();
    assert_eq!(state.topics["settings"].claims.len(), 2);
    validate_state(&state, "test").unwrap();
}

#[test]
fn invalid_operations_never_mutate_state() {
    let (state, _, _) = setup();
    let before = serde_json::to_value(&state).unwrap();
    let cancel = json!({"action":"cancel","id":"reload","claim":null,"reason":"Cancelled","sources":["correction"]});
    for ops in [
        json!([cancel.clone(), cancel]),
        json!([{"action":"cancel","id":"missing","claim":null,"reason":"Cancelled","sources":["correction"]}]),
        json!([{"action":"delete","id":"reload","claim":null,"reason":"","sources":[]}]),
        json!([{"action":"cancel","id":"reload","claim":null,"reason":"","sources":["correction"]}]),
        json!([{"action":"add","id":"","claim":claim("save","Duplicate"),"reason":"","sources":[]}]),
    ] {
        assert!(parse_patch(&state, &operations(ops)).is_err());
        assert_eq!(serde_json::to_value(&state).unwrap(), before);
    }
}

#[test]
fn operation_revision_still_requires_new_user_evidence() {
    let (mut state, supplied, fresh) = setup();
    let mut revised = claim("", "Changed rule");
    let text = operations(
        json!([{"action":"revise","id":"reload","claim":revised,"reason":"","sources":[]}]),
    );
    let update = parse_patch(&state, &text).unwrap();
    assert!(apply(&mut state, update, &supplied, &fresh).is_err());
    revised.change_reason = "Explicit user refinement".into();
    revised.sources = vec!["correction".into()];
    let text = operations(
        json!([{"action":"revise","id":"reload","claim":revised,"reason":"","sources":[]}]),
    );
    let update = parse_patch(&state, &text).unwrap();
    apply(&mut state, update, &supplied, &fresh).unwrap();
    assert_eq!(state.topics["settings"].claims[1].id, "reload");
    assert_eq!(state.topics["settings"].claims[1].text, "Changed rule");
}

#[test]
fn add_can_omit_unused_empty_operation_fields_but_cancel_needs_id() {
    let (state, _, _) = setup();
    let mut new = claim("new", "New rule");
    new.sources = vec!["correction".into()];
    let result = parse_patch(&state, &operations(json!([{"action":"add","claim":new}]))).unwrap();
    assert_eq!(result.updates[0].claims.len(), 3);
    assert!(parse_patch(
        &state,
        &operations(json!([{"action":"cancel","reason":"Cancelled","sources":["correction"]}]))
    )
    .is_err());
}

#[test]
fn receipts_distinguish_saved_noop_partial_and_uncertain_writes() {
    let (before, supplied, fresh) = setup();
    let mut after = before.clone();
    let text = operations(json!([
        {"action":"cancel","id":"reload","reason":"Cancelled","sources":["correction"]},
        {"action":"add","claim":{"id":"new","kind":"requirement","status":"requested","text":"Reload clears","sources":["correction"],"replaces":["reload"]}}
    ]));
    let update = parse_patch(&after, &text).unwrap();
    apply(&mut after, update, &supplied, &fresh).unwrap();
    after.revision = 1;
    let mut progress = WriteProgress {
        before: Some(before.clone()),
        committed: Some(after),
        writing: false,
        queued: false,
    };
    let receipt = write_receipt(&progress, true);
    assert_eq!(receipt["status"], "saved");
    assert_eq!(receipt["revision"], 1);
    assert_eq!(receipt["added"], 1);
    assert_eq!(receipt["archived"], 1);
    assert_eq!(receipt["revised"], 0);
    assert_eq!(write_receipt(&progress, false)["status"], "partially_saved");
    progress.writing = true;
    assert_eq!(write_receipt(&progress, false)["status"], "unknown");
    progress.writing = false;
    progress.committed = Some(before);
    assert_eq!(write_receipt(&progress, true)["status"], "unchanged");
    assert_eq!(write_receipt(&progress, false)["status"], "not_saved");
    assert!(write_receipt(&WriteProgress::default(), false)["revision"].is_null());
}
