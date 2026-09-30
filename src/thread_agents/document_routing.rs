//! Advisory selection of source chunks and subsets of already verified requirements.
use super::*;

pub(super) struct Classified {
    pub selection: Selection,
    pub expansion: Value,
}

fn classify_rules(
    config: &crate::classifier::ClassifierConfig,
    input: &Value,
    remaining: Duration,
    meter: &mut crate::usage::Meter,
) -> Result<Selection> {
    let packet = &input["verified_candidate"];
    let rules = packet["structured_requirements"]["rules"]
        .as_array()
        .filter(|r| !r.is_empty())
        .ok_or_else(|| AppError::new("missing verified rules"))?;
    let result = crate::classifier::select_rules(
        config,
        &input["document_request"],
        rules,
        &packet["structured_requirements"]["issues"],
        remaining,
        meter,
    )?;
    let rule_ids: Vec<_> = result.kept.into_iter().map(|id| id + 1).collect();
    if rule_ids.is_empty() {
        return Err(AppError::new("classifier selected no verified rules"));
    }
    Ok(Selection { selected_chunks:BTreeSet::new(), reuse_previous:false, rule_ids,
        issue_links:vec![], sections:vec![],
        reason:"Owner confirmed packet coverage; classifier selected verified rules; host preserves source text and linked issues".into() })
}

/// Optional classification; any failure returns to the existing document agent.
pub(super) fn classify(
    project: &Project,
    input: &Value,
    originals: &[Value],
    remaining: Duration,
    meter: &mut crate::usage::Meter,
    events: &mut Vec<Value>,
) -> Result<Option<Classified>> {
    let Some(config) = project
        .config
        .agent
        .classifier
        .as_ref()
        .filter(|c| c.enabled)
    else {
        return Ok(None);
    };
    if input["phase"] != "document_selection" || !input["previous_verified"].is_null() {
        return Ok(None);
    }
    if !input["verified_candidate"].is_null() {
        if input["approved_rule_packet"].as_str().is_none()
            || input["approved_rule_packet"] != input["verified_candidate"]["packet_id"]
        {
            return Ok(None);
        }
        return classify_rules(config, input, remaining, meter).map(|selection| {
            Some(Classified {
                selection,
                expansion: Value::Null,
            })
        });
    }
    if config.source_blocks {
        return super::document_blocks::classify(
            project, config, input, originals, remaining, events,
        )
        .map(Some);
    }
    let entries = input["indexes"]
        .as_array()
        .ok_or_else(|| AppError::new("missing classifier candidates"))?;
    let result = crate::classifier::select_chunks(
        config,
        &input["document_request"],
        entries,
        remaining,
        meter,
    )?;
    let mut kept: BTreeSet<usize> = result.kept.iter().copied().collect();
    let raw: Vec<_> = kept.iter().map(|i| i + 1).collect();
    for i in result.kept {
        for neighbor in [i.checked_sub(1), i.checked_add(1)].into_iter().flatten() {
            if neighbor < entries.len() && entries[neighbor]["path"] == entries[i]["path"] {
                kept.insert(neighbor);
            }
        }
    }
    let neighbors: Vec<_> = kept
        .iter()
        .map(|i| i + 1)
        .filter(|i| !raw.contains(i))
        .collect();
    let before_references = kept.clone();
    loop {
        let before = kept.len();
        let references: Vec<_> = kept
            .iter()
            .map(|i| {
                (
                    entries[*i]["index"].as_str().unwrap_or(""),
                    &entries[*i]["path"],
                )
            })
            .collect();
        for (i, entry) in entries.iter().enumerate() {
            if entry["path"].as_str().is_some_and(|p| {
                references
                    .iter()
                    .any(|(text, own_path)| **own_path != entry["path"] && text.contains(p))
            }) {
                kept.insert(i);
            }
        }
        if kept.len() == before {
            break;
        }
    }
    let references: Vec<_> = kept.difference(&before_references).map(|i| i + 1).collect();
    Ok(Some(Classified { expansion: json!({"raw_selected_chunks":raw,
        "neighbor_added_chunks":neighbors,"reference_added_chunks":references}), selection: Selection {
        selected_chunks: kept.into_iter().map(|i| i + 1).collect(),
        reuse_previous: false, rule_ids: vec![], issue_links: vec![], sections: vec![],
        reason: "Classifier retained relevant and uncertain chunks; originals still require document-agent review".into(),
    }}))
}

