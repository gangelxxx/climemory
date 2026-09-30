//! Final wire projection only: canonical receipts, questions and details stay expanded.
use serde_json::{json, Value};

pub(super) const FORMAT: &str = "cm/compact-1";
const CITATION_RULES: &str = "For json_value locations, cite source + json_pointer + value_line. value_line is a line inside the JSON string, never a file line. Expand source_ref through sources.";
const MEMORY_NOTICE: &str =
    "Memory excerpts are reported claims, not independently verified facts.";

pub(super) fn apply(output: &mut Value) {
    // The certification is internal, including on partial/legacy-shaped replies.
    let mut compact = output.clone();
    let eligible = output["status"] == "complete"
        && output["detail_level"] == "summary"
        && output.get("format").is_none();
    let conflict_free = output["conflicts"].as_array().is_some_and(Vec::is_empty);
    let quotes = current_quotes(output);
    if let Some(rows) = compact.get_mut("aspects").and_then(Value::as_array_mut) {
        for row in rows {
            let certified = row["self_contained"] == true
                && row["status"] == "found"
                && row["answer"].as_str().is_some_and(|s| !s.trim().is_empty())
                && row["evidence"].as_array().is_some_and(|e| !e.is_empty());
            if let Some(object) = row.as_object_mut() {
                object.remove("self_contained");
                if eligible && object.get("status") == Some(&json!("found")) {
                    object.remove("status");
                }
                if eligible && conflict_free && certified {
                    if let Some(exact) = joined_quotes(object.get("evidence"), &quotes) {
                        if !object.contains_key("answer_prefix")
                            && !object.contains_key("answer_from_evidence")
                        {
                            if let Some(prefix) = object
                                .get("answer")
                                .and_then(Value::as_str)
                                .and_then(|answer| exact_prefix(answer, &exact))
                            {
                                let prefix = prefix.to_owned();
                                object.remove("answer");
                                object.insert("answer_from_evidence".into(), json!(true));
                                if !prefix.is_empty() {
                                    object.insert("answer_prefix".into(), json!(prefix));
                                }
                            }
                        }
                    }
                    for key in ["question", "question_id", "question_ref"] {
                        object.remove(key);
                    }
                }
            }
        }
    }
    // Strip only the private marker from the fallback too.
    if let Some(rows) = output.get_mut("aspects").and_then(Value::as_array_mut) {
        for row in rows {
            if let Some(object) = row.as_object_mut() {
                object.remove("self_contained");
            }
        }
    }
    if !eligible {
        return;
    }
    if let Some(blocks) = compact
        .get_mut("source_blocks")
        .and_then(Value::as_object_mut)
    {
        for block in blocks.values_mut().filter_map(Value::as_object_mut) {
            for (field, expected) in [("authority", "user_document"), ("review", "source_context")]
            {
                if block.get(field).and_then(Value::as_str) == Some(expected) {
                    block.remove(field);
                }
            }
        }
    }
    normalize_evidence_sources(&mut compact);
    for field in ["aspects", "evidence"] {
        if let Some(value) = compact.get_mut(field) {
            table(value);
        }
    }
    let context = compact["context_session"].as_str().map(str::to_owned);
    for (field, expected) in [
        ("detail_level", json!("summary")),
        ("answer_format", json!("per_aspect")),
        ("response_mode", json!("full")),
        ("citation_rules", json!(CITATION_RULES)),
        ("memory_notice", json!(MEMORY_NOTICE)),
        ("conflicts", json!([])),
        ("reused_evidence", json!([])),
    ] {
        if compact.get(field) == Some(&expected) {
            compact.as_object_mut().unwrap().remove(field);
        }
    }
    if let Some(id) = context {
        if compact["details"] == format!("@context:{id} @details") {
            compact.as_object_mut().unwrap().remove("details");
        }
    }
    compact["format"] = json!(FORMAT);
    if compact.to_string().len() < output.to_string().len() {
        *output = compact;
    }
}

