use super::*;

#[test]
fn ambiguous_global_names_remain_unresolved() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/a.rs"), "pub fn shared() {}\n").unwrap();
    fs::write(temp.path().join("src/b.rs"), "pub fn shared() {}\n").unwrap();
    fs::write(
        temp.path().join("src/c.rs"),
        "pub fn caller() { shared(); external_only(); }\n",
    )
    .unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let resolved: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_code_edges WHERE dst_raw='shared' AND dst_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(resolved, 0);
    let external: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_code_edges WHERE dst_raw='external_only'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        external, 0,
        "external-only references should not enter the graph"
    );
}

#[test]
fn adding_a_definition_materializes_an_existing_raw_reference() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("src/caller.rs"),
        "pub fn caller() { added_later(); }\n",
    )
    .unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let before: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_code_edges WHERE dst_raw='added_later'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, 0);
    drop(connection);

    fs::write(
        temp.path().join("src/definition.rs"),
        "pub fn added_later() {}\n",
    )
    .unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let after: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_code_edges WHERE dst_raw='added_later' AND dst_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, 1);
}

#[test]
fn prepared_batches_are_bounded_by_files_and_resident_payload() {
    fn source(index: usize, size: i64) -> SourceFile {
        SourceFile {
            absolute: PathBuf::from(format!("fixture-{index}.rs")),
            path: format!("src/fixture-{index}.rs"),
            language: "rust".to_string(),
            size,
            modified_ns: 0,
            content_hash: None,
        }
    }

    let sources = (0..200).map(|index| source(index, 1)).collect::<Vec<_>>();
    let stored = sources
        .iter()
        .map(|source| {
            (
                source.path.clone(),
                StoredFile {
                    content_hash: String::new(),
                    size: source.size,
                    modified_ns: source.modified_ns,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut cursor = PreparedBatchCursor::new(&sources, &stored);
    assert_eq!(
        cursor.next_batch().unwrap().unwrap().len(),
        PREPARE_BATCH_MAX_FILES
    );
    assert_eq!(
        cursor.next_batch().unwrap().unwrap().len(),
        200 - PREPARE_BATCH_MAX_FILES
    );
    assert!(cursor.next_batch().unwrap().is_none());

    let dense = PreparedSource::Changed {
        source: source(0, 64),
        content_hash: "a".repeat(64),
        parsed: code::CodeParse {
            defs: (0..200)
                .map(|index| code::Def {
                    name: format!("dense_definition_{index}"),
                    kind: "function".to_string(),
                    line: index + 1,
                    end_line: index + 1,
                    signature: "pub fn dense_definition()".repeat(4),
                    is_test: false,
                })
                .collect(),
            refs: (0..400)
                .map(|index| code::Ref {
                    name: format!("dense_reference_{index}"),
                    line: index + 1,
                    kind: "call".to_string(),
                })
                .collect(),
        },
        chunks: vec![Chunk {
            key: "b".repeat(64),
            start_line: 1,
            end_line: 400,
            is_test: false,
            symbols: "dense ".repeat(200),
            body: "dense body\n".repeat(400),
        }],
    };
    let dense_bytes = dense.resident_bytes();
    assert!(
        dense_bytes > 128,
        "derived payload must outweigh two raw fixtures"
    );
    assert!(prepared_batch_would_overflow(
        1,
        dense_bytes,
        dense_bytes,
        dense_bytes.saturating_mul(2).saturating_sub(1),
    ));
}

#[test]
fn bulk_fts_is_reserved_for_initial_recovery_or_large_changes() {
    assert!(should_bulk_fts(false, false, 0, 10_000));
    assert!(should_bulk_fts(true, true, 0, 10_000));
    assert!(!should_bulk_fts(
        true,
        false,
        BULK_FTS_MIN_CHANGES - 1,
        1_000,
    ));
    assert!(should_bulk_fts(true, false, BULK_FTS_MIN_CHANGES, 1_000,));
    assert!(!should_bulk_fts(true, false, BULK_FTS_MIN_CHANGES, 10_000,));
    assert!(should_bulk_fts(true, false, 2_000, 10_000));
}

#[test]
fn interrupted_migration_is_recreated_instead_of_trusted() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let connection = open(&project).unwrap();
    // Simulate a crash after DELETE FROM source_code_meta: no version and
    // a leftover table with an incompatible layout.
    connection
        .execute_batch(
            "DELETE FROM source_code_meta;
             DROP TABLE source_code_symbols;
             CREATE TABLE source_code_symbols(bogus TEXT NOT NULL);
             INSERT INTO source_code_symbols(bogus) VALUES('legacy');",
        )
        .unwrap();
    assert!(!schema_is_current(&connection).unwrap());
    drop(connection);

    let connection = open(&project).unwrap();
    assert!(schema_is_current(&connection).unwrap());
    let bogus: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type='table' AND name='source_code_symbols' AND sql LIKE '%bogus%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(bogus, 0);
    let columns: i64 = connection
        .query_row(
            "SELECT count(*) FROM pragma_table_info('source_code_symbols')
             WHERE name IN ('id','path','name','kind','line','signature','is_test')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(columns, 7);
    assert_eq!(
        meta(&connection, "schema_version").unwrap().as_deref(),
        Some(SCHEMA_VERSION)
    );
}

#[test]
fn new_paths_skip_replacement_deletes_but_changed_paths_replace() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let source = temp.path().join("src/lib.rs");
    fs::write(&source, "pub fn first_version() {}\n").unwrap();

    DELETE_BATCH_RUNS.with(|runs| runs.set(0));
    let initial = index(&project, None).unwrap();
    assert_eq!(initial.indexed, 1);
    DELETE_BATCH_RUNS.with(|runs| assert_eq!(runs.get(), 0));

    fs::write(&source, "pub fn second_version() {}\n").unwrap();
    DELETE_BATCH_RUNS.with(|runs| runs.set(0));
    let replaced = index(&project, None).unwrap();
    assert_eq!(replaced.indexed, 1);
    DELETE_BATCH_RUNS.with(|runs| assert_eq!(runs.get(), 1));
}

#[test]
fn mixed_reconciliation_uses_set_based_storage_and_keeps_exact_results() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let changed = temp.path().join("src/changed.rs");
    let metadata = temp.path().join("src/metadata.rs");
    let removed = temp.path().join("src/removed.rs");
    fs::write(&changed, "pub fn old_changed() {}\n").unwrap();
    fs::write(&metadata, "pub fn metadata_only() {}\n").unwrap();
    fs::write(&removed, "pub fn removed_symbol() {}\n").unwrap();
    index(&project, None).unwrap();

    std::thread::sleep(Duration::from_millis(20));
    fs::write(&changed, "pub fn changed_v2() {}\n").unwrap();
    let metadata_bytes = fs::read(&metadata).unwrap();
    fs::write(&metadata, metadata_bytes).unwrap();
    fs::remove_file(&removed).unwrap();
    fs::write(
        temp.path().join("src/added.rs"),
        "pub fn newly_added() {}\n",
    )
    .unwrap();

    DELETE_BATCH_RUNS.with(|runs| runs.set(0));
    let stats = index(&project, None).unwrap();
    assert_eq!(stats.indexed, 2);
    assert_eq!(stats.unchanged, 1);
    assert_eq!(stats.removed, 1);
    assert_eq!(stats.files, 3);
    DELETE_BATCH_RUNS.with(|runs| assert_eq!(runs.get(), 2));

    let connection = open(&project).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM source_code_symbols", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert!(search_candidates(&connection, "changed v2", 20)
        .unwrap()
        .iter()
        .any(|candidate| candidate.path == "src/changed.rs"));
    assert!(search_candidates(&connection, "removed symbol", 20)
        .unwrap()
        .is_empty());
}

#[test]
fn unchanged_index_and_status_reuse_cached_counts_and_corpus_epoch() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn cached_summary_marker() {}\n",
    )
    .unwrap();
    let initial = index(&project, None).unwrap();

    COUNT_SCANS.with(|scans| scans.set(0));
    CORPUS_SCANS.with(|scans| scans.set(0));
    let discoveries_before = DISCOVERY_RUNS.with(std::cell::Cell::get);
    let unchanged = index(&project, None).unwrap();
    let discoveries_after = DISCOVERY_RUNS.with(std::cell::Cell::get);
    assert_eq!(unchanged.action, "unchanged");
    assert_eq!(unchanged.corpus_epoch, initial.corpus_epoch);
    assert_eq!(unchanged.files, initial.files);
    let fresh = status(&project).unwrap();
    assert!(fresh.fresh);
    assert_eq!(fresh.files, initial.files);
    assert_eq!(discoveries_after - discoveries_before, 1);
    COUNT_SCANS.with(|scans| assert_eq!(scans.get(), 0));
    CORPUS_SCANS.with(|scans| assert_eq!(scans.get(), 0));
}