pub(super) const REFINEMENT_INSTRUCTIONS: &str = "classifier_hint is an advisory coarse selection, not an allowlist. Review ALL supplied indexes against document_request. Distinguish a dependency required to interpret an applicable rule (definition, condition, exception, precedence, or incomplete boundary) from a link to an unrelated topic. Keep required dependencies and uncertain boundaries; a mere adjacent chunk or general further-reading link does not by itself make that chunk relevant. You may restore chunks excluded by the classifier. Select precise original line sections when the index supports them, retaining headings and all applicable conditions, exceptions and conflicts. If uncertain, keep the full chunk. Independent verification still reads full selected originals. Never invent source ranges or resolve conflicts by dropping a source.";

pub(super) const INSTRUCTIONS: &str = "Select original document chunks for document_request using the supplied task-independent indexes. Return action=context, text=empty, memory={selected_chunks,reuse_previous,rule_ids,issue_links,sections,reason}. selected_chunks contains one-based chunk IDs. Select all dependencies, definitions, exceptions and relevant global/default page rules inherited by the requested component. A GLOBAL marker is not a reason to include unrelated topics. Respect explicit exclusions in a narrow query: mentioning colors only to exclude them does not request colors. For broad all-requirements queries include inherited typography and accessibility. Follow references and incomplete boundaries; when uncertain include the potentially relevant chunks. Select nonempty selected_chunks when reading originals. For reuse_previous or rule_ids reuse, selected_chunks may be empty because no original chunks need reading. Give a concise single-line reason for selection or reuse. Compare previous_request with document_request: clarifications already present in BOTH are not new scope or newly resolved issues merely because the report repeats them. If previous_verified is supplied, it may be a parent consultation packet or the pre-report packet. reuse_previous may be true ONLY when that packet already covers the entire current documentary question: changed wording alone is not new scope. Check every requested topic, condition and exception against previous_request and previous_verified. A report may reuse it only when it changes neither document scope nor applicable conditions, resolves no previous issue, and requests no new documentary facts. Existing unresolved issues alone do not forbid reuse: retain them unchanged when the report explicitly leaves their status, evidence, conditions and precedence unchanged. If any resolution or new evidence is supplied, read originals again. Do not reuse a narrow parent packet for a broader owner request. Otherwise false and select chunks for the updated request. Never treat report observations as documentary evidence. You cannot edit files or choose precedence between conflicting sources. verified_candidate is an optional earlier verified packet from the same sources and clarifications. Its structured_requirements.rules have one-based positional IDs scoped to that packet. For a narrower question fully covered by that packet, return rule_ids for exactly the relevant rules, including all dependencies, default rules, exceptions and both sides of relevant conflicts; reuse_previous must be false. The host copies those rules unchanged and retains related or unknown-scope unresolved issues. Never select a subset if any requested fact is missing; return rule_ids=[] and select original chunks instead. A new question is not necessarily covered by an earlier packet. Do not invent rules or IDs. Return rule_ids=[] when using reuse_previous. The verified_candidate.issue_links are a stable, query-independent classification. Do not reclassify issues. Return issue_links=[]; the host applies the saved classification, includes all linked dependencies when any linked rule is selected, and retains unknown-scope issues. For original reading, sections may contain {chunk,start_line,end_line} to narrow selected chunks to a single contiguous original range per chunk. Include headings, definitions, conditions, exceptions, global rules and referenced dependencies. Use source line addresses from indexes; if unsure, omit the section entry to read the entire selected chunk. Never narrow merely to hide a conflict. The host only applies sections when a verifier will independently read the full selected original chunks to recover omissions. For reuse, sections must be empty. Selection is advisory, not a completeness guarantee.";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    pub selected_chunks: BTreeSet<usize>,
    pub reuse_previous: bool,
    #[serde(default)]
    pub rule_ids: Vec<usize>,
    pub reason: String,
    #[serde(default)]
    pub issue_links: Vec<IssueLink>,
    #[serde(default)]
    pub sections: Vec<Section>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IssueLink {
    pub id: usize,
    pub rule_ids: BTreeSet<usize>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Section {
    pub chunk: usize,
    pub start_line: usize,
    pub end_line: usize,
}
impl Selection {
    pub fn sliced_parts(&self, parts: &[Value]) -> Result<Vec<Value>> {
        let mut result = parts.to_vec();
        let mut seen = BTreeSet::new();
        for section in &self.sections {
            if self.reuse_previous
                || !self.rule_ids.is_empty()
                || !self.selected_chunks.contains(&section.chunk)
                || section.chunk == 0
                || section.chunk > parts.len()
                || !seen.insert(section.chunk)
            {
                return Err(AppError::new(
                    "invalid document section chunk or reuse combination",
                ));
            }
            let part = &parts[section.chunk - 1];
            let first = part["start_line"].as_u64().unwrap() as usize;
            let last = part["end_line"].as_u64().unwrap() as usize;
            if section.start_line < first
                || section.end_line > last
                || section.end_line < section.start_line
            {
                return Err(AppError::new(
                    "document section is outside the indexed original chunk",
                ));
            }
            let lines: Vec<_> = part["text"]
                .as_str()
                .unwrap()
                .split_inclusive('\n')
                .collect();
            let begin = section.start_line - first;
            let end = section.end_line - first + 1;
            let offset: usize = lines[..begin].iter().map(|s| s.len()).sum();
            let text = lines[begin..end].concat();
            let start_byte = part["start_byte"].as_u64().unwrap_or(0) + offset as u64;
            let sliced = &mut result[section.chunk - 1];
            sliced["start_line"] = json!(section.start_line);
            sliced["end_line"] = json!(section.end_line);
            sliced["start_byte"] = json!(start_byte);
            sliced["end_byte"] = json!(start_byte + text.len() as u64);
            sliced["text"] = json!(text);
        }
        Ok(result)
    }
    pub fn scoped_subset(
        &self,
        all: &super::requirements::Requirements,
    ) -> (Vec<usize>, super::requirements::Requirements) {
        let mut ids: BTreeSet<_> = self.rule_ids.iter().copied().collect();
        loop {
            let before = ids.len();
            for link in &self.issue_links {
                if !ids.is_disjoint(&link.rule_ids) {
                    ids.extend(&link.rule_ids);
                }
            }
            if ids.len() == before {
                break;
            }
        }
        let issues = all
            .issues
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                self.issue_links
                    .iter()
                    .find(|l| l.id == i + 1)
                    .is_none_or(|l| l.rule_ids.is_empty() || !ids.is_disjoint(&l.rule_ids))
            })
            .map(|(_, issue)| issue.clone())
            .collect();
        let rules = ids.iter().map(|id| all.rules[id - 1].clone()).collect();
        (
            ids.into_iter().collect(),
            super::requirements::Requirements { rules, issues },
        )
    }
    pub fn valid(
        &self,
        count: usize,
        reuse_allowed: bool,
        rule_count: usize,
        issue_count: usize,
    ) -> bool {
        !self.reason.trim().is_empty()
            && self.reason.chars().count() <= 1600
            && !self.reason.chars().any(char::is_control)
            && (!self.selected_chunks.is_empty()
                || self.reuse_previous
                || !self.rule_ids.is_empty())
            && self.selected_chunks.iter().all(|i| *i > 0 && *i <= count)
            && (!self.reuse_previous || reuse_allowed)
            && (!self.reuse_previous || self.rule_ids.is_empty())
            && self.rule_ids.iter().all(|id| *id > 0 && *id <= rule_count)
            && self.rule_ids.iter().collect::<BTreeSet<_>>().len() == self.rule_ids.len()
            && self.issue_links.iter().all(|l| {
                l.id > 0
                    && l.id <= issue_count
                    && l.rule_ids.iter().all(|id| *id > 0 && *id <= rule_count)
            })
            && self
                .issue_links
                .iter()
                .map(|l| l.id)
                .collect::<BTreeSet<_>>()
                .len()
                == self.issue_links.len()
    }
}

