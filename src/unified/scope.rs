//! Freeze source-free requested aspects before retrieval. Evidence cannot expand scope.
use super::*;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Intent {
    OriginalRequirement,
    ReportedState,
    VerificationStatus,
    IndependentProof,
    FactualQuestion,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub aspects: Vec<String>,
    pub intents: Vec<Intent>,
    #[serde(default)]
    pub presentation_requirements: Vec<String>,
}

impl Plan {
    pub fn source_requirements(&self) -> Vec<String> {
        self.intents
            .iter()
            .map(|intent| {
                if *intent == Intent::OriginalRequirement {
                    "user_document"
                } else {
                    "any"
                }
                .into()
            })
            .collect()
    }
}

pub(super) fn plan(
    project: &Project,
    question: &str,
    history: &[Value],
    deadline: Instant,
) -> Result<Plan> {
    let profile = project
        .config
        .memory
        .verification_agent
        .as_ref()
        .or(project.config.memory.chat_agent.as_ref())
        .unwrap_or(&project.config.memory.documents_agent);
    let value = worker::call(
        project,
        profile,
        "unified_plan",
        json!({"task_instructions":"List only the facts explicitly requested by the CURRENT question as short questions. Use previous topic questions only to resolve references. No sources are available. Preserve the user's level of specificity. Do not expand broad terms into invented examples, parenthetical lists, subrequirements or proof-of-absence checks. For a request about keyboard, focus and typography, ask just those three aspects; do not add shortcuts, Enter/Space, focus order or font weight unless the user explicitly asks. Retain every detail the user DID specify. Do not add implementation checks, typography or other domain tasks unless asked. Include documented applicable conditions/exceptions within the relevant requested fact; do not require proof that no other exception exists. Treat a message containing related questions or a numbered list as ONE retrieval task. Apply shared context to each question. Merge equivalent repeated questions, but never merge away distinct conditions, exceptions or requested facts. Preserve every requested fact, including the last item in a list. Return 1 to 16 distinct aspects, each at most 500 characters. Evidence and output constraints are acceptance criteria, not extra aspects: for example, a report of success is not independent proof constrains the proof question; do not add a question asking to confirm that constraint. Apply these constraints to every relevant aspect. A request to cite sources (including exact source citations) changes evidence formatting, not the facts requested. Never create a citations/sources aspect unless identifying or comparing the sources is itself the subject of the question. Keep a separate aspect when the user explicitly asks to explain or evaluate the constraint itself. Do not answer the questions. Return intents in the SAME order as aspects. Classify the user's requested proposition, not the type of source you hope to find: original_requirement asks what the user or authoritative specification requires; reported_state asks what memory/history reports, including what a named memory thread specifies; verification_status asks whether/how a claim was verified, so verification status is itself the requested fact; independent_proof asks to establish actual behavior with independent validation; factual_question covers other factual questions. Exact citations do not change intent. A question about the rule REPORTED IN MEMORY is reported_state even if that reported rule uses must, limit or specify. Distinguish asking whether proof is recorded from demanding that proof establish behavior. When the same provenance or verification qualification applies to several reported facts, use one shared verification_status aspect covering those facts; do not multiply the same qualification into a question for every fact unless the user explicitly requests separate per-claim verification details. Preserve distinct requested verification methods, dates or outcomes. Split a mixed original-requirement and reported-implementation question. Never classify by available sources; none are supplied.","presentation_rules":"Separate output-only directives into presentation_requirements: citation/address formatting, output format, language and brevity. Do not create factual aspects for them. Preserve genuine questions identifying or comparing sources, evaluating source authority, verification, applicability or provenance in aspects. Conditions, exceptions, evidence sufficiency and required proof constrain their factual aspects; they are not merely presentation. If a request mixes facts and formatting, retain every fact in aspects and only the output directives here. Do not answer either list. Return at most 8 nonblank presentation instructions, each at most 300 characters.","planning_examples":[{"question":"What is the retention period? Cite exact lines and reply briefly in French.","aspects":["What retention period is required?"],"intents":["original_requirement"],"presentation_requirements":["Cite exact lines.","Reply briefly in French."]},{"question":"Which document defines retention, and why is it authoritative? Cite exact lines.","aspects":["Which document defines retention?","What establishes that document’s authority?"],"intents":["factual_question","factual_question"],"presentation_requirements":["Cite exact lines."],"note":"Source identity and authority are requested facts; never discard them as formatting."},{"question":"What colors must enabled and disabled Save buttons use, and when must Save be disabled? Return exact source citations.","aspects":["What colors must enabled Save buttons use?","What colors must disabled Save buttons use?","When must Save be disabled?"],"intents":["original_requirement","original_requirement","original_requirement"],"note":"Cite evidence for each of these three facts; do not add an aspect asking to provide citations.","presentation_requirements":["Cite exact source lines for each answer."]},{"question":"Provide independent evidence proving shift-count limits. A memory report of success is not independent validation.","aspects":["What independent evidence proves shift-count limits?"],"intents":["independent_proof"],"note":"The second sentence is an evidence constraint; do not ask what qualifies as validation or whether reports count.","presentation_requirements":[]},{"question":"What independent evidence proves shift-count limits, and explain what qualifies as independent validation?","aspects":["What independent evidence proves shift-count limits?","What qualifies as independent validation?"],"intents":["independent_proof","factual_question"],"note":"Here the explanation is explicitly requested, so retain it.","presentation_requirements":[]},{"question":"According to memory, what operand size does binary-calculations specify, and was it independently verified?","aspects":["What operand size does binary-calculations memory report?","What verification is recorded for the reported operand size?"],"intents":["reported_state","verification_status"],"presentation_requirements":[]},{"question":"Summarize the deployment notes about rollout timing and rollback procedure, distinguishing reports from independent validation.","aspects":["What rollout timing do the deployment notes report?","What rollback procedure do the deployment notes report?","What verification is recorded for these deployment reports?"],"intents":["reported_state","reported_state","verification_status"],"note":"One shared provenance question covers both reported facts; do not demand separate proofs unless requested.","presentation_requirements":[]}],"question":question,"topic_questions":history.iter().map(|h|&h["question"]).collect::<Vec<_>>()}),
        json!({"type":"object","additionalProperties":false,"required":["aspects","intents","presentation_requirements"],"properties":{"presentation_requirements":{"type":"array","maxItems":8,"items":{"type":"string","minLength":1,"maxLength":300}},"intents":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","enum":["original_requirement","reported_state","verification_status","independent_proof","factual_question"]}},"aspects":{"type":"array","minItems":1,"maxItems":16,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":500}}}}),
        deadline,
    )?;
    validate_plan(value)
}

