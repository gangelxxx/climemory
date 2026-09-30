use super::*;

pub(super) fn search_candidates(
    connection: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<CodeEvidence>> {
    let terms = query_terms(query);
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let path_phrases = path_phrases(query);
    let structural_terms = structural_terms(query);
    let mut candidates = Vec::new();
    let mut selected_ids = BTreeSet::new();
    for term in &structural_terms {
        let mut statement = connection.prepare(
            "SELECT id,path,name,kind,line,signature,is_test
             FROM source_code_symbols
             WHERE name=?1 COLLATE NOCASE
             ORDER BY is_test,path,line LIMIT ?2",
        )?;
        let rows = statement.query_map(params![term, limit as i64], |row| {
            let path = row.get::<_, String>(1)?;
            let name = row.get::<_, String>(2)?;
            let signature = row.get::<_, String>(5)?;
            Ok((
                row.get::<_, String>(0)?,
                CodeEvidence {
                    score: structural_score(
                        EXACT_DEFINITION_BASE_SCORE,
                        &path,
                        &format!("{name} {signature}"),
                        &terms,
                        &path_phrases,
                    ),
                    path,
                    symbol: Some(name),
                    kind: Some(row.get(3)?),
                    start_line: row.get::<_, i64>(4)? as usize,
                    end_line: row.get::<_, i64>(4)? as usize,
                    reason: "definition".to_string(),
                    snippet: signature,
                    is_test: row.get::<_, i64>(6)? != 0,
                },
            ))
        })?;
        for row in rows {
            let (id, evidence) = row?;
            selected_ids.insert(id);
            candidates.push(evidence);
        }
    }

    let expression = terms
        .iter()
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut statement = connection.prepare(
        "SELECT c.path,c.start_line,c.end_line,c.is_test,c.symbols,
                snippet(source_code_fts,1,'','', ' … ',18),bm25(source_code_fts),c.body
         FROM source_code_fts JOIN source_code_chunks c ON c.id=source_code_fts.rowid
         WHERE source_code_fts MATCH ?1 ORDER BY bm25(source_code_fts) LIMIT ?2",
    )?;
    let rows = statement.query_map(params![expression, limit as i64], |row| {
        let path = row.get::<_, String>(0)?;
        let normalized_path = path.to_lowercase();
        let rank = row.get::<_, f64>(6)?;
        let body = row.get::<_, String>(7)?.to_lowercase();
        let coverage = terms
            .iter()
            .filter(|term| body.contains(term.as_str()))
            .count() as i64;
        let path_coverage = terms
            .iter()
            .filter(|term| normalized_path.contains(term.as_str()))
            .count() as i64;
        let path_phrase_coverage = path_phrases
            .iter()
            .filter(|phrase| normalized_path.contains(phrase.as_str()))
            .count() as i64;
        Ok(CodeEvidence {
            path,
            start_line: row.get::<_, i64>(1)? as usize,
            end_line: row.get::<_, i64>(2)? as usize,
            reason: "text".to_string(),
            symbol: None,
            kind: None,
            is_test: row.get::<_, i64>(3)? != 0,
            snippet: compact_snippet(&row.get::<_, String>(5)?),
            score: 30
                + coverage * 12
                + path_coverage * 20
                + path_phrase_coverage * 40
                + (-rank * 5.0).round().clamp(-10.0, 10.0) as i64,
        })
    })?;
    for row in rows {
        candidates.push(row?);
    }
    append_suffix_definitions(
        connection,
        &mut candidates,
        &mut selected_ids,
        &structural_terms,
        &terms,
        &path_phrases,
        limit,
    )?;
    append_relationship_evidence(
        connection,
        &mut candidates,
        &selected_ids,
        &terms,
        &path_phrases,
        limit,
    )?;

    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.is_test.cmp(&right.is_test))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.start_line.cmp(&right.start_line))
    });
    let mut dedup = BTreeSet::new();
    let mut per_path = BTreeMap::<String, usize>::new();
    candidates.retain(|item| {
        let unique = dedup.insert((
            item.path.clone(),
            item.start_line,
            item.end_line,
            item.reason.clone(),
        ));
        let count = per_path.entry(item.path.clone()).or_default();
        let within_path_limit = *count < 1;
        if unique && within_path_limit {
            *count += 1;
            true
        } else {
            false
        }
    });
    prioritize_production_and_test_evidence(&mut candidates, query_has_test_intent(query));
    candidates.truncate(limit);
    Ok(candidates)
}

