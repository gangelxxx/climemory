//! Deterministic retrieval of the same notes consumed and updated by agents.
use super::*;

pub(crate) fn note_limit(filters: &std::collections::BTreeMap<String, String>) -> usize {
    filters
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(8)
        .clamp(1, 8)
}

fn snapshot_source(project: &Project, id: &str, memory: &str, raw: &[u8]) -> Result<Option<Value>> {
    if memory.trim().is_empty() {
        return Ok(None);
    }
    let path = binding_path(project, id)?;
    let text =
        std::str::from_utf8(raw).map_err(|_| AppError::new("memory binding is not UTF-8"))?;
    Ok(Some(json!({
        "id":format!("memory:{id}"),"kind":"memory",
        "path":path.strip_prefix(fs::canonicalize(&project.root)?).map_err(|_|AppError::new("memory outside project"))?.to_string_lossy().replace('\\',"/"),
        "revision":crate::util::digest(raw),"authority":"advisory",
        "start_line":1,"end_line":text.lines().count(),"text":memory,"mandatory":false
    })))
}

/// Rank by identity, metadata and note content; never by generated source paths.
/// Parents follow direct matches. No model calls or storage mutations occur here.
pub(crate) fn select(
    project: &Project,
    task: &str,
    filters: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<Value>> {
    let mut snapshots = std::collections::BTreeMap::new();
    let records = list_memories(project, Some(&mut snapshots))?;
    let mut selected = rank_notes(records, task, filters, true);
    for record in selected.iter_mut().take(note_limit(filters)) {
        let id = record["thread_id"].as_str().unwrap();
        record["source"] = snapshot_source(
            project,
            id,
            record["memory"].as_str().unwrap(),
            &snapshots[id],
        )?
        .unwrap_or(Value::Null);
    }
    Ok(selected)
}

/// Explicit remote routing, separate from the deterministic context command.
pub(crate) fn route(project: &Project, task: &str) -> Result<(Vec<Value>, Value)> {
    let filters = Default::default();
    let mut snapshots = std::collections::BTreeMap::new();
    let inventory = list_memories(project, Some(&mut snapshots))?;
    let inventory_count = inventory.len();
    // Ordinary lexical/identity search first, including its related parent candidates.
    // Jev can filter this result, but cannot discover a thread the search missed.
    let records = rank_notes(inventory, task, &filters, true);
    let attach = |rows: &[Value]| -> Result<Vec<Value>> {
        rows.iter()
            .map(|row| {
                let mut row = row.clone();
                let id = row["thread_id"].as_str().unwrap();
                row["source"] = snapshot_source(
                    project,
                    id,
                    row["memory"].as_str().unwrap_or(""),
                    &snapshots[id],
                )?
                .unwrap_or(Value::Null);
                Ok(row)
            })
            .collect()
    };
    let fallback = |reason: &str| -> Result<(Vec<Value>, Value)> {
        Ok((
            attach(&records)?,
            json!({"status":"fallback","reason":reason,
            "stage":"search_then_classifier","inventory":inventory_count,
            "candidates":records.len(),"selected":records.len(),"excluded":0,"attempted":false}),
        ))
    };
    if records.is_empty() {
        return Ok((
            vec![],
            json!({"status":"empty","stage":"search_then_classifier",
            "inventory":inventory_count,"candidates":0,"selected":0,"excluded":0,"attempted":false,
            "reason":"No local matches; broaden or rephrase the search before consulting agents."}),
        ));
    }
    let Some(config) = project
        .config
        .agent
        .classifier
        .as_ref()
        .filter(|c| c.enabled)
    else {
        return fallback("classifier disabled");
    };
    if records.len() == 1 {
        return Ok((
            attach(&records)?,
            json!({"status":"local","stage":"search_then_classifier",
            "inventory":inventory_count,"candidates":1,"selected":1,"excluded":0,"attempted":false,
            "reason":"Single local candidate; classification cannot reduce consultations."}),
        ));
    }
    let candidates: Vec<_> = records
        .iter()
        .map(|r| {
            json!({"thread_id":r["thread_id"],
        "title":r["title"],"slug":r["slug"],"memory":r["memory"],"parent":r["parent"]})
        })
        .collect();
    let started = std::time::Instant::now();
    let mut meter = crate::usage::Meter::default();
    let selection = match crate::classifier::select(
        config,
        &json!(task),
        &candidates,
        Duration::from_millis(config.timeout_ms),
        &mut meter,
    ) {
        Ok(s) => s,
        Err(error) => {
            let (rows, mut meta) = fallback(&error.msg)?;
            meter.attach(&mut meta);
            meta["provider"] = json!(config.provider);
            meta["model"] = json!(config.model);
            meta["attempted"] = json!(meter.attempted);
            return Ok((rows, meta));
        }
    };
    let mut positions = selection.kept;
    let identity = |i: usize| {
        records[i]["slug"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(task.trim()))
            || records[i]["thread_id"] == task.trim()
    };
    // An explicit identity lookup cannot be vetoed by a classifier.
    for i in 0..records.len() {
        if identity(i) && !positions.contains(&i) {
            positions.push(i);
        }
    }
    // This is a filter over ordinary search, not a second ranking algorithm.
    positions.sort_unstable();
    // Parents were search candidates too. Do not re-add excluded parents: the
    // selected thread agent can consult its parent if it needs that context.
    let mut selected = Vec::new();
    for i in positions {
        let mut record = records[i].clone();
        let id = record["thread_id"].as_str().unwrap();
        record["source"] = snapshot_source(
            project,
            id,
            record["memory"].as_str().unwrap_or(""),
            &snapshots[id],
        )?
        .unwrap_or(Value::Null);
        record["selection_reason"] = json!(if identity(i) {
            "explicit_identity"
        } else {
            "search_then_classifier"
        });
        selected.push(record);
    }
    let mut metadata = json!({"status":"selected","stage":"search_then_classifier","inventory":inventory_count,
        "candidates":records.len(),"selected":selected.len(),"excluded":records.len()-selected.len(),
        "elapsed_ms":started.elapsed().as_millis(),"model":config.model});
    meter.attach(&mut metadata);
    metadata["provider"] = json!(config.provider);
    metadata["attempted"] = json!(meter.attempted);
    Ok((selected, metadata))
}

fn rank_notes(
    bindings: Vec<Value>,
    task: &str,
    filters: &std::collections::BTreeMap<String, String>,
    include_parents: bool,
) -> Vec<Value> {
    let query = PreparedQuery::new(task, filters);
    let mut ranked = Vec::new();
    for binding in &bindings {
        let slug = binding["slug"].as_str().unwrap_or_default();
        let identity = task.trim().eq_ignore_ascii_case(slug)
            || Some(task.trim()) == binding["thread_id"].as_str();
        let metadata = format!("{} {}", slug, binding["title"].as_str().unwrap_or_default());
        let memory = binding["memory"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.starts_with("Source:"))
            .collect::<Vec<_>>()
            .join("\n");
        let searchable = format!("{metadata}\n{memory}");
        // Listing and queries without scoring terms need no lexical tokenization.
        // Literal phrase matching still uses the original searchable text below.
        let (metadata_terms, memory_terms) = if query.terms.is_empty() {
            (BTreeSet::new(), BTreeSet::new())
        } else {
            (
                crate::terms::link_gate_tokens(&metadata),
                crate::terms::link_gate_tokens(&memory),
            )
        };
        if !identity
            && !query.matches_prepared(&searchable, |term| {
                metadata_terms.contains(term) || memory_terms.contains(term)
            })
        {
            continue;
        }
        let score = usize::from(identity) * 1000
            + query
                .terms
                .iter()
                .map(|term| {
                    usize::from(metadata_terms.contains(term)) * 4
                        + usize::from(memory_terms.contains(term))
                })
                .sum::<usize>();
        // Matching decides inclusion; a literal phrase may have no scoring terms.
        let mut binding = binding.clone();
        binding["selection_reason"] = json!("task_match");
        binding["score"] = json!(score);
        ranked.push((score, binding));
    }
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1["slug"].as_str().cmp(&b.1["slug"].as_str()))
    });
    let mut result = ranked
        .into_iter()
        .map(|(_, binding)| binding)
        .collect::<Vec<_>>();
    if !include_parents {
        return result;
    }
    // Bound parent expansion to the candidates actually delivered in the packet.
    let mut ids = result
        .iter()
        .map(|b| b["thread_id"].as_str().unwrap_or_default().to_owned())
        .collect::<BTreeSet<_>>();
    let mut parents = result
        .iter()
        .take(note_limit(filters))
        .filter_map(|b| b["parent"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    for _ in 0..MAX_DEPTH {
        let mut next = Vec::new();
        for id in parents {
            if !ids.insert(id.clone()) {
                continue;
            }
            if let Some(parent) = bindings.iter().find(|b| b["thread_id"] == id) {
                let mut parent = parent.clone();
                parent["selection_reason"] = json!("parent_context");
                if let Some(id) = parent["parent"].as_str() {
                    next.push(id.to_owned());
                }
                result.push(parent);
            }
        }
        parents = next;
        if parents.is_empty() {
            break;
        }
    }
    result
}

pub(super) struct PreparedQuery {
    terms: Vec<String>,
    empty: bool,
    all: bool,
    phrase: Option<String>,
}

fn normalize_phrase(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

impl PreparedQuery {
    pub(super) fn new(query: &str, filters: &std::collections::BTreeMap<String, String>) -> Self {
        let mode = filters.get("match").map(String::as_str);
        Self {
            terms: crate::terms::link_gate_terms(&crate::terms::search_terms(query)),
            empty: query.trim().is_empty(),
            all: matches!(mode, Some("all" | "thread-all" | "thread_all")),
            phrase: (mode == Some("phrase")).then(|| normalize_phrase(query)),
        }
    }

    fn matches_prepared(&self, text: &str, contains: impl Fn(&String) -> bool) -> bool {
        if self.empty {
            return true;
        }
        if let Some(phrase) = &self.phrase {
            return !phrase.is_empty()
                && format!(" {} ", normalize_phrase(text)).contains(&format!(" {phrase} "));
        }
        !self.terms.is_empty()
            && if self.all {
                self.terms.iter().all(contains)
            } else {
                self.terms.iter().any(contains)
            }
    }

    #[cfg(test)]
    pub(super) fn matches(&self, text: &str) -> bool {
        if self.terms.is_empty() || self.phrase.is_some() {
            return self.matches_prepared(text, |_| false);
        }
        let tokens = crate::terms::link_gate_tokens(text);
        self.matches_prepared(text, |term| tokens.contains(term))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn termless_queries_preserve_listing_phrase_and_identity_results() {
        let records = vec![
            json!({"thread_id":"first", "slug":"the", "title":"The", "memory":"Unrelated text"}),
            json!({"thread_id":"second", "slug":"settings", "title":"Settings", "memory":"the and\nSource: hidden"}),
        ];
        let filters = std::collections::BTreeMap::new();
        let listed = rank_notes(records.clone(), "  ", &filters, false);
        assert_eq!(
            listed
                .iter()
                .map(|r| r["slug"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["settings", "the"]
        );
        assert!(listed.iter().all(|r| r["score"] == 0));
        for query in ["!!!", "the and"] {
            assert!(rank_notes(records.clone(), query, &filters, false).is_empty());
        }
        let identity = rank_notes(records.clone(), "the", &filters, false);
        assert_eq!(identity.len(), 1);
        assert_eq!(identity[0]["score"], 1000);
        let phrase = std::collections::BTreeMap::from([("match".into(), "phrase".into())]);
        let matched = rank_notes(records.clone(), "the and", &phrase, false);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0]["slug"], "settings");
        assert_eq!(matched[0]["score"], 0);
        assert!(rank_notes(records, "!!!", &phrase, false).is_empty());
    }

    #[test]
    fn prepared_tokens_preserve_matching_and_ranking_weights() {
        let texts = [
            "SettingsStorage saves changed preferences",
            "settings_storage: saved values; saving settings",
            "Настройки приложения сохраняют значения",
            "API x y the and 123 abcdef123456",
            "retention policies refreshCache HTTPSClient",
            "",
        ];
        let queries = [
            "",
            "settings storage",
            "saved preferences",
            "saving",
            "the and",
            "API",
            "x",
            "!!!",
            "настройки значения",
            "retention policies",
            "refreshCache",
        ];
        for mode in ["any", "all", "thread-all", "thread_all", "phrase"] {
            let filters = std::collections::BTreeMap::from([("match".into(), mode.into())]);
            for query in queries {
                let prepared = PreparedQuery::new(query, &filters);
                let terms = crate::terms::link_gate_terms(&crate::terms::search_terms(query));
                for text in texts {
                    let overlaps = terms
                        .iter()
                        .map(|term| {
                            crate::terms::link_gate_overlap(std::slice::from_ref(term), text)
                        })
                        .collect::<Vec<_>>();
                    let expected = if query.trim().is_empty() {
                        true
                    } else if mode == "phrase" {
                        let phrase = normalize_phrase(query);
                        !phrase.is_empty()
                            && format!(" {} ", normalize_phrase(text))
                                .contains(&format!(" {phrase} "))
                    } else {
                        !terms.is_empty()
                            && if mode == "any" {
                                overlaps.iter().any(|v| *v)
                            } else {
                                overlaps.iter().all(|v| *v)
                            }
                    };
                    assert_eq!(
                        prepared.matches(text),
                        expected,
                        "{mode}: {query:?}, {text:?}"
                    );
                    let tokens = crate::terms::link_gate_tokens(text);
                    for (term, overlap) in terms.iter().zip(overlaps) {
                        assert_eq!(tokens.contains(term), overlap, "{term:?}, {text:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn selected_source_and_hint_share_a_snapshot_but_new_reads_refresh() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("memory/threads")).unwrap();
        crate::config::Config::default()
            .save(&temp.path().join("memory/config.json"))
            .unwrap();
        let project = Project::open(temp.path()).unwrap();
        let id = fresh_id();
        let doc = crate::model::ThreadDoc::new_memory(
            id.clone(),
            "settings".into(),
            "Settings".into(),
            "Preferences".into(),
            "ui".into(),
            vec![],
        )
        .unwrap();
        project.persist(&doc).unwrap();
        let mut binding = Binding {
            format: BINDING_FORMAT.into(),
            thread_id: id.clone(),
            agent: Some("agent_medium".into()),
            provider: None,
            model: None,
            parent: None,
            memory: "Nebulacache stores preferences.".into(),
            revision: 1,
            last_dialogue: None,
            updated: iso_now(),
            document: None,
        };
        let path = binding_path(&project, &id).unwrap();
        write_json(&path, &binding).unwrap();
        let raw = fs::read(&path).unwrap();
        let filters = Default::default();
        let selected = select(&project, "Nebulacache", &filters).unwrap();
        binding.memory = "Different memory.".into();
        binding.agent = Some("agent_low".into());
        binding.revision += 1;
        write_json(&path, &binding).unwrap();
        assert_eq!(
            selected[0]["source"]["text"],
            "Nebulacache stores preferences."
        );
        assert_eq!(selected[0]["source"]["revision"], crate::util::digest(&raw));
        let hint = context_hint(&project, &selected, &filters).unwrap();
        assert_eq!(hint["bindings"][0]["agent"], "agent_medium");
        assert_eq!(hint["bindings"][0]["ask_argv"][1], "settings");
        assert!(select(&project, "Nebulacache", &filters)
            .unwrap()
            .is_empty());
        assert_eq!(
            select(&project, "settings", &filters).unwrap()[0]["source"]["text"],
            "Different memory."
        );
        for index in 0..10 {
            let extra = crate::model::ThreadDoc::new_memory(
                fresh_id(),
                format!("extra-{index}"),
                "Extra".into(),
                "Preferences".into(),
                "ui".into(),
                vec![],
            )
            .unwrap();
            project.persist(&extra).unwrap();
            let mut child = binding.clone();
            child.thread_id = extra.meta.id.clone();
            child.parent = Some(id.clone());
            write_json(&binding_path(&project, &extra.meta.id).unwrap(), &child).unwrap();
        }
        let project = Project::open(temp.path()).unwrap();
        let limited = std::collections::BTreeMap::from([("limit".into(), "2".into())]);
        let all = select(&project, "", &limited).unwrap();
        assert_eq!(all.len(), 11);
        assert!(all[..2]
            .iter()
            .all(|r| r["source"]["text"] == "Different memory."));
        assert!(all[2..].iter().all(|r| r.get("source").is_none()));
        let hint = context_hint(&project, &all, &limited).unwrap();
        assert_eq!(hint["bindings_omitted"], 9);
        let child = select(&project, all[0]["thread_id"].as_str().unwrap(), &limited).unwrap();
        assert_eq!(child.len(), 2);
        assert_eq!(child[0]["slug"], "extra-0");
        assert_eq!(child[1]["slug"], "settings");
        assert_eq!(child[1]["selection_reason"], "parent_context");
        assert_eq!(child[1]["source"]["text"], "Different memory.");
        binding.thread_id = fresh_id();
        write_json(&path, &binding).unwrap();
        assert!(select(&project, "settings", &filters).is_err());
    }
}
