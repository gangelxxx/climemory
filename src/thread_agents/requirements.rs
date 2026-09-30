//! Typed document extraction and deterministic presentation. Never truncate rules.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct Requirements {
    pub rules: Vec<Rule>,
    pub issues: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rule {
    pub rule: String,
    pub when: String,
    pub sources: Vec<Source>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Source {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

pub(super) fn schema() -> Value {
    let source = json!({"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}},"required":["path","start_line","end_line"],"additionalProperties":false});
    let rule = json!({"type":"object","properties":{"rule":{"type":"string"},"when":{"type":"string"},"sources":{"type":"array","items":source}},"required":["rule","when","sources"],"additionalProperties":false});
    json!({"type":"object","properties":{"action":{"type":"string","enum":["context"]},"text":{"type":"string"},"memory":{"type":"object","properties":{"rules":{"type":"array","items":rule},"issues":{"type":"array","items":{"type":"string"}}},"required":["rules","issues"],"additionalProperties":false}},"required":["action","text","memory"],"additionalProperties":false})
}

impl Requirements {
    pub fn parse(value: &Value, read_sources: &[Value]) -> Result<Self> {
        Self::parse_inner(value, Some(read_sources))
    }
    pub fn draft(value: &Value) -> Result<Self> {
        Self::parse_inner(value, None)
    }
    fn parse_inner(value: &Value, read_sources: Option<&[Value]>) -> Result<Self> {
        let mut result: Self = serde_json::from_value(value.clone()).map_err(|_| {
            AppError::new(
                "document requirements must contain rules (rule, when, sources) and issues",
            )
        })?;
        if result.rules.len() > 256 || result.issues.len() > 64 {
            return Err(AppError::new(
                "too many document requirements; preserve conditions and narrow the question",
            ));
        }
        for rule in &mut result.rules {
            rule.rule = rule.rule.trim().into();
            rule.when = rule.when.trim().into();
            if rule.rule.is_empty()
                || rule
                    .rule
                    .chars()
                    .chain(rule.when.chars())
                    .any(char::is_control)
                || rule.sources.is_empty()
            {
                return Err(AppError::new("each document rule requires single-line text, an optional condition and source references"));
            }
            for source in &rule.sources {
                // Draft citations are untrusted hints for the verifier, never
                // delivered as evidence. It can also repair zero/reversed ranges.
                let Some(read_sources) = read_sources else {
                    continue;
                };
                if source.start_line == 0
                    || source.end_line < source.start_line
                    || source.end_line == usize::MAX
                {
                    return Err(AppError::new("invalid document source line range"));
                }
                let mut ranges: Vec<_> = read_sources
                    .iter()
                    .filter(|v| v["path"] == source.path)
                    .map(|v| {
                        (
                            v["start_line"].as_u64().unwrap() as usize,
                            v["end_line"].as_u64().unwrap() as usize,
                        )
                    })
                    .collect();
                ranges.sort();
                let mut cursor = source.start_line;
                for (start, end) in ranges {
                    if start <= cursor && end >= cursor {
                        cursor = end.saturating_add(1);
                    }
                    if cursor > source.end_line {
                        break;
                    }
                }
                if cursor <= source.end_line {
                    return Err(AppError::new(format!(
                        "document source was not read: {}:{}-{}",
                        source.path, source.start_line, source.end_line
                    )));
                }
            }
        }
        for issue in &mut result.issues {
            *issue = issue.trim().into();
            if issue.is_empty() || issue.chars().any(char::is_control) {
                return Err(AppError::new(
                    "document issues must be nonempty single-line text",
                ));
            }
        }
        // Identical model-produced rows add no information. Conditions participate
        // in equality, so different states and exceptions can never be merged away.
        let mut seen = BTreeSet::new();
        result
            .rules
            .retain(|r| seen.insert(serde_json::to_string(r).unwrap()));
        let mut seen = BTreeSet::new();
        result.issues.retain(|s| seen.insert(s.clone()));
        if result.render().chars().count() > MAX_MESSAGE {
            return Err(AppError::new(
                "document requirements exceed the output budget; nothing was truncated or saved",
            ));
        }
        Ok(result)
    }

    pub fn render(&self) -> String {
        let mut paths: Vec<&str> = Vec::new();
        let mut lines = Vec::new();
        for rule in &self.rules {
            let mut citations = Vec::new();
            for source in &rule.sources {
                let id = if let Some(i) = paths.iter().position(|p| *p == source.path) {
                    i + 1
                } else {
                    paths.push(&source.path);
                    paths.len()
                };
                citations.push(if source.start_line == source.end_line {
                    format!("{id}:{}", source.start_line)
                } else {
                    format!("{id}:{}-{}", source.start_line, source.end_line)
                });
            }
            citations.sort();
            citations.dedup();
            let condition = if rule.when.is_empty() {
                String::new()
            } else {
                format!("{}: ", rule.when)
            };
            lines.push(format!(
                "- {condition}{} [{}]",
                rule.rule,
                citations.join(", ")
            ));
        }
        for issue in &self.issues {
            lines.push(format!("- Unresolved: {issue}"));
        }
        for (i, path) in paths.iter().enumerate() {
            lines.push(format!("[{}] {path}", i + 1));
        }
        if lines.is_empty() {
            "No applicable requirements found in selected sections.".into()
        } else {
            lines.join("\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_render_keeps_conditions_and_groups_sources() {
        let source = json!({"path":"memory/docs/ui.md","start_line":1,"end_line":4});
        let active = json!({"rule":"Green","when":"active","sources":[source.clone()]});
        let disabled = json!({"rule":"Grey","when":"disabled","sources":[source.clone()]});
        let parsed = Requirements::parse(
            &json!({"rules":[active.clone(),disabled,active],"issues":["Size unspecified"]}),
            &[source],
        )
        .unwrap();
        assert_eq!(parsed.rules.len(), 2);
        let rendered = parsed.render();
        assert!(rendered.contains("active: Green"));
        assert!(rendered.contains("disabled: Grey"));
        assert!(rendered.contains("Unresolved: Size unspecified"));
        assert_eq!(rendered.matches("memory/docs/ui.md").count(), 1);
    }
    #[test]
    fn citations_require_contiguous_read_source_ranges() {
        let sources = [
            json!({"path":"memory/docs/ui.md","start_line":1,"end_line":2}),
            json!({"path":"memory/docs/ui.md","start_line":4,"end_line":6}),
        ];
        for (path, start, end) in [
            ("memory/docs/missing.md", 1, 1),
            ("memory/docs/ui.md", 1, 4),
            ("memory/docs/ui.md", 0, 1),
            ("memory/docs/ui.md", 4, 7),
        ] {
            assert!(Requirements::parse(&json!({"rules":[{"rule":"Green","when":"active","sources":[{"path":path,"start_line":start,"end_line":end}]}],"issues":[]}), &sources).is_err());
        }
        assert!(Requirements::parse(&json!({"rules":[],"issues":[]}), &sources).is_ok());
    }
}