// Resolve only evidence actually carried in this packet; no receipt lookup or
// substring/paraphrase inference is allowed. Duplicate refs are ambiguous.
fn current_quotes(output: &Value) -> std::collections::BTreeMap<String, String> {
    let mut quotes = std::collections::BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    for row in output["evidence"].as_array().into_iter().flatten() {
        let Some(id) = row["ref"].as_str() else {
            continue;
        };
        if !seen.insert(id) {
            quotes.remove(id);
            continue;
        }
        let quote = if row.get("source_block").is_some() {
            if row.get("quote").is_some() {
                None
            } else {
                row["source_block"]
                    .as_str()
                    .and_then(|id| output["source_blocks"].get(id))
                    .and_then(|block| block_line(block, row["line"].as_u64()?))
            }
        } else {
            row["quote"].as_str()
        };
        if let Some(quote) = quote {
            quotes.insert(id.to_owned(), quote.to_owned());
        }
    }
    quotes
}
fn block_line(block: &Value, line: u64) -> Option<&str> {
    if line == 0 {
        return None;
    }
    if let Some(numbered) = block.get("numbered_lines") {
        if block.get("lines").is_some() {
            return None;
        }
        let mut found = None;
        for pair in numbered.as_array()? {
            let pair = pair.as_array()?;
            if pair.len() != 2 {
                return None;
            }
            let n = pair[0].as_u64()?;
            let text = pair[1].as_str()?;
            if n == line {
                if found.is_some() {
                    return None;
                }
                found = Some(text);
            }
        }
        found
    } else {
        block["lines"]
            .as_array()?
            .get(usize::try_from(line).ok()?.checked_sub(1)?)?
            .as_str()
    }
}
// Return the literal qualification only; never infer or normalize attribution.
fn exact_prefix<'a>(answer: &'a str, quotes: &str) -> Option<&'a str> {
    if answer.chars().count() > 600 {
        return None;
    }
    if answer == quotes {
        return Some("");
    }
    let prefix = answer.strip_suffix(quotes)?.strip_suffix('\n')?;
    if prefix.trim().is_empty() || prefix.chars().count() > 120 {
        return None;
    }
    Some(prefix)
}

