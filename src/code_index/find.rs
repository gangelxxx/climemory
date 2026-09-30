use super::query::fresh_read;
use super::*;

/// Umbrella `code-find-symbol-navigation` Phase 1: exact symbol navigation
/// (definitions/callers/callees) read straight from the indexed symbols and
/// edges tables — no FTS ranking, no 1-evidence-per-path cap, so a model can
/// answer "where is X defined / who calls X / what does X call" in one round.
/// Phase 3 adds `edge_kind` (call/implementation/...) and `--transitive`
/// depth-BFS over resolved edges.
pub struct CodeFindMatch {
    role: &'static str,
    path: Option<String>,
    line: Option<usize>,
    symbol: Option<String>,
    kind: Option<String>,
    signature: Option<String>,
    is_test: bool,
    unresolved: bool,
    depth: usize,
    edge_kind: Option<String>,
    /// Item 636: true when the row comes from the substring fallback (the
    /// exact lookup found no function-level definition), emitted as
    /// `match_mode: "substring"` — a discovery hint only, never an edge seed.
    substring: bool,
}

/// Command-specific information for a next-page continuation.
pub struct FindContinuationSpec {
    pub kind: Option<String>,
    /// Definition filters (--path repeats as a union, --symbol-kind is an
    /// exact kind match): forwarded verbatim like every shaping flag.
    pub paths: Vec<String>,
    pub symbol_kind: Option<String>,
    pub transitive: bool,
    pub explicit_depth: Option<usize>,
    pub explicit_freshness: Option<String>,
    pub full_root: Option<String>,
}

pub struct CodeFind {
    status: CodeStatus,
    action: &'static str,
    matches: Vec<CodeFindMatch>,
    definitions: usize,
    callers: usize,
    callees: usize,
    substring_matches: usize,
    ambiguous: bool,
    transitive: bool,
    max_depth: Option<usize>,
    omitted: usize,
    presentation: SearchPresentation,
    estimated_tokens: usize,
    freshness_mode: &'static str,
    scan_reused: bool,
    scan_age_ms: Option<u64>,
    count_only: bool,
    /// Active definition filters, echoed on code_find_summary (the raw
    /// --path values as passed; --symbol-kind verbatim).
    path_filter: Vec<String>,
    symbol_kind: Option<String>,
    continuation: Option<FindContinuationSpec>,
}

impl CodeFind {
    pub fn records(&self, query: &str) -> Vec<Value> {
        if self.count_only {
            return vec![json!({
                "record": "code_find_count",
                "query": query,
                "state": self.status.state,
                "fresh": self.status.fresh,
                "action": self.action,
                "definitions": self.definitions,
                "callers": self.callers,
                "callees": self.callees,
                "substring_matches": self.substring_matches,
                "ambiguous": self.ambiguous,
                "transitive": self.transitive,
                "depth": self.max_depth,
                "freshness_mode": self.freshness_mode,
                "thread_authority_unchanged": true,
            })];
        }
        let mut summary = json!({
            "record": "code_find_summary",
            "query": query,
            "state": self.status.state,
            "fresh": self.status.fresh,
            "action": self.action,
            "definitions": self.definitions,
            "callers": self.callers,
            "callees": self.callees,
            "substring_matches": self.substring_matches,
            "ambiguous": self.ambiguous,
            "transitive": self.transitive,
            "depth": self.max_depth,
            "matches": self.matches.len(),
            "omitted": self.omitted,
            "complete": self.omitted == 0,
            "estimated_tokens": self.estimated_tokens,
            "freshness_mode": self.freshness_mode,
            "scan_reused": self.scan_reused,
            "scan_age_ms": self.scan_age_ms,
            "thread_authority_unchanged": true,
        });
        // Active definition filters echo on the summary (omitted when
        // absent, the no_hits_hint/truncated_by optional-field convention).
        if !self.path_filter.is_empty() {
            summary
                .as_object_mut()
                .unwrap()
                .insert("path_filter".to_string(), json!(self.path_filter));
        }
        if let Some(symbol_kind) = &self.symbol_kind {
            summary
                .as_object_mut()
                .unwrap()
                .insert("symbol_kind".to_string(), json!(symbol_kind));
        }
        if self.definitions + self.callers + self.callees + self.substring_matches == 0 {
            // Registry item 555: a bare matches:0 gives the model no next
            // step (the thread find no_hits_hint precedent) — name the
            // index's scope and the literal-search recovery instead of
            // letting it burn rounds probing field names here. The condition
            // uses hit counts, not the emitted page: an offset can skip all hits.
            summary.as_object_mut().unwrap().insert(
                "no_hits_hint".to_string(),
                json!("no matches; code find only covers indexed symbol definitions/calls — for a JSON field name or string literal use `cm code grep <literal>`, or check the symbol spelling"),
            );
        }
        let total = self.definitions + self.callers + self.callees + self.substring_matches;
        summary["total"] = json!(total);
        self.presentation
            .annotate_page(&mut summary, total, self.matches.len());
        if summary["remaining"].as_u64().unwrap_or(0) > 0 && self.continuation.is_some() {
            summary["continuation"] = self.continuation_edge(query);
        }
        let mut records = vec![summary];
        records.extend(self.matches.iter().map(|item| {
            let mut record = json!({
                "record": "code_find_match",
                "role": item.role,
                "path": item.path,
                "line": item.line,
                "symbol": item.symbol,
                "kind": item.kind,
                "signature": item.signature,
                "is_test": item.is_test,
                "unresolved": item.unresolved,
                "depth": item.depth,
                "edge_kind": item.edge_kind,
            });
            if item.substring {
                // Only substring-fallback rows carry the marker: exact rows
                // keep their pre-636 shape byte-for-byte (token economy).
                record
                    .as_object_mut()
                    .unwrap()
                    .insert("match_mode".to_string(), json!("substring"));
            }
            record
        }));
        records
    }