fn validate_plan(value: Value) -> Result<Plan> {
    let plan: Plan = serde_json::from_value(value)
        .map_err(|e| AppError::new(format!("invalid requested aspect plan: {e}")))?;
    if plan.intents.len() != plan.aspects.len()
        || plan.aspects.is_empty()
        || plan.aspects.len() > 16
        || plan
            .aspects
            .iter()
            .any(|q| q.trim().is_empty() || q.chars().count() > 500)
        || plan.aspects.iter().collect::<BTreeSet<_>>().len() != plan.aspects.len()
        || plan.presentation_requirements.len() > 8
        || plan
            .presentation_requirements
            .iter()
            .any(|rule| rule.trim().is_empty() || rule.chars().count() > 300)
    {
        return Err(AppError::new("invalid requested aspect plan"));
    }
    Ok(plan)
}

pub(super) fn reject_duplicate_answers(a: &Assembly, requested: &[String]) -> Result<()> {
    let duplicates: Vec<_> = requested
        .iter()
        .filter(|question| {
            a.aspects
                .iter()
                .filter(|aspect| &aspect.question == *question)
                .count()
                > 1
        })
        .take(8)
        .collect();
    if !duplicates.is_empty() {
        return Err(AppError::new(format!(
            "unified protocol: duplicate requested aspect answers: {duplicates:?}; regenerate exactly one answer per requested aspect, preserving supported facts and uncertainty"
        )));
    }
    Ok(())
}

pub(super) fn enforce(a: &mut Assembly, requested: &[String]) {
    // Unknown aspects cannot become new obligations. A missing/duplicated planned
    // aspect stays missing; never promote evidence or ignore genuine conflicts.
    let mut supplied = std::mem::take(&mut a.aspects);
    a.aspects = requested
        .iter()
        .map(|q| {
            if supplied.iter().filter(|p| &p.question == q).count() == 1 {
                let pos = supplied.iter().position(|p| &p.question == q).unwrap();
                supplied.remove(pos)
            } else {
                Aspect {
                    self_contained: false,
                    assessed: false,
                    answer: String::new(),
                    question: q.clone(),
                    status: "missing".into(),
                    evidence: vec![],
                }
            }
        })
        .collect();
}

