//! Compact public context only; cached evidence and verification remain unchanged.
use serde_json::{json, Value};
use std::collections::BTreeMap;
#[path = "compact_wire.rs"]
mod compact_wire;

pub(super) fn deduplicate(output: &mut Value) {
    // For partial answers, found aspects still explain which claims are usable.
    if output["status"] == "complete" {
        if let Some(aspects) = output.get_mut("aspects").and_then(Value::as_array_mut) {
            aspects.retain(|a| {
                a["status"] != "found" || a["answer"].as_str().is_some_and(|s| !s.trim().is_empty())
            });
            if aspects.is_empty() {
                output.as_object_mut().unwrap().remove("aspects");
            }
        }
    }
    let referenced: std::collections::BTreeSet<String> = ["aspects", "conflicts"]
        .iter()
        .filter_map(|field| output[*field].as_array())
        .flatten()
        .filter_map(|row| row["evidence"].as_array())
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let mut addresses = BTreeMap::new();
    if let Some(rows) = output["evidence"].as_array_mut() {
        for (i, row) in rows.iter_mut().enumerate() {
            let Some(source) = row["source"].as_str() else {
                continue;
            };
            let address = if row["authority"] == "user_document" {
                row["line"].as_u64().map(|line| format!("{source}:L{line}"))
            } else {
                row["json_pointer"]
                    .as_str()
                    .zip(row["memory_line"].as_u64())
                    .map(|(pointer, line)| format!("{source}#{pointer}:line{line}"))
            };
            if let Some(address) = address.filter(|a| referenced.contains(a)) {
                let reference = row["ref"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("e{}", i + 1));
                row["ref"] = json!(reference);
                addresses.entry(address).or_insert(reference);
            }
        }
    }
    for field in ["aspects", "conflicts"] {
        if let Some(rows) = output.get_mut(field).and_then(Value::as_array_mut) {
            for row in rows {
                if let Some(refs) = row["evidence"].as_array_mut() {
                    for reference in refs.iter_mut() {
                        if let Some(short) = reference.as_str().and_then(|s| addresses.get(s)) {
                            *reference = json!(short);
                        }
                    }
                    let mut seen = std::collections::BTreeSet::new();
                    refs.retain(|r| seen.insert(r.to_string()));
                }
            }
        }
    }
    if output["answer_complete"].as_bool() == Some(output["status"] == "complete") {
        output.as_object_mut().unwrap().remove("answer_complete");
    }
}

/// Keep selected document citations inline; retain recovery excerpts when claims are unresolved.
pub(super) fn detail_level(output: &mut Value, requested: bool) {
    summary_quotes(output, requested);
    if requested {
        if let Some(rows) = output.get_mut("aspects").and_then(Value::as_array_mut) {
            for row in rows {
                if let Some(object) = row.as_object_mut() {
                    object.remove("self_contained");
                }
            }
        }
    }
    output["response_mode"] = json!("full");
    // Complete per-aspect answers already carry the response. An absent
    // redundant aggregate answer must not disable summary delivery or capsules.
    let complete_aspects = output["status"] == "complete"
        && output["aspects"].as_array().is_some_and(|aspects| {
            !aspects.is_empty()
                && aspects.iter().all(|aspect| {
                    aspect["status"] == "found"
                        && aspect["answer"]
                            .as_str()
                            .is_some_and(|s| !s.trim().is_empty())
                })
        });
    let full = requested
        || output["omitted_evidence"].as_u64().is_some_and(|n| n > 0)
        || output["conflicts"]
            .as_array()
            .is_some_and(|conflicts| conflicts.iter().any(|c| c["unresolved"] == true))
        || (output["status"] != "complete"
            && output["aspects"].as_array().is_none_or(Vec::is_empty))
        || (!complete_aspects
            && output["answer"]
                .as_str()
                .is_none_or(|answer| answer.trim().is_empty()));
    output["detail_level"] = json!(if full { "full" } else { "summary" });
    if !requested {
        let failed = output["errors"].as_array().map_or(0, Vec::len);
        if failed > 0 {
            output["limitations"] = json!([format!(
                "{failed} retrieval operation(s) failed; coverage remains incomplete."
            )]);
        }
        if let Some(object) = output.as_object_mut() {
            for key in ["coverage", "calls_scheduled", "errors", "continue"] {
                object.remove(key);
            }
            if object
                .get("unprocessed_thread_count")
                .and_then(Value::as_u64)
                == Some(0)
            {
                object.remove("unprocessed_thread_count");
                object.remove("unprocessed_threads");
            }
            if object.get("omitted_evidence").and_then(Value::as_u64) == Some(0) {
                object.remove("omitted_evidence");
            }
        }
    }
    if !requested {
        if let Some(id) = output["context_session"].as_str() {
            output["details"] = json!(format!("@context:{id} @details"));
        }
    }
    if !full {
        redundant_topic_headings(output);
        let mut remaining_memory_chars = 2400usize;
        let mut omitted_memory_quotes = 0usize;
        let mut included_memory_quotes = false;
        if let Some(rows) = output.get_mut("evidence").and_then(Value::as_array_mut) {
            for row in rows {
                if let Some(object) = row.as_object_mut() {
                    let authority = object.get("authority").and_then(Value::as_str);
                    if authority != Some("user_document") {
                        let size = object
                            .get("quote")
                            .and_then(Value::as_str)
                            .map(|s| s.chars().count());
                        if matches!(authority, Some("advisory_memory" | "agent_memory"))
                            && size
                                .is_some_and(|n| n > 0 && n <= 600 && n <= remaining_memory_chars)
                        {
                            remaining_memory_chars -= size.unwrap();
                            included_memory_quotes = true;
                        } else if object.remove("quote").is_some() {
                            omitted_memory_quotes += 1;
                        }
                    }
                }
            }
        }
        if included_memory_quotes {
            output["memory_notice"] =
                json!("Memory excerpts are reported claims, not independently verified facts.");
        }
        if omitted_memory_quotes > 0 {
            output["omitted_memory_quotes"] = json!(omitted_memory_quotes);
        }
    }
}

