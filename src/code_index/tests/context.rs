use super::*;

#[test]
fn strict_context_detects_same_size_same_mtime_content_changes() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let source_path = temp.path().join("source.rs");
    let original = "pub fn old_name() {}\n";
    let replacement = "pub fn new_name() {}\n";
    assert_eq!(original.len(), replacement.len());
    fs::write(&source_path, original).unwrap();
    index(&project, None).unwrap();
    let baseline = context(&project, "old_name", true, FreshnessMode::Strict, 2_000, 10).unwrap();
    assert!(!baseline.scan_reused);
    let original_modified = fs::metadata(&source_path).unwrap().modified().unwrap();

    fs::write(&source_path, replacement).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&source_path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .unwrap();
    assert!(status(&project).unwrap().fresh);

    let reused = context(
        &project,
        "new_name",
        true,
        FreshnessMode::Session,
        2_000,
        10,
    )
    .unwrap();
    assert!(reused.scan_reused);
    assert_eq!(reused.action, "reused");

    let strict = context(&project, "new_name", true, FreshnessMode::Strict, 2_000, 10).unwrap();
    assert!(strict.status.fresh);
    assert_eq!(strict.action, "reconciled");
    assert!(strict
        .evidence
        .iter()
        .any(|item| item.symbol.as_deref() == Some("new_name")));
}

#[test]
fn adjacent_task_words_normalize_to_source_path_phrases() {
    let phrases = path_phrases("agent run terminal outcome");
    assert!(phrases.contains(&"agent-run-terminal-outcome".to_string()));
    assert!(phrases.contains(&"run-terminal-outcome".to_string()));
    assert!(structural_terms("agent run terminal outcome")
        .contains(&"agentrunterminaloutcome".to_string()));
}

#[test]
fn result_diversity_prefers_production_then_keeps_a_representative_test() {
    fn evidence(path: &str, is_test: bool, score: i64) -> CodeEvidence {
        CodeEvidence {
            path: path.to_string(),
            start_line: 1,
            end_line: 10,
            reason: "text".to_string(),
            symbol: None,
            kind: None,
            is_test,
            snippet: path.to_string(),
            score,
        }
    }

    let original = vec![
        evidence("tests/high.test.ts", true, 900),
        evidence("tests/second.test.ts", true, 800),
        evidence("src/owner.ts", false, 700),
        evidence("src/server.ts", false, 600),
        evidence("tests/third.test.ts", true, 500),
        evidence("src/wait.ts", false, 400),
        evidence("src/session.ts", false, 300),
    ];
    let mut diversified = original.clone();
    prioritize_production_and_test_evidence(&mut diversified, false);
    assert_eq!(
        diversified
            .iter()
            .take(5)
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>(),
        vec![
            "src/owner.ts",
            "src/server.ts",
            "src/wait.ts",
            "src/session.ts",
            "tests/high.test.ts",
        ]
    );

    let mut test_query = original.clone();
    prioritize_production_and_test_evidence(&mut test_query, true);
    assert_eq!(
        test_query
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>(),
        original
            .iter()
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>()
    );
    assert!(query_has_test_intent(
        "one two three four five six seven eight nine ten eleven twelve longword test"
    ));
}

#[test]
fn compound_symbol_suffix_is_ranked_as_the_owner_definition() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src/agents")).unwrap();
    fs::create_dir_all(temp.path().join("src/gateway")).unwrap();
    fs::write(
        temp.path().join("src/agents/agent-run-terminal-outcome.ts"),
        "/** Builds the terminal outcome for an agent run. */\nexport function buildAgentRunTerminalOutcome() { return { reason: 'done' }; }\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("src/gateway/session-lifecycle-state.ts"),
        "import { buildAgentRunTerminalOutcome } from '../agents/agent-run-terminal-outcome';\nexport function persistLifecycle() { return buildAgentRunTerminalOutcome(); }\n",
    )
    .unwrap();
    index(&project, None).unwrap();

    let context = context(
        &project,
        "terminal outcome lifecycle",
        false,
        FreshnessMode::Strict,
        1_000,
        20,
    )
    .unwrap();
    let owner = context.evidence.first().unwrap();
    assert_eq!(owner.path, "src/agents/agent-run-terminal-outcome.ts");
    assert_eq!(
        owner.symbol.as_deref(),
        Some("buildAgentRunTerminalOutcome")
    );
    assert_eq!(owner.reason, "definition");
}