fn append_suffix_definitions(
    connection: &Connection,
    candidates: &mut Vec<CodeEvidence>,
    selected_ids: &mut BTreeSet<String>,
    structural_terms: &[String],
    query_terms: &[String],
    path_phrases: &[String],
    limit: usize,
) -> Result<()> {
    let suffix_terms = structural_terms
        .iter()
        .filter(|term| term.chars().count() >= 10)
        .collect::<Vec<_>>();
    if suffix_terms.is_empty() {
        return Ok(());
    }
    let mut seen_paths = BTreeSet::new();
    let candidate_paths = candidates
        .iter()
        .filter(|item| item.reason == "text" && seen_paths.insert(item.path.clone()))
        .map(|item| item.path.clone())
        .collect::<Vec<_>>();
    let mut added = 0usize;
    let mut statement = connection.prepare(
        "SELECT id,path,name,kind,line,signature,is_test
         FROM source_code_symbols WHERE path=?1 ORDER BY line,name,id",
    )?;
    for path in candidate_paths {
        let rows = statement.query_map(params![path], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)? as usize,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)? != 0,
            ))
        })?;
        for row in rows {
            let (id, path, name, kind, line, signature, is_test) = row?;
            let normalized_name = name.to_lowercase();
            if !suffix_terms.iter().any(|term| {
                normalized_name != term.as_str() && normalized_name.ends_with(term.as_str())
            }) {
                continue;
            }
            selected_ids.insert(id);
            candidates.push(CodeEvidence {
                score: structural_score(
                    SUFFIX_DEFINITION_BASE_SCORE,
                    &path,
                    &format!("{name} {signature}"),
                    query_terms,
                    path_phrases,
                ),
                path,
                start_line: line,
                end_line: line,
                reason: "definition".to_string(),
                symbol: Some(name),
                kind: Some(kind),
                snippet: signature,
                is_test,
            });
            added += 1;
            if added == limit.min(32) {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn append_relationship_evidence(
    connection: &Connection,
    candidates: &mut Vec<CodeEvidence>,
    selected_ids: &BTreeSet<String>,
    terms: &[String],
    path_phrases: &[String],
    limit: usize,
) -> Result<()> {
    for id in selected_ids {
        let mut callers = connection.prepare(
            "SELECT e.src_path,e.line,s.name,s.kind,COALESCE(s.signature,''),
                    COALESCE(s.is_test,(SELECT is_test FROM source_code_chunks c WHERE c.path=e.src_path ORDER BY c.start_line LIMIT 1),0)
             FROM source_code_edges e
             LEFT JOIN source_code_symbols s ON s.id=e.src_id
             WHERE e.dst_id=?1 ORDER BY COALESCE(s.is_test,0) DESC,e.src_path,e.line LIMIT ?2",
        )?;
        let rows = callers.query_map(params![id, limit as i64], |row| {
            let path = row.get::<_, String>(0)?;
            let symbol = row.get::<_, Option<String>>(2)?;
            let snippet = row.get::<_, String>(4)?;
            Ok(CodeEvidence {
                score: structural_score(
                    CALLER_BASE_SCORE,
                    &path,
                    &format!("{} {snippet}", symbol.as_deref().unwrap_or_default()),
                    terms,
                    path_phrases,
                ),
                path,
                start_line: row.get::<_, i64>(1)? as usize,
                end_line: row.get::<_, i64>(1)? as usize,
                reason: "caller".to_string(),
                symbol,
                kind: row.get(3)?,
                snippet,
                is_test: row.get::<_, i64>(5)? != 0,
            })
        })?;
        for row in rows {
            candidates.push(row?);
        }
        let mut callees = connection.prepare(
            "SELECT d.path,d.line,d.name,d.kind,d.signature,d.is_test
             FROM source_code_edges e JOIN source_code_symbols d ON d.id=e.dst_id
             WHERE e.src_id=?1 ORDER BY d.is_test,d.path,d.line LIMIT ?2",
        )?;
        let rows = callees.query_map(params![id, limit as i64], |row| {
            let path = row.get::<_, String>(0)?;
            let name = row.get::<_, String>(2)?;
            let signature = row.get::<_, String>(4)?;
            Ok(CodeEvidence {
                score: structural_score(
                    CALLEE_BASE_SCORE,
                    &path,
                    &format!("{name} {signature}"),
                    terms,
                    path_phrases,
                ),
                path,
                start_line: row.get::<_, i64>(1)? as usize,
                end_line: row.get::<_, i64>(1)? as usize,
                reason: "callee".to_string(),
                symbol: Some(name),
                kind: Some(row.get(3)?),
                snippet: signature,
                is_test: row.get::<_, i64>(5)? != 0,
            })
        })?;
        for row in rows {
            candidates.push(row?);
        }
    }
    Ok(())
}

pub(super) fn prioritize_production_and_test_evidence(
    candidates: &mut Vec<CodeEvidence>,
    test_intent: bool,
) {
    if test_intent {
        return;
    }
    let preferred_production = candidates
        .iter()
        .filter(|item| !item.is_test)
        .take(PRODUCTION_PREFIX)
        .cloned()
        .collect::<Vec<_>>();
    let Some(representative_test) = candidates.iter().find(|item| item.is_test).cloned() else {
        return;
    };
    if preferred_production.is_empty() {
        return;
    }
    let mut selected = preferred_production
        .iter()
        .map(evidence_identity)
        .collect::<BTreeSet<_>>();
    selected.insert(evidence_identity(&representative_test));
    let original = std::mem::take(candidates);
    candidates.extend(preferred_production);
    candidates.push(representative_test);
    candidates.extend(
        original
            .into_iter()
            .filter(|item| !selected.contains(&evidence_identity(item))),
    );
}

fn evidence_identity(item: &CodeEvidence) -> (String, usize, usize, String) {
    (
        item.path.clone(),
        item.start_line,
        item.end_line,
        item.reason.clone(),
    )
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    terms.sort_by_key(|term| std::cmp::Reverse(term.chars().count()));
    terms.truncate(12);
    terms
}

fn structural_score(
    base: i64,
    path: &str,
    text: &str,
    terms: &[String],
    path_phrases: &[String],
) -> i64 {
    let normalized_path = path.to_lowercase();
    let haystack = format!("{normalized_path} {}", text.to_lowercase());
    let coverage = terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count() as i64;
    let path_coverage = terms
        .iter()
        .filter(|term| normalized_path.contains(term.as_str()))
        .count() as i64;
    let path_phrase_coverage = path_phrases
        .iter()
        .filter(|phrase| normalized_path.contains(phrase.as_str()))
        .count() as i64;
    base + coverage * 120 + path_coverage * 40 + path_phrase_coverage * 100
}

pub(super) fn path_phrases(query: &str) -> Vec<String> {
    let ordered = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    let mut phrases = Vec::new();
    for width in (2..=4).rev() {
        for window in ordered.windows(width) {
            let phrase = window.join("-");
            if seen.insert(phrase.clone()) {
                phrases.push(phrase);
                if phrases.len() == 32 {
                    return phrases;
                }
            }
        }
    }
    phrases
}

pub(super) fn query_has_test_intent(query: &str) -> bool {
    const TEST_INTENT: &[&str] = &["test", "tests", "testing", "spec", "specs", "e2e"];
    query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .map(str::to_lowercase)
        .any(|term| TEST_INTENT.contains(&term.as_str()))
}

pub(super) fn structural_terms(query: &str) -> Vec<String> {
    const GENERIC: &[&str] = &[
        "code", "context", "source", "file", "function", "method", "class", "test", "tests",
    ];
    let ordered = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| term.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    let mut values = Vec::new();
    if ordered.len() == 1 {
        for term in &ordered {
            if !GENERIC.contains(&term.as_str()) && seen.insert(term.clone()) {
                values.push(term.clone());
            }
        }
    }
    // Natural-language task descriptions commonly split a camelCase symbol. The
    // joined adjacent forms recover `terminalOutcome` from "terminal outcome"
    // without fuzzy global name resolution.
    for width in (2..=4).rev() {
        for window in ordered.windows(width) {
            let joined = window.concat();
            if joined.chars().count() >= 6 && seen.insert(joined.clone()) {
                values.push(joined);
                if values.len() == 24 {
                    return values;
                }
            }
        }
    }
    values
}

fn compact_snippet(value: &str) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 160 {
        return compact;
    }
    compact.chars().take(157).collect::<String>() + "…"
}