/// The verifier chooses scope-preserving excerpts; the host checks only literal
/// provenance. Apply before delivery receipts so unseen remainder text is never
/// recorded as delivered. Canonical responses and details keep the full original.
fn summary_quotes(output: &mut Value, details: bool) {
    let eligible = !details
        && output["status"] == "complete"
        && output["conflicts"].as_array().is_some_and(Vec::is_empty);
    for row in output["evidence"].as_array_mut().into_iter().flatten() {
        let Some(object) = row.as_object_mut() else {
            continue;
        };
        let Some(excerpt) = object.remove("summary_quote") else {
            continue;
        };
        if eligible
            && matches!(
                object.get("authority").and_then(Value::as_str),
                Some("advisory_memory" | "agent_memory")
            )
            && excerpt.as_str().is_some_and(|text| {
                !text.trim().is_empty()
                    && text.chars().count() <= 600
                    && object
                        .get("quote")
                        .and_then(Value::as_str)
                        .is_some_and(|original| original.contains(text))
            })
        {
            object.insert("quote".into(), excerpt);
        }
    }
}

/// Drop only known topic labels, never arbitrary headings that can carry scope.
fn redundant_topic_headings(output: &mut Value) {
    if output["status"] != "complete" {
        return;
    }
    let Some(rows) = output["evidence"].as_array() else {
        return;
    };
    let mut removed = std::collections::BTreeSet::new();
    for heading in rows {
        let Some(reference) = heading["ref"].as_str() else {
            continue;
        };
        let Some(line) = heading["line"].as_u64() else {
            continue;
        };
        let text = heading["quote"].as_str().unwrap_or("").trim();
        if heading["authority"] != "user_document" || !text.starts_with('#') {
            continue;
        }
        let label = text.trim_start_matches('#').trim().to_lowercase();
        if ![
            "accessibility",
            "typography",
            "доступность",
            "типографика",
            "无障碍",
            "排版",
        ]
        .contains(&label.as_str())
        {
            continue;
        }
        let body = rows.iter().find(|r| {
            r["source"] == heading["source"]
                && r["authority"] == "user_document"
                && r["line"].as_u64() == line.checked_add(1)
                && r["quote"]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty() && !s.trim().starts_with('#'))
        });
        let Some(body_ref) = body.and_then(|r| r["ref"].as_str()) else {
            continue;
        };
        let mut used = false;
        let safe = ["aspects", "conflicts"].iter().all(|field| {
            output[*field].as_array().into_iter().flatten().all(|a| {
                let Some(ids) = a["evidence"].as_array() else {
                    return true;
                };
                if !ids.iter().any(|id| id.as_str() == Some(reference)) {
                    return true;
                }
                used = true;
                *field == "aspects"
                    && a["status"] == "found"
                    && ids.iter().any(|id| id.as_str() == Some(body_ref))
            })
        });
        if used && safe {
            removed.insert(reference.to_owned());
        }
    }
    if let Some(aspects) = output["aspects"].as_array_mut() {
        for a in aspects {
            if let Some(ids) = a["evidence"].as_array_mut() {
                ids.retain(|id| !id.as_str().is_some_and(|s| removed.contains(s)));
            }
        }
    }
    if let Some(rows) = output["evidence"].as_array_mut() {
        rows.retain(|row| !row["ref"].as_str().is_some_and(|s| removed.contains(s)));
    }
}

pub(super) fn memory_locations(output: &mut Value) {
    if let Some(rows) = output["evidence"].as_array_mut() {
        let mut has_value_lines = false;
        for row in rows {
            if let Some(line) = row.as_object_mut().and_then(|r| r.remove("memory_line")) {
                row["value_line"] = line;
                row["location_kind"] = json!("json_value");
                has_value_lines = true;
            }
        }
        if has_value_lines {
            output["citation_rules"]=json!("For json_value locations, cite source + json_pointer + value_line. value_line is a line inside the JSON string, never a file line. Expand source_ref through sources.");
        }
    }
}

pub(super) fn question_addresses(output: &mut Value, previous: &Value, reusable: bool) {
    if let Some(aspects) = output["aspects"].as_array_mut() {
        for aspect in aspects {
            let Some(question) = aspect["question"].as_str().map(str::to_owned) else {
                continue;
            };
            let id = format!("q{}", &crate::util::digest(&question)[..16]);
            // Short labels are already cheaper than a question reference.
            if question.len() <= id.len() + 8 {
                continue;
            }
            let was_visible = reusable
                && previous["aspects"].as_array().is_some_and(|rows| {
                    rows.iter().any(|r| {
                        r["question"] == question
                            && (previous["status"] != "complete"
                                || r["answer"].as_str().is_some_and(|a| !a.trim().is_empty()))
                    })
                });
            if was_visible {
                aspect.as_object_mut().unwrap().remove("question");
                aspect["question_ref"] = json!(id);
            } else {
                aspect["question_id"] = json!(id);
            }
        }
    }
}