#[test]
fn an_initialized_empty_corpus_is_fresh_not_missing() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    index(&project, None).unwrap();
    let status = status(&project).unwrap();
    assert_eq!(status.state, "fresh");
    assert!(status.fresh);
    assert_eq!(status.files, 0);
    assert!(context(&project, "anything", false, FreshnessMode::Strict, 1, 20).is_err());
}

#[test]
fn index_is_incremental_and_context_combines_symbols_text_and_tests() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::create_dir_all(temp.path().join("tests")).unwrap();
    fs::write(
        temp.path().join("src/service.ts"),
        "// retry grace for terminal outcomes\nexport const terminalOutcome = () => retryGrace();\nfunction retryGrace() { return true; }\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("tests/service.test.ts"),
        "import { terminalOutcome } from '../src/service';\ntest('terminal outcome retry grace', () => terminalOutcome());\n",
    )
    .unwrap();

    let first = index(&project, None).unwrap();
    assert_eq!(first.action, "built");
    assert_eq!(first.files, 2);
    assert!(first.symbols >= 2);
    let second = index(&project, None).unwrap();
    assert_eq!(second.action, "unchanged");
    assert_eq!(second.unchanged, 2);

    let context = context(
        &project,
        "terminal outcome retry grace",
        false,
        FreshnessMode::Strict,
        1_000,
        20,
    )
    .unwrap();
    assert!(context.status.fresh);
    assert!(context
        .evidence
        .iter()
        .any(|item| item.path == "src/service.ts"));
    assert!(context.evidence.iter().any(|item| item.is_test));
}

#[test]
fn context_survives_an_unwritable_scan_lease() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/lib.rs"), "pub fn lease_marker() {}\n").unwrap();
    index(&project, None).unwrap();
    // Make the lease path a directory so the atomic write fails.
    fs::remove_file(scan_lease_path(&project)).ok();
    fs::create_dir_all(scan_lease_path(&project)).unwrap();

    let result = context(
        &project,
        "lease_marker",
        false,
        FreshnessMode::Strict,
        1_000,
        10,
    )
    .unwrap();
    assert!(!result.evidence.is_empty());
}

#[test]
fn symbol_anchor_extent_resolves_to_the_next_symbol_or_file_end() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("src/anchor.rs"),
        "pub fn anchored() {}\n// padding\n// more padding\npub fn follower() {}\n",
    )
    .unwrap();
    index(&project, None).unwrap();

    // The extent runs through the line before the next symbol.
    assert_eq!(
        symbol_anchor_extent(&project, "src/anchor.rs", "anchored").unwrap(),
        (1, 3)
    );
    // The last symbol extends to the file end.
    assert_eq!(
        symbol_anchor_extent(&project, "src/anchor.rs", "follower").unwrap(),
        (4, 4)
    );
    // Names are exact and path-scoped: case and other files do not leak in.
    assert!(symbol_anchor_extent(&project, "src/anchor.rs", "Anchored").is_err());
    fs::write(temp.path().join("src/other.rs"), "pub fn elsewhere() {}\n").unwrap();
    index(&project, None).unwrap();
    assert!(symbol_anchor_extent(&project, "src/anchor.rs", "elsewhere").is_err());
    assert_eq!(
        symbol_anchor_extent(&project, "src/other.rs", "elsewhere").unwrap(),
        (1, 1)
    );
}

