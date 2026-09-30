//! Direct source selection. Jev decides; the host preserves original addresses.
use super::document_routing::{Classified, Section, Selection};
use super::*;
use std::time::Instant;

const BLOCK_BYTES: usize = 1400;
const MAX_BLOCKS: usize = 512;

fn blocks(parts: &[Value]) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    let mut headings: Vec<String> = Vec::new();
    let mut previous_path = Value::Null;
    for (chunk, part) in parts.iter().enumerate() {
        if part["path"] != previous_path {
            headings.clear();
            previous_path = part["path"].clone();
        }
        let text = part["text"]
            .as_str()
            .ok_or_else(|| AppError::new("missing original text"))?;
        let first = part["start_line"]
            .as_u64()
            .ok_or_else(|| AppError::new("missing original lines"))? as usize;
        let lines: Vec<_> = text.split_inclusive('\n').collect();
        let mut start = 0;
        while start < lines.len() {
            let heading = lines[start].trim_start();
            let level = heading.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&level) && heading.as_bytes().get(level) == Some(&b' ') {
                headings.truncate(level - 1);
                headings.push(heading.trim().to_owned());
            }
            let mut end = start;
            let mut size = 0;
            while end < lines.len() {
                if lines[end].len() > BLOCK_BYTES {
                    return Err(AppError::new("source block line exceeds classifier limit"));
                }
                if end > start
                    && (size + lines[end].len() > BLOCK_BYTES || lines[end].starts_with('#'))
                {
                    break;
                }
                size += lines[end].len();
                end += 1;
                if lines[end - 1].trim().is_empty() {
                    break;
                }
            }
            result.push(json!({"chunk":chunk+1,"path":part["path"],
                "start_line":first+start,"end_line":first+end-1,
                "headings":headings,"text":lines[start..end].concat(),
                "before":if start>0 {lines[start-1]} else {""},
                "after":lines.get(end).copied().unwrap_or("")}));
            if result.len() > MAX_BLOCKS {
                return Err(AppError::new("source block count limit"));
            }
            start = end;
        }
    }
    Ok(result)
}

