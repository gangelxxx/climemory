type SourceScan = (Vec<(Vec<CodeGrepMatch>, usize)>, bool);
use super::*;

/// One scope selector in command-line order (the
/// code-grep-path-scopes-slash-literals Proposal): --path selects an exact
/// project-relative file or a directory subtree, --glob a gitignore-style
/// pattern whose '!' prefix negates.
pub enum GrepSelector {
    Path(String),
    Glob(String),
}

/// The effective file scope for one grep run: the --path/--glob selectors in
/// command-line order, compiled into one gitignore-precedence filter (later
/// selectors override earlier ones, so a negated glob can carve matches out
/// of a path-selected subtree and a later positive glob re-includes them).
/// Summary/count records expose the structured paths/globs arrays and retain
/// the legacy single `glob` field when exactly one --glob was supplied.
pub struct GrepScope {
    selectors: Vec<GrepSelector>,
}

impl GrepScope {
    pub fn new(selectors: Vec<GrepSelector>) -> Self {
        Self { selectors }
    }

    pub fn is_empty(&self) -> bool {
        self.selectors.is_empty()
    }

    /// The --path selectors in command-line order.
    pub fn paths(&self) -> Vec<&str> {
        self.selectors
            .iter()
            .filter_map(|selector| match selector {
                GrepSelector::Path(path) => Some(path.as_str()),
                GrepSelector::Glob(_) => None,
            })
            .collect()
    }

    /// The --glob selectors in command-line order ('!' prefixes kept).
    pub fn globs(&self) -> Vec<&str> {
        self.selectors
            .iter()
            .filter_map(|selector| match selector {
                GrepSelector::Glob(glob) => Some(glob.as_str()),
                GrepSelector::Path(_) => None,
            })
            .collect()
    }

    /// Restrict discovery to anchored paths when other selectors only exclude
    /// files. Positive globs and basename paths may select files elsewhere.
    fn discovery_paths(&self, project: &Project) -> Option<Vec<PathBuf>> {
        if self.is_empty() {
            return None;
        }
        let paths: Option<Vec<_>> = self.selectors
            .iter()
            .filter(|selector| !matches!(selector, GrepSelector::Glob(pattern) if pattern.starts_with('!')))
            .map(|selector| {
                let GrepSelector::Path(path) = selector else {
                    return None;
                };
                let normalized = Self::normalize_path(path);
                if normalized.is_empty()
                    || normalized.ends_with(char::is_whitespace)
                    || normalized.contains(['*', '?', '[', ']', '{', '}', '!', '#', ':'])
                    || normalized
                        .split('/')
                        .any(|part| matches!(part, "" | "." | ".."))
                {
                    return None;
                }
                if !normalized.contains('/') && !project.root.join(&normalized).is_dir() {
                    return None;
                }
                Some(PathBuf::from(normalized))
            })
            .collect();
        // Exclusions alone still search the whole project.
        paths.filter(|paths| !paths.is_empty())
    }

    fn normalize_path(path: &str) -> String {
        let normalized = path.replace('\\', "/");
        normalized
            .strip_prefix("./")
            .unwrap_or(&normalized)
            .trim_end_matches('/')
            .to_string()
    }

    /// The legacy single-glob echo: Some(pattern) only when exactly one
    /// --glob was supplied.
    fn legacy_glob(&self) -> Option<&str> {
        let globs = self.globs();
        (globs.len() == 1).then(|| globs[0])
    }