fn joined_quotes(
    refs: Option<&Value>,
    quotes: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let refs = refs?.as_array()?;
    if refs.is_empty() {
        return None;
    }
    let parts = refs
        .iter()
        .map(|id| quotes.get(id.as_str()?))
        .collect::<Option<Vec<_>>>()?;
    Some(
        parts
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn normalize_evidence_sources(output: &mut Value) {
    let mut candidate = output.clone();
    let mut sources = match candidate.get("sources") {
        Some(Value::Object(s)) if s.values().all(Value::is_string) => s.clone(),
        None => serde_json::Map::new(),
        _ => return,
    };
    let Some(rows) = candidate.get_mut("evidence").and_then(Value::as_array_mut) else {
        return;
    };
    for row in rows {
        let Some(object) = row.as_object_mut() else {
            return;
        };
        match (object.get("source"), object.get("source_ref")) {
            (Some(Value::String(path)), None) => {
                let alias = sources
                    .iter()
                    .find(|(_, value)| value.as_str() == Some(path))
                    .map(|(key, _)| key.clone())
                    .unwrap_or_else(|| {
                        let mut i = 1;
                        while sources.contains_key(&format!("s{i}")) {
                            i += 1;
                        }
                        format!("s{i}")
                    });
                sources.insert(alias.clone(), json!(path));
                object.remove("source");
                object.insert("source_ref".into(), json!(alias));
            }
            (None, Some(Value::String(alias))) if sources.contains_key(alias) => {}
            _ => return,
        }
    }
    table(&mut candidate["evidence"]);
    if !candidate["evidence"].is_object() {
        return;
    }
    candidate["sources"] = Value::Object(sources);
    if candidate.to_string().len() < output.to_string().len() {
        *output = candidate;
    }
}

fn table(value: &mut Value) {
    let Some(rows) = value.as_array().filter(|rows| rows.len() >= 2) else {
        return;
    };
    let Some(first) = rows[0].as_object() else {
        return;
    };
    let columns: Vec<_> = first.keys().cloned().collect();
    // Preserve absent versus null: mixed shapes stay ordinary objects.
    if columns.is_empty()
        || !rows
            .iter()
            .all(|r| r.as_object().is_some_and(|r| r.keys().eq(first.keys())))
    {
        return;
    }
    let matrix: Vec<Vec<_>> = rows
        .iter()
        .map(|r| columns.iter().map(|k| r[k].clone()).collect())
        .collect();
    let candidate = json!({"columns":columns,"rows":matrix});
    if candidate.to_string().len() < value.to_string().len() {
        *value = candidate;
    }
}

/// Strict reference decoder used by tests, not a second production protocol path.
#[cfg(test)]
pub(super) fn expand(mut value: Value) -> Value {
    if value.get("format").is_none() {
        expand_source_blocks(&mut value);
        return value;
    }
    assert_eq!(value["format"], FORMAT);
    value.as_object_mut().unwrap().remove("format");
    for field in ["aspects", "evidence"] {
        if !value[field].is_object() {
            continue;
        }
        let obj = value[field].as_object().unwrap();
        assert_eq!(obj.len(), 2);
        let columns = obj["columns"].as_array().unwrap();
        let names: Vec<_> = columns.iter().map(|c| c.as_str().unwrap()).collect();
        assert_eq!(
            names
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            names.len()
        );
        let rows: Vec<Value> = obj["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let cells = row.as_array().unwrap();
                assert_eq!(cells.len(), names.len());
                Value::Object(
                    names
                        .iter()
                        .zip(cells)
                        .map(|(k, v)| ((*k).to_owned(), v.clone()))
                        .collect(),
                )
            })
            .collect();
        value[field] = json!(rows);
    }
    expand_source_blocks(&mut value);
    let quotes = current_quotes(&value);
    if let Some(rows) = value.get_mut("aspects").and_then(Value::as_array_mut) {
        for row in rows {
            let object = row.as_object_mut().unwrap();
            object.entry("status").or_insert(json!("found"));
            if object.get("answer_from_evidence") == Some(&json!(true)) {
                let answer = joined_quotes(object.get("evidence"), &quotes)
                    .expect("answer refs must resolve in current reply");
                assert!(!object.contains_key("answer"));
                let answer = if let Some(prefix) = object.remove("answer_prefix") {
                    let prefix = prefix.as_str().expect("answer prefix must be literal text");
                    assert!(!prefix.trim().is_empty() && prefix.chars().count() <= 120);
                    format!("{prefix}\n{answer}")
                } else {
                    answer
                };
                assert!(answer.chars().count() <= 600);
                object.remove("answer_from_evidence");
                object.insert("answer".into(), json!(answer));
            }
        }
    }
    if let Some(blocks) = value
        .get_mut("source_blocks")
        .and_then(Value::as_object_mut)
    {
        for block in blocks.values_mut() {
            let object = block.as_object_mut().unwrap();
            object.entry("authority").or_insert(json!("user_document"));
            object.entry("review").or_insert(json!("source_context"));
        }
    }
    let object = value.as_object_mut().unwrap();
    object.entry("detail_level").or_insert(json!("summary"));
    object.entry("response_mode").or_insert(json!("full"));
    object.entry("conflicts").or_insert(json!([]));
    object.entry("reused_evidence").or_insert(json!([]));
    if object.contains_key("aspects") {
        object.entry("answer_format").or_insert(json!("per_aspect"));
    }
    if let Some(id) = object
        .get("context_session")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        object
            .entry("details")
            .or_insert(json!(format!("@context:{id} @details")));
    }
    value
}

