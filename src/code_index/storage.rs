use super::*;

pub(super) fn should_bulk_fts(
    storage_initialized: bool,
    recovery_required: bool,
    changes: usize,
    corpus_files: usize,
) -> bool {
    !storage_initialized
        || recovery_required
        || (changes >= BULK_FTS_MIN_CHANGES
            && (changes as u128) * 100
                >= (corpus_files.max(1) as u128) * (BULK_FTS_MIN_CHANGE_PERCENT as u128))
}

pub(super) fn begin_bulk_fts(connection: &Connection) -> Result<()> {
    #[cfg(test)]
    BULK_FTS_STARTS.with(|starts| starts.set(starts.get() + 1));
    set_meta(connection, "fts_rebuild_required", "1")?;
    connection.execute_batch(DROP_FTS_TRIGGERS)?;
    Ok(())
}

pub(super) fn open(project: &Project) -> Result<Connection> {
    fs::create_dir_all(&project.data)?;
    let connection = connect(project, true)?;
    ensure_schema(&connection)?;
    Ok(connection)
}

pub(super) fn open_for_read(project: &Project) -> Result<Connection> {
    if project.store_path().is_file() {
        let connection = connect(project, false)?;
        if schema_is_current(&connection)? {
            return Ok(connection);
        }
    }
    // Schema migration from a read path runs under the writer lock so it
    // cannot race an in-flight indexing process.
    let _lock = project.code_index_lock()?;
    open(project)
}

fn connect(project: &Project, configure_writes: bool) -> Result<Connection> {
    #[cfg(test)]
    CONNECTION_OPENS.with(|opens| opens.set(opens.get() + 1));
    for suffix in ["", "-wal", "-shm", "-journal"] {
        Project::checked_path(
            &project.data,
            &project.data.join(format!("code-index.db{suffix}")),
        )?;
    }
    let connection = Connection::open(project.store_path())?;
    connection.busy_timeout(Duration::from_secs(3))?;
    if configure_writes {
        let journal_mode =
            connection.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            connection.pragma_update(None, "journal_mode", "WAL")?;
        }
        connection.pragma_update(None, "synchronous", "NORMAL")?;
    }
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -32_768)?;
    Ok(connection)
}

pub(super) fn schema_is_current(connection: &Connection) -> Result<bool> {
    let has_meta = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='source_code_meta')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !has_meta || meta(connection, "schema_version")?.as_deref() != Some(SCHEMA_VERSION) {
        return Ok(false);
    }
    let core_tables = connection.query_row(
        "SELECT count(*) FROM sqlite_master
         WHERE type='table' AND name IN (
             'source_code_files','source_code_symbols','source_code_edges',
             'source_code_edge_staging','source_code_chunks','source_code_fts'
         )",
        [],
        |row| row.get::<_, usize>(0),
    )?;
    Ok(core_tables == 6)
}

fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_code_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);",
    )?;
    let version = meta(connection, "schema_version")?;
    // A missing version with leftover tables means an interrupted migration:
    // drop and recreate atomically instead of trusting the old layout.
    if version.as_deref() != Some(SCHEMA_VERSION) {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "DROP TABLE IF EXISTS source_code_fts;
             DROP TABLE IF EXISTS source_code_chunks;
             DROP TABLE IF EXISTS source_code_edge_staging;
             DROP TABLE IF EXISTS source_code_edges;
             DROP TABLE IF EXISTS source_code_symbols;
             DROP TABLE IF EXISTS source_code_files;
             DELETE FROM source_code_meta;",
        )?;
        transaction.execute_batch(SCHEMA)?;
        set_meta(&transaction, "schema_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        return Ok(());
    }
    connection.execute_batch(SCHEMA)?;
    set_meta(connection, "schema_version", SCHEMA_VERSION)
}

pub(super) fn meta(connection: &Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT value FROM source_code_meta WHERE key=?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

pub(super) fn set_meta(connection: &Connection, key: &str, value: &str) -> Result<()> {
    if meta(connection, key)?.as_deref() == Some(value) {
        return Ok(());
    }
    connection.execute(
        "INSERT INTO source_code_meta(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub(super) fn clear_meta(connection: &Connection, key: &str) -> Result<()> {
    connection.execute("DELETE FROM source_code_meta WHERE key=?1", params![key])?;
    Ok(())
}

pub(super) fn stored_files(connection: &Connection) -> Result<BTreeMap<String, StoredFile>> {
    let mut statement = connection.prepare(
        "SELECT path,content_hash,size,modified_ns FROM source_code_files ORDER BY path",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get(0)?,
            StoredFile {
                content_hash: row.get(1)?,
                size: row.get(2)?,
                modified_ns: row.get(3)?,
            },
        ))
    })?;
    let mut values = BTreeMap::new();
    for row in rows {
        let (path, value) = row?;
        values.insert(path, value);
    }
    Ok(values)
}