    /// One gitignore-precedence override filter for the whole selector list,
    /// or None for an unscoped run. A --path naming a directory selects its
    /// subtree (`dir/**`), a file exactly itself; a missing path contributes
    /// a pattern that matches nothing, so a typo scopes to zero files (the
    /// no-files-selected diagnostic) instead of failing the scan.
    fn build_filter(&self, project: &Project) -> Result<Option<ignore::overrides::Override>> {
        if self.is_empty() {
            return Ok(None);
        }
        let mut builder = ignore::overrides::OverrideBuilder::new(&project.root);
        for selector in &self.selectors {
            match selector {
                GrepSelector::Glob(pattern) => {
                    builder.add(pattern).map_err(|error| {
                        AppError::new(format!("invalid --glob '{pattern}': {error}"))
                    })?;
                }
                GrepSelector::Path(path) => {
                    // '/' separators work on every platform; a leading './',
                    // Windows-style '\' separators, and trailing slashes
                    // normalize away; '.' (or a bare '/') means the whole
                    // project subtree.
                    let normalized = Self::normalize_path(path);
                    let pattern = match fs::metadata(project.root.join(&normalized)) {
                        _ if normalized.is_empty() || normalized == "." => "**".to_string(),
                        Ok(metadata) if metadata.is_dir() => format!("{normalized}/**"),
                        _ => normalized.to_string(),
                    };
                    builder.add(&pattern).map_err(|error| {
                        AppError::new(format!("invalid --path '{path}': {error}"))
                    })?;
                }
            }
        }
        builder
            .build()
            .map(Some)
            .map_err(|error| AppError::new(format!("invalid --path/--glob scope: {error}")))
    }
}

/// Umbrella `code-find-symbol-navigation` Phase 2 plus the
/// `code-grep-regex-flag` Proposal: line-oriented occurrence search over the
/// source inventory — the ripgrep-core answer ("which lines contain L")
/// inside cm, in literal (default) or --regex mode (see GrepMatcher).
/// Nothing derived is read or written: the scan reads working-tree files, so
/// it is fresh by construction and needs no index, lock, or freshness mode.
pub struct CodeGrepMatch {
    path: String,
    line: usize,
    text: String,
}

/// Line matcher for `code grep`. Literal mode is the default: a plain
/// substring test with a lowercase fold for --ignore-case. Regex mode
/// (--regex, the code-grep-regex-flag Proposal) compiles the positional as a
/// Rust regex-crate pattern (linear-time, no backtracking) matched per line;
/// --ignore-case maps to the regex crate's Unicode simple case folding,
/// which folds MORE than literal mode's lowercase fold (Greek final sigma ς
/// matches σ/Σ; pinned by code_grep_regex_fold_is_unicode_simple_folding_unlike_literal).
/// Default matching is per line; multiline mode projects whole-file match
/// ranges onto existing lines, preserving the line-oriented output contract.
pub enum GrepMatcher {
    Literal { needle: String, ignore_case: bool },
    Regex(regex::Regex),
}

impl GrepMatcher {
    pub fn literal(literal: &str, ignore_case: bool) -> Self {
        Self::Literal {
            needle: if ignore_case {
                literal.to_lowercase()
            } else {
                literal.to_string()
            },
            ignore_case,
        }
    }

    pub fn regex(pattern: &str, ignore_case: bool) -> Result<Self> {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .map(Self::Regex)
            .map_err(|error| {
                AppError::with_hint(
                    format!("invalid --regex pattern '{pattern}': {error}"),
                    "cm code grep \"fn (grep|find)\" --regex",
                )
            })
    }

    pub fn mode(&self) -> &'static str {
        match self {
            Self::Literal { .. } => "literal",
            Self::Regex(_) => "regex",
        }
    }

    fn is_match(&self, line: &str) -> bool {
        match self {
            Self::Literal {
                needle,
                ignore_case,
            } => {
                if *ignore_case {
                    line.to_lowercase().contains(needle.as_str())
                } else {
                    line.contains(needle.as_str())
                }
            }
            Self::Regex(regex) => regex.is_match(line),
        }
    }

    fn matching_lines(&self, text: &str, line_count: usize) -> Vec<bool> {
        // Normalize CRLF while retaining the final newline for regex anchors.
        // Literal folding is applied before computing offsets, so expanding
        // Unicode lowercase mappings cannot shift the projected line numbers.
        let normalized = text.replace("\r\n", "\n");
        let haystack = match self {
            Self::Literal {
                ignore_case: true, ..
            } => normalized.to_lowercase(),
            _ => normalized,
        };
        let mut starts = vec![0];
        starts.extend(haystack.match_indices('\n').map(|(offset, _)| offset + 1));
        let mut selected = vec![false; line_count];
        let mut mark = |start: usize, end: usize| {
            let first = starts
                .partition_point(|offset| *offset <= start)
                .saturating_sub(1);
            let last = starts
                .partition_point(|offset| *offset <= end.saturating_sub(1).max(start))
                .saturating_sub(1);
            if first < line_count {
                selected[first..=last.min(line_count - 1)].fill(true);
            }
        };
        match self {
            Self::Literal { needle, .. } => {
                for (start, found) in haystack.match_indices(needle.as_str()) {
                    mark(start, start + found.len());
                }
            }
            Self::Regex(regex) => {
                for found in regex.find_iter(&haystack) {
                    mark(found.start(), found.end());
                }
            }
        }
        selected
    }
}