    fn continuation_edge(&self, query: &str) -> Value {
        let spec = self
            .continuation
            .as_ref()
            .expect("continuation_edge is called only with a spec");
        let mut argv: Vec<String> = vec!["code".to_string(), "find".to_string()];
        if let Some(kind) = &spec.kind {
            argv.push(format!("--kind={kind}"));
        }
        for path in &spec.paths {
            argv.push(format!("--path={path}"));
        }
        if let Some(symbol_kind) = &spec.symbol_kind {
            argv.push(format!("--symbol-kind={symbol_kind}"));
        }
        if spec.transitive {
            argv.push("--transitive".to_string());
        }
        if let Some(depth) = spec.explicit_depth {
            argv.push("--depth".to_string());
            argv.push(depth.to_string());
        }
        if let Some(freshness) = &spec.explicit_freshness {
            argv.push("--freshness".to_string());
            argv.push(freshness.clone());
        }
        let total = self.definitions + self.callers + self.callees + self.substring_matches;
        self.presentation.continuation_args(
            &mut argv,
            self.presentation.offset.min(total) + self.matches.len(),
        );
        if let Some(root) = &spec.full_root {
            argv.push("--dir".to_string());
            argv.push(root.clone());
        }
        argv.extend(["--".into(), query.to_string()]);
        json!({"argv": argv})
    }
}

