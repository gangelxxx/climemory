//! Bounded, local aspect coverage before any thread agent is scheduled.
use super::*;

pub(super) fn choose(
    question: &str,
    index: &Index,
    candidates: &[String],
    aspects: &[String],
    intents: &[scope::Intent],
    limit: usize,
    initial: bool,
) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let catalog: Vec<_> = candidates
        .iter()
        .filter_map(|id| {
            let thread = index.thread(id)?;
            let mut terms = index::terms(&thread.title);
            for fragment in &thread.fragments {
                terms.extend(index::terms(&fragment.text));
            }
            let documentary = index
                .sources
                .iter()
                .any(|s| s.id == thread.source && s.authority == "user_document");
            Some((id, terms, documentary))
        })
        .collect();
    let named = named_candidate(index, candidates, question);
    let named_title = named
        .and_then(|id| index.thread(id))
        .map(|t| words(&t.title));
    let reported_are_named = named_title.as_ref().is_some_and(|title| {
        let reports: Vec<_> = aspects
            .iter()
            .zip(intents)
            .filter(|(_, i)| **i == scope::Intent::ReportedState)
            .collect();
        !reports.is_empty()
            && reports
                .iter()
                .all(|(aspect, _)| contains_phrase(&words(aspect), title))
    });
    let mut chosen = Vec::new();
    let mut coverage = Vec::new();
    let aspect_terms: Vec<_> = aspects.iter().map(|aspect| index::terms(aspect)).collect();
    for (aspect_number, (query, intent)) in aspect_terms.iter().zip(intents).enumerate() {
        // Explicit source identity outranks incidental vocabulary in unrelated
        // memories. This only seeds retrieval; verification can expand it.
        let scoped = named.filter(|_| {
            let title = named_title.as_ref().unwrap();
            let aspect = words(&aspects[aspect_number]);
            match intent {
                scope::Intent::ReportedState => contains_phrase(&aspect, title),
                scope::Intent::VerificationStatus => {
                    contains_phrase(&aspect, title)
                        || (reported_are_named
                            && [
                                "these claims",
                                "these reported claims",
                                "these reports",
                                "these facts",
                                "those claims",
                                "those reported claims",
                                "those reports",
                                "those facts",
                            ]
                            .iter()
                            .any(|phrase| contains_phrase(&aspect, &words(phrase))))
                }
                _ => false,
            }
        });
        let mut best = scoped.cloned();
        let mut best_score = 0;
        for (id, terms, documentary) in &catalog {
            if scoped.is_some() {
                break;
            }
            if *intent == scope::Intent::OriginalRequirement && !documentary {
                continue;
            }
            let score: usize = query
                .intersection(terms)
                .map(|term| {
                    1000 / (catalog
                        .iter()
                        .filter(|(_, terms, _)| terms.contains(term))
                        .count()
                        .max(1)
                        * aspect_terms
                            .iter()
                            .filter(|terms| terms.contains(term))
                            .count()
                            .max(1))
                })
                .sum();
            // Equal scores retain the configured backend's ranking.
            if score > best_score {
                best = Some((*id).clone());
                best_score = score;
            }
        }
        coverage.push(json!({"aspect":aspect_number,"thread":best}));
        if let Some(id) = best {
            if !chosen.contains(&id) {
                chosen.push(id);
            }
        }
        if chosen.len() == limit {
            break;
        }
    }
    // Do not schedule speculative siblings when aspect winners already exist.
    // A single backend-ranked fallback preserves discovery when planner wording
    // has no lexical overlap; the verifier can still request other candidates.
    if initial && chosen.is_empty() {
        let documents_only = !intents.is_empty()
            && intents
                .iter()
                .all(|intent| *intent == scope::Intent::OriginalRequirement);
        for (id, _, documentary) in catalog {
            if (!documents_only || documentary) && !chosen.contains(id) {
                chosen.push(id.clone());
                break;
            }
        }
    }
    crate::statistics::event(
        "aspect_root_selection",
        json!({"initial":initial,"named_candidate":named,"candidates":candidates.len(),"aspect_roots":coverage,"selected":chosen,"limit":limit}),
    );
    chosen
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn contains_phrase(text: &[String], phrase: &[String]) -> bool {
    !phrase.is_empty() && text.windows(phrase.len()).any(|part| part == phrase)
}