#[derive(Clone, Copy, Default)]
pub struct GrepModes {
    pub invert_match: bool,
    pub multiline: bool,
    pub count_by_file: bool,
}

pub struct CodeGrepNeedle {
    query: String,
    matches: Vec<CodeGrepMatch>,
    total: usize,
    omitted: usize,
    file_counts: Vec<(String, usize)>,
}

/// Command-specific information for a next-page continuation.
pub struct GrepContinuationSpec {
    pub pattern_mode: bool,
    pub full_root: Option<String>,
}

pub struct CodeGrep {
    needles: Vec<CodeGrepNeedle>,
    files_scanned: usize,
    skipped_files: usize,
    discovery_ms: u128,
    scan_ms: u128,
    estimated_tokens: usize,
    count_only: bool,
    modes: GrepModes,
    mode: &'static str,
    presentation: SearchPresentation,
    continuation: Option<GrepContinuationSpec>,
}

impl CodeGrep {
    /// Continue the same needle at the next offset.
    fn continuation_edge(
        &self,
        needle: &CodeGrepNeedle,
        ignore_case: bool,
        scope: &GrepScope,
    ) -> Value {
        let spec = self
            .continuation
            .as_ref()
            .expect("continuation_edge is called only with a spec");
        let mut argv: Vec<String> = vec!["code".to_string(), "grep".to_string()];
        if spec.pattern_mode {
            argv.push(format!("--pattern={}", needle.query));
        }
        if self.mode == "regex" {
            argv.push("--regex".to_string());
        }
        if ignore_case {
            argv.push("--ignore-case".to_string());
        }
        if self.modes.invert_match {
            argv.push("--invert-match".to_string());
        }
        if self.modes.multiline {
            argv.push("--multiline".to_string());
        }
        // The scope selectors forward verbatim in command-line order so the
        // continued run sees the identical include/exclude precedence.
        for selector in &scope.selectors {
            match selector {
                GrepSelector::Path(path) => {
                    argv.push(format!("--path={path}"));
                }
                GrepSelector::Glob(glob) => {
                    argv.push(format!("--glob={glob}"));
                }
            }
        }
        self.presentation.continuation_args(
            &mut argv,
            self.presentation.offset.min(needle.total) + needle.matches.len(),
        );
        if let Some(root) = &spec.full_root {
            argv.push("--dir".to_string());
            argv.push(root.clone());
        }
        if !spec.pattern_mode {
            argv.extend(["--".into(), needle.query.clone()]);
        }
        json!({"argv": argv})
    }

