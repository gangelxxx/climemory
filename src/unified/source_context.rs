//! Bounded context from already-selected, fully consulted small user documents.
use super::*;

const MAX_RECEIPTS: usize = 128;

pub(super) struct Capsule {
    path: String,
    revision: String,
    lines: Vec<String>,
    quoted_bytes: usize,
}

pub(super) struct Delivery<'a> {
    pub candidates: &'a [Capsule],
    pub evidence_budget: usize,
    pub received: &'a mut BTreeMap<String, String>,
    pub requested: &'a [String],
    pub intents: &'a [scope::Intent],
}

pub(super) fn validate_receipts(receipts: &BTreeMap<String, String>) -> Result<()> {
    if receipts.len() > MAX_RECEIPTS
        || receipts.iter().any(|(path, revision)| {
            path.is_empty()
                || path.len() > 4096
                || path.chars().any(char::is_control)
                || revision.len() != 64
                || !revision
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
    {
        return Err(AppError::new(
            "invalid delivered source context; start a new topic without @context",
        ));
    }
    Ok(())
}

pub(super) fn candidates(
    index: &Index,
    result: &Value,
    restricted: &BTreeSet<String>,
) -> Vec<Capsule> {
    if result["status"] != "complete" || !result["conflicts"].as_array().is_some_and(Vec::is_empty)
    {
        return vec![];
    }
    let reviewed: BTreeSet<_> = result["coverage"]["reviewed_threads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let selected = result["evidence"].as_array().cloned().unwrap_or_default();
    let mut choices = Vec::new();
    for source in &index.sources {
        if !index::source_is_cohesive(source) {
            continue;
        }
        let rows: Vec<_> = selected
            .iter()
            .filter(|r| r["source"] == source.path)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let owners: Vec<_> = index
            .threads
            .iter()
            .filter(|t| t.source == source.id)
            .collect();
        if owners.len() != 1
            || !reviewed.contains(owners[0].id.as_str())
            || restricted.contains(&owners[0].id)
        {
            continue;
        }
        let lines: Vec<String> = source.text.lines().map(str::to_owned).collect();
        // Every quote of this source must retain its exact original line address.
        if rows.is_empty() || !rows.iter().all(|r| matches_line(r, &lines)) {
            continue;
        }
        // Do not trust merely having one owner: all nonblank lines must belong to
        // that consulted owner, with the exact current source text and address.
        if !lines
            .iter()
            .enumerate()
            .filter(|(_, text)| !text.trim().is_empty())
            .all(|(n, text)| {
                owners[0]
                    .fragments
                    .iter()
                    .any(|f| f.source == source.id && f.line == n + 1 && &f.text == text)
            })
        {
            continue;
        }
        choices.push(Capsule {
            path: source.path.clone(),
            revision: source.revision.clone(),
            lines,
            quoted_bytes: rows
                .iter()
                .filter_map(|r| r["quote"].as_str())
                .map(str::len)
                .sum(),
        });
    }
    choices.sort_by(|a, b| {
        b.quoted_bytes
            .cmp(&a.quoted_bytes)
            .then(a.path.cmp(&b.path))
    });
    choices
}

fn matches_line(row: &Value, lines: &[String]) -> bool {
    row["line"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| lines.get(n))
        .is_some_and(|line| row["quote"].as_str() == Some(line.as_str()))
}

pub(super) fn apply(
    output: &mut Value,
    candidates: &[Capsule],
    delivered: &mut BTreeMap<String, String>,
    evidence_budget: usize,
) -> bool {
    if output["status"] != "complete"
        || output["detail_level"] != "summary"
        || !output["conflicts"].as_array().is_some_and(Vec::is_empty)
        || output.get("source_blocks").is_some()
    {
        return false;
    }
    for candidate in candidates {
        if delivered.get(&candidate.path) == Some(&candidate.revision)
            || (delivered.len() >= MAX_RECEIPTS && !delivered.contains_key(&candidate.path))
        {
            continue;
        }
        let Some(rows) = output.get("evidence").and_then(Value::as_array) else {
            return false;
        };
        let selected: Vec<_> = rows
            .iter()
            .filter(|r| r["source"] == candidate.path)
            .collect();
        if selected.is_empty() || !selected.iter().all(|r| matches_line(r, &candidate.lines)) {
            continue;
        }
        let addressed: Vec<_> = candidate
            .lines
            .iter()
            .enumerate()
            .map(|(n, line)| json!([n + 1, line]))
            .collect();
        let block = json!({"b1":{"source":candidate.path,"authority":"user_document","review":"source_context","numbered_lines":addressed}});
        // Conservative: retain the cost of inline selected quotes too, even
        // though the capsule removes them. Never increase the existing allowance.
        let evidence_chars: usize = rows.iter().map(|r| r.to_string().chars().count()).sum();
        if evidence_chars.saturating_add(block.to_string().chars().count()) > evidence_budget {
            continue;
        }
        for row in output["evidence"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .filter(|r| r["source"] == candidate.path)
        {
            let obj = row.as_object_mut().unwrap();
            obj.remove("quote");
            obj.remove("source");
            obj.insert("source_block".into(), json!("b1"));
        }
        output["source_blocks"] = block;
        delivered.insert(candidate.path.clone(), candidate.revision.clone());
        return true; // One selected source per reply; never prefetch an unselected document.
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(text: &str) -> (Index, Value) {
        let source = index::Source {
            id: "doc-test".into(),
            path: "memory/docs/test.md".into(),
            revision: digest(text),
            authority: "user_document".into(),
            text: text.into(),
            title: "Test".into(),
            agent: "cheap".into(),
            parent: None,
            pointer: None,
            addresses: vec![],
            claims: vec![],
        };
        let index = index::build(vec![source], None, true).unwrap();
        let result = json!({"status":"complete","conflicts":[],"coverage":{"reviewed_threads":["doc-test"]},"evidence":[{"source":"memory/docs/test.md","line":3,"quote":text.lines().nth(2).unwrap_or("")}]});
        (index, result)
    }
    #[test]
    fn crlf_blank_lines_and_unreviewed_context_survive_without_changing_claims() {
        let (index, mut result) =
            setup("# UI\r\n\r\n  Save is green.  \r\n\r\nKeyboard is required.\r\n");
        let candidates = candidates(&index, &result, &BTreeSet::new());
        assert_eq!(candidates.len(), 1);
        result["detail_level"] = json!("summary");
        result["aspects"] = json!([{"status":"found","answer":"Save is green","evidence":["e1"]}]);
        result["evidence"][0]["ref"] = json!("e1");
        let claims = result["aspects"].clone();
        let mut delivered = BTreeMap::new();
        apply(&mut result, &candidates, &mut delivered, 24000);
        assert_eq!(
            result["source_blocks"]["b1"]["numbered_lines"],
            json!([
                [1, "# UI"],
                [2, ""],
                [3, "  Save is green.  "],
                [4, ""],
                [5, "Keyboard is required."]
            ])
        );
        assert_eq!(result["source_blocks"]["b1"]["review"], "source_context");
        assert_eq!(result["aspects"], claims);
        assert!(result["evidence"][0].get("quote").is_none());
        assert_eq!(result["evidence"][0]["line"], 3);
        assert_eq!(delivered.len(), 1);
        let mut again = json!({"status":"complete","detail_level":"summary","conflicts":[],"evidence":[{"source":"memory/docs/test.md","line":3,"quote":"  Save is green.  "}]});
        apply(&mut again, &candidates, &mut delivered, 24000);
        assert!(again.get("source_blocks").is_none());
        assert_eq!(again["evidence"][0]["quote"], "  Save is green.  ");
    }
    #[test]
    fn no_capsule_for_partial_conflict_unconsulted_owner_mismatch_or_large_source() {
        let (index, result) = setup("# UI\n\nSave is green.\n");
        for case in ["partial", "conflict", "unconsulted", "quote", "address"] {
            let mut bad = result.clone();
            match case {
                "partial" => bad["status"] = json!("partial"),
                "conflict" => bad["conflicts"] = json!([{"unresolved":false}]),
                "unconsulted" => bad["coverage"]["reviewed_threads"] = json!([]),
                "quote" => bad["evidence"][0]["quote"] = json!("Save green."),
                _ => bad["evidence"][0]["line"] = json!(2),
            };
            assert!(
                candidates(&index, &bad, &BTreeSet::new()).is_empty(),
                "{case}"
            );
        }
        let (large, large_result) = setup(&format!("# UI\n\n{}", "界".repeat(1000)));
        assert!(candidates(&large, &large_result, &BTreeSet::new()).is_empty());
        let mut broken = index.clone();
        broken
            .threads
            .iter_mut()
            .find(|t| t.id == "doc-test")
            .unwrap()
            .fragments
            .clear();
        assert!(candidates(&broken, &result, &BTreeSet::new()).is_empty());
    }
    #[test]
    fn receipt_capacity_revision_budget_and_single_choice() {
        let (index, mut result) = setup("# UI\n\nSave is green.\n");
        assert!(candidates(&index, &result, &BTreeSet::from(["doc-test".into()])).is_empty());
        let mut choices = candidates(&index, &result, &BTreeSet::new());
        result["detail_level"] = json!("summary");
        let original = result.clone();
        let mut received: BTreeMap<_, _> = (0..128)
            .map(|n| (format!("doc{n}"), "a".repeat(64)))
            .collect();
        apply(&mut result, &choices, &mut received, 24000);
        assert_eq!(result, original);
        assert_eq!(received.len(), 128);
        received.remove("doc0");
        received.insert("memory/docs/test.md".into(), "b".repeat(64));
        let before = received.clone();
        apply(&mut result, &choices, &mut received, 1);
        assert_eq!(result, original);
        assert_eq!(received, before);
        // Updating an existing receipt is allowed even at the capacity limit.
        apply(&mut result, &choices, &mut received, 24000);
        assert!(result["source_blocks"].is_object());
        assert_eq!(received["memory/docs/test.md"], index.sources[0].revision);
        assert_eq!(received.len(), 128);
        choices.push(Capsule {
            path: "memory/docs/other.md".into(),
            revision: "c".repeat(64),
            lines: choices[0].lines.clone(),
            quoted_bytes: 1,
        });
        let mut two = original;
        let mut second = two["evidence"][0].clone();
        second["source"] = json!("memory/docs/other.md");
        two["evidence"].as_array_mut().unwrap().push(second);
        let mut fresh = BTreeMap::new();
        apply(&mut two, &choices, &mut fresh, 24000);
        assert_eq!(two["source_blocks"].as_object().unwrap().len(), 1);
        assert_eq!(fresh.len(), 1);
        assert_eq!(two["evidence"][1]["quote"], "Save is green.");
    }
    #[test]
    fn receipts_are_bounded_and_invalid_state_fails_closed() {
        for bad in [
            BTreeMap::from([("x".into(), "not-a-revision".into())]),
            (0..129)
                .map(|n| (format!("doc{n}"), "a".repeat(64)))
                .collect(),
        ] {
            assert!(validate_receipts(&bad).is_err());
        }
        assert!(validate_receipts(&BTreeMap::new()).is_ok());
    }
}