#[test]
fn symbol_anchor_extent_miss_ambiguity_and_index_state_are_actionable() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("src/dup.rs"),
        "pub struct Alpha {}\nimpl Alpha {\n    pub fn dup(&self) {}\n}\npub struct Beta {}\nimpl Beta {\n    pub fn dup(&self) {}\n}\n",
    )
    .unwrap();

    // A missing index fails with the `cm code index` recovery.
    let missing = symbol_anchor_extent(&project, "src/dup.rs", "dup").unwrap_err();
    assert!(missing.msg.contains("index is missing"), "{missing:?}");
    assert!(
        missing
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("cm code index"),
        "{missing:?}"
    );

    index(&project, None).unwrap();

    // Zero matches hint `cm code context` to locate the symbol, and (registry
    // item 413) name up to 3 indexed symbols of the cited file.
    let miss = symbol_anchor_extent(&project, "src/dup.rs", "nope").unwrap_err();
    assert!(miss.msg.contains("no symbol named 'nope'"), "{miss:?}");
    assert!(
        miss.hint
            .as_deref()
            .unwrap_or_default()
            .contains("cm code context \"nope\""),
        "{miss:?}"
    );
    let hint = miss.hint.as_deref().unwrap_or_default();
    assert!(hint.contains("indexed symbols in src/dup.rs:"), "{miss:?}");
    let indexed = miss
        .details
        .extra
        .as_ref()
        .unwrap()
        .get("indexed_symbols")
        .unwrap();
    let names = indexed
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 3, "{miss:?}");
    assert!(names.contains(&"dup".to_string()), "{miss:?}");
    // `dup` has two definitions in the fixture: candidates dedupe by name so
    // 3 slots name 3 distinct symbols.
    let unique: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "{miss:?}");
    assert!(
        indexed
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["line"].is_number()),
        "{miss:?}"
    );

    // A file with zero indexed symbols keeps the plain remedies hint.
    fs::write(temp.path().join("src/empty.rs"), "\n").unwrap();
    index(&project, None).unwrap();
    let bare = symbol_anchor_extent(&project, "src/empty.rs", "nope").unwrap_err();
    assert!(
        !bare
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("indexed symbols"),
        "{bare:?}"
    );
    assert!(bare.details.extra.is_none(), "{bare:?}");

    // With 4+ distinct symbols the list caps at 3, nearest levenshtein first.
    fs::write(
        temp.path().join("src/many.rs"),
        "pub fn alpha_one() {}\npub fn alpha_two() {}\npub fn alpha_three() {}\npub fn zzz_far() {}\n",
    )
    .unwrap();
    index(&project, None).unwrap();
    let capped = symbol_anchor_extent(&project, "src/many.rs", "alpha_on").unwrap_err();
    let capped_names = capped.details.extra.as_ref().unwrap()["indexed_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(capped_names.len(), 3, "{capped:?}");
    assert_eq!(
        capped_names,
        vec!["alpha_one", "alpha_two", "alpha_three"],
        "{capped:?}"
    );

    // Several same-name definitions fail naming the candidate lines.
    let ambiguous = symbol_anchor_extent(&project, "src/dup.rs", "dup").unwrap_err();
    assert!(ambiguous.msg.contains("ambiguous"), "{ambiguous:?}");
    assert!(ambiguous.msg.contains("3"), "{ambiguous:?}");
    assert!(ambiguous.msg.contains("7"), "{ambiguous:?}");

    // A stale index refuses to resolve and points at `cm code index`.
    fs::write(temp.path().join("src/dup.rs"), "pub fn replaced() {}\n").unwrap();
    let stale = symbol_anchor_extent(&project, "src/dup.rs", "replaced").unwrap_err();
    assert!(stale.msg.contains("index is stale"), "{stale:?}");
    assert!(
        stale
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("cm code index"),
        "{stale:?}"
    );
}