#[allow(clippy::too_many_arguments)]
pub fn find(
    project: &Project,
    name: &str,
    auto_index: bool,
    freshness_mode: FreshnessMode,
    presentation: SearchPresentation,
    count_only: bool,
    kind_filter: Option<&str>,
    path_filter: &[String],
    symbol_kind: Option<&str>,
    transitive: bool,
    max_depth: usize,
    continuation: Option<FindContinuationSpec>,
) -> Result<CodeFind> {
    let (collected, meta) = fresh_read(project, auto_index, freshness_mode, |connection| {
        let (mut matches, ids, caller_src_ids) =
            find_matches(connection, name, kind_filter, path_filter, symbol_kind)?;
        if transitive && max_depth > 1 && !ids.is_empty() {
            expand_transitive(
                connection,
                &ids,
                &caller_src_ids,
                kind_filter,
                max_depth,
                &mut matches,
            )?;
        }
        Ok(matches)
    })?;
    let mut matches = collected;
    // `definitions`/`ambiguous` keep their pre-636 exact-only semantics:
    // several same-name exact definitions are ambiguous, a substring
    // fallback returning many symbols is the expected discovery shape.
    let definitions = matches
        .iter()
        .filter(|m| m.role == "definition" && !m.substring)
        .count();
    let callers = matches.iter().filter(|m| m.role == "caller").count();
    let callees = matches.iter().filter(|m| m.role == "callee").count();
    let substring_matches = matches.iter().filter(|m| m.substring).count();
    let ambiguous = definitions > 1;
    let total = matches.len();
    let modified: BTreeMap<String, Option<SystemTime>> = if presentation.sort == SearchSort::Mtime {
        matches
            .iter()
            .filter_map(|m| m.path.as_ref())
            .map(|path| (path.clone(), ()))
            .collect::<BTreeMap<_, _>>()
            .into_keys()
            .map(|path| {
                let time = fs::metadata(project.root.join(&path))
                    .and_then(|m| m.modified())
                    .ok();
                (path, time)
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    matches.sort_by_cached_key(|m| {
        let rank = if presentation.sort == SearchSort::Relevance {
            (
                m.substring,
                match m.role {
                    "definition" => 0,
                    "caller" => 1,
                    _ => 2,
                },
                m.unresolved,
                m.depth,
            )
        } else {
            (false, 0, false, 0)
        };
        (
            std::cmp::Reverse(
                m.path
                    .as_ref()
                    .and_then(|p| modified.get(p))
                    .copied()
                    .flatten(),
            ),
            rank,
            m.path.is_none(),
            m.path.clone(),
            m.line,
            m.role,
            m.edge_kind.clone(),
            m.symbol.clone(),
            m.depth,
        )
    });
    if presentation.reverse {
        matches.reverse();
    }
    let matches = presentation.page(matches);
    let result = CodeFind {
        status: meta.status,
        action: meta.action,
        omitted: total.saturating_sub(matches.len()),
        matches,
        definitions,
        callers,
        callees,
        substring_matches,
        ambiguous,
        transitive,
        max_depth: transitive.then_some(max_depth),
        presentation,
        estimated_tokens: 0,
        freshness_mode: freshness_mode.label(),
        scan_reused: meta.scan_reused,
        scan_age_ms: meta.scan_age_ms,
        count_only,
        path_filter: path_filter.to_vec(),
        symbol_kind: symbol_kind.map(str::to_string),
        continuation,
    };
    Ok(result)
}

/// Item 636: ctags kinds that name a container rather than a function-level
/// symbol — an exact lookup hitting only these counts as a miss for the
/// substring fallback.
const CONTAINER_KINDS: &[&str] = &["module", "namespace", "package"];

/// Wrap the probe for a LIKE contains-match with every LIKE metachar
/// escaped, so a probe like `foo_%` matches literally (paired with the
/// query's `ESCAPE '\'` clause).
fn like_contains_pattern(probe: &str) -> String {
    let mut pattern = String::with_capacity(probe.len() + 2);
    pattern.push('%');
    for ch in probe.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

/// Shared row mapping for the exact and item-636 substring definition
/// queries (identical SELECT column order): (symbol id, definition match).
fn definition_row(
    row: &rusqlite::Row<'_>,
    substring: bool,
) -> rusqlite::Result<(String, CodeFindMatch)> {
    Ok((
        row.get::<_, String>(0)?,
        CodeFindMatch {
            role: "definition",
            path: Some(row.get(1)?),
            line: Some(row.get::<_, i64>(4)? as usize),
            symbol: Some(row.get(2)?),
            kind: Some(row.get(3)?),
            signature: Some(row.get(5)?),
            is_test: row.get::<_, i64>(6)? != 0,
            unresolved: false,
            depth: 1,
            edge_kind: None,
            substring,
        },
    ))
}

/// Definition-row filters shared by the exact and item-636 substring
/// SELECTs: --path repeats as a UNION of escaped LIKE contains-matches on the
/// project-relative stored path ('\' normalized to '/' so the flag accepts
/// either separator on every platform), --symbol-kind is an exact match on
/// the extracted kind (function/method/struct/...). Caller/callee expansion
/// never sees them. `?1` is already bound by the caller; appended parameters
/// continue the numbering.
fn append_definition_filters(
    sql: &mut String,
    values: &mut Vec<String>,
    path_filter: &[String],
    symbol_kind: Option<&str>,
) {
    if let Some(kind) = symbol_kind {
        values.push(kind.to_string());
        sql.push_str(&format!(" AND kind=?{}", values.len()));
    }
    if !path_filter.is_empty() {
        sql.push_str(" AND (");
        for (index, path) in path_filter.iter().enumerate() {
            if index > 0 {
                sql.push_str(" OR ");
            }
            values.push(like_contains_pattern(&path.replace('\\', "/")));
            sql.push_str(&format!("path LIKE ?{} ESCAPE '\\'", values.len()));
        }
        sql.push(')');
    }
}

/// One row per same-name definition (NOCASE, the search.rs exact-match
/// pattern), then every incoming edge (callers) and the outgoing edges of the
/// matched definitions (callees). Edges materialize only for names matching
/// some project symbol, so an unresolved caller or callee (no resolved
/// target) is an ambiguous-name site; calls to names the corpus does not
/// define (external/library) never materialize edges and are simply absent.
/// Callees collapse repeated call sites to the same target (DISTINCT) — the
/// question is "what does X call", not "how often". With `kind_filter` only
/// edges of that kind (e.g. call) qualify. Returns the matched definition
/// ids alongside (the transitive BFS seeds them).
///
/// Item 636: when the exact lookup finds no FUNCTION-LEVEL definition (a
/// feature-area probe matching only a container such as a module, or nothing
/// at all), symbols whose names contain the probe are appended as
/// substring-marked definitions — a discovery hint that saves the code grep
/// rounds a feature-area navigation otherwise pays. Substring rows are never
/// pushed into `ids`: they seed no callee collection and no transitive BFS.
fn find_matches(
    connection: &Connection,
    name: &str,
    kind_filter: Option<&str>,
    path_filter: &[String],
    symbol_kind: Option<&str>,
) -> Result<(Vec<CodeFindMatch>, Vec<String>, Vec<String>)> {
    let mut matches = Vec::new();
    let mut ids = Vec::new();
    let mut caller_src_ids = Vec::new();
    {
        let mut sql = String::from(
            "SELECT id,path,name,kind,line,signature,is_test
             FROM source_code_symbols
             WHERE name=?1 COLLATE NOCASE",
        );
        let mut values = vec![name.to_string()];
        append_definition_filters(&mut sql, &mut values, path_filter, symbol_kind);
        sql.push_str(" ORDER BY is_test,path,line,id");
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            definition_row(row, false)
        })?;
        for row in rows {
            let (id, definition) = row?;
            ids.push(id);
            matches.push(definition);
        }
    }

    // Item 636 substring fallback: the exact lookup above found no
    // FUNCTION-LEVEL definition (only containers such as the module named
    // like the probe, or nothing at all), so a feature-area probe would
    // otherwise cost code grep rounds to reach the feature's entry points.
    // Append every symbol whose name CONTAINS the probe (LIKE metachars
    // escaped, NOCASE like the exact arm) as a substring-marked definition.
    // Probes under 3 chars would flood the cap with noise and never fall
    // back. The rows are not pushed into `ids`: no callees, no transitive.
    let function_level_hit = matches
        .iter()
        .any(|m| !CONTAINER_KINDS.contains(&m.kind.as_deref().unwrap_or("")));
    if name.chars().count() >= 3 && !function_level_hit {
        let mut sql = String::from(
            "SELECT id,path,name,kind,line,signature,is_test
             FROM source_code_symbols
             WHERE name LIKE ?1 ESCAPE '\\'",
        );
        let mut values = vec![like_contains_pattern(name)];
        append_definition_filters(&mut sql, &mut values, path_filter, symbol_kind);
        sql.push_str(" ORDER BY is_test,name COLLATE NOCASE,path,line,id");
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            definition_row(row, true)
        })?;
        for row in rows {
            let (id, definition) = row?;
            // The container that triggered the fallback (the module named
            // exactly like the probe) is already an exact row — skip it.
            if !ids.contains(&id) {
                matches.push(definition);
            }
        }
    }

    {
        let mut statement = connection.prepare(
            "SELECT e.src_path, e.line, s.name, COALESCE(s.is_test,0), e.dst_id IS NULL, e.kind, e.src_id
             FROM source_code_edges e
             LEFT JOIN source_code_symbols s ON s.id=e.src_id
             WHERE e.dst_raw=?1 COLLATE NOCASE
               AND (?2 IS NULL OR e.kind=?2)
             ORDER BY e.src_path,e.line,e.id",
        )?;
        let rows = statement.query_map(params![name, kind_filter], |row| {
            Ok((
                row.get::<_, Option<String>>(6)?,
                CodeFindMatch {
                    role: "caller",
                    path: Some(row.get(0)?),
                    line: Some(row.get::<_, i64>(1)? as usize),
                    symbol: row.get(2)?,
                    kind: None,
                    signature: None,
                    is_test: row.get::<_, i64>(3)? != 0,
                    unresolved: row.get::<_, i64>(4)? != 0,
                    depth: 1,
                    edge_kind: Some(row.get(5)?),
                    substring: false,
                },
            ))
        })?;
        for row in rows {
            let (src_id, caller) = row?;
            // Every emitted caller row (even an unresolved one, which the
            // resolved-edge BFS cannot see) seeds the caller walk's visited
            // set, so a symbol reaching the seed through a resolved chain is
            // never re-emitted at depth >= 2.
            if let Some(src_id) = src_id {
                caller_src_ids.push(src_id);
            }
            matches.push(caller);
        }
    }

    // Ambiguous same-name definitions can each hold an edge to the same
    // target; DISTINCT collapses repeats per definition, this set collapses
    // them across definitions so one logical callee is one row.
    let mut seen_callees = BTreeSet::new();
    for id in &ids {
        let mut statement = connection.prepare(
            "SELECT DISTINCT e.dst_raw, d.path, d.line, d.kind, COALESCE(d.is_test,0), d.id IS NULL, e.kind
             FROM source_code_edges e
             LEFT JOIN source_code_symbols d ON d.id=e.dst_id
             WHERE e.src_id=?1
               AND (?2 IS NULL OR e.kind=?2)
             ORDER BY e.dst_raw COLLATE NOCASE,d.path,d.line,d.kind",
        )?;
        let rows = statement.query_map(params![id, kind_filter], |row| {
            let unresolved = row.get::<_, i64>(5)? != 0;
            Ok(CodeFindMatch {
                role: "callee",
                path: row.get::<_, Option<String>>(1)?,
                line: row.get::<_, Option<i64>>(2)?.map(|line| line as usize),
                symbol: Some(row.get(0)?),
                kind: row.get(3)?,
                signature: None,
                is_test: row.get::<_, i64>(4)? != 0,
                unresolved,
                depth: 1,
                edge_kind: Some(row.get(6)?),
                substring: false,
            })
        })?;
        for row in rows {
            let callee = row?;
            let key = (
                callee.symbol.clone(),
                callee.path.clone(),
                callee.line,
                callee.kind.clone(),
            );
            if seen_callees.insert(key) {
                matches.push(callee);
            }
        }
    }
    Ok((matches, ids, caller_src_ids))
}