pub(super) fn enforce_authority(a: &mut Assembly, index: &Index, requirements: &[String]) {
    let fragments = index.fragments();
    for (aspect, required) in a.aspects.iter_mut().zip(requirements) {
        if required == "user_document" && aspect.status == "found" {
            let documents_only = !aspect.evidence.is_empty()
                && aspect.evidence.iter().all(|id| {
                    fragments.get(id).is_some_and(|f| {
                        index
                            .sources
                            .iter()
                            .any(|s| s.id == f.source && s.authority == "user_document")
                    })
                });
            if !documents_only {
                aspect.status = "missing".into();
                aspect.answer = crate::ui::tr(
                    "Original requirement not established; memory is advisory.",
                    "Исходное требование не подтверждено; память носит справочный характер.",
                    "原始要求尚未确认；记忆仅供参考。",
                )
                .into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presentation_constraints_are_bounded_without_filtering_source_questions() {
        let original = json!({"aspects":["Which document defines retention?","Why is this source authoritative?"],"intents":["factual_question","factual_question"],"presentation_requirements":["Cite exact lines.","用中文简短回答。"]});
        let plan = validate_plan(original.clone()).unwrap();
        assert_eq!(
            plan.aspects,
            [
                "Which document defines retention?",
                "Why is this source authoritative?"
            ]
        );
        assert_eq!(plan.source_requirements(), ["any", "any"]);
        assert_eq!(
            plan.presentation_requirements,
            ["Cite exact lines.", "用中文简短回答。"]
        );
        let mut legacy = original.clone();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("presentation_requirements");
        assert!(validate_plan(legacy)
            .unwrap()
            .presentation_requirements
            .is_empty());
        for invalid in [
            json!([" "]),
            json!(["界".repeat(301)]),
            json!(vec!["Cite lines"; 9]),
            json!("Cite lines"),
            json!([false]),
        ] {
            let mut value = original.clone();
            value["presentation_requirements"] = invalid;
            assert!(validate_plan(value).is_err());
        }
        let mut boundary = original;
        boundary["presentation_requirements"] = json!(vec!["界".repeat(300); 8]);
        assert_eq!(
            validate_plan(boundary)
                .unwrap()
                .presentation_requirements
                .len(),
            8
        );
    }
    #[test]
    fn intent_owns_source_requirement_without_conflating_reports_and_proof() {
        let intents = vec![
            Intent::OriginalRequirement,
            Intent::ReportedState,
            Intent::VerificationStatus,
            Intent::IndependentProof,
            Intent::FactualQuestion,
        ];
        let plan = Plan {
            aspects: vec!["question".into(); intents.len()],
            intents,
            presentation_requirements: vec![],
        };
        assert_eq!(
            plan.source_requirements(),
            ["user_document", "any", "any", "any", "any"]
        );
        for invalid in [
            json!({"aspects":["Memory reports?"],"source_requirements":["any"]}),
            json!({"aspects":["Memory reports?"]}),
            json!({"aspects":["Memory reports?"],"intents":["unknown"]}),
        ] {
            assert!(serde_json::from_value::<Plan>(invalid).is_err());
        }
    }

    #[test]
    fn requirement_cannot_be_closed_by_memory_or_mixed_evidence() {
        let index: Index=serde_json::from_value(json!({"format":1,"revision":"r","links":[],
            "sources":[{"id":"doc","path":"docs/ui.md","revision":"r","authority":"user_document","text":"Blue","title":"UI","agent":"cheap","parent":null},
                       {"id":"memo","path":"memory.json","revision":"r","authority":"advisory_memory","text":"Reported green","title":"History","agent":"cheap","parent":null}],
            "threads":[{"id":"doc","source":"doc","title":"UI","parent":null,"agent":"cheap","fragments":[{"id":"d","source":"doc","line":1,"text":"Blue"}],"passport":{"summary":"","questions":[],"terms":[],"subtree_terms":[],"checked_candidates":[]}},
                       {"id":"memo","source":"memo","title":"History","parent":null,"agent":"cheap","fragments":[{"id":"m","source":"memo","line":1,"text":"Reported green"}],"passport":{"summary":"","questions":[],"terms":[],"subtree_terms":[],"checked_candidates":[]}}]})).unwrap();
        for (ids, required, status) in [
            (vec!["m"], "user_document", "missing"),
            (vec!["d", "m"], "user_document", "missing"),
            (vec!["d"], "user_document", "found"),
            (vec!["m"], "any", "found"),
        ] {
            let mut a:Assembly=serde_json::from_value(json!({"answer":"Green","select":ids,"need":[],"conflicts":[],"aspects":[{"question":"Color?","answer":"Green","status":"found","evidence":ids}]})).unwrap();
            enforce_authority(&mut a, &index, &[required.into()]);
            assert_eq!(a.aspects[0].status, status);
            assert_eq!(a.aspects[0].evidence, ids);
            if status == "missing" {
                assert!(a.aspects[0].answer.contains("not established"));
            }
        }
    }

    #[test]
    fn extras_cannot_expand_scope_and_missing_requested_facts_stay_missing() {
        let mut a = Assembly {
            answer: String::new(),
            excerpts: vec![],
            audit_error: None,
            select: vec!["line".into()],
            aspects: vec![
                Aspect {
                    self_contained: false,
                    assessed: false,
                    answer: String::new(),
                    question: "Color".into(),
                    status: "found".into(),
                    evidence: vec!["line".into()],
                },
                Aspect {
                    self_contained: false,
                    assessed: false,
                    answer: String::new(),
                    question: "Prove no other exceptions".into(),
                    status: "missing".into(),
                    evidence: vec![],
                },
            ],
            need: vec![],
            conflicts: vec![],
        };
        enforce(&mut a, &["Color".into(), "Accessibility".into()]);
        assert_eq!(a.aspects.len(), 2);
        assert_eq!(a.aspects[0].status, "found");
        assert_eq!(a.aspects[1].question, "Accessibility");
        assert_eq!(a.aspects[1].status, "missing");
    }
}