// Match whole normalized title phrases, without stemming or unordered term
// matching. Inspect the full index so a second named source outside the selected
// candidates still prevents accidental single-source prioritization.
fn named_candidate<'a>(
    index: &Index,
    candidates: &'a [String],
    question: &str,
) -> Option<&'a String> {
    // Strip only an explicit topic marker, never arbitrary introductory prose
    // (which could negate or contrast the attribution that follows).
    let trimmed = question.trim();
    let scoped_question = trimmed
        .split_once(':')
        .filter(|(marker, _)| {
            ["new topic", "новая тема", "新话题"].contains(&marker.trim().to_lowercase().as_str())
        })
        .map_or(trimmed, |(_, rest)| rest.trim());
    let query = words(scoped_question);
    let mut matches = index.threads.iter().filter(|thread| {
        let title = words(&thread.title);
        // Single generic words are too weak to establish explicit source scope.
        title.len() >= 2 && query.windows(title.len()).any(|part| part == title)
    });
    let named = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    // Mere mentions and comparisons do not establish attribution. Other
    // languages and formulations retain normal ranked routing.
    let mut prefix = words("according to");
    prefix.extend(words(&named.title));
    if !query.starts_with(&prefix) {
        return None;
    }
    candidates.iter().find(|id| **id == named.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        let mut index = Index::default();
        for (id, documentary, text) in [
            (
                "memory",
                false,
                "Keyboard focus typography implementation reported green",
            ),
            ("parent", true, "User requirements for future UI work"),
            (
                "access",
                true,
                "Keyboard operation and visible focus are required",
            ),
            ("font", true, "Typography uses system sans-serif"),
            ("russian", true, "Кнопка синяя. Клавиатура доступна."),
            ("chinese", true, "按钮颜色为绿色。"),
        ] {
            index.sources.push(serde_json::from_value(json!({"id":id,"path":id,"revision":"r","authority":if documentary {"user_document"} else {"advisory_memory"},"text":text,"title":id,"agent":"cheap","parent":null})).unwrap());
            index.threads.push(serde_json::from_value(json!({"id":id,"source":id,"title":id,"parent":null,"agent":"cheap","fragments":[{"id":format!("{id}:L1"),"source":id,"line":1,"text":text}],"passport":{"summary":"","questions":[],"terms":[],"subtree_terms":[],"checked_candidates":[]}})).unwrap());
        }
        index
    }

    #[test]
    fn followup_covers_document_aspects_without_generic_or_advisory_roots() {
        let index = index();
        let candidates = ["memory", "parent", "access", "font"].map(str::to_owned);
        assert_eq!(
            choose(
                "",
                &index,
                &candidates,
                &[
                    "Keyboard requirements?".into(),
                    "Focus requirements?".into(),
                    "Typography requirements?".into()
                ],
                &[scope::Intent::OriginalRequirement; 3],
                3,
                false
            ),
            ["access", "font"]
        );
    }

    #[test]
    fn mixed_scope_retains_reports_and_never_injects_unlisted_candidates() {
        let index = index();
        let candidates = ["memory", "access", "font"].map(str::to_owned);
        let aspects = [
            "Keyboard required?".into(),
            "What implementation was reported?".into(),
        ];
        let intents = [
            scope::Intent::OriginalRequirement,
            scope::Intent::ReportedState,
        ];
        assert_eq!(
            choose("", &index, &candidates, &aspects, &intents, 3, false),
            ["access", "memory"]
        );
        assert_eq!(
            choose(
                "",
                &index,
                &candidates[..1],
                &aspects[..1],
                &intents[..1],
                3,
                true
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            choose("", &index, &candidates, &aspects, &intents, 1, true),
            ["access"]
        );
        assert!(choose("", &index, &candidates, &aspects, &intents, 0, true).is_empty());
    }

    #[test]
    fn initial_aspect_winner_does_not_fill_with_unneeded_siblings() {
        let index = index();
        let candidates = ["memory", "parent", "access", "font"].map(str::to_owned);
        assert_eq!(
            choose(
                "",
                &index,
                &candidates,
                &["Typography?".into()],
                &[scope::Intent::OriginalRequirement],
                3,
                true
            ),
            ["font"]
        );
        assert_eq!(
            choose(
                "",
                &index,
                &candidates,
                &["Keyboard?".into(), "Typography?".into()],
                &[scope::Intent::OriginalRequirement; 2],
                3,
                true
            ),
            ["access", "font"]
        );
    }

    #[test]
    fn no_overlap_uses_one_ranked_eligible_fallback_only_for_initial_search() {
        let index = index();
        let candidates = ["memory", "access", "font"].map(str::to_owned);
        for (intent, expected) in [
            (scope::Intent::OriginalRequirement, "access"),
            (scope::Intent::ReportedState, "memory"),
        ] {
            assert_eq!(
                choose(
                    "",
                    &index,
                    &candidates,
                    &["Unmatched vocabulary?".into()],
                    &[intent],
                    3,
                    true
                ),
                [expected]
            );
            assert!(choose(
                "",
                &index,
                &candidates,
                &["Unmatched vocabulary?".into()],
                &[intent],
                3,
                false
            )
            .is_empty());
        }
        assert!(choose(
            "",
            &index,
            &[],
            &["Unmatched?".into()],
            &[scope::Intent::ReportedState],
            3,
            true
        )
        .is_empty());
    }

    #[test]
    fn unicode_questions_use_the_same_local_tokenizer() {
        let index = index();
        let candidates = ["memory", "russian", "chinese"].map(str::to_owned);
        for (question, expected) in [
            ("Клавиатура доступна?", "russian"),
            ("按钮颜色为绿色。", "chinese"),
        ] {
            assert_eq!(
                choose(
                    "",
                    &index,
                    &candidates,
                    &[question.into()],
                    &[scope::Intent::OriginalRequirement],
                    3,
                    false
                ),
                [expected]
            );
        }
    }
    fn named_index() -> Index {
        let mut index = index();
        index
            .threads
            .iter_mut()
            .find(|t| t.id == "memory")
            .unwrap()
            .title = "Binary calculations".into();
        index
            .threads
            .iter_mut()
            .find(|t| t.id == "access")
            .unwrap()
            .title = "Access policy".into();
        index
    }

    #[test]
    fn topic_markers_preserve_explicit_attribution_without_accepting_prose() {
        let index = named_index();
        let candidates = ["access", "memory"].map(str::to_owned);
        for marker in ["New topic", " NEW TOPIC ", "Новая тема", "新话题"] {
            assert_eq!(
                named_candidate(
                    &index,
                    &candidates,
                    &format!(
                        "{marker}: According to Binary-calculations memory, what is reported?"
                    )
                ),
                Some(&candidates[1])
            );
        }
        for query in [
            "Do not answer: According to Binary calculations memory",
            "New topic is not according to Binary calculations memory",
            "New topic: Unlike Binary calculations memory",
            "New topic: According to Binary calculations and Access policy",
        ] {
            assert!(named_candidate(&index, &candidates, query).is_none());
        }
    }

    #[test]
    fn explicit_hyphenated_source_seeds_report_and_verification_without_filtering_catalog() {
        let index = named_index();
        let candidates = ["access", "font", "memory"].map(str::to_owned);
        let aspects = [
            "What keyboard requirements does Binary calculations report?".into(),
            "What verification is recorded for these reported claims?".into(),
        ];
        assert_eq!(choose("According to BINARY-calculations memory, what keyboard requirements and verification are reported?", &index, &candidates, &aspects,
            &[scope::Intent::ReportedState, scope::Intent::VerificationStatus], 3, true), ["memory"]);
        // Authoritative requirements retain their ordinary routing even when the
        // request also names an advisory memory source.
        assert_eq!(
            choose(
                "Compare Binary calculations memory with typography requirements",
                &index,
                &candidates,
                &["Typography requirements?".into()],
                &[scope::Intent::OriginalRequirement],
                3,
                true
            ),
            ["font"]
        );
        assert_eq!(candidates, ["access", "font", "memory"]);
    }

    #[test]
    fn multiple_explicit_sources_fall_back_even_when_one_is_not_a_candidate() {
        let index = named_index();
        for candidates in [
            vec!["access".into(), "font".into(), "memory".into()],
            vec!["font".into(), "memory".into()],
        ] {
            let aspects = ["Typography?".into()];
            let intents = [scope::Intent::ReportedState];
            assert_eq!(
                choose(
                    "According to Binary calculations and Access policy",
                    &index,
                    &candidates,
                    &aspects,
                    &intents,
                    3,
                    true
                ),
                choose("", &index, &candidates, &aspects, &intents, 3, true)
            );
        }
    }

    #[test]
    fn unknown_partial_or_unavailable_names_never_add_candidates() {
        let index = named_index();
        for question in [
            "Unknown memory",
            "Binary calculation",
            "Nonbinary calculations",
            "According to Binary calculations",
        ] {
            let candidates = ["font", "access"].map(str::to_owned);
            assert_eq!(
                choose(
                    question,
                    &index,
                    &candidates,
                    &["Typography?".into()],
                    &[scope::Intent::ReportedState],
                    3,
                    true
                ),
                ["font"]
            );
        }
        assert!(named_candidate(&index, &["memory".into()], "Nonbinary calculations").is_none());
        assert!(named_candidate(&index, &["memory".into()], "calculations Binary").is_none());
    }

    #[test]
    fn ambiguous_duplicate_titles_do_not_establish_scope() {
        let mut index = named_index();
        index
            .threads
            .iter_mut()
            .find(|t| t.id == "font")
            .unwrap()
            .title = "Binary calculations".into();
        assert!(named_candidate(
            &index,
            &["memory".into()],
            "According to Binary calculations"
        )
        .is_none());
    }
    #[test]
    fn mention_comparison_and_unrelated_aspects_retain_ranked_routing() {
        let index = named_index();
        let candidates = ["font", "access", "memory"].map(str::to_owned);
        for question in [
            "Unlike Binary calculations, what typography is reported for settings?",
            "Compare Binary calculations with typography",
            "What is Binary calculations?",
        ] {
            assert_eq!(
                choose(
                    question,
                    &index,
                    &candidates,
                    &["Typography?".into()],
                    &[scope::Intent::ReportedState],
                    3,
                    true
                ),
                ["font"]
            );
        }
        let question = "According to Binary calculations memory, what is reported? Also what typography verification is recorded for settings?";
        assert_eq!(
            choose(
                question,
                &index,
                &candidates,
                &[
                    "What does Binary calculations report?".into(),
                    "What typography verification is recorded for settings?".into()
                ],
                &[
                    scope::Intent::ReportedState,
                    scope::Intent::VerificationStatus
                ],
                3,
                true
            ),
            ["memory", "font"]
        );
        // A generic anaphor is not bound when reported facts span other scopes.
        assert_eq!(
            choose(
                "According to Binary calculations memory, and elsewhere, what is reported?",
                &index,
                &candidates,
                &[
                    "What does Binary calculations report?".into(),
                    "Typography?".into(),
                    "What typography verification covers these reports?".into()
                ],
                &[
                    scope::Intent::ReportedState,
                    scope::Intent::ReportedState,
                    scope::Intent::VerificationStatus
                ],
                3,
                true
            ),
            ["memory", "font"]
        );
    }
}