    /// Per needle one summary record (its `query` labels the group) followed
    /// by that needle's match records — a single needle emits exactly the
    /// pre-batching record shape. --count emits one count record per needle.
    /// Both record kinds expose the effective scope as structured
    /// paths/globs arrays; the legacy single `glob` field is retained only
    /// for a zero-or-one-glob scope (omitted once several globs compose).
    pub fn records(&self, ignore_case: bool, scope: &GrepScope) -> Vec<Value> {
        let scope_fields = |record: &mut Value| {
            let object = record.as_object_mut().unwrap();
            if self.modes.invert_match {
                object.insert("invert_match".to_string(), json!(true));
            }
            if self.modes.multiline {
                object.insert("multiline".to_string(), json!(true));
            }
            object.insert("paths".to_string(), json!(scope.paths()));
            object.insert("globs".to_string(), json!(scope.globs()));
            if scope.globs().len() <= 1 {
                object.insert("glob".to_string(), json!(scope.legacy_glob()));
            }
        };
        if self.count_only {
            return self
                .needles
                .iter()
                .map(|needle| {
                    let mut record = json!({
                        "record": "code_grep_count",
                        "query": needle.query,
                        "mode": self.mode,
                        "ignore_case": ignore_case,
                        "files_scanned": self.files_scanned,
                        "skipped_files": self.skipped_files,
                        "matches": needle.total,
                        "thread_authority_unchanged": true,
                    });
                    scope_fields(&mut record);
                    if self.modes.count_by_file {
                        record["files"] = json!(needle
                            .file_counts
                            .iter()
                            .map(|(path, matches)| { json!({"path": path, "matches": matches}) })
                            .collect::<Vec<_>>());
                    }
                    record
                })
                .collect();
        }
        let mut records = Vec::new();
        for needle in &self.needles {
            let mut summary = json!({
                "record": "code_grep_summary",
                "query": needle.query,
                "mode": self.mode,
                "ignore_case": ignore_case,
                "files_scanned": self.files_scanned,
                "skipped_files": self.skipped_files,
                "matches": needle.total,
                "emitted": needle.matches.len(),
                "omitted": needle.omitted,
                "complete": needle.omitted == 0,
                "discovery_ms": self.discovery_ms,
                "scan_ms": self.scan_ms,
                "estimated_tokens": self.estimated_tokens,
                "thread_authority_unchanged": true,
            });
            scope_fields(&mut summary);
            if needle.total == 0 {
                // Registry item 555: the thread find no_hits_hint precedent —
                // name the cheapest recovery probes instead of a bare 0.
                summary.as_object_mut().unwrap().insert(
                    "no_hits_hint".to_string(),
                    json!(no_hits_hint(
                        self.mode,
                        ignore_case,
                        scope,
                        self.files_scanned
                    )),
                );
            }
            self.presentation
                .annotate_page(&mut summary, needle.total, needle.matches.len());
            if summary["remaining"].as_u64().unwrap_or(0) > 0 {
                summary["truncated_hint"] = json!("result capped by --limit; continue with next_offset and the same ordering, or narrow --path/--glob, or use --count; a shell-filtered pipe drops this summary record");
                if self.continuation.is_some() {
                    summary["continuation"] = self.continuation_edge(needle, ignore_case, scope);
                }
            }
            records.push(summary);
            records.extend(needle.matches.iter().map(|item| {
                let record = json!({
                    "record": "code_match",
                    "path": item.path,
                    "line": item.line,
                    "text": item.text,
                });
                record
            }));
        }
        records
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Keep the existing command/session boundary inputs explicit; bundling them solely for arity would obscure independent controls."
)]
pub fn grep(
    project: &Project,
    queries: &[String],
    ignore_case: bool,
    regex_mode: bool,
    scope: &GrepScope,
    presentation: SearchPresentation,
    count_only: bool,
    modes: GrepModes,
    continuation: Option<GrepContinuationSpec>,
) -> Result<CodeGrep> {
    // Compile every matcher before any I/O so an invalid --regex pattern
    // fails closed with no partial scan (the item-327 convention).
    let matchers = queries
        .iter()
        .map(|query| {
            if regex_mode {
                GrepMatcher::regex(query, ignore_case)
            } else {
                Ok(GrepMatcher::literal(query, ignore_case))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    // All path selectors apply before any file content is read: the scope
    // filters the grep inventory (recognized source extensions plus UTF-8
    // text config/doc files, gitignore rules intact), it never widens it.
    let filter = scope.build_filter(project)?;
    let discovery_paths = scope.discovery_paths(project);
    let inventory = discover_inventory_grep(project, discovery_paths.as_deref())?;
    let files = inventory
        .sources
        .iter()
        .filter(|source| {
            // Override inverts gitignore semantics: a plain pattern yields
            // Whitelist on match, so whitelisted = included by the scope.
            filter.as_ref().is_none_or(|filter| {
                filter
                    .matched(Path::new(&source.path), false)
                    .is_whitelist()
            })
        })
        .collect::<Vec<_>>();
    let scan_started = Instant::now();
    // One scan for every needle (registry item 536): each line is tested
    // against all matchers, so a batch costs one inventory walk.
    let count_only = count_only || modes.count_by_file;
    // In ascending file/line orders, a file cannot contribute beyond the
    // page end. Keep exact counts while avoiding unused line allocations.
    let retain = if !presentation.reverse && presentation.sort != SearchSort::Relevance {
        presentation.limit.map_or(usize::MAX, |limit| {
            presentation.offset.saturating_add(limit)
        })
    } else {
        usize::MAX
    };
    let outcomes = scan_sources(&files, &matchers, modes, count_only, retain);
    let mut per_needle: Vec<Vec<CodeGrepMatch>> = matchers.iter().map(|_| Vec::new()).collect();
    let mut file_counts: Vec<Vec<(String, usize)>> = matchers.iter().map(|_| Vec::new()).collect();
    let mut totals = vec![0; matchers.len()];
    let mut skipped_files = 0;
    for (source, (file_matches, skipped)) in files.iter().zip(outcomes) {
        for (index, (matches, total)) in file_matches.into_iter().enumerate() {
            per_needle[index].extend(matches);
            totals[index] += total;
            if modes.count_by_file && !skipped {
                file_counts[index].push((source.path.clone(), total));
            }
        }
        skipped_files += skipped as usize;
    }
    let mut needles = Vec::new();
    for (index, (query, mut matches)) in queries.iter().zip(per_needle).enumerate() {
        let modified: BTreeMap<&str, i64> = if presentation.sort == SearchSort::Mtime {
            files
                .iter()
                .map(|file| (file.path.as_str(), file.modified_ns))
                .collect()
        } else {
            BTreeMap::new()
        };
        let matcher = &matchers[index];
        matches.sort_by_cached_key(|item| {
            let rank = if presentation.sort == SearchSort::Relevance && !modes.invert_match {
                match matcher {
                    GrepMatcher::Literal {
                        needle,
                        ignore_case,
                    } => {
                        let text = if *ignore_case {
                            item.text.trim().to_lowercase()
                        } else {
                            item.text.trim().to_string()
                        };
                        usize::from(text != *needle)
                    }
                    GrepMatcher::Regex(regex) => usize::from(
                        !regex
                            .find(item.text.trim())
                            .is_some_and(|m| m.start() == 0 && m.end() == item.text.trim().len()),
                    ),
                }
            } else {
                0
            };
            (
                std::cmp::Reverse(modified.get(item.path.as_str()).copied().unwrap_or(0)),
                rank,
                item.path.clone(),
                item.line,
            )
        });
        if presentation.reverse {
            matches.reverse();
        }
        let matches = presentation.page(matches);
        let total = totals[index];
        file_counts[index].sort_by(|a, b| a.0.cmp(&b.0));
        needles.push(CodeGrepNeedle {
            query: query.clone(),
            omitted: total.saturating_sub(matches.len()),
            matches,
            total,
            file_counts: std::mem::take(&mut file_counts[index]),
        });
    }
    let result = CodeGrep {
        needles,
        files_scanned: files.len() - skipped_files,
        skipped_files,
        discovery_ms: inventory.discovery_ms,
        scan_ms: scan_started.elapsed().as_millis(),
        estimated_tokens: 0,
        count_only,
        modes,
        mode: matchers.first().map_or(
            if regex_mode { "regex" } else { "literal" },
            GrepMatcher::mode,
        ),
        presentation,
        continuation,
    };
    Ok(result)
}

/// Empty-result recovery hint (registry item 555), the code-side sibling of
/// thread find's no_hits_hint: names the cheapest next probes instead of
/// letting a bare matches:0 burn rephrase rounds. A 0-file scan under a
/// --path/--glob scope means no files were SELECTED — the hint names the
/// selectors as the likely cause and the grep inventory scope (recognized
/// source extensions plus UTF-8 text config/doc files) — while a 0-match scan over
/// selected files keeps the generic broaden-the-search probes; an
/// already-folded or already-regex search is not told to retry the flag it
/// already used.
fn no_hits_hint(mode: &str, ignore_case: bool, scope: &GrepScope, files_scanned: usize) -> String {
    if files_scanned == 0 && !scope.is_empty() {
        let mut selectors = Vec::new();
        for path in scope.paths() {
            selectors.push(format!("--path '{path}'"));
        }
        for glob in scope.globs() {
            selectors.push(format!("--glob '{glob}'"));
        }
        return format!(
            "no files selected by {} — the grep inventory covers recognized source extensions plus UTF-8 text config/doc files (json/jsonl/md/yaml/yml/toml/lock); check the selectors or drop them",
            selectors.join(", ")
        );
    }
    let mut hint = String::from("no matches; ");
    if !ignore_case {
        hint.push_str("retry with --ignore-case, ");
    }
    if mode == "literal" {
        hint.push_str("broaden the substring or try --regex for a pattern, ");
    } else {
        hint.push_str("broaden the pattern, ");
    }
    hint.push_str("or use `cm code find <name>` for symbol definitions/callers");
    hint
}

/// Scan full lines; output selection happens after sorting all candidates.
fn scan_source(
    source: &SourceFile,
    matchers: &[GrepMatcher],
    modes: GrepModes,
    count_only: bool,
    retain: usize,
) -> (Vec<(Vec<CodeGrepMatch>, usize)>, bool) {
    let empty = || matchers.iter().map(|_| (Vec::new(), 0)).collect();
    if source.size as u64 > MAX_SOURCE_BYTES {
        return (empty(), true);
    }
    let Ok(bytes) = fs::read(&source.absolute) else {
        return (empty(), true);
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let mut matches: Vec<(Vec<CodeGrepMatch>, usize)> = empty();
    for (matcher, (needle_matches, total)) in matchers.iter().zip(matches.iter_mut()) {
        let selected = modes
            .multiline
            .then(|| matcher.matching_lines(&text, lines.len()));
        for (index, line) in lines.iter().enumerate() {
            let matched = selected
                .as_ref()
                .map_or_else(|| matcher.is_match(line), |selected| selected[index]);
            if matched != modes.invert_match {
                *total += 1;
                if count_only || needle_matches.len() >= retain {
                    continue;
                }
                needle_matches.push(CodeGrepMatch {
                    path: source.path.clone(),
                    line: index + 1,
                    text: line.to_string(),
                });
            }
        }
    }
    (matches, false)
}

#[cfg(feature = "code-index")]
pub(super) fn worker_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::env::var("RAYON_NUM_THREADS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|threads| *threads > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map_or(1, usize::from)
                    .min(4)
            });
        if threads <= 1 {
            return None;
        }
        // A failed pool allocation falls back to serial processing.
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .ok()
    })
    .as_ref()
}

#[cfg(feature = "code-index")]
fn scan_sources(
    sources: &[&SourceFile],
    matchers: &[GrepMatcher],
    modes: GrepModes,
    count_only: bool,
    retain: usize,
) -> Vec<SourceScan> {
    let work_bytes = sources
        .iter()
        .fold(0usize, |bytes, source| {
            bytes.saturating_add(source.size.max(0) as usize)
        })
        .saturating_mul(matchers.len());
    if sources.len() > 1 && (sources.len() >= 32 || work_bytes >= 1024 * 1024) {
        if let Some(pool) = worker_pool() {
            return pool.install(|| {
                sources
                    .par_iter()
                    .map(|source| scan_source(source, matchers, modes, count_only, retain))
                    .collect()
            });
        }
    }
    sources
        .iter()
        .map(|source| scan_source(source, matchers, modes, count_only, retain))
        .collect()
}

#[cfg(not(feature = "code-index"))]
fn scan_sources(
    sources: &[&SourceFile],
    matchers: &[GrepMatcher],
    modes: GrepModes,
    count_only: bool,
    retain: usize,
) -> Vec<SourceScan> {
    sources
        .iter()
        .map(|source| scan_source(source, matchers, modes, count_only, retain))
        .collect()
}
