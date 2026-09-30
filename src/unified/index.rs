use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Source {
    pub id: String,
    pub path: String,
    pub revision: String,
    pub authority: String,
    pub text: String,
    pub title: String,
    pub agent: String,
    pub parent: Option<String>,
    #[serde(default)]
    pub pointer: Option<String>,
    #[serde(default)]
    pub addresses: Vec<SourceAddress>,
    #[serde(default)]
    pub claims: Vec<crate::session_ingest::Claim>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct SourceAddress {
    pub pointer: String,
    pub line: usize,
}
impl Source {
    pub fn address(&self, line: usize) -> (&str, usize) {
        self.addresses
            .get(line.saturating_sub(1))
            .map(|a| (a.pointer.as_str(), a.line))
            .unwrap_or((self.pointer.as_deref().unwrap_or("/memory"), line))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Fragment {
    pub id: String,
    pub source: String,
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Passport {
    pub summary: String,
    pub questions: Vec<String>,
    pub terms: BTreeSet<String>,
    pub subtree_terms: BTreeSet<String>,
    pub reviewed_revision: Option<String>,
    pub checked_candidates: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Thread {
    pub id: String,
    pub source: String,
    pub title: String,
    pub parent: Option<String>,
    pub agent: String,
    pub fragments: Vec<Fragment>,
    pub passport: Passport,
    #[serde(default)]
    pub elements: Vec<crate::session_ingest::Claim>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Link {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub evidence: Vec<String>,
    pub confirmed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Index {
    pub format: u32,
    pub revision: String,
    pub sources: Vec<Source>,
    pub threads: Vec<Thread>,
    pub links: Vec<Link>,
    #[serde(default)]
    pub postings: BTreeMap<String, Vec<String>>,
}

pub(super) fn terms(s: &str) -> BTreeSet<String> {
    crate::terms::link_gate_terms(&crate::terms::search_terms(s))
        .into_iter()
        .collect()
}

pub(super) fn sources(project: &Project) -> Result<Vec<Source>> {
    let mut result = Vec::new();
    for d in project.user_documents()? {
        let path = d["path"].as_str().unwrap().to_owned();
        result.push(Source {
            id: format!("doc-{}", &digest(&path)[..16]),
            title: path.clone(),
            path,
            revision: d["revision"].as_str().unwrap().into(),
            authority: "user_document".into(),
            text: d["text"].as_str().unwrap().into(),
            agent: project.config.memory.documents_agent.clone(),
            parent: None,
            addresses: Vec::new(),
            pointer: None,
            claims: Vec::new(),
        });
    }
    if project.config.memory.mode != crate::config::MemoryMode::DocsOnly {
        let fresh_project = Project::open(&project.root)?;
        for m in crate::thread_agents::source_inventory(&fresh_project)? {
            result.push(Source {
                id: format!("memory-{}", m["thread_id"].as_str().unwrap()),
                path: m["path"].as_str().unwrap().into(),
                revision: m["revision"].as_str().unwrap().into(),
                authority: "advisory_memory".into(),
                text: m["memory"].as_str().unwrap_or("").into(),
                title: m["title"].as_str().unwrap_or("").into(),
                agent: m["agent"]
                    .as_str()
                    .unwrap_or(&project.config.memory.documents_agent)
                    .into(),
                parent: m["parent"].as_str().map(|p| format!("memory-{p}")),
                addresses: Vec::new(),
                pointer: Some("/memory".into()),
                claims: Vec::new(),
            });
        }
    }
    if project.config.memory.mode != crate::config::MemoryMode::DocsOnly {
        for c in crate::session_ingest::current_claim_sources(project)? {
            result.push(Source {
                id: c["id"].as_str().unwrap().into(),
                path: c["path"].as_str().unwrap().into(),
                revision: c["revision"].as_str().unwrap().into(),
                authority: "advisory_memory".into(),
                text: c["text"].as_str().unwrap().into(),
                title: c["title"].as_str().unwrap().into(),
                parent: c["parent"].as_str().map(str::to_owned),
                pointer: None,
                addresses: serde_json::from_value(c["addresses"].clone())?,
                agent: project.config.memory.documents_agent.clone(),
                claims: serde_json::from_value(c["claims"].clone())?,
            });
        }
    }
    result.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(result)
}

const INDEX_FORMAT: u32 = 2;

/// Small authored documents remain a single original unit, independent of query.
pub(super) fn source_is_cohesive(source: &Source) -> bool {
    source.authority == "user_document" && source.text.len() <= 2048
}

fn split(source: &Source) -> Vec<Thread> {
    let mut result = vec![Thread {
        id: source.id.clone(),
        source: source.id.clone(),
        title: source.title.clone(),
        parent: source.parent.clone(),
        agent: source.agent.clone(),
        fragments: vec![],
        passport: Passport::default(),
        elements: source.claims.clone(),
    }];
    let cohesive = source_is_cohesive(source);
    let mut stack: Vec<(usize, String)> = vec![(0, source.id.clone())];
    let mut occurrences = BTreeMap::<String, usize>::new();
    let mut current = 0;
    let mut size = 0;
    let mut fence = false;
    for (n, text) in source.text.lines().enumerate() {
        let trimmed = text.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
        }
        let depth = text.chars().take_while(|c| *c == '#').count();
        if !cohesive
            && !fence
            && (1..=6).contains(&depth)
            && text.as_bytes().get(depth) == Some(&b' ')
        {
            while stack.last().is_some_and(|(d, _)| *d >= depth) {
                stack.pop();
            }
            let parent = stack.last().unwrap().1.clone();
            let title = text[depth..].trim().to_owned();
            let key = format!("{parent}/{title}");
            let occurrence = occurrences.entry(key.clone()).or_default();
            *occurrence += 1;
            let id = format!("section-{}", &digest(format!("{key}/{occurrence}"))[..16]);
            result.push(Thread {
                id: id.clone(),
                source: source.id.clone(),
                title,
                parent: Some(parent),
                agent: source.agent.clone(),
                fragments: vec![],
                passport: Passport::default(),
                elements: vec![],
            });
            current = result.len() - 1;
            stack.push((depth, id));
            size = 0;
        }
        // Bound agent inputs even for unstructured text. Preserve original line addresses.
        if size + text.len() > 12_000 && !result[current].fragments.is_empty() {
            let parent = stack.last().unwrap().1.clone();
            let id = format!("part-{}", &digest(format!("{parent}/line/{n}"))[..16]);
            result.push(Thread {
                id,
                source: source.id.clone(),
                title: format!("{} (continued)", result[current].title),
                parent: Some(parent),
                agent: source.agent.clone(),
                fragments: vec![],
                passport: Passport::default(),
                elements: vec![],
            });
            current = result.len() - 1;
            size = 0;
        }
        if !trimmed.is_empty() {
            result[current].fragments.push(Fragment {
                id: format!("{}:L{}", source.id, n + 1),
                source: source.id.clone(),
                line: n + 1,
                text: text.into(),
            });
            size += text.len();
        }
    }
    for thread in &mut result {
        thread.passport.terms = terms(&format!(
            "{} {}",
            thread.title,
            thread
                .fragments
                .iter()
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ));
        thread.passport.subtree_terms = thread.passport.terms.clone();
    }
    result
}

pub(super) fn build(sources: Vec<Source>, old: Option<&Index>) -> Result<Index> {
    let revision = digest(serde_json::to_vec(&sources)?);
    let mut index = Index {
        format: INDEX_FORMAT,
        revision,
        sources,
        ..Index::default()
    };
    for source in &index.sources {
        if let Some(previous) = old.filter(|o| {
            o.format == INDEX_FORMAT
                && o.sources.iter().any(|s| s == source)
                && original_fragments_match(o, source)
        }) {
            index.threads.extend(
                previous
                    .threads
                    .iter()
                    .filter(|t| t.source == source.id)
                    .cloned(),
            );
        } else {
            index.threads.extend(split(source));
        }
    }
    if index.threads.len() > 8192 {
        return Err(AppError::new(
            "unified index exceeds 8192 threads; sources were not truncated",
        ));
    }
    // Element identity follows the original evidence, not a mutable topic boundary.
    for t in &mut index.threads {
        if index
            .sources
            .iter()
            .find(|s| s.id == t.source)
            .is_some_and(|s| s.claims.is_empty())
        {
            let mut ids = BTreeSet::new();
            for e in &mut t.elements {
                e.id = digest(serde_json::to_vec(&json!([t.source, e.kind, e.sources]))?);
            }
            t.elements.retain(|e| ids.insert(e.id.clone()));
        }
    }
    let valid: BTreeSet<_> = index.threads.iter().map(|t| t.id.clone()).collect();
    if valid.len() != index.threads.len() {
        return Err(AppError::new(
            "derived thread ID collision; no index was published",
        ));
    }
    for t in &mut index.threads {
        if t.parent.as_ref().is_some_and(|p| !valid.contains(p)) {
            t.parent = None;
        }
        t.passport.subtree_terms = t.passport.terms.clone();
    }
    prepare_hierarchy(&mut index);
    if index.threads.len() > 8192 {
        return Err(AppError::new("unified hierarchy exceeds 8192 threads"));
    }
    // Bottom-up aggregation visits each acyclic parent edge once. Cycles retain own terms
    // and remain discoverable through the lexical fallback; never recurse indefinitely.
    let positions: BTreeMap<_, _> = index
        .threads
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.clone(), i))
        .collect();
    let mut children = vec![0usize; index.threads.len()];
    for t in &index.threads {
        if let Some(p) = t.parent.as_ref().and_then(|p| positions.get(p)) {
            children[*p] += 1;
        }
    }
    let mut ready: Vec<_> = children
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == 0)
        .map(|(i, _)| i)
        .collect();
    while let Some(i) = ready.pop() {
        if let Some(p) = index.threads[i]
            .parent
            .as_ref()
            .and_then(|p| positions.get(p))
            .copied()
        {
            let terms = index.threads[i].passport.subtree_terms.clone();
            index.threads[p].passport.subtree_terms.extend(terms);
            children[p] -= 1;
            if children[p] == 0 {
                ready.push(p);
            }
        }
    }
    if let Some(old) = old.filter(|old| old.format == INDEX_FORMAT) {
        for link in &old.links {
            let endpoints_current = [&link.from, &link.to].iter().all(|id| {
                index
                    .threads
                    .iter()
                    .find(|t| &t.id == *id)
                    .is_some_and(|t| {
                        old.sources
                            .iter()
                            .find(|s| s.id == t.source)
                            .zip(index.sources.iter().find(|s| s.id == t.source))
                            .is_some_and(|(a, b)| a == b)
                    })
            });
            if endpoints_current {
                index.links.push(link.clone());
            }
        }
    }
    refresh_revision(&mut index)?;
    Ok(index)
}

/// Derived routing only: never rewrite parent bindings in authored source memory.
fn prepare_hierarchy(index: &mut Index) {
    let positions: BTreeMap<_, _> = index
        .threads
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.clone(), i))
        .collect();
    let mut done = BTreeSet::new();
    for start in 0..index.threads.len() {
        let mut path = BTreeSet::new();
        let mut current = Some(start);
        while let Some(i) = current {
            if done.contains(&i) {
                break;
            }
            if !path.insert(i) {
                index.threads[i].parent = None;
                break;
            }
            current = index.threads[i]
                .parent
                .as_ref()
                .and_then(|p| positions.get(p))
                .copied();
        }
        done.extend(path);
    }
    let roots: Vec<_> = index
        .threads
        .iter()
        .filter(|t| t.parent.is_none())
        .map(|t| t.id.clone())
        .collect();
    if roots.len() > 1 {
        for t in &mut index.threads {
            if t.parent.is_none() {
                t.parent = Some("cm-routing-root".into());
            }
        }
        index.threads.push(Thread {
            id: "cm-routing-root".into(),
            source: "cm-routing-root".into(),
            title: "Project memory".into(),
            parent: None,
            agent: index
                .sources
                .first()
                .map(|s| s.agent.clone())
                .unwrap_or_default(),
            fragments: vec![],
            passport: Passport::default(),
            elements: vec![],
        });
    }
}

fn refresh_revision(index: &mut Index) -> Result<()> {
    index.postings.clear();
    for t in &index.threads {
        if t.id == "cm-routing-root" {
            continue;
        }
        let mut own = terms(&t.title);
        for f in &t.fragments {
            own.extend(terms(&f.text));
        }
        own.extend(terms(&t.passport.summary));
        for q in &t.passport.questions {
            own.extend(terms(q));
        }
        for term in own {
            index.postings.entry(term).or_default().push(t.id.clone());
        }
    }
    let mut layout: Vec<_> = index
        .threads
        .iter()
        .map(|t| {
            (
                &t.id,
                &t.parent,
                t.fragments.iter().map(|f| &f.id).collect::<Vec<_>>(),
            )
        })
        .collect();
    layout.sort_by(|a, b| a.0.cmp(b.0));
    index.revision = digest(serde_json::to_vec(&(index.format, &index.sources, layout))?);
    Ok(())
}

pub(super) fn apply_groups(
    index: &mut Index,
    selections: &BTreeMap<String, worker::Selection>,
) -> Result<()> {
    for (id, selection) in selections {
        if selection.groups.is_empty() {
            continue;
        }
        let Some(parent) = index.thread(id).cloned() else {
            continue;
        };
        if !(2..=8).contains(&selection.groups.len())
            || selection.groups.iter().any(|g| {
                g.title.trim().is_empty()
                    || g.title.chars().count() > 160
                    || g.fragments.len() < 2
                    || g.fragments.len() >= parent.fragments.len()
            })
            || index
                .sources
                .iter()
                .find(|source| source.id == parent.source)
                .is_none_or(|source| {
                    source.authority != "user_document" || source_is_cohesive(source)
                })
            || parent.fragments.len() < 8
            || index.threads.iter().any(|t| t.parent.as_ref() == Some(id))
        {
            continue;
        }
        let own: BTreeSet<_> = parent.fragments.iter().map(|f| f.id.as_str()).collect();
        let supplied: Vec<_> = selection
            .groups
            .iter()
            .flat_map(|g| g.fragments.iter().map(String::as_str))
            .collect();
        if supplied.len() != own.len() || supplied.iter().copied().collect::<BTreeSet<_>>() != own {
            continue;
        }
        if index.threads.len() + selection.groups.len() > 8192 {
            continue;
        }
        let mut children = Vec::new();
        for group in &selection.groups {
            let fragments: Vec<_> = parent
                .fragments
                .iter()
                .filter(|f| group.fragments.contains(&f.id))
                .cloned()
                .collect();
            let child_id = format!(
                "semantic-{}",
                &digest(serde_json::to_vec(&json!([
                    id,
                    group.title,
                    group.fragments
                ]))?)[..16]
            );
            if index.thread(&child_id).is_some() {
                return Err(AppError::new("semantic thread identity collision"));
            }
            let lexical = terms(&format!(
                "{} {}",
                group.title,
                fragments
                    .iter()
                    .map(|f| f.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
            children.push(Thread {
                id: child_id,
                source: parent.source.clone(),
                title: group.title.clone(),
                parent: Some(id.clone()),
                agent: parent.agent.clone(),
                fragments,
                passport: Passport {
                    terms: lexical.clone(),
                    subtree_terms: lexical,
                    ..Passport::default()
                },
                elements: parent
                    .elements
                    .iter()
                    .filter(|e| e.sources.iter().all(|f| group.fragments.contains(f)))
                    .cloned()
                    .collect(),
            });
        }
        let t = index.threads.iter_mut().find(|t| &t.id == id).unwrap();
        t.fragments.clear();
        index
            .links
            .retain(|link| link.from != *id && link.to != *id);
        index.threads.extend(children);
    }
    refresh_revision(index)?;
    Ok(())
}

fn original_fragments_match(index: &Index, source: &Source) -> bool {
    let expected: BTreeMap<_, _> = source
        .text
        .lines()
        .enumerate()
        .filter(|(_, s)| !s.trim().is_empty())
        .map(|(n, s)| (n + 1, s))
        .collect();
    let rows: Vec<_> = index
        .threads
        .iter()
        .filter(|t| t.source == source.id)
        .flat_map(|t| &t.fragments)
        .collect();
    let mut seen = BTreeSet::new();
    rows.len() == expected.len()
        && rows.iter().all(|f| {
            f.source == source.id
                && f.id == format!("{}:L{}", source.id, f.line)
                && expected.get(&f.line).is_some_and(|text| *text == f.text)
                && seen.insert(f.line)
        })
}

impl Index {
    pub fn thread(&self, id: &str) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }
    pub fn fragments(&self) -> BTreeMap<String, Fragment> {
        self.threads
            .iter()
            .flat_map(|t| t.fragments.iter().cloned().map(|f| (f.id.clone(), f)))
            .collect()
    }
    pub fn search(&self, query: &str) -> Vec<String> {
        let mut scores = BTreeMap::<String, f64>::new();
        for term in terms(query) {
            if let Some(ids) = self.postings.get(&term) {
                let weight = 1.0 + ((self.threads.len() + 1) as f64 / (ids.len() + 1) as f64).ln();
                for id in ids {
                    *scores.entry(id.clone()).or_default() += weight;
                }
            }
        }
        let mut hits: Vec<_> = scores.into_iter().collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hits.into_iter().map(|(id, _)| id).collect()
    }
    pub fn candidates(&self, t: &Thread) -> Vec<String> {
        self.threads
            .iter()
            .filter(|other| {
                other.id != t.id
                    && !other.fragments.is_empty()
                    && (other.source == t.source
                        || !other
                            .passport
                            .subtree_terms
                            .is_disjoint(&t.passport.subtree_terms))
            })
            .map(|t| t.id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source(text: &str) -> Source {
        Source {
            id: "doc-ui".into(),
            path: "memory/docs/ui.md".into(),
            revision: digest(text),
            authority: "user_document".into(),
            text: text.into(),
            title: "UI".into(),
            agent: "agent_low".into(),
            parent: None,
            addresses: Vec::new(),
            pointer: None,
            claims: Vec::new(),
        }
    }
    #[test]
    fn small_user_document_has_one_owner_and_exact_original_line_addresses() {
        let text =
            "# Правила\r\n\r\n## Width\r\n  Width is 12px.  \r\n## Height\r\nHeight is 24px.\r\n";
        let s = source(text);
        let idx = build(vec![s.clone()], None).unwrap();
        assert_eq!(idx.format, INDEX_FORMAT);
        assert_eq!(idx.threads.len(), 1);
        let t = &idx.threads[0];
        assert_eq!(t.id, s.id);
        assert_eq!(t.agent, s.agent);
        assert_eq!(idx.sources[0], s);
        assert_eq!(t.fragments.len(), 5);
        assert_eq!(t.fragments[2].id, "doc-ui:L4");
        assert_eq!(t.fragments[2].line, 4);
        assert_eq!(t.fragments[2].text, "  Width is 12px.  ");
        assert_eq!(idx.search("height"), ["doc-ui"]);
        assert_eq!(idx.search("width"), ["doc-ui"]);
    }

    #[test]
    fn cohesion_boundary_is_utf8_bytes_and_does_not_apply_to_memory() {
        let prefix = "# Root\n## Section\n";
        let exact = source(&format!("{prefix}{}", "x".repeat(2048 - prefix.len())));
        assert!(source_is_cohesive(&exact));
        assert_eq!(split(&exact).len(), 1);
        let over = source(&(exact.text.clone() + "x"));
        assert!(!source_is_cohesive(&over));
        assert!(split(&over).iter().any(|t| t.title == "Section"));
        let unicode = source(&format!("{prefix}{}", "界".repeat(700)));
        assert!(unicode.text.chars().count() < 2048);
        assert!(!source_is_cohesive(&unicode));
        assert!(split(&unicode).len() > 1);
        let mut memory = source("# Topic\n## Rules\nRecorded history");
        memory.authority = "advisory_memory".into();
        assert!(!source_is_cohesive(&memory));
        assert!(split(&memory).len() > 1);
    }

    #[test]
    fn old_format_rebuilds_unchanged_sources_and_drops_derived_links() {
        let s = source("# Rules\nRequired 12px.");
        let mut old = build(vec![s.clone()], None).unwrap();
        old.format = 1;
        old.threads[0].id = "legacy-section".into();
        old.links.push(Link {
            from: "legacy-section".into(),
            to: "legacy-section".into(),
            kind: "clarifies".into(),
            evidence: vec!["doc-ui:L2".into()],
            confirmed: true,
        });
        refresh_revision(&mut old).unwrap();
        let rebuilt = build(vec![s.clone()], Some(&old)).unwrap();
        assert_eq!(rebuilt.threads.len(), 1);
        assert_eq!(rebuilt.threads[0].id, s.id);
        assert_ne!(rebuilt.revision, old.revision);
        assert!(rebuilt.links.is_empty());
        let reused = build(vec![s], Some(&rebuilt)).unwrap();
        assert_eq!(reused.revision, rebuilt.revision);
    }

    #[test]
    fn semantic_groups_cannot_split_cohesive_original_but_large_docs_still_split() {
        let body = (1..=8).map(|n| format!("Rule {n}.\n")).collect::<String>();
        for large in [false, true] {
            let s = if large {
                large_source(&body)
            } else {
                source(&body)
            };
            let mut idx = build(vec![s], None).unwrap();
            let selection: worker::Selection = serde_json::from_value(json!({
                "select":[], "need":[], "gaps":[], "summary":"", "elements":[], "questions":[], "links":[], "checked":[], "groups":[
                    {"title":"First", "fragments":(1..=4).map(|n|format!("doc-ui:L{n}")).collect::<Vec<_>>()},
                    {"title":"Second", "fragments":(5..=8).map(|n|format!("doc-ui:L{n}")).collect::<Vec<_>>()}
                ]
            })).unwrap();
            apply_groups(&mut idx, &BTreeMap::from([("doc-ui".into(), selection)])).unwrap();
            assert_eq!(idx.threads.len(), if large { 3 } else { 1 });
            assert_eq!(idx.fragments().len(), 8);
        }
    }
    fn large_source(text: &str) -> Source {
        source(&format!("{text}\n{}", " ".repeat(2049)))
    }
    #[test]
    fn postings_rebuild_from_originals_and_never_select_synthetic_root() {
        let a = large_source("# Settings\n## Save\nGreen enabled.");
        let mut b = source("History retained.");
        b.id = "history".into();
        b.title = "History".into();
        let old = build(vec![a, b.clone()], None).unwrap();
        let hits = old.search("green");
        assert_eq!(hits.len(), 1);
        assert_eq!(old.thread(&hits[0]).unwrap().title, "Save");
        assert!(old.search("Project memory").is_empty());
        let mut encoded = serde_json::to_value(&old).unwrap();
        encoded.as_object_mut().unwrap().remove("postings");
        let legacy: Index = serde_json::from_value(encoded).unwrap();
        let updated = build(
            vec![large_source("# Settings\n## Save\nBlue enabled."), b],
            Some(&legacy),
        )
        .unwrap();
        assert!(updated.search("green").is_empty());
        assert_eq!(updated.search("blue").len(), 1);
        assert_eq!(updated.search("history"), vec!["history"]);
        assert!(updated.search("").is_empty());
    }
    #[test]
    fn passport_search_bounds_wide_levels_and_keeps_deep_exceptions() {
        let mut text = "# Root\n## Reference\n### Rules\nHistory retains 240 entries.\n## Overrides\n### Safety\nNever erase all records.\n".to_string();
        for i in 0..650 {
            text += &format!("## Display {i}\nSpacing configurable.\n");
        }
        let idx = build(vec![source(&text)], None).unwrap();
        let root = idx.threads.iter().find(|t| t.title == "Root").unwrap();
        let candidates = crate::unified::routing::targets(&idx, root);
        let result =
            crate::unified::routing::search_passports(&idx, &candidates, "History retention", 8);
        assert_eq!(result.len(), 8);
        let titles: Vec<_> = result
            .iter()
            .map(|id| idx.thread(id).unwrap().title.as_str())
            .collect();
        assert!(titles.contains(&"Reference"));
        assert!(titles.contains(&"Overrides"));
        assert!(!titles.contains(&"Rules"));
        assert_eq!(
            result,
            crate::unified::routing::search_passports(&idx, &candidates, "History retention", 8)
        );
        assert!(
            crate::unified::routing::search_passports(&idx, &candidates, "History", 0).is_empty()
        );
    }
    #[test]
    fn stable_sections_original_lines_and_distant_exceptions() {
        let a = build(
            vec![large_source(
                "# UI\n## Buttons\nButtons blue\n## Exceptions\nDeletion red",
            )],
            None,
        )
        .unwrap();
        let b = build(
            vec![large_source(
                "\n# UI\n## Buttons\nButtons blue\n## Exceptions\nDeletion red",
            )],
            Some(&a),
        )
        .unwrap();
        assert_eq!(
            a.threads.iter().map(|t| &t.id).collect::<Vec<_>>(),
            b.threads.iter().map(|t| &t.id).collect::<Vec<_>>()
        );
        assert_ne!(a.revision, b.revision);
        let roots = a.search("buttons");
        assert_eq!(roots.len(), 1);
        assert_eq!(a.thread(&roots[0]).unwrap().title, "Buttons");
        assert_eq!(a.search("deletion").len(), 1);
        assert!(a.search("nonexistent").is_empty());
        assert!(a.threads.iter().any(|t| t.title == "Exceptions"));
        assert!(b
            .fragments()
            .values()
            .any(|f| f.line == 4 && f.text == "Buttons blue"));
    }
    #[test]
    fn cyclic_source_parents_become_a_reachable_derived_tree() {
        let mut a = source("A facts");
        a.id = "a".into();
        a.parent = Some("b".into());
        let mut b = source("B facts");
        b.id = "b".into();
        b.parent = Some("a".into());
        let index = build(vec![a, b], None).unwrap();
        assert_eq!(index.sources[0].parent.as_deref(), Some("b"));
        assert_eq!(index.sources[1].parent.as_deref(), Some("a"));
        assert!(index.search("unmatched question").is_empty());
        let roots: Vec<_> = index
            .threads
            .iter()
            .filter(|t| t.parent.is_none())
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(roots.len(), 1);
        assert!(!index
            .thread(&roots[0])
            .unwrap()
            .passport
            .subtree_terms
            .is_disjoint(&terms("facts")));
        let mut visited = BTreeSet::new();
        let mut queue = roots;
        while let Some(id) = queue.pop() {
            assert!(visited.insert(id.clone()));
            queue.extend(
                index
                    .threads
                    .iter()
                    .filter(|t| t.parent.as_ref() == Some(&id))
                    .map(|t| t.id.clone()),
            );
        }
        assert_eq!(visited.len(), 2);
    }
    #[test]
    fn removed_sources_remove_threads_and_links() {
        let a = build(vec![source("# Buttons\nBlue")], None).unwrap();
        let b = build(vec![], Some(&a)).unwrap();
        assert!(b.threads.is_empty());
        assert!(b.links.is_empty());
    }
}