/// Phase 3: BFS over RESOLVED edges (dst_id/src_id never NULL — ambiguous or
/// external sites cannot expand) up to `max_depth`, with a visited-set cycle
/// guard. Caller and callee chains expand INDEPENDENTLY, so a depth-N caller
/// is a genuine caller chain (calls something that ... calls the seed) and a
/// depth-N callee a genuine callee chain — paths never zigzag between
/// directions. Each newly reached symbol becomes a match at its BFS depth
/// pointing at the symbol's definition site. Both directions honor
/// `kind_filter`.
fn expand_transitive(
    connection: &Connection,
    ids: &[String],
    caller_src_ids: &[String],
    kind_filter: Option<&str>,
    max_depth: usize,
    matches: &mut Vec<CodeFindMatch>,
) -> Result<()> {
    // The caller walk also treats every emitted depth-1 caller (including
    // unresolved name-only rows) as visited: it is already on the result,
    // and the resolved-edge frontier alone cannot see those rows.
    expand_direction(
        connection,
        ids,
        caller_src_ids,
        kind_filter,
        max_depth,
        matches,
        "caller",
    )?;
    expand_direction(
        connection,
        ids,
        &[],
        kind_filter,
        max_depth,
        matches,
        "callee",
    )?;
    Ok(())
}

