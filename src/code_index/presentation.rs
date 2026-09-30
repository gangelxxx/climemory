use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchSort {
    #[default]
    Path,
    Mtime,
    Relevance,
}

impl SearchSort {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.unwrap_or("path") {
            "path" => Ok(Self::Path),
            "mtime" => Ok(Self::Mtime),
            "relevance" => Ok(Self::Relevance),
            _ => Err(AppError::new("--sort must be path, mtime or relevance")),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Mtime => "mtime",
            Self::Relevance => "relevance",
        }
    }
}

/// Explicit output selection. No implicit result or token cap.
#[derive(Clone, Default)]
pub struct SearchPresentation {
    pub limit: Option<usize>,
    pub offset: usize,
    pub sort: SearchSort,
    pub reverse: bool,
    pub before: usize,
    pub after: usize,
}

impl SearchPresentation {
    pub fn page<T>(&self, matches: Vec<T>) -> Vec<T> {
        matches
            .into_iter()
            .skip(self.offset)
            .take(self.limit.unwrap_or(usize::MAX))
            .collect()
    }

    pub fn annotate_page(&self, summary: &mut Value, total: usize, emitted: usize) {
        let skipped = self.offset.min(total);
        let remaining = total.saturating_sub(skipped).saturating_sub(emitted);
        summary["offset"] = json!(self.offset);
        summary["limit"] = json!(self.limit);
        summary["sort"] = json!(self.sort.label());
        summary["reverse"] = json!(self.reverse);
        summary["skipped"] = json!(skipped);
        summary["remaining"] = json!(remaining);
        summary["next_offset"] = json!((remaining > 0).then(|| skipped + emitted));
        if skipped > 0 || remaining > 0 {
            summary["truncated_by"] = json!(match (skipped > 0, remaining > 0) {
                (true, true) => "offset+limit",
                (true, false) => "offset",
                _ => "limit",
            });
        }
    }

    pub fn continuation_args(&self, argv: &mut Vec<String>, next_offset: usize) {
        if let Some(limit) = self.limit {
            argv.extend(["--limit".into(), limit.to_string()]);
        }
        argv.extend(["--offset".into(), next_offset.to_string()]);
        if self.before > 0 {
            argv.extend(["--before-context".into(), self.before.to_string()]);
        }
        if self.after > 0 {
            argv.extend(["--after-context".into(), self.after.to_string()]);
        }
    }

    /// Read context only for the selected page, once per file. Overlapping
    /// windows share each context line once per query group; matching lines
    /// already have their own records. Preserve full line text.
    pub fn add_context(&self, project: &Project, records: &mut [Value]) {
        if self.before == 0 && self.after == 0 {
            return;
        }
        let mut files: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        let mut start = 0;
        while start < records.len() {
            let end = (start + 1..records.len())
                .find(|&i| {
                    !matches!(
                        records[i]["record"].as_str(),
                        Some("code_match" | "code_find_match")
                    )
                })
                .unwrap_or(records.len());
            if matches!(
                records[start]["record"].as_str(),
                Some("code_grep_summary" | "code_find_summary")
            ) {
                if self.before == self.after {
                    records[start]["context_lines"] = json!(self.before);
                }
                records[start]["before_context"] = json!(self.before);
                records[start]["after_context"] = json!(self.after);
                let mut occupied = BTreeSet::new();
                for record in &records[start + 1..end] {
                    if let (Some(path), Some(line)) =
                        (record["path"].as_str(), record["line"].as_u64())
                    {
                        occupied.insert((path.to_string(), line as usize));
                    }
                }
                for record in &mut records[start + 1..end] {
                    let (Some(path), Some(line)) =
                        (record["path"].as_str(), record["line"].as_u64())
                    else {
                        record["context_available"] = json!(false);
                        continue;
                    };
                    let path = path.to_string();
                    let lines = files.entry(path.clone()).or_insert_with(|| {
                        let file = open_regular_file_no_follow(&project.root.join(&path)).ok()?;
                        let mut bytes = Vec::new();
                        file.take(MAX_SOURCE_BYTES + 1)
                            .read_to_end(&mut bytes)
                            .ok()?;
                        if bytes.len() as u64 > MAX_SOURCE_BYTES {
                            return None;
                        }
                        Some(
                            String::from_utf8_lossy(&bytes)
                                .lines()
                                .map(str::to_string)
                                .collect(),
                        )
                    });
                    let Some(lines) = lines
                        .as_ref()
                        .filter(|lines| line > 0 && line as usize <= lines.len())
                    else {
                        record["context_available"] = json!(false);
                        continue;
                    };
                    let index = line as usize - 1;
                    if record["record"] == "code_match"
                        && record["text"].as_str() != Some(lines[index].as_str())
                    {
                        // A concurrent edit must not attach context to the wrong hit.
                        record["context_available"] = json!(false);
                        continue;
                    }
                    let mut context = |range: std::ops::Range<usize>| -> Vec<Value> {
                        range
                            .filter(|&i| occupied.insert((path.clone(), i + 1)))
                            .map(|i| json!({"line": i + 1, "text": lines[i]}))
                            .collect()
                    };
                    record["context_before"] =
                        json!(context(index.saturating_sub(self.before)..index));
                    record["context_after"] = json!(context(
                        index + 1
                            ..index
                                .saturating_add(1)
                                .saturating_add(self.after)
                                .min(lines.len())
                    ));
                    record["context_available"] = json!(true);
                    if record["record"] == "code_find_match" {
                        record["text"] = json!(lines[index]);
                    }
                }
            }
            start = end;
        }
    }
}