pub(super) fn store_prepared_batch(
    connection: &mut Connection,
    prepared: &[PreparedSource],
    stored: &BTreeMap<String, StoredFile>,
    full_edge_rebuild: bool,
    edge_marker_set: &mut bool,
    dirty_paths: &mut BTreeSet<String>,
    dirty_names: &mut BTreeSet<String>,
) -> Result<()> {
    let transaction = connection.transaction()?;
    let has_changed = prepared
        .iter()
        .any(|item| matches!(item, PreparedSource::Changed { .. }));
    if has_changed && !*edge_marker_set {
        set_meta(&transaction, "edge_rebuild_required", "1")?;
        *edge_marker_set = true;
    }
    let mut old_targets = BTreeMap::new();
    let mut new_targets = BTreeMap::new();
    for item in prepared {
        let PreparedSource::Changed { source, parsed, .. } = item else {
            continue;
        };
        dirty_paths.insert(source.path.clone());
        if !full_edge_rebuild {
            let source_old_targets = if stored.contains_key(&source.path) {
                definition_targets_for_path(&transaction, &source.path)?
            } else {
                BTreeMap::new()
            };
            extend_definition_targets(&mut old_targets, source_old_targets);
            extend_definition_targets(&mut new_targets, parsed_definition_targets(source, parsed));
        }
    }
    if !full_edge_rebuild {
        dirty_names.extend(changed_global_definition_names(
            &transaction,
            &old_targets,
            &new_targets,
        )?);
    }
    reset_storage_staging(&transaction)?;
    let replacement_paths = stage_prepared_storage(&transaction, prepared, stored)?;
    apply_storage_staging(&transaction, replacement_paths > 0)?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn reset_storage_staging(connection: &Connection) -> Result<()> {
    connection.execute_batch(STORAGE_STAGING_SCHEMA)?;
    Ok(())
}

fn stage_prepared_storage(
    connection: &Connection,
    prepared: &[PreparedSource],
    stored: &BTreeMap<String, StoredFile>,
) -> Result<usize> {
    let mut insert_delete = connection
        .prepare("INSERT OR IGNORE INTO source_code_store_delete_paths(value) VALUES(?1)")?;
    let mut insert_metadata = connection.prepare(
        "INSERT INTO source_code_store_metadata(path,size,modified_ns) VALUES(?1,?2,?3)",
    )?;
    let mut insert_file = connection.prepare(
        "INSERT INTO source_code_store_files(path,language,content_hash,size,modified_ns)
         VALUES(?1,?2,?3,?4,?5)",
    )?;
    let mut insert_symbol = connection.prepare(
        "INSERT INTO source_code_store_symbols(id,path,name,kind,line,signature,is_test)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
    )?;
    let mut insert_edge = connection.prepare(
        "INSERT INTO source_code_store_edges(src_id,src_path,dst_raw,line,kind)
         VALUES(?1,?2,?3,?4,?5)",
    )?;
    let mut insert_chunk = connection.prepare(
        "INSERT INTO source_code_store_chunks(
             chunk_key,path,start_line,end_line,is_test,symbols,body
         ) VALUES(?1,?2,?3,?4,?5,?6,?7)",
    )?;
    let mut replacement_paths = 0usize;
    for item in prepared {
        match item {
            PreparedSource::Unchanged => {}
            PreparedSource::Metadata(source) => {
                insert_metadata.execute(params![source.path, source.size, source.modified_ns])?;
            }
            PreparedSource::Changed {
                source,
                content_hash,
                parsed,
                chunks,
            } => {
                if stored.contains_key(&source.path) {
                    insert_delete.execute(params![source.path])?;
                    replacement_paths += 1;
                }
                insert_file.execute(params![
                    source.path,
                    source.language,
                    content_hash,
                    source.size,
                    source.modified_ns
                ])?;
                // Ownership candidates are CALLABLES only (function/method
                // declarations and named or variable-bound function/arrow
                // expressions — both tag as `function`/`method`); a struct,
                // class or const/static range never owns a call.
                let owners = parsed
                    .defs
                    .iter()
                    .enumerate()
                    .filter(|(_, definition)| {
                        definition.kind == "function" || definition.kind == "method"
                    })
                    .map(|(ordinal, definition)| {
                        (
                            definition.line,
                            definition.end_line,
                            ordinal,
                            code::symbol_id(
                                &source.path,
                                &definition.kind,
                                &definition.name,
                                definition.line,
                            ),
                        )
                    })
                    .collect::<Vec<_>>();
                let mut definition_ids = BTreeSet::new();
                for definition in &parsed.defs {
                    let id = code::symbol_id(
                        &source.path,
                        &definition.kind,
                        &definition.name,
                        definition.line,
                    );
                    if !definition_ids.insert(id.clone()) {
                        continue;
                    }
                    insert_symbol.execute(params![
                        id,
                        source.path,
                        definition.name,
                        definition.kind,
                        definition.line as i64,
                        definition.signature,
                        definition.is_test as i64
                    ])?;
                }
                let mut references = BTreeSet::new();
                for reference in &parsed.refs {
                    if code::is_stdlib_combinator(&reference.name)
                        || reference.name.chars().count() < 2
                    {
                        continue;
                    }
                    // Lexical ownership: the owner is the DEEPEST ENCLOSING
                    // callable — the smallest callable range containing the
                    // reference — not the nearest preceding definition. Calls
                    // inside React hooks, arrow functions and nested/anonymous
                    // callbacks stay with their enclosing named callable; a
                    // reference outside every callable keeps a NULL owner.
                    let owner = owners
                        .iter()
                        .filter(|(line, end_line, _, _)| {
                            *line <= reference.line && reference.line <= *end_line
                        })
                        .min_by_key(|(line, end_line, ordinal, _)| (end_line - line, *ordinal))
                        .map(|(_, _, _, id)| id.as_str());
                    // Kind joins the dedup key: one line can legitimately
                    // hold two same-name refs of different kinds (e.g. an
                    // impl header and a tuple-struct call), and both edges
                    // must survive.
                    if !references.insert((
                        owner,
                        reference.name.as_str(),
                        reference.line,
                        reference.kind.as_str(),
                    )) {
                        continue;
                    }
                    insert_edge.execute(params![
                        owner,
                        source.path,
                        reference.name,
                        reference.line as i64,
                        reference.kind
                    ])?;
                }
                for chunk in chunks {
                    insert_chunk.execute(params![
                        chunk.key,
                        source.path,
                        chunk.start_line as i64,
                        chunk.end_line as i64,
                        chunk.is_test as i64,
                        chunk.symbols,
                        chunk.body
                    ])?;
                }
            }
        }
    }
    Ok(replacement_paths)
}