/// Expandable, message-local paths; canonical evidence remains in topic bookkeeping.
fn compact_transport(output: &mut Value, _previous: &Value, _reusable: bool) {
    // Complete certified claims may omit questions. Never reference a question
    // from canonical history that the consumer might not actually have received.
    let original = output.clone();
    let mut counts = BTreeMap::<String, usize>::new();
    if let Some(rows) = output["evidence"].as_array() {
        for row in rows {
            if let Some(path) = row["source"].as_str() {
                *counts.entry(path.into()).or_default() += 1;
            }
        }
    }
    let aliases: BTreeMap<_, _> = counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .enumerate()
        .map(|(i, (path, _))| (path, format!("s{}", i + 1)))
        .collect();
    if aliases.is_empty() {
        return;
    }
    if let Some(rows) = output["evidence"].as_array_mut() {
        for row in rows {
            if let Some(alias) = row["source"].as_str().and_then(|path| aliases.get(path)) {
                let alias = alias.clone();
                row.as_object_mut().unwrap().remove("source");
                row["source_ref"] = json!(alias);
            }
        }
    }
    output["sources"] = json!(aliases
        .into_iter()
        .map(|(path, id)| (id, path))
        .collect::<BTreeMap<_, _>>());
    if output.to_string().len() >= original.to_string().len() {
        *output = original;
    }
}

/// Factor only response-local metadata after canonical delivery receipts are recorded.
/// Defaults never apply to previously delivered/reused evidence or saved details.
fn evidence_defaults(output: &mut Value) {
    if output.get("evidence_defaults").is_some() {
        return;
    }
    let Some(rows) = output["evidence"].as_array().filter(|rows| rows.len() >= 2) else {
        return;
    };
    let defaults: serde_json::Map<String, Value> = ["authority", "json_pointer", "location_kind"]
        .into_iter()
        .filter_map(|key| {
            let value = rows[0].get(key).filter(|v| v.is_string())?;
            rows.iter()
                .all(|row| row.get(key) == Some(value))
                .then(|| (key.to_owned(), value.clone()))
        })
        .collect();
    if defaults.is_empty() {
        return;
    }
    let mut compact = output.clone();
    for row in compact["evidence"].as_array_mut().unwrap() {
        if let Some(object) = row.as_object_mut() {
            for key in defaults.keys() {
                object.remove(key);
            }
        }
    }
    compact["evidence_defaults"] = Value::Object(defaults);
    if compact.to_string().len() < output.to_string().len() {
        *output = compact;
    }
}

/// Reuse delivered, resolved evidence while retaining unresolved recovery excerpts.
#[cfg(test)]
pub(super) fn topic_update(
    full: &str,
    previous: &Value,
    known: &mut BTreeMap<String, Value>,
    reusable: bool,
) -> crate::util::Result<String> {
    topic_update_with_aliases(full, previous, known, reusable, None, None)
}

