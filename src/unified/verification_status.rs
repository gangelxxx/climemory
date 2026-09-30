//! Distinguish an evidenced verification record from a request to prove behavior.
use super::*;

pub(super) const RULES: &str = "For verification_status aspects only, return verification instead of status. state=explicit_unverified when an original explicitly says the claim was not independently verified; reported_only when an original explicitly attributes validation to a report; independent_record when an original records applicable independent validation; unknown when the reviewed sources do not establish every requested verification state; conflicting when applicable verification records disagree about the SAME claim. Known states need support entries containing an original evidence id and a short verbatim quote explicitly supporting that classification; include every support id in the aspect evidence. Different known states for DIFFERENT claims are known_mixed, not a conflict: preserve each claim and outcome in the answer, cite each, and give each support entry its own known state. known_mixed requires at least two different supported known states; if any requested claim has unknown verification state, use unknown instead. Only known_mixed support entries carry state. Do not infer explicit_unverified from absence of proof, an unrelated quotation, or a worker gap. unknown has no support entries and remains missing. A recorded negative verification state answers a verification_status question; it does not prove behavior. The host derives coverage from this typed result. Other intents, especially independent_proof, must use status and must not return verification. Preserve reported qualifications and the limits of reviewed sources.";

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum State {
    ExplicitUnverified,
    ReportedOnly,
    IndependentRecord,
    KnownMixed,
    Unknown,
    Conflicting,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Support {
    id: String,
    quote: String,
    #[serde(default)]
    state: Option<State>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    state: State,
    support: Vec<Support>,
}

pub(super) fn schema(mut variant: Value) -> Value {
    variant["properties"]
        .as_object_mut()
        .unwrap()
        .remove("status");
    let required = variant["required"].as_array_mut().unwrap();
    required.retain(|field| field != "status");
    required.push(json!("verification"));
    let mut known = json!({"type":"object","additionalProperties":false,
        "required":["state","support"],"properties":{
            "state":{"type":"string","enum":["explicit_unverified","reported_only","independent_record","conflicting"]},
            "support":{"type":"array","minItems":1,"maxItems":16,"items":{
                "type":"object","additionalProperties":false,"required":["id","quote"],
                "properties":{"id":variant["properties"]["evidence"]["items"],
                    "quote":{"type":"string","minLength":1,"maxLength":600}}}}}});
    let mut unknown = known.clone();
    unknown["properties"]["state"]["enum"] = json!(["unknown"]);
    unknown["properties"]["support"]["minItems"] = json!(0);
    unknown["properties"]["support"]["maxItems"] = json!(0);
    let mut mixed = known.clone();
    mixed["properties"]["state"]["enum"] = json!(["known_mixed"]);
    mixed["properties"]["support"]["minItems"] = json!(2);
    mixed["properties"]["support"]["items"]["properties"]["state"] = json!({"type":"string","enum":["explicit_unverified","reported_only","independent_record"]});
    mixed["properties"]["support"]["items"]["required"] = json!(["id", "quote", "state"]);
    // An empty original set permits only unknown; an arbitrary string must not
    // become a supposedly grounded verification record.
    if variant["properties"]["evidence"]["maxItems"] == 0 {
        known = Value::Null;
    }
    variant["properties"]["verification"] = if known.is_null() {
        unknown
    } else {
        json!({"anyOf":[known,mixed,unknown]})
    };
    variant
}

pub(super) fn normalize(
    raw: &mut Value,
    index: &Index,
    requested: &[String],
    intents: &[scope::Intent],
    reviewed: &BTreeSet<String>,
) -> Result<()> {
    let fragments = index.fragments();
    let Some(aspects) = raw.get_mut("aspects").and_then(Value::as_array_mut) else {
        return Ok(()); // The ordinary protocol parser reports malformed shapes.
    };
    for aspect in aspects {
        let intent = aspect["question"].as_str().and_then(|question| {
            requested
                .iter()
                .position(|q| q == question)
                .and_then(|position| intents.get(position))
        });
        if intent != Some(&scope::Intent::VerificationStatus) {
            if aspect.get("verification").is_some() {
                return Err(AppError::new(
                    "unified protocol: verification result is only valid for verification_status",
                ));
            }
            continue;
        }
        if aspect.get("status").is_some() {
            return Err(AppError::new(
                "unified protocol: verification_status requires verification instead of status",
            ));
        }
        let finding: Finding = serde_json::from_value(aspect["verification"].clone())
            .map_err(|e| AppError::new(format!("unified protocol: verification result: {e}")))?;
        let evidence: Vec<String> = serde_json::from_value(aspect["evidence"].clone())
            .map_err(|e| AppError::new(format!("unified protocol: verification evidence: {e}")))?;
        let unknown = matches!(finding.state, State::Unknown);
        let mixed = matches!(finding.state, State::KnownMixed);
        let component_states: BTreeSet<_> = finding
            .support
            .iter()
            .filter_map(|support| support.state.as_ref())
            .collect();
        if (unknown && !finding.support.is_empty())
            || (!unknown && finding.support.is_empty())
            || (mixed && component_states.len() < 2)
            || finding.support.len() > 16
            || finding.support.iter().any(|support| {
                support.quote.trim().is_empty()
                    || if mixed {
                        !matches!(
                            support.state,
                            Some(
                                State::ExplicitUnverified
                                    | State::ReportedOnly
                                    | State::IndependentRecord
                            )
                        )
                    } else {
                        support.state.is_some()
                    }
                    || support.quote.chars().count() > 600
                    || !reviewed.contains(&support.id)
                    || !evidence.contains(&support.id)
                    || !fragments
                        .get(&support.id)
                        .is_some_and(|fragment| fragment.text.contains(&support.quote))
            })
        {
            return Err(AppError::new(
                "unified protocol: verification result requires verbatim support from cited reviewed originals; unknown has no support",
            ));
        }
        let status = match finding.state {
            State::Unknown => "missing",
            State::Conflicting => "conflicting",
            State::ExplicitUnverified
            | State::ReportedOnly
            | State::IndependentRecord
            | State::KnownMixed => "found",
        };
        let object = aspect.as_object_mut().unwrap();
        object.remove("verification");
        object.insert("status".into(), json!(status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        index_with("Reported tests passed. Not independently verified.")
    }

    fn index_with(text: &str) -> Index {
        serde_json::from_value(json!({"format":1,"revision":"r","links":[],
            "sources":[{"id":"memo","path":"memory.json","revision":"r","authority":"advisory_memory","text":text,"title":"History","agent":"cheap","parent":null}],
            "threads":[{"id":"memo","source":"memo","title":"History","parent":null,"agent":"cheap","fragments":[{"id":"memo:L1","source":"memo","line":1,"text":text}],"passport":{"summary":"","questions":[],"terms":[],"subtree_terms":[],"checked_candidates":[]}}]})).unwrap()
    }

    fn answer(state: &str, support: Value) -> Value {
        json!({"aspects":[{"question":"What verification is recorded?","answer":"Reported tests, not independently verified.",
            "evidence":["memo:L1"],"verification":{"state":state,"support":support}}]})
    }

    fn normalize_answer(value: &mut Value, intent: scope::Intent) -> Result<()> {
        normalize(
            value,
            &index(),
            &["What verification is recorded?".into()],
            &[intent],
            &["memo:L1".into()].into(),
        )
    }

    #[test]
    fn cited_explicit_negative_is_known_but_unknown_remains_missing() {
        let mut known = answer(
            "explicit_unverified",
            json!([{"id":"memo:L1","quote":"Not independently verified."}]),
        );
        normalize_answer(&mut known, scope::Intent::VerificationStatus).unwrap();
        assert_eq!(known["aspects"][0]["status"], "found");
        assert!(known["aspects"][0].get("verification").is_none());
        let mut unknown = answer("unknown", json!([]));
        normalize_answer(&mut unknown, scope::Intent::VerificationStatus).unwrap();
        assert_eq!(unknown["aspects"][0]["status"], "missing");
    }

    #[test]
    fn typed_result_never_upgrades_legacy_missing_or_independent_proof() {
        let mut legacy = json!({"aspects":[{"question":"What verification is recorded?","answer":"Not independently verified.","status":"missing","evidence":["memo:L1"]}]});
        let before = legacy.clone();
        assert!(normalize_answer(&mut legacy, scope::Intent::VerificationStatus).is_err());
        assert_eq!(legacy, before);
        normalize_answer(&mut legacy, scope::Intent::IndependentProof).unwrap();
        assert_eq!(legacy, before);
        let mut typed = answer(
            "explicit_unverified",
            json!([{"id":"memo:L1","quote":"Not independently verified."}]),
        );
        assert!(normalize_answer(&mut typed, scope::Intent::IndependentProof).is_err());
    }

    #[test]
    fn typed_states_reject_empty_fabricated_unreviewed_or_uncited_support() {
        for support in [
            json!([]),
            json!([{"id":"memo:L1","quote":""}]),
            json!([{"id":"memo:L1","quote":"Independently verified."}]),
            json!([{"id":"unopened:L1","quote":"Not independently verified."}]),
        ] {
            assert!(normalize_answer(
                &mut answer("explicit_unverified", support),
                scope::Intent::VerificationStatus
            )
            .is_err());
        }
        let mut uncited = answer(
            "explicit_unverified",
            json!([{"id":"memo:L1","quote":"Not independently verified."}]),
        );
        uncited["aspects"][0]["evidence"] = json!([]);
        assert!(normalize_answer(&mut uncited, scope::Intent::VerificationStatus).is_err());
        let mut unknown = answer(
            "unknown",
            json!([{"id":"memo:L1","quote":"Not independently verified."}]),
        );
        assert!(normalize_answer(&mut unknown, scope::Intent::VerificationStatus).is_err());
    }

    #[test]
    fn aliases_restore_typed_support_without_changing_quotes() {
        let refs = references::References::new(["memo:L1".into()]);
        let original = answer(
            "reported_only",
            json!([{"id":"memo:L1","quote":"Reported tests passed."}]),
        );
        let mut value = original.clone();
        refs.encode(&mut value);
        assert_eq!(value["aspects"][0]["verification"]["support"][0]["id"], "1");
        refs.decode(&mut value);
        assert_eq!(value, original);
        normalize_answer(&mut value, scope::Intent::VerificationStatus).unwrap();
        assert_eq!(value["aspects"][0]["status"], "found");
    }

    #[test]
    fn independent_record_is_knowledge_not_proof_and_conflicts_remain_conflicting() {
        for (state, text, expected) in [
            ("independent_record", "An independent auditor reran rollout checks and recorded success.", "found"),
            ("conflicting", "Rollout record A says independently verified; record B says not independently verified.", "conflicting"),
        ] {
            let mut value = answer(state, json!([{"id":"memo:L1","quote":text}]));
            value["aspects"][0]["answer"] = json!(text);
            normalize(&mut value, &index_with(text), &["What verification is recorded?".into()], &[scope::Intent::VerificationStatus], &["memo:L1".into()].into()).unwrap();
            assert_eq!(value["aspects"][0]["status"], expected);
            let mut proof = answer(state, json!([{"id":"memo:L1","quote":text}]));
            assert!(normalize_answer(&mut proof, scope::Intent::IndependentProof).is_err());
        }
        let mut invalid = answer("invented_state", json!([]));
        assert!(normalize_answer(&mut invalid, scope::Intent::VerificationStatus).is_err());
    }

    #[test]
    fn grouped_known_outcomes_are_not_a_conflict_but_unknown_component_never_closes() {
        let text = "Rollout was independently verified. Rollback was not independently verified.";
        let support = json!([
            {"id":"memo:L1","quote":"Rollout was independently verified.","state":"independent_record"},
            {"id":"memo:L1","quote":"Rollback was not independently verified.","state":"explicit_unverified"}
        ]);
        let mut value = answer("known_mixed", support.clone());
        value["aspects"][0]["answer"] = json!(text);
        let requested = ["What verification is recorded?".into()];
        let reviewed = ["memo:L1".into()].into();
        normalize(
            &mut value,
            &index_with(text),
            &requested,
            &[scope::Intent::VerificationStatus],
            &reviewed,
        )
        .unwrap();
        assert_eq!(value["aspects"][0]["status"], "found");
        assert_eq!(value["aspects"][0]["answer"], text);

        for invalid_state in [
            "unknown",
            "conflicting",
            "known_mixed",
            "independent_record",
        ] {
            let mut mixed = answer("known_mixed", support.clone());
            mixed["aspects"][0]["verification"]["support"][1]["state"] = json!(invalid_state);
            assert!(normalize(
                &mut mixed,
                &index_with(text),
                &requested,
                &[scope::Intent::VerificationStatus],
                &reviewed
            )
            .is_err());
        }
        let mut unknown = answer("unknown", json!([]));
        unknown["aspects"][0]["answer"] = json!("Rollout verification is recorded; rollback verification is not established in reviewed sources.");
        normalize(
            &mut unknown,
            &index_with("Rollout was independently verified."),
            &requested,
            &[scope::Intent::VerificationStatus],
            &reviewed,
        )
        .unwrap();
        assert_eq!(unknown["aspects"][0]["status"], "missing");
    }

    #[test]
    fn schema_uses_typed_result_only_and_without_originals_requires_unknown() {
        let base = json!({"type":"object","required":["question","status","evidence","answer"],"properties":{"status":{"type":"string"},"evidence":{"type":"array","items":{"type":"string","enum":["memo:L1"]}}}});
        let typed = schema(base.clone());
        assert!(typed["properties"].get("status").is_none());
        assert!(typed["required"]
            .as_array()
            .unwrap()
            .contains(&json!("verification")));
        let mut empty = base;
        empty["properties"]["evidence"]["maxItems"] = json!(0);
        assert_eq!(
            schema(empty)["properties"]["verification"]["properties"]["state"]["enum"],
            json!(["unknown"])
        );
    }
}
