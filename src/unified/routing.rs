//! Recursive routing contracts and bottom-up replies. Passports never become evidence.
use super::*;

/// Route a changed question locally when it introduces terms absent from cached excerpts.
/// This selects owners only: their agents must still read them and verification must run.
pub(super) fn missing_followup_roots(
    index: &Index,
    question: &str,
    candidates: &[String],
    selections: &BTreeMap<String, worker::Selection>,
    limit: usize,
    aspects: &[String],
    intents: &[scope::Intent],
) -> BTreeSet<String> {
    let selected: BTreeSet<_> = selections.values().flat_map(|s| s.select.iter()).collect();
    let fragments = index.fragments();
    let cached_terms: BTreeSet<_> = selections
        .values()
        .flat_map(|s| s.select.iter())
        .filter_map(|id| fragments.get(id))
        .flat_map(|f| index::terms(&f.text))
        .collect();
    let query = index::terms(question);
    let novel: BTreeSet<_> = query.difference(&cached_terms).cloned().collect();
    let novel_candidates: Vec<_> = candidates
        .iter()
        .filter(|id| {
            index.thread(id).is_some_and(|t| {
                t.fragments.iter().any(|f| {
                    !selected.contains(&f.id) && !index::terms(&f.text).is_disjoint(&novel)
                })
            })
        })
        .take(limit)
        .cloned()
        .collect();
    let mut roots = super::root_selection::choose(
        question,
        index,
        &novel_candidates,
        aspects,
        intents,
        limit.min(3),
        false,
    );
    if roots.is_empty() {
        // A planner can paraphrase away the literal indexed vocabulary. Retry
        // the user's own words without relaxing documentary source eligibility.
        let intent = if !intents.is_empty()
            && intents
                .iter()
                .all(|intent| *intent == scope::Intent::OriginalRequirement)
        {
            scope::Intent::OriginalRequirement
        } else {
            scope::Intent::FactualQuestion
        };
        roots = super::root_selection::choose(
            question,
            index,
            &novel_candidates,
            &[question.into()],
            &[intent],
            limit.min(3),
            false,
        );
    }
    roots.into_iter().collect()
}

