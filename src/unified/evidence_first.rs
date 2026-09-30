//! Source-grounded retrieval representation; saved agent interpretations stay canonical.
use super::{scope::Intent, *};

/// Input must be rendered from canonical full evidence, before delivery receipts
/// or excerpt projection. The caller removes summary_quote on a clone first.
pub(super) fn candidate(output: &Value, requested: &[String], intents: &[Intent]) -> Option<Value> {
    if output["status"] != "complete"
        || output["detail_level"] != "summary"
        || !output["conflicts"].as_array().is_some_and(Vec::is_empty)
        || [
            "answer_mode",
            "answer",
            "answer_unchanged",
            "source_blocks",
            "reused_evidence",
        ]
        .iter()
        .any(|key| output.get(key).is_some())
    {
        return None;
    }
    let mut references = BTreeSet::new();
    for row in output["evidence"].as_array()? {
        let reference = row["ref"].as_str()?;
        if reference.is_empty()
            || !references.insert(reference)
            || !matches!(
                row["authority"].as_str()?,
                "advisory_memory" | "agent_memory"
            )
            || row["quote"].as_str()?.trim().is_empty()
            || row["source"].as_str()?.trim().is_empty()
            || row.get("summary_quote").is_some()
        {
            return None;
        }
    }
    let aspects = output["aspects"].as_array()?;
    if aspects.is_empty() || aspects.len() != requested.len() || aspects.len() != intents.len() {
        return None;
    }
    for ((aspect, question), intent) in aspects.iter().zip(requested).zip(intents) {
        if !matches!(intent, Intent::ReportedState | Intent::VerificationStatus)
            || question.trim().is_empty()
            || aspect["question"].as_str()? != question
            || aspect["status"] != "found"
            || aspect["answer"].as_str()?.trim().is_empty()
            || aspect.as_object()?.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "question"
                        | "question_id"
                        | "status"
                        | "answer"
                        | "evidence"
                        | "self_contained"
                )
            })
        {
            return None;
        }
        let ids = aspect["evidence"].as_array()?;
        if ids.is_empty()
            || ids
                .iter()
                .any(|id| id.as_str().is_none_or(|id| !references.contains(id)))
        {
            return None;
        }
    }
    let mut result = output.clone();
    result["answer_mode"] = json!("evidence");
    for aspect in result["aspects"].as_array_mut()? {
        let object = aspect.as_object_mut()?;
        object.remove("answer");
        object.remove("self_contained");
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        json!({"status":"complete","detail_level":"summary","conflicts":[],
            "aspects":[{"question":"What limit is reported?","status":"found","answer":"The draft reports an extra unsupported implementation detail.","evidence":["e1"],"self_contained":true}],
            "evidence":[{"ref":"e1","authority":"advisory_memory","source":"memory/a.json","quote":"Reported limit: 7; only while enabled. Not independently verified."}]})
    }
    fn project(v: &Value) -> Option<Value> {
        candidate(
            v,
            &["What limit is reported?".into()],
            &[Intent::ReportedState],
        )
    }
    #[test]
    fn keeps_questions_and_exact_qualified_originals_without_draft_claims() {
        let original = fixture();
        let result = project(&original).unwrap();
        assert_eq!(result["answer_mode"], "evidence");
        assert_eq!(result["evidence"], original["evidence"]);
        assert_eq!(
            result["aspects"][0]["question"],
            original["aspects"][0]["question"]
        );
        assert!(result["aspects"][0].get("answer").is_none());
        assert!(result["aspects"][0].get("self_contained").is_none());
        assert!(original["aspects"][0]["answer"].is_string());
        assert!(candidate(
            &original,
            &["What limit is reported?".into()],
            &[Intent::VerificationStatus]
        )
        .is_some());
    }
    #[test]
    fn ambiguous_or_incomplete_delivery_keeps_normal_representation() {
        for mutation in [
            "partial",
            "conflict",
            "authority",
            "quote",
            "missing_ref",
            "duplicate_ref",
            "unknown",
            "missing",
            "excerpt",
            "aggregate",
            "reused",
            "capsule",
        ] {
            let mut value = fixture();
            match mutation {
                "partial" => value["status"] = json!("partial"),
                "conflict" => value["conflicts"] = json!([{"unresolved":false}]),
                "authority" => value["evidence"][0]["authority"] = json!("user_document"),
                "quote" => value["evidence"][0]["quote"] = json!(" "),
                "missing_ref" => value["aspects"][0]["evidence"] = json!(["unknown"]),
                "duplicate_ref" => {
                    let row = value["evidence"][0].clone();
                    value["evidence"].as_array_mut().unwrap().push(row);
                }
                "unknown" => value["aspects"][0]["qualification"] = json!("Retain"),
                "missing" => value["aspects"][0]["status"] = json!("missing"),
                "excerpt" => value["evidence"][0]["summary_quote"] = json!("7"),
                "aggregate" => value["answer"] = json!("Additional interpretation"),
                "reused" => value["reused_evidence"] = json!(["e1"]),
                _ => value["source_blocks"] = json!({}),
            }
            assert!(project(&value).is_none(), "{mutation}");
        }
        for intent in [
            Intent::OriginalRequirement,
            Intent::FactualQuestion,
            Intent::IndependentProof,
        ] {
            assert!(
                candidate(&fixture(), &["What limit is reported?".into()], &[intent]).is_none()
            );
        }
        assert!(candidate(
            &fixture(),
            &["Different question".into()],
            &[Intent::ReportedState]
        )
        .is_none());
        assert!(candidate(&fixture(), &[], &[]).is_none());
    }
}