#[cfg(test)]
fn expand_source_blocks(value: &mut Value) {
    if let Some(blocks) = value
        .get("source_blocks")
        .and_then(Value::as_object)
        .cloned()
    {
        if let Some(rows) = value.get_mut("evidence").and_then(Value::as_array_mut) {
            for row in rows {
                if let Some(block) = row.get("source_block").and_then(Value::as_str) {
                    let block = blocks
                        .get(block)
                        .expect("source block must be in this response");
                    let quote = json!(block_line(block, row["line"].as_u64().unwrap())
                        .expect("exact original line must exist"));
                    assert!(block["source"].is_string());
                    let object = row.as_object_mut().unwrap();
                    assert!(!object.contains_key("quote"));
                    assert!(!object.contains_key("source"));
                    object.insert("quote".into(), quote.clone());
                    object.insert("source".into(), block["source"].clone());
                    object.remove("source_block");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn extractive_packet() -> Value {
        json!({"status":"complete","detail_level":"summary","conflicts":[],"aspects":[{"question":"Which button rules apply?","status":"found","self_contained":true,"answer":"Enabled Save is green.\nDisabled Save is grey; never save while disabled.","evidence":["e1","e2"]}],"evidence":[{"ref":"e1","quote":"Enabled Save is green."},{"ref":"e2","source_block":"b1","line":2}],"source_blocks":{"b1":{"source":"ui.md","authority":"user_document","review":"source_context","lines":["","Disabled Save is grey; never save while disabled."]}}})
    }
    #[test]
    fn exact_ordered_extraction_roundtrips_source_blocks_and_defaults() {
        let mut output = extractive_packet();
        let answer = output["aspects"][0]["answer"].clone();
        apply(&mut output);
        assert_eq!(output["format"], FORMAT);
        assert_eq!(output["aspects"][0]["answer_from_evidence"], true);
        assert!(output["aspects"][0].get("answer").is_none());
        assert!(output["aspects"][0].get("status").is_none());
        assert!(output["source_blocks"]["b1"].get("review").is_none());
        let expanded = expand(output);
        assert_eq!(expanded["aspects"][0]["answer"], answer);
        assert_eq!(expanded["aspects"][0]["status"], "found");
        assert_eq!(expanded["source_blocks"]["b1"]["review"], "source_context");
    }
    #[test]
    fn literal_qualification_prefix_roundtrips_without_inference() {
        let mut packet = extractive_packet();
        let prefix = "Reported only; not independently verified. 確認なし";
        let original = format!(
            "{prefix}\n{}",
            packet["aspects"][0]["answer"].as_str().unwrap()
        );
        packet["aspects"][0]["answer"] = json!(original);
        apply(&mut packet);
        assert_eq!(packet["aspects"][0]["answer_prefix"], prefix);
        assert_eq!(packet["aspects"][0]["answer_from_evidence"], true);
        assert_eq!(expand(packet)["aspects"][0]["answer"], original);
        assert_eq!(exact_prefix("bare claim", "bare claim"), Some(""));
        assert_eq!(exact_prefix("Reported: bare claim", "bare claim"), None);
        assert_eq!(exact_prefix("Reported\nbare claim!", "bare claim"), None);
        assert_eq!(exact_prefix("\nbare claim", "bare claim"), None);
        let prefix = "界".repeat(120);
        assert_eq!(
            exact_prefix(&format!("{prefix}\nclaim"), "claim"),
            Some(prefix.as_str())
        );
        assert!(exact_prefix(&format!("{prefix}界\nclaim"), "claim").is_none());
        assert!(exact_prefix(&"x".repeat(601), &"x".repeat(601)).is_none());
    }
    #[test]
    fn bounded_multipurpose_original_roundtrips_but_attribution_stays_custom() {
        let quote = "Reported binary constraints: signed 0/1 operands, at most 1024 digits; shift counts 0..1024. Division truncates toward zero; remainder follows dividend sign. These are unverified reports, not independent proof.";
        let mut packet = json!({"status":"complete","detail_level":"summary","conflicts":[],"aspects":[{"question":"What division rule was reported?","answer":quote,"status":"found","self_contained":true,"evidence":["e1"]}],"evidence":[{"ref":"e1","quote":quote,"source":"memory.json","value_line":3}]});
        apply(&mut packet);
        assert_eq!(packet["aspects"][0]["answer_from_evidence"], true);
        let expanded = expand(packet);
        assert_eq!(expanded["aspects"][0]["answer"], quote);
        assert_eq!(expanded["evidence"][0]["quote"], quote);
        let custom = "Reported only, not independently verified: division truncates toward zero.";
        let mut packet = json!({"status":"complete","detail_level":"summary","conflicts":[],"aspects":[{"question":"What division rule was reported?","answer":custom,"status":"found","self_contained":true,"evidence":["e1"]}],"evidence":[{"ref":"e1","quote":"Division truncates toward zero.","source":"memory.json","value_line":3}]});
        apply(&mut packet);
        assert!(packet["aspects"][0].get("answer_from_evidence").is_none());
        assert_eq!(expand(packet)["aspects"][0]["answer"], custom);
    }
    #[test]
    fn numbered_lines_resolve_original_addresses_without_counting_positions() {
        let mut packet = extractive_packet();
        packet["source_blocks"]["b1"]
            .as_object_mut()
            .unwrap()
            .remove("lines");
        packet["source_blocks"]["b1"]["numbered_lines"] = json!([
            [1, ""],
            [16, "Disabled Save is grey; never save while disabled."],
            [20, "Other context."]
        ]);
        packet["evidence"][1]["line"] = json!(16);
        let original = packet["aspects"][0]["answer"].clone();
        apply(&mut packet);
        assert_eq!(packet["aspects"][0]["answer_from_evidence"], true);
        let expanded = expand(packet);
        assert_eq!(expanded["aspects"][0]["answer"], original);
        assert_eq!(expanded["evidence"][1]["line"], 16);
        assert_eq!(
            block_line(&json!({"numbered_lines":[[16,"a"],[16,"b"]]}), 16),
            None
        );
        assert_eq!(
            block_line(&json!({"numbered_lines":[[16,"a"]],"lines":["x"]}), 16),
            None
        );
        assert_eq!(
            block_line(&json!({"lines":["","legacy"]}), 2),
            Some("legacy")
        );
    }
    #[test]
    fn extraction_never_infers_qualifications_or_uses_earlier_receipts() {
        for case in [
            "uncertified",
            "partial",
            "conflict",
            "reordered",
            "missing",
            "qualified",
            "duplicate",
            "unknown_review",
        ] {
            let mut output = extractive_packet();
            match case {
                "uncertified" => output["aspects"][0]["self_contained"] = json!(false),
                "partial" => output["status"] = json!("partial"),
                "conflict" => output["conflicts"] = json!([{"description":"disagreement"}]),
                "reordered" => output["aspects"][0]["evidence"] = json!(["e2", "e1"]),
                "missing" => {
                    output["evidence"].as_array_mut().unwrap().remove(0);
                    output["reused_evidence"] = json!(["e1"]);
                }
                "qualified" => {
                    output["aspects"][0]["answer"] = json!("Reported only: Enabled Save is green.")
                }
                "duplicate" => output["evidence"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"ref":"e1","quote":"another value"})),
                _ => {
                    output["source_blocks"]["b1"]["review"] = json!("other");
                    output["aspects"][0]["self_contained"] = json!(false);
                }
            }
            let original = output["aspects"][0]["answer"].clone();
            apply(&mut output);
            let expanded = expand(output);
            assert_eq!(expanded["aspects"][0]["answer"], original, "{case}");
            assert!(
                expanded["aspects"][0].get("answer_from_evidence").is_none(),
                "{case}"
            );
            if case == "unknown_review" {
                assert_eq!(expanded["source_blocks"]["b1"]["review"], "other");
            }
        }
    }
    #[test]
    fn mixed_source_addresses_share_a_table_only_when_globally_smaller() {
        let mut v = json!({"status":"complete","detail_level":"summary","sources":{"s1":"memory/docs/requirements.md"},"evidence":[
            {"source_ref":"s1","line":1,"quote":"First complete exact source requirement.","ref":"e1"},
            {"source_ref":"s1","line":2,"quote":"Second complete exact source requirement.","ref":"e2"},
            {"source":"memory/docs/exception.md","line":8,"quote":"Exception applies only while disabled.","ref":"e3"}
        ]});
        let original = v.clone();
        normalize_evidence_sources(&mut v);
        assert!(v["evidence"].is_object());
        assert!(v.to_string().len() < original.to_string().len());
        v["format"] = json!(FORMAT);
        let expanded = expand(v);
        for (i, row) in expanded["evidence"].as_array().unwrap().iter().enumerate() {
            assert_eq!(row["quote"], original["evidence"][i]["quote"]);
            assert_eq!(row["line"], original["evidence"][i]["line"]);
            let path = row["source_ref"].as_str().unwrap();
            let expected = if i == 2 {
                &original["evidence"][i]["source"]
            } else {
                &original["sources"]["s1"]
            };
            assert_eq!(&expanded["sources"][path], expected);
        }
        for malformed in [
            json!({"source_ref":"missing","line":3}),
            json!({"source":"a","source_ref":"s1"}),
        ] {
            let mut next = original.clone();
            next["evidence"][2] = malformed;
            let before = next.clone();
            normalize_evidence_sources(&mut next);
            assert_eq!(next, before);
        }
    }
    #[test]
    fn tables_roundtrip_all_fields_quotes_and_nulls() {
        let rows = json!([
            {"question":"Enable?","answer":"Enabled means green except disabled state.","evidence":["e1"],"status":"found","extra":null},
            {"question":"Disable?","answer":"Disabled means grey; must not save.","evidence":["e2"],"status":"found","extra":null}
        ]);
        let mut original = json!({"status":"complete","detail_level":"summary","aspects":rows,"conflicts":[],"response_mode":"full","answer_format":"per_aspect","reused_evidence":[]});
        let before = original.clone();
        apply(&mut original);
        assert_eq!(original["format"], FORMAT);
        assert!(original["aspects"]["rows"].is_array());
        assert!(original.to_string().len() < before.to_string().len());
        assert_eq!(expand(original), before);
    }
    #[test]
    fn mixed_rows_and_partial_fallback_preserve_recovery_data() {
        let mut mixed = json!([{"quote":"x","line":1},{"quote":"y","value_line":2}]);
        let before = mixed.clone();
        table(&mut mixed);
        assert_eq!(mixed, before);
        for status in ["partial", "error"] {
            let mut v = json!({"status":status,"detail_level":"full","aspects":[{"question":"Still missing?","answer":"Maybe","status":"missing","self_contained":true}],"conflicts":[{"unresolved":true}],"limitations":["Timeout"]});
            let mut expected = v.clone();
            expected["aspects"][0]
                .as_object_mut()
                .unwrap()
                .remove("self_contained");
            apply(&mut v);
            assert_eq!(v, expected);
        }
    }
    #[test]
    fn certified_claims_only_omit_questions_when_complete_and_conflict_free() {
        for (certified, status, conflict, remove) in [
            (true, "found", false, true),
            (false, "found", false, false),
            (true, "missing", false, false),
            (true, "found", true, false),
        ] {
            let mut v = json!({"status":"complete","detail_level":"summary","aspects":[{"question":"Which button and state must be green?","question_id":"q1","answer":"Enabled primary Save must be green.","status":status,"self_contained":certified,"evidence":["e1"]}],"conflicts":if conflict{json!([{"description":"conflict","unresolved":false}])}else{json!([])}});
            apply(&mut v);
            let v = expand(v);
            assert_eq!(v["aspects"][0].get("question").is_none(), remove);
            assert!(v["aspects"][0].get("self_contained").is_none());
            assert_eq!(v["aspects"][0]["evidence"], json!(["e1"]));
        }
    }
}