pub(super) fn schema() -> Value {
    json!({"type":"object","properties":{"action":{"type":"string","enum":["context"]},"text":{"type":"string"},"memory":{"type":"object","properties":{"selected_chunks":{"type":"array","items":{"type":"integer","minimum":1}},"rule_ids":{"type":"array","items":{"type":"integer","minimum":1}},"reuse_previous":{"type":"boolean"},"sections":{"type":"array","items":{"type":"object","properties":{"chunk":{"type":"integer","minimum":1},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}},"required":["chunk","start_line","end_line"],"additionalProperties":false}},"issue_links":{"type":"array","items":{"type":"object","properties":{"id":{"type":"integer","minimum":1},"rule_ids":{"type":"array","items":{"type":"integer","minimum":1}}},"required":["id","rule_ids"],"additionalProperties":false}},"reason":{"type":"string"}},"required":["selected_chunks","reuse_previous","rule_ids","issue_links","sections","reason"],"additionalProperties":false}},"required":["action","text","memory"],"additionalProperties":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subset_ids_must_exist_be_unique_and_not_mix_reuse_modes() {
        let mut s = Selection {
            selected_chunks: BTreeSet::from([1]),
            reuse_previous: false,
            rule_ids: vec![1, 3],
            reason: "Narrow question covered".into(),
            issue_links: vec![],
            sections: vec![],
        };
        assert!(s.valid(1, false, 3, 0));
        s.selected_chunks.clear();
        assert!(s.valid(1, false, 3, 0));
        assert!(!s.valid(1, false, 0, 0));
        s.rule_ids = vec![0];
        assert!(!s.valid(1, false, 3, 0));
        s.rule_ids = vec![1, 1];
        assert!(!s.valid(1, false, 3, 0));
        s.rule_ids = vec![4];
        assert!(!s.valid(1, false, 3, 0));
        s.rule_ids = vec![1];
        s.reuse_previous = true;
        assert!(!s.valid(1, true, 3, 0));
    }
    #[test]
    fn issue_scope_closes_dependencies_and_preserves_unknown_issues() {
        let all: super::super::requirements::Requirements = serde_json::from_value(json!({
            "rules": [
                {"rule":"Font", "when":"always", "sources":[]},
                {"rule":"Green", "when":"enabled", "sources":[]},
                {"rule":"Blue", "when":"enabled", "sources":[]},
                {"rule":"White text", "when":"enabled", "sources":[]}
            ], "issues":["Color conflict", "Contrast dependency", "Unknown", "Unlinked"]
        }))
        .unwrap();
        let mut s: Selection = serde_json::from_value(json!({
            "selected_chunks":[], "reuse_previous":false,"rule_ids":[1],"reason":"Font only",
            "issue_links":[{"id":1,"rule_ids":[2,3]},{"id":2,"rule_ids":[3,4]},{"id":3,"rule_ids":[]},{"id":4,"rule_ids":[]}]
        })).unwrap();
        assert!(s.valid(1, false, 4, 4));
        let (ids, subset) = s.scoped_subset(&all);
        assert_eq!(ids, vec![1]);
        assert_eq!(subset.issues, vec!["Unknown", "Unlinked"]);
        s.rule_ids = vec![2];
        let (ids, subset) = s.scoped_subset(&all);
        assert_eq!(ids, vec![2, 3, 4]);
        assert_eq!(subset.issues, all.issues);
        s.issue_links[0].id = 5;
        assert!(!s.valid(1, false, 4, 4));
        s.issue_links[0].id = 2;
        assert!(!s.valid(1, false, 4, 4));
        s.issue_links[0].id = 1;
        s.issue_links[0].rule_ids.insert(5);
        assert!(!s.valid(1, false, 4, 4));
        s.issue_links[0].rule_ids.remove(&5);
        s.issue_links.pop();
        assert!(
            s.valid(1, false, 4, 4),
            "selection no longer classifies issues"
        );
    }

    #[test]
    fn section_reads_preserve_original_unicode_bytes_and_line_addresses() {
        // Escapes keep the fixture independent of the shell's source-file encoding.
        let prefix = "\u{416}\r\n";
        let selected_text = "\u{1f7e2}\u{e9}\r\n";
        let original = format!("{prefix}{selected_text}End");
        assert_eq!(prefix.len(), 4);
        assert_eq!(selected_text.len(), 8);
        let parts = vec![
            json!({"path":"memory/docs/ui.md","start_line":5,"end_line":7,
            "start_byte":100,"end_byte":100+original.len(),"text":original}),
        ];
        let mut s: Selection=serde_json::from_value(json!({"selected_chunks":[1],
            "reuse_previous":false,"reason":"One rule", "sections":[{"chunk":1,"start_line":6,"end_line":6}]})).unwrap();
        let selected = s.sliced_parts(&parts).unwrap();
        assert_eq!(selected[0]["text"], selected_text);
        assert_eq!(selected[0]["start_line"], 6);
        assert_eq!(selected[0]["end_line"], 6);
        assert_eq!(selected[0]["start_byte"], 104);
        assert_eq!(selected[0]["end_byte"], 112);
        assert_eq!(&original[4..12], selected_text);
        s.sections[0].end_line = 8;
        assert!(s.sliced_parts(&parts).is_err());
        s.sections[0].end_line = 6;
        s.reuse_previous = true;
        assert!(s.sliced_parts(&parts).is_err());
        s.reuse_previous = false;
        s.sections.clear();
        assert_eq!(s.sliced_parts(&parts).unwrap(), parts);
    }
}