pub(super) fn classify(
    project: &Project,
    config: &crate::classifier::ClassifierConfig,
    input: &Value,
    parts: &[Value],
    remaining: Duration,
    events: &mut Vec<Value>,
) -> Result<Classified> {
    let started = Instant::now();
    let candidates = blocks(parts)?;
    let source_revision = crate::util::digest(&serde_json::to_vec(parts)?);
    let mut kept = BTreeSet::new();
    let mut offset = 0;
    let mut batches = 0;
    let mut cache_hits = 0;
    while offset < candidates.len() {
        let mut end = (offset + config.max_candidates.min(20)).min(candidates.len());
        let (state, questions) = loop {
            if end <= offset {
                return Err(AppError::new("source block request limit"));
            }
            let (state, questions) = crate::classifier::block_request(
                &input["document_request"],
                &candidates[offset..end],
            );
            let bytes = serde_json::to_vec(
                &json!({"model":config.model,"state":state,"questions":questions}),
            )?;
            if bytes.len() <= config.max_input_bytes {
                break (state, questions);
            }
            end -= 1;
        };
        let key = crate::util::digest(&serde_json::to_vec(&json!([
            config.policy(),
            source_revision,
            state,
            questions
        ]))?);
        let path = directory(project, "runtime/documents")?.join(format!("blocks-{key}.json"));
        checked_file(&path)?;
        let cached = if path.exists() {
            serde_json::from_slice::<Value>(&read_state_bytes(&path)?).ok()
        } else {
            None
        };
        let cached_selection = cached.as_ref().and_then(|v| {
            crate::classifier::parse_blocks(v, end - offset, config.exclude_confidence).ok()
        });
        let selection = if let Some(selection) = cached_selection {
            cache_hits += 1;
            crate::statistics::cache("document_selection", true);
            selection
        } else {
            let mut meter = crate::usage::Meter::default();
            let call_started = Instant::now();
            let response = crate::classifier::decide(
                config,
                &state,
                &questions,
                remaining.saturating_sub(started.elapsed()),
                &mut meter,
            );
            let parsed = response
                .as_ref()
                .map_err(|e| AppError::new(e.to_string()))
                .and_then(|v| {
                    crate::classifier::parse_blocks(v, end - offset, config.exclude_confidence)
                });
            if meter.attempted {
                let mut event = json!({"event":"classifier_call","phase":"document_selection",
                    "mode":"source_blocks","provider":config.provider,"model":config.model,
                    "candidates":end-offset,"status":if parsed.is_ok() {"completed"} else {"error"},
                    "elapsed_ms":call_started.elapsed().as_millis()});
                meter.attach(&mut event);
                events.push(event);
            }
            let selection = parsed?;
            write_json(&path, &response?)?;
            selection
        };
        kept.extend(selection.kept.into_iter().map(|i| offset + i));
        batches += 1;
        offset = end;
    }
    if kept.is_empty() {
        return Err(AppError::new("classifier excluded all source blocks"));
    }
    let raw_count = kept.len();
    // Explicit cross-document references remain conservative. A verifier still reads
    // every full selected chunk, including any content outside the extraction range.
    loop {
        let before = kept.len();
        let references: Vec<_> = kept
            .iter()
            .map(|i| {
                (
                    candidates[*i]["path"].clone(),
                    candidates[*i]["text"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        for (i, block) in candidates.iter().enumerate() {
            if references.iter().any(|(own, text)| {
                *own != block["path"] && text.contains(block["path"].as_str().unwrap())
            }) {
                kept.insert(i);
            }
        }
        if kept.len() == before {
            break;
        }
    }
    let mut ranges = std::collections::BTreeMap::<usize, (usize, usize)>::new();
    for i in &kept {
        let block = &candidates[*i];
        let chunk = block["chunk"].as_u64().unwrap() as usize;
        let part = &parts[chunk - 1];
        let first = part["start_line"].as_u64().unwrap() as usize;
        let last = part["end_line"].as_u64().unwrap() as usize;
        let start = (block["start_line"].as_u64().unwrap() as usize)
            .saturating_sub(1)
            .max(first);
        let end = (block["end_line"].as_u64().unwrap() as usize + 1).min(last);
        ranges
            .entry(chunk)
            .and_modify(|r| {
                r.0 = r.0.min(start);
                r.1 = r.1.max(end);
            })
            .or_insert((start, end));
        // A source chunk boundary may split a definition from its rule.
        for neighbor in [
            chunk.checked_sub(1).filter(|n| *n > 0),
            (chunk < parts.len()).then_some(chunk + 1),
        ]
        .into_iter()
        .flatten()
        {
            let p = &parts[neighbor - 1];
            if p["path"] == part["path"]
                && ((neighbor < chunk && start == first) || (neighbor > chunk && end == last))
            {
                let line = p[if neighbor < chunk {
                    "end_line"
                } else {
                    "start_line"
                }]
                .as_u64()
                .unwrap() as usize;
                ranges
                    .entry(neighbor)
                    .and_modify(|r| {
                        r.0 = r.0.min(line);
                        r.1 = r.1.max(line);
                    })
                    .or_insert((line, line));
            }
        }
    }
    Ok(Classified {
        expansion: json!({"mode":"source_blocks","blocks":candidates.len(),"raw_selected_blocks":raw_count,
            "selected_blocks":kept.len(),"batches":batches,"cached_batches":cache_hits}),
        selection: Selection { selected_chunks:ranges.keys().copied().collect(),
            sections:ranges.into_iter().map(|(chunk,(start_line,end_line))| Section {chunk,start_line,end_line}).collect(),
            reuse_previous:false,rule_ids:vec![],issue_links:vec![],
            reason:"Jev selected original blocks; host retained boundaries and explicit references; full selected originals require verification".into() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blocks_preserve_unicode_lines_and_headings_without_invented_addresses() {
        let text = format!(
            "# Настройки\n\n{}\nSave must stay disabled.\n",
            "Русский текст.\n".repeat(200)
        );
        let parts =
            vec![json!({"path":"memory/docs/ui.md","text":text,"start_line":1,"end_line":204})];
        let b = blocks(&parts).unwrap();
        assert!(b.len() > 2);
        assert_eq!(
            b.iter()
                .map(|v| v["text"].as_str().unwrap())
                .collect::<String>(),
            text
        );
        assert!(b.iter().all(|v| v["headings"][0] == "# Настройки"));
        let mut line = 1;
        for v in b {
            assert_eq!(v["start_line"], line);
            line = v["end_line"].as_u64().unwrap() + 1;
        }
        assert_eq!(line, text.lines().count() as u64 + 1);
    }
    #[test]
    fn oversize_lines_fall_back_instead_of_truncating_evidence() {
        assert!(blocks(&[json!({"path":"x","text":"x".repeat(1401),"start_line":1})]).is_err());
    }
}
