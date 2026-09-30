//! Alternative retrieval representation, not a lossless paraphrase transform.
//! Called only after attaching a newly delivered, fully consulted source capsule.
use super::{scope::Intent, *};

pub(super) fn candidate(output: &Value, requested: &[String], intents: &[Intent]) -> Option<Value> {
    if output["status"] != "complete"
        || output["detail_level"] != "summary"
        || !output["conflicts"].as_array().is_some_and(Vec::is_empty)
        || output.get("answer_mode").is_some()
        || output.get("answer").is_some()
        || output.get("answer_unchanged").is_some()
        || output
            .get("reused_evidence")
            .is_some_and(|v| !v.as_array().is_some_and(Vec::is_empty))
    {
        return None;
    }
    let blocks = output["source_blocks"].as_object()?;
    if blocks.len() != 1 {
        return None;
    }
    let (block_id, block) = blocks.iter().next()?;
    if block["authority"] != "user_document"
        || block["review"] != "source_context"
        || block["source"].as_str()?.is_empty()
    {
        return None;
    }
    let mut lines = BTreeMap::new();
    for row in block["numbered_lines"].as_array()? {
        let pair = row.as_array()?;
        if pair.len() != 2 {
            return None;
        }
        let line = pair[0].as_u64()?;
        if line == 0 || lines.insert(line, pair[1].as_str()?).is_some() {
            return None;
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut refs = BTreeSet::new();
    for row in output["evidence"].as_array()? {
        let reference = row["ref"].as_str()?;
        if reference.is_empty()
            || !refs.insert(reference)
            || row["authority"] != "user_document"
            || row["source_block"].as_str()? != block_id
            || lines
                .get(&row["line"].as_u64()?)
                .is_none_or(|line| line.trim().is_empty())
        {
            return None;
        }
    }
    let aspects = output["aspects"].as_array()?;
    if aspects.is_empty() || aspects.len() != requested.len() || aspects.len() != intents.len() {
        return None;
    }
    for ((aspect, question), intent) in aspects.iter().zip(requested).zip(intents) {
        if !matches!(
            intent,
            Intent::OriginalRequirement | Intent::FactualQuestion
        ) || aspect["question"].as_str()? != question
            || question.trim().is_empty()
            || aspect["status"] != "found"
            || aspect["answer"].as_str()?.trim().is_empty()
            || aspect.get("answer_from_evidence").is_some()
            || aspect.get("answer_prefix").is_some()
            || aspect.get("question_ref").is_some()
        {
            return None;
        }
        // Unknown semantics are retained through the ordinary answer representation.
        if aspect.as_object()?.keys().any(|key| {
            !matches!(
                key.as_str(),
                "question" | "question_id" | "status" | "answer" | "evidence" | "self_contained"
            )
        }) {
            return None;
        }
        let evidence = aspect["evidence"].as_array()?;
        if evidence.is_empty()
            || evidence
                .iter()
                .any(|id| id.as_str().is_none_or(|id| !refs.contains(id)))
        {
            return None;
        }
    }
    let mut result = output.clone();
    result["answer_mode"] = json!("source_context");
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
        json!({"status":"complete","detail_level":"summary","conflicts":[],"aspects":[{"question":"What color applies to the enabled panel?","status":"found","answer":"The enabled panel must be blue; this states a requirement, not proof of implementation.","evidence":["e1"],"self_contained":true}],"evidence":[{"ref":"e1","authority":"user_document","source_block":"b1","line":3}],"source_blocks":{"b1":{"source":"memory/docs/ui.md","authority":"user_document","review":"source_context","numbered_lines":[[1,"# Requirements"],[2,""],[3,"Enabled panel must be blue."]]}}})
    }
    fn project(v: &Value) -> Option<Value> {
        candidate(
            v,
            &["What color applies to the enabled panel?".into()],
            &[Intent::OriginalRequirement],
        )
    }
    #[test]
    fn source_mode_preserves_exact_source_questions_and_addresses_without_certifying_paraphrase() {
        let original = fixture();
        let result = project(&original).unwrap();
        assert_eq!(result["answer_mode"], "source_context");
        assert!(result["aspects"][0].get("answer").is_none());
        assert!(result["aspects"][0].get("self_contained").is_none());
        assert_eq!(
            result["aspects"][0]["question"],
            original["aspects"][0]["question"]
        );
        assert_eq!(result["aspects"][0]["status"], "found");
        assert_eq!(result["source_blocks"], original["source_blocks"]);
        assert_eq!(result["evidence"], original["evidence"]);
        assert!(original["aspects"][0]["answer"].is_string());
    }
    #[test]
    fn unsupported_delivery_shapes_and_semantics_keep_original_answers() {
        for change in [
            "partial",
            "conflict",
            "memory",
            "reused",
            "missing_ref",
            "duplicate_ref",
            "unknown",
            "missing_line",
            "aggregate",
            "empty_answer",
        ] {
            let mut v = fixture();
            match change {
                "partial" => v["status"] = json!("partial"),
                "conflict" => v["conflicts"] = json!([{"unresolved":false}]),
                "memory" => v["evidence"][0]["authority"] = json!("advisory_memory"),
                "reused" => v["reused_evidence"] = json!(["e2"]),
                "missing_ref" => v["aspects"][0]["evidence"] = json!(["e2"]),
                "duplicate_ref" => {
                    let row = v["evidence"][0].clone();
                    v["evidence"].as_array_mut().unwrap().push(row);
                }
                "unknown" => v["aspects"][0]["qualification"] = json!("Keep this"),
                "missing_line" => v["evidence"][0]["line"] = json!(4),
                "aggregate" => v["answer"] = json!("Extra interpretation"),
                _ => v["aspects"][0]["answer"] = json!(" "),
            }
            assert!(project(&v).is_none(), "{change}");
        }
        for intent in [
            Intent::ReportedState,
            Intent::VerificationStatus,
            Intent::IndependentProof,
        ] {
            assert!(candidate(
                &fixture(),
                &["What color applies to the enabled panel?".into()],
                &[intent]
            )
            .is_none());
        }
        assert!(candidate(&fixture(), &[], &[]).is_none());
        assert!(candidate(
            &fixture(),
            &["Different question".into()],
            &[Intent::FactualQuestion]
        )
        .is_none());
    }
}