pub(super) fn stage_delete_paths<'a>(
    connection: &Connection,
    paths: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let mut insert = connection
        .prepare("INSERT OR IGNORE INTO source_code_store_delete_paths(value) VALUES(?1)")?;
    for path in paths {
        insert.execute(params![path])?;
    }
    Ok(())
}

pub(super) fn apply_storage_staging(
    connection: &Connection,
    _deletes_existing: bool,
) -> Result<()> {
    #[cfg(test)]
    if _deletes_existing {
        DELETE_BATCH_RUNS.with(|runs| runs.set(runs.get() + 1));
    }
    connection.execute_batch(APPLY_STORAGE_STAGING)?;
    Ok(())
}

pub(super) fn reconcile_project_edges(
    connection: &mut Connection,
    full_rebuild: bool,
    dirty_paths: &BTreeSet<String>,
    dirty_names: &BTreeSet<String>,
) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS source_code_dirty_paths(
             value TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         CREATE TEMP TABLE IF NOT EXISTS source_code_dirty_names(
             value TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         DELETE FROM source_code_dirty_paths;
         DELETE FROM source_code_dirty_names;",
    )?;
    {
        let mut insert = transaction
            .prepare("INSERT OR IGNORE INTO source_code_dirty_paths(value) VALUES(?1)")?;
        for path in dirty_paths {
            insert.execute(params![path])?;
        }
    }
    {
        let mut insert = transaction
            .prepare("INSERT OR IGNORE INTO source_code_dirty_names(value) VALUES(?1)")?;
        for name in dirty_names {
            insert.execute(params![name])?;
        }
    }
    if full_rebuild {
        transaction.execute("DELETE FROM source_code_edges", [])?;
        transaction.execute(
            "INSERT INTO source_code_edges(src_id,src_path,dst_id,dst_raw,line,kind)
             SELECT s.src_id,s.src_path,NULL,s.dst_raw,s.line,s.kind
             FROM source_code_edge_staging s
             WHERE EXISTS(
                 SELECT 1 FROM source_code_symbols d WHERE d.name=s.dst_raw
             )",
            [],
        )?;
    } else {
        transaction.execute(
            "DELETE FROM source_code_edges
             WHERE src_path IN (SELECT value FROM source_code_dirty_paths)
                OR (
                    dst_raw IN (SELECT value FROM source_code_dirty_names)
                    AND NOT EXISTS(
                        SELECT 1 FROM source_code_symbols same_file
                        WHERE same_file.name=source_code_edges.dst_raw
                          AND same_file.path=source_code_edges.src_path
                    )
                )",
            [],
        )?;
        transaction.execute(
            "INSERT INTO source_code_edges(src_id,src_path,dst_id,dst_raw,line,kind)
             SELECT s.src_id,s.src_path,NULL,s.dst_raw,s.line,s.kind
             FROM source_code_edge_staging s
             WHERE (
                 s.src_path IN (SELECT value FROM source_code_dirty_paths)
                 OR (
                     s.dst_raw IN (SELECT value FROM source_code_dirty_names)
                     AND NOT EXISTS(
                         SELECT 1 FROM source_code_symbols same_file
                         WHERE same_file.name=s.dst_raw AND same_file.path=s.src_path
                     )
                 )
             )
             AND EXISTS(
                 SELECT 1 FROM source_code_symbols d WHERE d.name=s.dst_raw
             )",
            [],
        )?;
    }
    let affected = if full_rebuild {
        String::new()
    } else {
        "WHERE e.src_path IN (SELECT value FROM source_code_dirty_paths)
            OR (
                e.dst_raw IN (SELECT value FROM source_code_dirty_names)
                AND NOT EXISTS(
                    SELECT 1 FROM source_code_symbols same_file
                    WHERE same_file.name=e.dst_raw AND same_file.path=e.src_path
                )
            )"
        .to_string()
    };
    transaction.execute_batch(&format!(
        "UPDATE source_code_edges AS e
         SET dst_id = CASE
             WHEN (
                 SELECT count(*) FROM source_code_symbols same_file
                 WHERE same_file.name=e.dst_raw AND same_file.path=e.src_path
             ) = 1
             THEN (
                 SELECT same_file.id FROM source_code_symbols same_file
                 WHERE same_file.name=e.dst_raw AND same_file.path=e.src_path
                 ORDER BY same_file.line,same_file.id LIMIT 1
             )
             WHEN (
                 SELECT count(*) FROM source_code_symbols same_file
                 WHERE same_file.name=e.dst_raw AND same_file.path=e.src_path
             ) = 0
             AND (
                 SELECT count(*) FROM source_code_symbols anywhere
                 WHERE anywhere.name=e.dst_raw
             ) = 1
             THEN (
                 SELECT anywhere.id FROM source_code_symbols anywhere
                 WHERE anywhere.name=e.dst_raw
                 ORDER BY anywhere.path,anywhere.line,anywhere.id LIMIT 1
             )
             ELSE NULL
         END
         {affected};"
    ))?;
    transaction.commit()?;
    Ok(())
}