/// Search only the accessible passport catalog. Descendant terms route to their
/// immediate owner; original child content is never sent to the caller here.
pub(super) fn search_passports(
    index: &Index,
    ids: &[String],
    question: &str,
    limit: usize,
) -> Vec<String> {
    let query = index::terms(question);
    let constraints = index::terms("exception exceptions constraint constraints conflict conflicts disabled never unless override исключение исключения запрет");
    let mut catalog: Vec<_> = ids
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|id| index.thread(id))
        .map(|t| {
            let mut terms = t.passport.subtree_terms.clone();
            terms.extend(index::terms(&t.title));
            (t, terms)
        })
        .collect();
    let frequency: BTreeMap<_, _> = query
        .iter()
        .map(|q| {
            (
                q,
                catalog
                    .iter()
                    .filter(|(_, terms)| terms.contains(q))
                    .count(),
            )
        })
        .collect();
    let mut ranked: Vec<_> = catalog
        .drain(..)
        .map(|(t, terms)| {
            // Rare task words outrank common boilerplate. Keep exception branches
            // discoverable even when their wording differs from the question.
            let score: usize = query
                .intersection(&terms)
                .map(|q| 1000 / frequency[q].max(1))
                .sum();
            let safeguard = !terms.is_disjoint(&constraints);
            (t.id.clone(), score, safeguard)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut selected: Vec<String> = ranked.iter().take(limit).map(|r| r.0.clone()).collect();
    // Reserve a small portion for constraints instead of allowing a large set
    // of literal matches to hide an exceptional rule.
    let reserve = (limit / 4).max(1).min(limit);
    for (id, _, _) in ranked.iter().filter(|r| r.2).take(reserve) {
        if !selected.contains(id) && !selected.is_empty() {
            if let Some(pos) = selected
                .iter()
                .rposition(|s| !ranked.iter().any(|r| &r.0 == s && r.2))
            {
                selected[pos] = id.clone();
            }
        }
    }
    selected
}

pub(super) fn targets(index: &Index, t: &Thread) -> Vec<String> {
    let mut ids: BTreeSet<String> = index
        .threads
        .iter()
        .filter(|n| n.parent.as_ref() == Some(&t.id))
        .map(|n| n.id.clone())
        .collect();
    // Search may start at a section rather than its document root. Its sibling
    // headings must remain discoverable even when they use different vocabulary.
    // Expose passports only; sibling originals still require their own worker.
    if t.parent.is_some()
        && index
            .sources
            .iter()
            .any(|s| s.id == t.source && s.authority == "user_document")
    {
        ids.extend(
            index
                .threads
                .iter()
                .filter(|n| n.source == t.source && n.parent == t.parent)
                .map(|n| n.id.clone()),
        );
    }
    for link in &index.links {
        if link.from == t.id {
            ids.insert(link.to.clone());
        }
        if link.to == t.id {
            ids.insert(link.from.clone());
        }
    }
    // Authored relative Markdown links are discovery edges, not verified facts.
    // Include introductory links inherited from containing document sections.
    if let Some(source) = index
        .sources
        .iter()
        .find(|s| s.id == t.source && s.authority == "user_document")
    {
        let mut owner = Some(t);
        let mut visited = BTreeSet::new();
        while let Some(node) =
            owner.filter(|n| n.source == t.source && visited.insert(n.id.clone()))
        {
            for fragment in &node.fragments {
                for path in document_links(&source.path, &fragment.text) {
                    ids.extend(
                        index
                            .sources
                            .iter()
                            .filter(|s| s.authority == "user_document" && s.path == path)
                            .map(|s| s.id.clone()),
                    );
                }
            }
            owner = node.parent.as_ref().and_then(|id| index.thread(id));
        }
    }
    // Never delegate back to an ancestor; its own evidence is already in the traversal.
    let mut ancestor = Some(t.id.clone());
    let mut seen = BTreeSet::new();
    while let Some(id) = ancestor {
        if !seen.insert(id.clone()) {
            break;
        }
        ids.remove(&id);
        ancestor = index.thread(&id).and_then(|n| n.parent.clone());
    }
    ids.into_iter().collect()
}

fn document_links(source: &str, text: &str) -> Vec<String> {
    static LINKS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern =
        LINKS.get_or_init(|| regex::Regex::new(r#"\]\(<?([^\s<>\)]+)>?(?:\s+[^\)]*)?\)"#).unwrap());
    pattern
        .captures_iter(text)
        .filter_map(|capture| {
            let target = capture[1].split('#').next()?.split('?').next()?;
            if target.is_empty()
                || target.contains(':')
                || target.starts_with('/')
                || target.contains('\\')
            {
                return None;
            }
            let mut parts: Vec<_> = source.split('/').collect();
            parts.pop();
            for part in target.split('/') {
                match part {
                    "" | "." => {}
                    ".." => {
                        parts.pop()?;
                    }
                    _ => parts.push(part),
                }
            }
            Some(parts.join("/"))
        })
        .collect()
}

pub(super) fn passports(index: &Index, ids: &[String], question: &str) -> Vec<Value> {
    let query = index::terms(question);
    ids.iter().filter_map(|id|index.thread(id)).map(|t| {
        let own_preview: String = t.fragments.iter().map(|f|f.text.as_str()).collect::<Vec<_>>().join(" ").chars().take(1200).collect();
        let matching: Vec<_> = t.passport.subtree_terms.intersection(&query).collect();
        let topics: Vec<_> = t.passport.subtree_terms.iter().filter(|s|s.chars().any(char::is_alphabetic)).take(48).collect();
        let safeguards: Vec<_> = t.passport.subtree_terms.iter().filter(|s|["exception","exceptions","conflict","constraint","constraints","disabled","never","must","unless","override","исключение","исключения","запрет"].contains(&s.as_str())).collect();
        json!({"id":t.id,"title":t.title,"summary":t.passport.summary,"questions":t.passport.questions,
            "own_preview":own_preview,"subtree_topics":topics,"query_matches":matching,"constraint_topics":safeguards,
            "child_count":index.threads.iter().filter(|n|n.parent.as_ref()==Some(&t.id)).count(),
            "authority":index.sources.iter().find(|s|s.id==t.source).map(|s|&s.authority),
            "role":"Routing hints only. Own preview is incomplete; consult the owner for evidence."})
    }).collect()
}

pub(super) fn would_cycle(
    from: &str,
    to: &str,
    selections: &BTreeMap<String, worker::Selection>,
) -> bool {
    let mut pending = vec![to.to_string()];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if id == from {
            return true;
        }
        if seen.insert(id.clone()) {
            if let Some(s) = selections.get(&id) {
                pending.extend(s.need.iter().cloned());
            }
        }
    }
    false
}

fn passthrough(selection: &worker::Selection, child: &worker::Branch) -> bool {
    selection.need.len() == 1
        && selection.select.is_empty()
        && selection.summary.trim().is_empty()
        && selection.gaps.is_empty()
        && selection.host_notes.is_empty()
        && selection.elements.is_empty()
        && selection.links.is_empty()
        && child.gaps.is_empty()
        && !child.evidence.is_empty()
        && !child.summary.trim().is_empty()
        && child.summary.chars().count() <= 1600
}

/// Return original fragment IDs upward; the parent agent sees child answers, never
/// the originals of unopened child branches. A failed child remains an explicit gap.
#[allow(clippy::too_many_arguments)]
pub(super) fn fold(
    project: &Project,
    index: &Index,
    id: &str,
    question: &str,
    selections: &mut BTreeMap<String, worker::Selection>,
    completed: &mut BTreeSet<String>,
    active: &mut BTreeSet<String>,
    calls: &mut usize,
    deadline: Instant,
) -> Result<worker::Branch> {
    if active.len() >= 64 || !active.insert(id.into()) {
        return Err(AppError::new("recursive reply cycle or depth limit"));
    }
    let selection = selections
        .get(id)
        .cloned()
        .ok_or_else(|| AppError::new(format!("missing child reply: {id}")))?;
    if completed.contains(id) {
        active.remove(id);
        return Ok(selection.branch.unwrap());
    }
    let mut children = Vec::new();
    let mut single_reply = None;
    let mut inherited_links = Vec::new();
    let mut allowed: BTreeSet<String> = selection.select.iter().cloned().collect();
    let mut gaps = selection.gaps.clone();
    for child in &selection.need {
        let reply = fold(
            project, index, child, question, selections, completed, active, calls, deadline,
        )?;
        inherited_links.extend(reply.links.iter().cloned());
        allowed.extend(reply.evidence.iter().cloned());
        gaps.extend(reply.gaps.iter().cloned());
        if passthrough(&selection, &reply) {
            single_reply = Some(reply.clone());
        }
        children.push(json!({"thread":child,"reply":reply}));
    }
    let reply = if children.is_empty() {
        worker::Branch {
            links: vec![],
            summary: selection.summary,
            evidence: selection.select,
            gaps,
        }
    } else if let Some(reply) = single_reply {
        // Global verification and grounding still run. No new completeness
        // claim is made here; preserve the child's result without a rewrite.
        crate::statistics::event(
            "single_child_reply_reused",
            json!({"thread":id,"child":selection.need[0]}),
        );
        reply
    } else {
        if *calls + 2 >= project.config.memory.max_steps || Instant::now() >= deadline {
            return Err(AppError::new("recursive reply budget exhausted"));
        }
        *calls += 1;
        let t = index.thread(id).unwrap();
        let all = index.fragments();
        let originals: Vec<_> = allowed.iter().filter_map(|id|all.get(id)).map(|f|json!({"id":f.id,"text":f.text,"source":f.source,"owner_thread":index.threads.iter().find(|t|t.fragments.iter().any(|n|n.id==f.id)).map(|t|&t.id)})).collect();
        let owners: BTreeSet<_> = originals
            .iter()
            .filter_map(|v| v["owner_thread"].as_str())
            .collect();
        let mut schema = json!({"type":"object","additionalProperties":false,"required":["summary","evidence","gaps"],"properties":{
            "summary":{"type":"string","maxLength":1600,"description":"Target 1200 characters; 1600 is the tolerance limit."},"evidence":worker::strings(),"gaps":worker::strings()}});
        schema["properties"]["links"] = json!({"type":"array","items":{"type":"object","additionalProperties":false,"required":["from","target","kind","evidence"],"properties":{"from":{"type":"string"},"target":{"type":"string"},"kind":{"type":"string","enum":["applies_to","clarifies","exception_to","depends_on","contradicts"]},"evidence":worker::strings()}}});
        if owners.len() < 2 {
            schema["properties"]["links"]["maxItems"] = json!(0);
        } else {
            for field in ["from", "target"] {
                schema["properties"]["links"]["items"]["properties"][field]["enum"] = json!(owners);
            }
        }
        schema["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("links"));
        if allowed.is_empty() {
            schema["properties"]["evidence"]["maxItems"] = json!(0);
        } else {
            schema["properties"]["evidence"]["items"]["enum"] = json!(allowed);
        }
        let mut payload = json!({
            "task_instructions":"You are the parent thread agent receiving child replies. Combine your own findings and these replies into a concise answer to the ORIGINAL question. Preserve conditions, exceptions, disagreements and unresolved gaps. Child replies are advisory: originals are the evidence. Cite only supplied original fragment IDs. Never claim unopened branches were read. Optionally confirm links between consulted thread owners using supplied ORIGINAL evidence from both endpoints; never infer links solely from passports. Use thread IDs for from/target. Do not decide global completeness here. Keep every supplied evidence ID and unresolved child gap; compression must not discard them.",
            "question":question,"thread":{"id":id,"title":t.title},"own":selection.select,"children":children,"originals":originals
        });
        let refs = super::references::References::new(allowed.iter().cloned());
        payload["reference_rules"] = json!("Evidence IDs are short local strings for THIS call. Copy only supplied IDs. Do not reconstruct document hashes or line addresses. Link endpoint thread IDs are unchanged.");
        refs.encode(&mut schema);
        let mut attempt = 0;
        let mut reply = loop {
            refs.encode(&mut payload);
            let mut raw = worker::call(
                project,
                &t.agent,
                "unified_reply",
                payload.clone(),
                schema.clone(),
                deadline,
            )?;
            refs.decode(&mut raw);
            let parsed = serde_json::from_value::<worker::Branch>(raw);
            if let Ok(reply) = parsed {
                if reply.gaps.len() <= 32
                    && reply.gaps.iter().all(|g| g.chars().count() <= 500)
                    && reply.evidence.iter().all(|id| allowed.contains(id))
                {
                    break reply;
                }
            }
            let remaining_parents = selections
                .iter()
                .filter(|(other, s)| {
                    other.as_str() != id && !s.need.is_empty() && !completed.contains(*other)
                })
                .count();
            crate::feedback::event(
                "cm_action_finished",
                json!({"action":"recursive_reply_validation","thread":id,"status":"error","error":"Invalid parent reply references or limits.","error_already_counted":false}),
            );
            // One correction, reserving remaining parents, verification and grounding.
            if attempt != 0
                || *calls + 2 + remaining_parents >= project.config.memory.max_steps
                || deadline
                    .checked_sub(Duration::from_secs(5))
                    .is_none_or(|d| Instant::now() >= d)
            {
                return Err(AppError::new("invalid recursive parent reply"));
            }
            attempt += 1;
            *calls += 1;
            payload["repair"] = json!({"error":"Previous reply had invalid references, shape or gap limits. Regenerate from originals. Copy evidence IDs exactly from allowed_evidence; at most 32 gaps, each at most 500 characters.","allowed_evidence":allowed});
        };
        if reply.summary.chars().count() > 1600 {
            let mut shortened = None;
            // Optional work must leave every remaining parent, final verification
            // and grounding a call, including siblings outside the current stack.
            let remaining_parents = selections
                .iter()
                .filter(|(other, s)| {
                    other.as_str() != id && !s.need.is_empty() && !completed.contains(*other)
                })
                .count();
            let shortening_deadline = deadline.checked_sub(Duration::from_secs(5));
            if *calls + 2 + remaining_parents < project.config.memory.max_steps
                && shortening_deadline.is_some_and(|d| Instant::now() < d)
            {
                *calls += 1;
                // Compression never changes evidence, links or unresolved gaps.
                if let Ok(value) = worker::call(
                    project,
                    &t.agent,
                    "unified_shorten",
                    json!({"task_instructions":"Shorten the supplied summary to at most 1200 characters without adding facts. Preserve important conditions, exceptions and uncertainty. Evidence and gaps are retained separately by the host. Return only summary.","summary":reply.summary,"question":question}),
                    json!({"type":"object","additionalProperties":false,"required":["summary"],"properties":{"summary":{"type":"string","maxLength":1200}}}),
                    shortening_deadline
                        .unwrap()
                        .min(Instant::now() + Duration::from_secs(15)),
                ) {
                    shortened = value["summary"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 1600)
                        .map(str::to_owned);
                }
            }
            reply.summary = shortened.unwrap_or_else(|| "Summary compression unavailable; use the preserved original evidence and child findings.".into());
        }
        let proposed = reply.links.len();
        reply.links.retain(|link| {
            [
                "applies_to",
                "clarifies",
                "exception_to",
                "depends_on",
                "contradicts",
            ]
            .contains(&link.kind.as_str())
                && link.from != link.target
                && link.evidence.iter().all(|id| allowed.contains(id))
                && [&link.from, &link.target].iter().all(|id| {
                    index
                        .thread(id)
                        .is_some_and(|t| t.fragments.iter().any(|f| link.evidence.contains(&f.id)))
                })
        });
        reply.links.truncate(32);
        if proposed != reply.links.len() {
            crate::feedback::event(
                "cm_action_finished",
                json!({"action":"recursive_link_validation","thread":id,"status":"error","error":"Unsupported optional relations discarded; original evidence retained.","error_already_counted":false}),
            );
        }
        reply.links.extend(inherited_links);
        // The host preserves evidence and uncertainty even if compression drops them.
        reply.evidence = allowed.into_iter().collect();
        reply.gaps.extend(gaps);
        reply.gaps.sort();
        reply.gaps.dedup();
        reply
    };
    active.remove(id);
    selections.get_mut(id).unwrap().branch = Some(reply.clone());
    completed.insert(id.into());
    Ok(reply)
}

#[cfg(test)]
mod passthrough_tests {
    use super::*;

    #[test]
    fn document_link_paths_are_local_and_resolve_relative_to_source() {
        assert_eq!(
            document_links(
                "memory/docs/governance/policy.md",
                r#"[a](./guide/workflow.md#finish) [b](../checks.md "Checks") [c](<guide/README.md>)"#
            ),
            [
                "memory/docs/governance/guide/workflow.md",
                "memory/docs/checks.md",
                "memory/docs/governance/guide/README.md"
            ]
        );
        assert!(document_links("memory/docs/a.md", "[a](https://example.com/doc.md) [b](#section) [c](/root.md) [d](../../../../outside.md)").is_empty());
    }

    #[test]
    fn only_empty_router_and_gapless_evidenced_child_can_bypass_synthesis() {
        let s: worker::Selection = serde_json::from_value(json!({"select":[],"elements":[],"summary":"","questions":[],"links":[],"checked":[],"need":["child"],"gaps":[]})).unwrap();
        let child: worker::Branch = serde_json::from_value(
            json!({"summary":"Exact rule","evidence":["id"],"gaps":[],"links":[]}),
        )
        .unwrap();
        assert!(passthrough(&s, &child));
        for field in ["select", "need", "gaps", "host_notes"] {
            let mut value = serde_json::to_value(&s).unwrap();
            value[field].as_array_mut().unwrap().push(json!("extra"));
            assert!(!passthrough(
                &serde_json::from_value(value).unwrap(),
                &child
            ));
        }
        let mut own = s.clone();
        own.summary = "An exception applies".into();
        assert!(!passthrough(&own, &child));
        let mut incomplete = child.clone();
        incomplete.gaps.push("Unopened exception".into());
        assert!(!passthrough(&s, &incomplete));
        incomplete.gaps.clear();
        incomplete.evidence.clear();
        assert!(!passthrough(&s, &incomplete));
    }
}
