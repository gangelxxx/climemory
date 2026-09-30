use super::*;

#[derive(Clone, Copy, Default)]
struct Metrics {
    bytes: usize,
    wordish: usize,
    structural: usize,
    non_ascii: usize,
}

impl Metrics {
    fn read(text: &str) -> Self {
        let (wordish, structural, non_ascii) = crate::token_estimate::token_components(text);
        Self {
            bytes: text.len(),
            wordish,
            structural,
            non_ascii,
        }
    }

    fn replace(&mut self, old: Self, new: Self) {
        self.bytes = self.bytes - old.bytes + new.bytes;
        self.wordish = self.wordish - old.wordish + new.wordish;
        self.structural = self.structural - old.structural + new.structural;
        self.non_ascii = self.non_ascii - old.non_ascii + new.non_ascii;
    }

    fn stats(self) -> OutputStats {
        OutputStats {
            bytes: self.bytes,
            chars: self.wordish + self.structural + self.non_ascii,
            estimated_tokens: self.wordish.div_ceil(4)
                + self.structural.div_ceil(2)
                + self.non_ascii,
        }
    }
}

/// Code search emits independent records, without the batch-dependent health
/// projection. Serialize data once; only summary fields change at the fixpoint.
fn serialize_search_records(records: &mut [Value], summaries: &[usize]) -> Result<String> {
    let summaries: BTreeSet<usize> = summaries.iter().copied().collect();
    for &index in &summaries {
        if !records.get(index).is_some_and(Value::is_object) {
            return Err(AppError::new("output summary must be a JSON object"));
        }
    }
    let mut total = Metrics::default();
    let mut fragments = records
        .iter()
        .map(|record| {
            let mut text = serde_json::to_string(record)?;
            text.push('\n');
            let metrics = Metrics::read(&text);
            total.replace(Metrics::default(), metrics);
            Ok((text, metrics))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut last = None;
    for _ in 0..8 {
        let stats = total.stats();
        if last == Some(stats) {
            let mut text = String::with_capacity(stats.bytes);
            for (fragment, _) in fragments {
                text.push_str(&fragment);
            }
            return Ok(text);
        }
        for &index in &summaries {
            let record = &mut records[index];
            record["output_chars"] = json!(stats.chars);
            record["output_bytes"] = json!(stats.bytes);
            record["estimated_tokens"] = json!(stats.estimated_tokens);
            let mut text = serde_json::to_string(record)?;
            text.push('\n');
            let metrics = Metrics::read(&text);
            total.replace(fragments[index].1, metrics);
            fragments[index] = (text, metrics);
        }
        last = Some(stats);
    }
    // Retain the established non-convergence handling for unusual inputs.
    annotate_summaries(records, &summaries.into_iter().collect::<Vec<_>>())?;
    serialize_records(records)
}

pub fn write_search_records(records: &mut [Value], summaries: &[usize]) -> Result<()> {
    let text = serialize_search_records(records, summaries)?;
    match std::io::stdout().lock().write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_serialization_matches_full_batch_statistics_and_bytes() {
        for groups in [1, 2, 10] {
            for width in [0, 1, 7, 99, 1000] {
                let mut records = Vec::new();
                let mut indexes = Vec::new();
                for group in 0..groups {
                    indexes.push(records.len());
                    records.push(
                        json!({"record":"code_grep_summary", "query":group, "estimated_tokens":0}),
                    );
                    records.push(json!({"record":"code_match", "line":group+1, "text":format!("{}\t\n\\\"", "Ж😀ab_".repeat(width))}));
                }
                let mut expected = records.clone();
                annotate_summaries(&mut expected, &indexes).unwrap();
                let serialized = serialize_search_records(&mut records, &indexes).unwrap();
                assert_eq!(records, expected);
                assert_eq!(serialized, serialize_records(&expected).unwrap());
                assert_eq!(records[0]["output_bytes"], serialized.len());
                assert_eq!(records[0]["output_chars"], serialized.chars().count());
                assert_eq!(records[0]["estimated_tokens"], estimate_tokens(&serialized));
            }
        }
    }

    #[test]
    fn cached_serialization_validates_indexes_and_handles_empty_output() {
        assert_eq!(serialize_search_records(&mut [], &[]).unwrap(), "");
        assert!(serialize_search_records(&mut [], &[0]).is_err());
        assert!(serialize_search_records(&mut [json!(null)], &[0]).is_err());
        let mut records = vec![json!({"record":"code_find_count", "matches":0})];
        let mut expected = records.clone();
        annotate_summaries(&mut expected, &[0]).unwrap();
        assert_eq!(
            serialize_search_records(&mut records, &[0, 0]).unwrap(),
            serialize_records(&expected).unwrap()
        );
    }
}