type CodeCounts = (usize, usize, usize, usize);

pub(super) fn cached_counts(connection: &Connection) -> Result<Option<CodeCounts>> {
    let values = [
        meta(connection, META_COUNT_FILES)?,
        meta(connection, META_COUNT_SYMBOLS)?,
        meta(connection, META_COUNT_EDGES)?,
        meta(connection, META_COUNT_CHUNKS)?,
    ];
    let parsed = values
        .iter()
        .map(|value| {
            value
                .as_deref()
                .and_then(|value| value.parse::<usize>().ok())
        })
        .collect::<Option<Vec<_>>>();
    Ok(parsed.map(|values| (values[0], values[1], values[2], values[3])))
}

pub(super) fn store_cached_counts(connection: &Connection, counts: CodeCounts) -> Result<()> {
    set_meta(connection, META_COUNT_FILES, &counts.0.to_string())?;
    set_meta(connection, META_COUNT_SYMBOLS, &counts.1.to_string())?;
    set_meta(connection, META_COUNT_EDGES, &counts.2.to_string())?;
    set_meta(connection, META_COUNT_CHUNKS, &counts.3.to_string())
}

pub(super) fn counts_for_read(connection: &Connection) -> Result<CodeCounts> {
    match cached_counts(connection)? {
        Some(counts) => Ok(counts),
        None => counts(connection),
    }
}

pub(super) fn counts(connection: &Connection) -> Result<CodeCounts> {
    #[cfg(test)]
    COUNT_SCANS.with(|scans| scans.set(scans.get() + 1));
    let files = scalar_count(connection, "source_code_files")?;
    let symbols = scalar_count(connection, "source_code_symbols")?;
    let edges = scalar_count(connection, "source_code_edges")?;
    let chunks = scalar_count(connection, "source_code_chunks")?;
    Ok((files, symbols, edges, chunks))
}

fn scalar_count(connection: &Connection, table: &str) -> Result<usize> {
    let sql = format!("SELECT count(*) FROM {table}");
    Ok(connection.query_row(&sql, [], |row| row.get::<_, i64>(0))? as usize)
}

pub(super) fn corpus_epoch(connection: &Connection) -> Result<String> {
    #[cfg(test)]
    CORPUS_SCANS.with(|scans| scans.set(scans.get() + 1));
    let mut statement =
        connection.prepare("SELECT path,content_hash FROM source_code_files ORDER BY path")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut hasher = Sha256::new();
    for row in rows {
        let (path, content_hash) = row?;
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(content_hash.as_bytes());
        hasher.update(b"\n");
    }
    Ok(format!("{:x}", hasher.finalize()))
}