#[test]
fn scale_fixture_covers_initial_unchanged_and_one_file_reconciliation() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    for index in 0..300 {
        fs::write(
            temp.path().join(format!("src/fixture_{index:03}.rs")),
            format!(
                "pub fn target_{index}() {{}}\npub fn caller_{index}() {{ target_{index}(); }}\n"
            ),
        )
        .unwrap();
    }

    let initial = index(&project, None).unwrap();
    assert_eq!(initial.indexed, 300);
    assert_eq!(initial.files, 300);
    assert_eq!(initial.timings.discovery_backend, "walk");
    assert_eq!(initial.timings.read_files, 300);
    assert!(initial.timings.read_bytes > 0);
    assert!(initial.timings.prepared_peak_bytes > 0);
    assert!(initial.timings.lookahead_peak_bytes > 0);
    let unchanged = index(&project, None).unwrap();
    assert_eq!(unchanged.indexed, 0);
    assert_eq!(unchanged.unchanged, 300);
    assert_eq!(unchanged.timings.read_files, 0);
    assert_eq!(unchanged.timings.read_bytes, 0);

    let before_touch = discover_sources(&project, None)
        .unwrap()
        .0
        .into_iter()
        .map(|source| (source.path, source.modified_ns))
        .collect::<BTreeMap<_, _>>();
    std::thread::sleep(Duration::from_millis(20));
    for index in 0..BULK_FTS_MIN_CHANGES {
        let path = temp.path().join(format!("src/fixture_{index:03}.rs"));
        let bytes = fs::read(&path).unwrap();
        fs::write(path, bytes).unwrap();
    }
    let after_touch = discover_sources(&project, None).unwrap().0;
    let metadata_changes = after_touch
        .iter()
        .filter(|source| before_touch.get(&source.path) != Some(&source.modified_ns))
        .count();
    assert!(metadata_changes >= BULK_FTS_MIN_CHANGES);
    BULK_FTS_STARTS.with(|starts| starts.set(0));
    let metadata_only = index(&project, None).unwrap();
    assert_eq!(metadata_only.indexed, 0);
    assert_eq!(metadata_only.timings.fts_ms, 0);
    assert_eq!(metadata_only.timings.read_files, BULK_FTS_MIN_CHANGES);
    BULK_FTS_STARTS.with(|starts| assert_eq!(starts.get(), 0));

    fs::write(
        temp.path().join("src/fixture_000.rs"),
        "pub fn target_0_v2() {}\npub fn caller_0() { target_0_v2(); }\n",
    )
    .unwrap();
    let reconciled = index(&project, None).unwrap();
    assert_eq!(reconciled.indexed, 1);
    assert_eq!(reconciled.unchanged, 299);
    assert_eq!(reconciled.files, 300);

    for index in 0..BULK_FTS_MIN_CHANGES {
        fs::write(
            temp.path().join(format!("src/fixture_{index:03}.rs")),
            format!(
                "// bulk_reload_marker\npub fn bulk_target_{index}() {{}}\npub fn bulk_caller_{index}() {{ bulk_target_{index}(); }}\n"
            ),
        )
        .unwrap();
    }
    DELETE_BATCH_RUNS.with(|runs| runs.set(0));
    BULK_FTS_STARTS.with(|starts| starts.set(0));
    let bulk = index(&project, None).unwrap();
    assert_eq!(bulk.indexed, BULK_FTS_MIN_CHANGES);
    assert_eq!(bulk.timings.read_files, BULK_FTS_MIN_CHANGES);
    BULK_FTS_STARTS.with(|starts| assert_eq!(starts.get(), 1));
    DELETE_BATCH_RUNS.with(|runs| {
        assert_eq!(
            runs.get(),
            BULK_FTS_MIN_CHANGES.div_ceil(PREPARE_BATCH_MAX_FILES)
        )
    });
    let connection = open(&project).unwrap();
    assert_eq!(
        meta(&connection, "fts_rebuild_required")
            .unwrap()
            .as_deref(),
        Some("0")
    );
    assert!(!search_candidates(&connection, "bulk reload marker", 20)
        .unwrap()
        .is_empty());
}