/// One chain direction: "caller" follows dst_id -> src_id (callers of
/// callers), "callee" follows src_id -> dst_id (callees of callees).
fn expand_direction(
    connection: &Connection,
    ids: &[String],
    extra_visited: &[String],
    kind_filter: Option<&str>,
    max_depth: usize,
    matches: &mut Vec<CodeFindMatch>,
    role: &'static str,
) -> Result<()> {
    let mut visited: BTreeSet<String> = ids.iter().cloned().collect();
    visited.extend(extra_visited.iter().cloned());
    // The level-1 frontier: resolved direct neighbors in this direction
    // (already emitted as depth-1 matches by find_matches). Frontier
    // membership is what lets their neighbors expand at depth 2; visited —
    // not the frontier filter — is what stops re-emission.
    let mut frontier = BTreeSet::new();
    for id in ids {
        for neighbor in neighbors(connection, id, kind_filter, role)? {
            frontier.insert(neighbor);
        }
    }
    for neighbor in &frontier {
        visited.insert(neighbor.clone());
    }
    let mut frontier: Vec<String> = frontier.into_iter().collect();
    for depth in 2..=max_depth {
        if frontier.is_empty() {
            break;
        }
        let mut next = BTreeSet::new();
        for id in &frontier {
            for neighbor in neighbors(connection, id, kind_filter, role)? {
                if !visited.contains(&neighbor) {
                    next.insert(neighbor);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        for new_id in &next {
            visited.insert(new_id.clone());
        }
        for new_id in &next {
            if let Some(found) = symbol_row(connection, new_id)? {
                matches.push(found.into_match(role, depth));
            }
        }
        frontier = next.into_iter().collect();
    }
    Ok(())
}

/// Resolved direct neighbors of `id` in one chain direction
/// (`kind_filter`-aware), skipping dangling endpoints.
fn neighbors(
    connection: &Connection,
    id: &str,
    kind_filter: Option<&str>,
    role: &str,
) -> Result<Vec<String>> {
    let sql = match role {
        "caller" => {
            "SELECT DISTINCT e.src_id FROM source_code_edges e
             WHERE e.dst_id=?1 AND e.src_id IS NOT NULL
               AND (?2 IS NULL OR e.kind=?2)"
        }
        "callee" => {
            "SELECT DISTINCT e.dst_id FROM source_code_edges e
             WHERE e.src_id=?1 AND e.dst_id IS NOT NULL
               AND (?2 IS NULL OR e.kind=?2)"
        }
        other => unreachable!("neighbors role is caller or callee, got {other}"),
    };
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(params![id, kind_filter], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

struct SymbolRow {
    path: String,
    name: String,
    kind: String,
    line: usize,
    is_test: bool,
    signature: String,
}

impl SymbolRow {
    fn into_match(self, role: &'static str, depth: usize) -> CodeFindMatch {
        CodeFindMatch {
            role,
            path: Some(self.path),
            line: Some(self.line),
            symbol: Some(self.name),
            kind: Some(self.kind),
            signature: Some(self.signature),
            is_test: self.is_test,
            unresolved: false,
            depth,
            edge_kind: None,
            substring: false,
        }
    }
}

fn symbol_row(connection: &Connection, id: &str) -> Result<Option<SymbolRow>> {
    let mut statement = connection.prepare(
        "SELECT path,name,kind,line,is_test,signature FROM source_code_symbols WHERE id=?1",
    )?;
    statement
        .query_row(params![id], |row| {
            Ok(SymbolRow {
                path: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                line: row.get::<_, i64>(3)? as usize,
                is_test: row.get::<_, i64>(4)? != 0,
                signature: row.get(5)?,
            })
        })
        .optional()
        .map_err(Into::into)
}