pub(super) fn topic_update_with_aliases(
    full: &str,
    previous: &Value,
    known: &mut BTreeMap<String, Value>,
    reusable: bool,
    aliases: Option<&super::evidence_aliases::Aliases>,
    source_delivery: Option<super::source_context::Delivery<'_>>,
) -> crate::util::Result<String> {
    let mut output: Value = serde_json::from_str(full)?;
    let complete = output["status"] == "complete" && output["detail_level"] == "summary";
    // A failed branch does not invalidate independent, source-checked found aspects.
    // Evidence used by an unresolved aspect/conflict must still be sent in full.
    let mut resolved = std::collections::BTreeSet::new();
    let mut unresolved = std::collections::BTreeSet::new();
    if let Some(aspects) = output["aspects"].as_array() {
        for aspect in aspects {
            let refs = if aspect["status"] == "found" {
                &mut resolved
            } else {
                &mut unresolved
            };
            if let Some(evidence) = aspect["evidence"].as_array() {
                refs.extend(evidence.iter().filter_map(Value::as_str).map(str::to_owned));
            }
        }
    }
    if let Some(conflicts) = output["conflicts"].as_array() {
        for conflict in conflicts.iter().filter(|c| c["unresolved"] == true) {
            if let Some(evidence) = conflict["evidence"].as_array() {
                unresolved.extend(evidence.iter().filter_map(Value::as_str).map(str::to_owned));
            }
        }
    }
    let eligible = |id: &str| (complete || resolved.contains(id)) && !unresolved.contains(id);
    let delta = reusable
        && !known.is_empty()
        && output["evidence"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|r| r["ref"].as_str().is_some_and(&eligible))
        });
    if !reusable {
        known.clear();
    }
    let mut reused = Vec::new();
    if let Some(rows) = output.get_mut("evidence").and_then(Value::as_array_mut) {
        rows.retain(|row| {
            let Some(id) = row["ref"].as_str() else {
                return true;
            };
            let already_known = delta && eligible(id) && known.get(id) == Some(row);
            if already_known {
                reused.push(json!(id));
            }
            if eligible(id) {
                known.insert(id.to_owned(), row.clone());
            }
            !already_known
        });
    }
    // Bound bookkeeping independently of topic length; forgotten records are safely resent.
    while known.len() > 512 {
        known.pop_first();
    }
    output["response_mode"] = json!(if delta { "delta" } else { "full" });
    if delta {
        output["reused_evidence"] = json!(reused);
        if complete
            && output["answer"].is_string()
            && previous["status"] == "complete"
            && output["answer"] == previous["answer"]
        {
            output.as_object_mut().unwrap().remove("answer");
            output["answer_unchanged"] = json!(true);
        }
    }
    if let Some(aliases) = aliases {
        aliases.project(&mut output);
    }
    let mut source_first = None;
    if let Some(delivery) = source_delivery {
        if super::source_context::apply(
            &mut output,
            delivery.candidates,
            delivery.received,
            delivery.evidence_budget,
        ) {
            source_first =
                super::source_first::candidate(&output, delivery.requested, delivery.intents);
        }
    }
    compact_transport(&mut output, previous, reusable);
    evidence_defaults(&mut output);
    compact_wire::apply(&mut output);
    if let Some(mut alternative) = source_first {
        compact_transport(&mut alternative, previous, reusable);
        evidence_defaults(&mut alternative);
        compact_wire::apply(&mut alternative);
        if alternative.to_string().len() < output.to_string().len() {
            output = alternative;
        }
    }
    Ok(serde_json::to_string(&output)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_aspect_answers_allow_summary_without_redundant_aggregate() {
        for aggregate in [None, Some(json!("")), Some(json!(" \n")), Some(Value::Null)] {
            let mut value = json!({"status":"complete","aspects":[{"status":"found","answer":"The enabled action is green.","evidence":["e1"]}],"evidence":[{"ref":"e1","source":"rules.md","authority":"user_document","line":1,"quote":"The enabled action is green."}],"conflicts":[],"omitted_evidence":0});
            if let Some(aggregate) = aggregate {
                value["answer"] = aggregate;
            }
            let aspects = value["aspects"].clone();
            let evidence = value["evidence"].clone();
            detail_level(&mut value, false);
            assert_eq!(value["detail_level"], "summary");
            assert_eq!(value["aspects"], aspects);
            assert_eq!(value["evidence"], evidence);
        }
    }

    #[test]
    fn missing_aggregate_does_not_hide_incomplete_or_requested_recovery() {
        for case in [
            "partial",
            "missing",
            "blank",
            "absent_answer",
            "empty_aspects",
            "absent_aspects",
            "omitted",
            "conflict",
            "details",
        ] {
            let mut value = json!({"status":"complete","answer":"","aspects":[{"status":"found","answer":"The enabled action is green.","evidence":["e1"]}],"evidence":[{"ref":"e1","source":"rules.md","authority":"user_document","line":1,"quote":"The enabled action is green."}],"conflicts":[],"omitted_evidence":0});
            match case {
                "partial" => value["status"] = json!("partial"),
                "missing" => value["aspects"][0]["status"] = json!("missing"),
                "blank" => value["aspects"][0]["answer"] = json!(" \n"),
                "absent_answer" => {
                    value["aspects"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("answer");
                }
                "empty_aspects" => value["aspects"] = json!([]),
                "absent_aspects" => {
                    value.as_object_mut().unwrap().remove("aspects");
                }
                "omitted" => value["omitted_evidence"] = json!(1),
                "conflict" => value["conflicts"] = json!([{"unresolved":true}]),
                _ => {}
            }
            let evidence = value["evidence"].clone();
            detail_level(&mut value, case == "details");
            assert_eq!(value["detail_level"], "full", "{case}");
            assert_eq!(value["evidence"], evidence, "{case}");
        }
    }
    #[test]
    fn certified_then_uncertified_never_references_an_undelivered_question() {
        let full = json!({"status":"complete","detail_level":"summary","context_session":"topic","conflicts":[],"aspects":[{"question":"Which exact width applies to the enabled panel?","answer":"The enabled panel width must be 12px.","status":"found","evidence":["e1"],"self_contained":true}],"evidence":[{"ref":"e1","quote":"Enabled panel width: 12px.","source":"rules.md","line":2}]});
        let mut known = BTreeMap::new();
        let first: Value = serde_json::from_str(
            &super::topic_update(&full.to_string(), &Value::Null, &mut known, false).unwrap(),
        )
        .unwrap();
        assert!(first["aspects"][0].get("question").is_none());
        assert_eq!(known["e1"], full["evidence"][0]);
        for status in ["complete", "partial"] {
            let mut next = full.clone();
            next["aspects"][0]["self_contained"] = json!(false);
            next["status"] = json!(status);
            let wire: Value = serde_json::from_str(
                &super::topic_update(&next.to_string(), &full, &mut known, true).unwrap(),
            )
            .unwrap();
            let wire = compact_wire::expand(wire);
            assert_eq!(
                wire["aspects"][0]["question"],
                full["aspects"][0]["question"]
            );
            assert!(wire["aspects"][0].get("question_ref").is_none());
        }
        assert_eq!(
            full["aspects"][0]["question"],
            "Which exact width applies to the enabled panel?"
        );
    }
    fn topic_update(
        full: &str,
        previous: &Value,
        known: &mut BTreeMap<String, Value>,
        reusable: bool,
    ) -> crate::util::Result<String> {
        let raw = super::topic_update(full, previous, known, reusable)?;
        Ok(compact_wire::expand(serde_json::from_str(&raw)?).to_string())
    }
    fn response(status: &str) -> Value {
        json!({"status":status,"answer_complete":status == "complete","answer":"Save green; historical implementation reported blue.",
            "evidence":[{"source":"memory/docs/ui.md","authority":"user_document","line":3,"quote":"Save green."},
                {"source":"memory/thread-agents/settings.json","authority":"agent_memory","json_pointer":"/memory","memory_line":2,"quote":"Reported: Save blue."}],
            "aspects":[{"question":"Save color?","status":"found","evidence":["memory/docs/ui.md:L3"]}],
            "conflicts":[{"kind":"implementation_discrepancy","description":"Implementation differs.","unresolved":false,"evidence":["memory/docs/ui.md:L3","memory/thread-agents/settings.json#/memory:line2"]}]})
    }
    #[test]
    fn summary_excerpts_keep_original_details_and_actual_delivery_receipts() {
        let original = "Reported rules: division truncates toward zero. Other historical notes.";
        let excerpt = "Reported rules: division truncates toward zero.";
        let canonical = json!({"status":"complete","answer":"Reported division truncates toward zero.","conflicts":[],"aspects":[{"status":"found","answer":"Reported division truncates toward zero.","evidence":["e1"]}],"evidence":[{"ref":"e1","source":"memory.json","authority":"advisory_memory","json_pointer":"/memory","value_line":3,"quote":original,"summary_quote":excerpt}]});
        let mut summary = canonical.clone();
        detail_level(&mut summary, false);
        assert_eq!(summary["evidence"][0]["quote"], excerpt);
        assert_eq!(summary["evidence"][0]["json_pointer"], "/memory");
        assert_eq!(summary["evidence"][0]["value_line"], 3);
        assert!(summary["evidence"][0].get("summary_quote").is_none());
        let mut known = BTreeMap::new();
        topic_update(&summary.to_string(), &Value::Null, &mut known, false).unwrap();
        assert_eq!(known["e1"]["quote"], excerpt);
        let mut full = canonical.clone();
        detail_level(&mut full, true);
        assert_eq!(full["evidence"][0]["quote"], original);
        assert!(full["evidence"][0].get("summary_quote").is_none());
        let next: Value = serde_json::from_str(
            &topic_update(&full.to_string(), &summary, &mut known, true).unwrap(),
        )
        .unwrap();
        assert_eq!(next["evidence"][0]["quote"], original);
        assert_eq!(canonical["evidence"][0]["quote"], original);
    }

    #[test]
    fn excerpts_do_not_replace_recovery_documents_or_invalid_cached_quotes() {
        for case in [
            "partial",
            "conflict",
            "document",
            "forged",
            "empty",
            "type",
            "oversized",
        ] {
            let mut packet = json!({"status":"complete","conflicts":[],"evidence":[{"authority":"advisory_memory","quote":"Condition: green only while enabled.","summary_quote":"green only while enabled."}]});
            match case {
                "partial" => packet["status"] = json!("partial"),
                "conflict" => packet["conflicts"] = json!([{"unresolved":true}]),
                "document" => packet["evidence"][0]["authority"] = json!("user_document"),
                "forged" => packet["evidence"][0]["summary_quote"] = json!("green always"),
                "empty" => packet["evidence"][0]["summary_quote"] = json!(" "),
                "type" => packet["evidence"][0]["summary_quote"] = json!(true),
                _ => {
                    packet["evidence"][0]["quote"] = json!("界".repeat(601));
                    packet["evidence"][0]["summary_quote"] = packet["evidence"][0]["quote"].clone();
                }
            }
            let original = packet["evidence"][0]["quote"].clone();
            summary_quotes(&mut packet, false);
            assert_eq!(packet["evidence"][0]["quote"], original, "{case}");
            assert!(
                packet["evidence"][0].get("summary_quote").is_none(),
                "{case}"
            );
        }
    }

    #[test]
    fn transport_shares_paths_and_keeps_question_references_resolvable() {
        let path = "memory/docs/a-very-long-original-source-file-name.md";
        let full = json!({"status":"complete","detail_level":"summary","aspects":[{"question":"What color and conditions apply to Save?","answer":"Green when enabled","status":"found","evidence":["a","b"]}],"evidence":[{"ref":"a","source":path,"line":1,"quote":"Green"},{"ref":"b","source":path,"line":2,"quote":"When enabled"}]});
        let mut known = BTreeMap::new();
        let first: Value = serde_json::from_str(
            &topic_update(&full.to_string(), &Value::Null, &mut known, false).unwrap(),
        )
        .unwrap();
        assert_eq!(first["sources"]["s1"], path);
        assert_eq!(first["evidence"][0]["source_ref"], "s1");
        assert_eq!(known["a"]["source"], path);
        let repeat: Value = serde_json::from_str(
            &topic_update(&full.to_string(), &full, &mut known, true).unwrap(),
        )
        .unwrap();
        assert_eq!(
            repeat["aspects"][0]["question"],
            first["aspects"][0]["question"]
        );
        assert!(repeat["aspects"][0].get("question_ref").is_none());
        assert!(repeat.get("sources").is_none());
        assert_eq!(repeat["reused_evidence"], json!(["a", "b"]));
        let reset: Value = serde_json::from_str(
            &topic_update(&full.to_string(), &full, &mut known, false).unwrap(),
        )
        .unwrap();
        assert!(reset["aspects"][0]["question"].is_string());
        // A short path must not grow the response just to add an alias table.
        let mut short = json!({"evidence":[{"source":"x"},{"source":"x"}]});
        compact_transport(&mut short, &Value::Null, false);
        assert!(short.get("sources").is_none());
    }

    #[test]
    fn memory_location_keeps_json_pointer_and_distinguishes_value_from_file_line() {
        let mut v = response("partial");
        deduplicate(&mut v);
        let original = v["aspects"].clone();
        memory_locations(&mut v);
        assert_eq!(v["evidence"][0]["line"], 3);
        assert_eq!(v["evidence"][1]["json_pointer"], "/memory");
        assert_eq!(v["evidence"][1]["value_line"], 2);
        assert_eq!(v["evidence"][1]["location_kind"], "json_value");
        assert!(v["evidence"][1].get("line").is_none());
        assert!(v["evidence"][1].get("memory_line").is_none());
        assert_eq!(v["aspects"], original);
        assert!(v["citation_rules"]
            .as_str()
            .unwrap()
            .contains("never a file line"));
    }

    #[test]
    fn missing_aspect_is_compact_without_hiding_the_gap_or_details() {
        let mut value = response("partial");
        value["aspects"][0]["status"] = json!("missing");
        value["aspects"][0]["search_state"] = json!("not_found_in_reviewed_sources");
        value["evidence"][1]["quote"] = json!("x".repeat(900));
        let original = value.clone();
        detail_level(&mut value, false);
        assert_eq!(value["detail_level"], "summary");
        assert_eq!(value["status"], "partial");
        assert_eq!(value["aspects"], original["aspects"]);
        assert_eq!(value["evidence"][0], original["evidence"][0]);
        assert_eq!(value["omitted_memory_quotes"], 1);
        let mut details = original.clone();
        detail_level(&mut details, true);
        assert_eq!(details["evidence"], original["evidence"]);
    }

    #[test]
    fn topic_bookkeeping_spans_followups_and_preserves_history_across_partial() {
        let snapshot = |answer: &str, refs: &[&str], status: &str| json!({"status":status,"detail_level":if status=="complete" {"summary"} else {"full"},"answer":answer,"evidence":refs.iter().map(|id|json!({"ref":id,"source":"doc","line":1})).collect::<Vec<_>>()});
        let mut known = BTreeMap::new();
        let a = snapshot("A", &["eA"], "complete");
        topic_update(&a.to_string(), &Value::Null, &mut known, false).unwrap();
        let b = snapshot("B", &["eB"], "complete");
        let update: Value =
            serde_json::from_str(&topic_update(&b.to_string(), &a, &mut known, true).unwrap())
                .unwrap();
        assert_eq!(update["evidence"], b["evidence"]);
        let back: Value =
            serde_json::from_str(&topic_update(&a.to_string(), &b, &mut known, true).unwrap())
                .unwrap();
        assert_eq!(back["reused_evidence"], json!(["eA"]));
        assert_eq!(back["answer"], "A");
        let refreshed: Value =
            serde_json::from_str(&topic_update(&a.to_string(), &a, &mut known, false).unwrap())
                .unwrap();
        assert_eq!(refreshed["response_mode"], "full");
        assert_eq!(refreshed["evidence"], a["evidence"]);
        let partial = snapshot("Missing rule", &["eA"], "partial");
        let recovery: Value = serde_json::from_str(
            &topic_update(&partial.to_string(), &a, &mut known, true).unwrap(),
        )
        .unwrap();
        assert_eq!(recovery["response_mode"], "full");
        assert_eq!(recovery["evidence"], partial["evidence"]);
        assert!(known.contains_key("eA"));
    }

    #[test]
    fn partial_delivery_reuses_only_resolved_aspects_and_keeps_conflict_recovery() {
        let mut known = BTreeMap::new();
        let partial = json!({"status":"partial","detail_level":"full","answer":"Some facts missing",
            "evidence":[{"ref":"a","quote":"A"},{"ref":"b","quote":"B"}],
            "aspects":[{"status":"found","evidence":["a"]},{"status":"missing","evidence":["b"]}],
            "limitations":["Missing requirement"]});
        topic_update(&partial.to_string(), &Value::Null, &mut known, false).unwrap();
        assert!(known.contains_key("a"));
        assert!(!known.contains_key("b"));
        let repeat: Value = serde_json::from_str(
            &topic_update(&partial.to_string(), &partial, &mut known, true).unwrap(),
        )
        .unwrap();
        assert_eq!(repeat["status"], "partial");
        assert_eq!(repeat["reused_evidence"], json!(["a"]));
        assert_eq!(repeat["evidence"], json!([{"ref":"b","quote":"B"}]));
        assert_eq!(repeat["aspects"], partial["aspects"]);
        assert_eq!(repeat["limitations"], partial["limitations"]);
        assert_eq!(repeat["answer"], partial["answer"]);
        let mut conflict = partial.clone();
        conflict["conflicts"] = json!([{"unresolved":true,"evidence":["a"]}]);
        let recovery: Value = serde_json::from_str(
            &topic_update(&conflict.to_string(), &partial, &mut known, true).unwrap(),
        )
        .unwrap();
        assert_eq!(recovery["evidence"], partial["evidence"]);
        let complete = json!({"status":"complete","detail_level":"summary","answer":"A and B","evidence":partial["evidence"]});
        let done: Value = serde_json::from_str(
            &topic_update(&complete.to_string(), &partial, &mut known, true).unwrap(),
        )
        .unwrap();
        assert_eq!(done["reused_evidence"], json!(["a"]));
        assert_eq!(done["evidence"], json!([{"ref":"b","quote":"B"}]));
        assert_eq!(done["answer"], complete["answer"]);
        let reset: Value = serde_json::from_str(
            &topic_update(&complete.to_string(), &partial, &mut known, false).unwrap(),
        )
        .unwrap();
        assert_eq!(reset["response_mode"], "full");
        assert_eq!(reset["evidence"], complete["evidence"]);
    }

    #[test]
    fn bounded_memory_quotes_are_exact_and_never_promoted_to_verified_facts() {
        let mut value = response("complete");
        value["evidence"] = json!((0..5).map(|i| json!({"ref":format!("m{i}"),"authority":"advisory_memory","quote":"界".repeat(600)})).collect::<Vec<_>>());
        let original = value.clone();
        detail_level(&mut value, false);
        for i in 0..4 {
            assert_eq!(
                value["evidence"][i]["quote"],
                original["evidence"][i]["quote"]
            );
            assert_eq!(value["evidence"][i]["authority"], "advisory_memory");
        }
        assert!(value["evidence"][4].get("quote").is_none());
        assert_eq!(value["omitted_memory_quotes"], 1);
        assert!(value["memory_notice"]
            .as_str()
            .unwrap()
            .contains("not independently verified"));
        let mut oversized = response("complete");
        oversized["evidence"][1]["quote"] = json!("界".repeat(601));
        detail_level(&mut oversized, false);
        assert!(oversized["evidence"][1].get("quote").is_none());
        assert_eq!(oversized["evidence"][0]["quote"], "Save green.");
        let mut full = original.clone();
        detail_level(&mut full, true);
        assert_eq!(full["evidence"], original["evidence"]);
    }

    #[test]
    fn local_failure_keeps_inline_citations_without_expanding_advisory_memory() {
        let mut value = response("partial");
        value["context_session"] = json!("topic");
        value["errors"] = json!(["internal worker id: invalid semantic element"]);
        value["coverage"] = json!({"index_revision":"private hash"});
        value["calls_scheduled"] = json!(9);
        value["unprocessed_threads"] = json!(["Application"]);
        value["unprocessed_thread_count"] = json!(1);
        value["evidence"][1]["quote"] = json!("x".repeat(700));
        let original = value.clone();
        deduplicate(&mut value);
        detail_level(&mut value, false);
        assert_eq!(value["status"], "partial");
        assert_eq!(value["detail_level"], "summary");
        assert_eq!(value["evidence"][0]["quote"], "Save green.");
        assert!(value["evidence"][1].get("quote").is_none());
        assert_eq!(value["aspects"][0]["status"], "found");
        assert_eq!(
            value["unprocessed_threads"],
            original["unprocessed_threads"]
        );
        assert!(value["limitations"][0]
            .as_str()
            .unwrap()
            .contains("incomplete"));
        for field in ["coverage", "errors", "calls_scheduled"] {
            assert!(value.get(field).is_none());
        }
        let mut details = original.clone();
        detail_level(&mut details, true);
        assert_eq!(details["evidence"], original["evidence"]);
        assert_eq!(details["errors"], original["errors"]);
    }

    #[test]
    fn unresolved_scope_and_conflicts_keep_recovery_quotes() {
        for kind in ["conflict", "no_assembly", "omitted"] {
            let mut value = response("partial");
            match kind {
                "missing" => value["aspects"][0]["status"] = json!("missing"),
                "conflict" => value["conflicts"][0]["unresolved"] = json!(true),
                "omitted" => value["omitted_evidence"] = json!(1),
                _ => value["aspects"] = json!([]),
            }
            let evidence = value["evidence"].clone();
            detail_level(&mut value, false);
            assert_eq!(value["detail_level"], "full");
            assert_eq!(value["evidence"], evidence);
            assert_eq!(value["status"], "partial");
        }
    }

    #[test]
    fn details_preserve_quotes_for_partial_or_empty_answers() {
        for (status, answer, requested) in [
            ("partial", "partial answer", true),
            ("complete", " ", false),
            ("complete", "answer", true),
        ] {
            let mut value = response(status);
            value["answer"] = json!(answer);
            let original = value["evidence"].clone();
            detail_level(&mut value, requested);
            assert_eq!(value["detail_level"], "full");
            assert_eq!(value["evidence"], original);
        }
    }

    #[test]
    fn complete_response_keeps_quotes_and_conflicts_with_local_references() {
        let mut value = response("complete");
        let original = value.clone();
        deduplicate(&mut value);
        assert!(value.get("aspects").is_none());
        assert!(value.get("answer_complete").is_none());
        assert_eq!(value["answer"], original["answer"]);
        assert_eq!(value["conflicts"][0]["evidence"], json!(["e1", "e2"]));
        for (i, reference) in ["e1", "e2"].iter().enumerate() {
            let mut evidence = value["evidence"][i].clone();
            assert_eq!(
                evidence.as_object_mut().unwrap().remove("ref"),
                Some(json!(reference))
            );
            assert_eq!(evidence, original["evidence"][i]);
        }
        assert!(value.to_string().len() < original.to_string().len());
        let once = value.clone();
        deduplicate(&mut value);
        assert_eq!(value, once);
    }
    #[test]
    fn partial_response_preserves_scope_and_omitted_source_addresses() {
        let mut value = response("partial");
        value["aspects"].as_array_mut().unwrap().push(json!({"question":"Missing rule","status":"missing","evidence":["memory/docs/omitted.md:L5"]}));
        value["omitted_evidence"] = json!(1);
        value["errors"] = json!(["Timeout"]);
        deduplicate(&mut value);
        assert_eq!(value["aspects"].as_array().unwrap().len(), 2);
        assert_eq!(value["aspects"][0]["evidence"], json!(["e1"]));
        assert_eq!(
            value["aspects"][1]["evidence"],
            json!(["memory/docs/omitted.md:L5"])
        );
        assert_eq!(value["omitted_evidence"], 1);
        assert_eq!(value["errors"], json!(["Timeout"]));
    }
}

#[cfg(test)]
mod heading_tests {
    use super::*;
    #[test]
    fn compact_headings_keep_scope_conflicts_and_full_details() {
        let base = json!({"status":"complete","answer":"Keyboard works","aspects":[{"status":"found","evidence":["h","r"]}],"conflicts":[],"evidence":[
            {"ref":"h","authority":"user_document","source":"ui.md","line":1,"quote":"## Accessibility"},
            {"ref":"r","authority":"user_document","source":"ui.md","line":2,"quote":"All controls support keyboard operation."}]});
        let mut compact = base.clone();
        detail_level(&mut compact, false);
        assert_eq!(compact["evidence"].as_array().unwrap().len(), 1);
        assert_eq!(compact["aspects"][0]["evidence"], json!(["r"]));
        for case in [
            "details",
            "scope",
            "conflict",
            "only_heading",
            "different_source",
            "nonadjacent",
            "partial",
        ] {
            let mut v = base.clone();
            match case {
                "scope" => v["evidence"][0]["quote"] = json!("## Mobile only"),
                "conflict" => v["conflicts"] = json!([{"evidence":["h","r"]}]),
                "only_heading" => v["aspects"][0]["evidence"] = json!(["h"]),
                "different_source" => v["evidence"][1]["source"] = json!("other.md"),
                "nonadjacent" => v["evidence"][1]["line"] = json!(4),
                "partial" => v["status"] = json!("partial"),
                _ => {}
            }
            let expected = v["evidence"].clone();
            detail_level(&mut v, case == "details");
            assert_eq!(v["evidence"], expected, "{case}");
        }
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    fn expand(value: Value) -> Value {
        let mut value = compact_wire::expand(value);
        let defaults = value.as_object_mut().unwrap().remove("evidence_defaults");
        if let Some(defaults) = defaults.and_then(|v| v.as_object().cloned()) {
            for row in value["evidence"].as_array_mut().unwrap() {
                for (key, value) in &defaults {
                    row.as_object_mut()
                        .unwrap()
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
            }
        }
        value
    }

    fn memory() -> Value {
        json!({"status":"complete","detail_level":"summary","answer":"Reports, not independent proof.","aspects":[{"status":"found","answer":"Reported only","evidence":["m1","m2"]}],"evidence":[
            {"ref":"m1","source":"a.json","authority":"advisory_memory","json_pointer":"/memory","location_kind":"json_value","value_line":1,"quote":"Reported blue unless disabled."},
            {"ref":"m2","source":"b.json","authority":"advisory_memory","json_pointer":"/memory","location_kind":"json_value","value_line":2,"quote":"Validation not independently verified."}]})
    }

    #[test]
    fn metadata_factoring_is_lossless_and_preserves_all_citation_text() {
        let original = memory();
        let mut compact = original.clone();
        evidence_defaults(&mut compact);
        assert_eq!(
            compact["evidence_defaults"],
            json!({"authority":"advisory_memory","json_pointer":"/memory","location_kind":"json_value"})
        );
        assert!(compact.to_string().len() < original.to_string().len());
        assert_eq!(expand(compact.clone()), original);
        let once = compact.clone();
        evidence_defaults(&mut compact);
        assert_eq!(compact, once);
    }

    #[test]
    fn unequal_missing_metadata_and_small_packets_never_acquire_false_defaults() {
        for case in [
            "mixed",
            "pointer",
            "missing",
            "single",
            "empty",
            "no_saving",
        ] {
            let mut original = memory();
            match case {
                "mixed" => {
                    original["evidence"][1] = json!({"ref":"d","source":"doc.md","authority":"user_document","line":2,"quote":"Blue"});
                }
                "pointer" => original["evidence"][1]["json_pointer"] = json!("/claim/text"),
                "missing" => {
                    original["evidence"][1]
                        .as_object_mut()
                        .unwrap()
                        .remove("authority");
                }
                "single" => {
                    original["evidence"].as_array_mut().unwrap().truncate(1);
                }
                "empty" => original["evidence"] = json!([]),
                _ => original["evidence"] = json!([{"authority":"x"},{"authority":"x"}]),
            }
            let mut compact = original.clone();
            evidence_defaults(&mut compact);
            assert_eq!(expand(compact.clone()), original, "{case}");
            match case {
                "pointer" => assert!(compact["evidence_defaults"].get("json_pointer").is_none()),
                "missing" => assert!(compact["evidence_defaults"].get("authority").is_none()),
                _ => assert!(compact.get("evidence_defaults").is_none(), "{case}"),
            }
        }
    }

    #[test]
    fn partial_metadata_is_lossless_and_does_not_hide_conflicts_or_gaps() {
        let mut original = memory();
        original["status"] = json!("partial");
        original["detail_level"] = json!("full");
        original["aspects"][0]["status"] = json!("missing");
        original["conflicts"] = json!([{"unresolved":true,"description":"Contradictory claims","evidence":["m1","m2"]}]);
        original["limitations"] = json!(["Not all applicable sources reviewed"]);
        let mut compact = original.clone();
        evidence_defaults(&mut compact);
        assert!(compact.get("evidence_defaults").is_some());
        assert_eq!(expand(compact), original);
    }

    #[test]
    fn delta_defaults_apply_only_to_new_rows_and_receipts_stay_expanded() {
        let original = memory();
        let mut known = BTreeMap::new();
        let first: Value = serde_json::from_str(
            &topic_update(&original.to_string(), &Value::Null, &mut known, false).unwrap(),
        )
        .unwrap();
        assert!(first.get("evidence_defaults").is_some());
        assert_eq!(known["m1"], original["evidence"][0]);
        let repeated: Value = serde_json::from_str(
            &topic_update(&original.to_string(), &original, &mut known, true).unwrap(),
        )
        .unwrap();
        assert_eq!(repeated["reused_evidence"], json!(["m1", "m2"]));
        assert!(repeated["evidence"].as_array().unwrap().is_empty());
        assert!(repeated.get("evidence_defaults").is_none());
        let mut next = original.clone();
        next["evidence"].as_array_mut().unwrap().extend([
            json!({"ref":"d1","source":"one.md","authority":"user_document","line":1,"quote":"Required green"}),
            json!({"ref":"d2","source":"two.md","authority":"user_document","line":2,"quote":"Except disabled"}),
        ]);
        let delta: Value = serde_json::from_str(
            &topic_update(&next.to_string(), &original, &mut known, true).unwrap(),
        )
        .unwrap();
        assert_eq!(
            delta["evidence_defaults"],
            json!({"authority":"user_document"})
        );
        assert_eq!(delta["reused_evidence"], json!(["m1", "m2"]));
        assert_eq!(known["m1"]["authority"], "advisory_memory");
        assert_eq!(
            expand(delta)["evidence"],
            json!([next["evidence"][2], next["evidence"][3]])
        );
        assert_eq!(known["d1"]["authority"], "user_document");
    }
}
