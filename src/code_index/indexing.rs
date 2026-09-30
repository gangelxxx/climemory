use super::*;

#[cfg(test)]
pub fn index(project: &Project, scope: Option<&Path>) -> Result<CodeIndexStats> {
    let _lock = project.code_index_lock()?;
    invalidate_scan_lease(project)?;
    let inventory = discover_inventory(project, scope)?;
    index_inventory_locked(project, inventory)
}

pub(super) fn index_inventory_locked(
    project: &Project,
    inventory: SourceInventory,
) -> Result<CodeIndexStats> {
    let work_started = Instant::now();
    let sources = &inventory.sources;
    let mut connection = open(project)?;
    // Only a full index republishes the scan epoch at the end; clearing it
    // for a scoped run would needlessly degrade global freshness on a no-op.
    if inventory.complete {
        clear_meta(&connection, "scan_epoch")?;
    }
    let stored = stored_files(&connection)?;
    let storage_initialized = meta(&connection, "storage_initialized")?.as_deref() == Some("1")
        || meta(&connection, "corpus_epoch")?.is_some();
    let fts_recovery = meta(&connection, "fts_rebuild_required")?.as_deref() == Some("1");
    let edge_recovery = meta(&connection, "edge_rebuild_required")?.as_deref() == Some("1");
    let seen = sources
        .iter()
        .map(|source| source.path.clone())
        .collect::<BTreeSet<_>>();
    let removed_paths = if inventory.complete {
        stored
            .keys()
            .filter(|path| !seen.contains(*path))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut timings = CodeIndexTimings {
        discovery_ms: inventory.discovery_ms,
        discovery_backend: inventory.discovery_backend.as_str().to_string(),
        ..CodeIndexTimings::default()
    };
    let corpus_files = stored.len().max(sources.len());
    let mut bulk_fts = should_bulk_fts(
        storage_initialized,
        fts_recovery,
        removed_paths.len(),
        corpus_files,
    );
    let full_edge_rebuild = !storage_initialized || edge_recovery;
    if bulk_fts {
        begin_bulk_fts(&connection)?;
    }
    let mut edge_marker_set = full_edge_rebuild;
    if edge_marker_set {
        set_meta(&connection, "edge_rebuild_required", "1")?;
    }
    let mut indexed = 0usize;
    let mut unchanged = 0usize;
    let mut dirty_paths = BTreeSet::new();
    let mut dirty_names = BTreeSet::new();

    let mut prepared_batches = PreparedBatchCursor::new(sources, &stored);
    loop {
        let prepare_started = Instant::now();
        let Some(prepared) = prepared_batches.next_batch()? else {
            break;
        };
        timings.prepare_ms += prepare_started.elapsed().as_millis();
        let has_writes = prepared
            .iter()
            .any(|item| !matches!(item, PreparedSource::Unchanged));
        let batch_indexed = prepared
            .iter()
            .filter(|item| matches!(item, PreparedSource::Changed { .. }))
            .count();
        let batch_unchanged = prepared.len() - batch_indexed;
        if !bulk_fts
            && should_bulk_fts(
                true,
                false,
                indexed
                    .saturating_add(batch_indexed)
                    .saturating_add(removed_paths.len()),
                corpus_files,
            )
        {
            begin_bulk_fts(&connection)?;
            bulk_fts = true;
        }
        let storage_started = Instant::now();
        if has_writes {
            store_prepared_batch(
                &mut connection,
                &prepared,
                &stored,
                full_edge_rebuild,
                &mut edge_marker_set,
                &mut dirty_paths,
                &mut dirty_names,
            )?;
        }
        indexed += batch_indexed;
        unchanged += batch_unchanged;
        timings.storage_ms += storage_started.elapsed().as_millis();
    }
    let prepare_work = prepared_batches.work;
    timings.read_work_ms = prepare_work.read.as_millis();
    timings.hash_work_ms = prepare_work.hash.as_millis();
    timings.parse_work_ms = prepare_work.parse.as_millis();
    timings.chunk_work_ms = prepare_work.chunk.as_millis();
    timings.read_files = prepare_work.read_files;
    timings.read_bytes = prepare_work.read_bytes;
    timings.prepared_peak_bytes = prepared_batches.prepared_peak_bytes;
    timings.lookahead_peak_bytes = prepared_batches.lookahead_peak_bytes;

    let removed = removed_paths.len();
    for paths in removed_paths.chunks(DELETE_BATCH_FILES) {
        let storage_started = Instant::now();
        let transaction = connection.transaction()?;
        let mut old_targets = BTreeMap::new();
        for path in paths {
            if !edge_marker_set {
                set_meta(&transaction, "edge_rebuild_required", "1")?;
                edge_marker_set = true;
            }
            dirty_paths.insert(path.clone());
            if !full_edge_rebuild {
                extend_definition_targets(
                    &mut old_targets,
                    definition_targets_for_path(&transaction, path)?,
                );
            }
        }
        if !full_edge_rebuild {
            dirty_names.extend(changed_global_definition_names(
                &transaction,
                &old_targets,
                &BTreeMap::new(),
            )?);
        }
        reset_storage_staging(&transaction)?;
        stage_delete_paths(&transaction, paths.iter().map(String::as_str))?;
        apply_storage_staging(&transaction, true)?;
        transaction.commit()?;
        timings.storage_ms += storage_started.elapsed().as_millis();
    }

    if bulk_fts {
        let fts_started = Instant::now();
        connection.execute_batch(CREATE_FTS_TRIGGERS)?;
        connection.execute(
            "INSERT INTO source_code_fts(source_code_fts) VALUES('rebuild')",
            [],
        )?;
        set_meta(&connection, "fts_rebuild_required", "0")?;
        timings.fts_ms = fts_started.elapsed().as_millis();
    }
    if full_edge_rebuild || !dirty_paths.is_empty() || !dirty_names.is_empty() {
        let edge_started = Instant::now();
        reconcile_project_edges(
            &mut connection,
            full_edge_rebuild,
            &dirty_paths,
            &dirty_names,
        )?;
        set_meta(&connection, "edge_rebuild_required", "0")?;
        timings.edge_ms = edge_started.elapsed().as_millis();
    }

    let finalize_started = Instant::now();
    let cached_counts = cached_counts(&connection)?;
    let cached_corpus_epoch = match meta(&connection, META_DERIVED_CORPUS_EPOCH)? {
        Some(epoch) => Some(epoch),
        None => meta(&connection, "corpus_epoch")?,
    };
    let derived_changed = indexed > 0 || removed > 0 || full_edge_rebuild;
    let requires_snapshot_validation = inventory.complete
        && (derived_changed || prepare_work.read_files > 0 || inventory.content_epoch.is_some());
    let refresh_derived_summary =
        derived_changed || cached_counts.is_none() || cached_corpus_epoch.is_none();
    let counts = if refresh_derived_summary {
        counts(&connection)?
    } else {
        cached_counts.expect("checked cached counts")
    };
    let corpus_epoch = if refresh_derived_summary {
        corpus_epoch(&connection)?
    } else {
        cached_corpus_epoch.expect("checked cached corpus epoch")
    };
    timings.finalize_ms = finalize_started.elapsed().as_millis();
    if requires_snapshot_validation {
        let validation_started = Instant::now();
        ensure_inventory_stable_before_publish(project, &inventory)?;
        timings.validation_ms = validation_started.elapsed().as_millis();
    }
    let publish_started = Instant::now();
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let publish_result = (|| -> Result<()> {
        if refresh_derived_summary {
            store_cached_counts(&connection, counts)?;
            set_meta(&connection, META_DERIVED_CORPUS_EPOCH, &corpus_epoch)?;
        }
        set_meta(&connection, "storage_initialized", "1")?;
        if inventory.complete {
            set_meta(&connection, "scan_epoch", &inventory.scan_epoch)?;
            set_meta(&connection, "corpus_epoch", &corpus_epoch)?;
        }
        Ok(())
    })();
    match publish_result {
        Ok(()) => connection.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(error);
        }
    }
    if inventory.complete && inventory.content_epoch.is_some() {
        write_scan_lease_best_effort(project, &inventory);
    }
    timings.publish_ms = publish_started.elapsed().as_millis();
    timings.total_ms =
        inventory.discovery_ms + inventory.content_scan_ms + work_started.elapsed().as_millis();
    Ok(CodeIndexStats {
        state: if inventory.complete {
            "fresh"
        } else {
            "partial"
        },
        action: if indexed == 0 && removed == 0 {
            "unchanged"
        } else if stored.is_empty() {
            "built"
        } else {
            "reconciled"
        },
        complete: inventory.complete,
        scanned: sources.len(),
        indexed,
        unchanged,
        removed,
        files: counts.0,
        symbols: counts.1,
        edges: counts.2,
        chunks: counts.3,
        corpus_epoch,
        scan_epoch: inventory.scan_epoch,
        timings,
    })
}

fn ensure_inventory_stable_before_publish(
    project: &Project,
    inventory: &SourceInventory,
) -> Result<()> {
    #[cfg(test)]
    pause_before_inventory_validation(&project.root);
    let mut current = discover_inventory(project, None)?;
    if inventory.content_epoch.is_some() {
        fingerprint_inventory_contents(&mut current)?;
    }
    if current.scan_epoch == inventory.scan_epoch
        && current.content_epoch == inventory.content_epoch
    {
        return Ok(());
    }
    invalidate_scan_lease(project)?;
    Err(AppError::with_hint(
        "source files changed while the source-code index was being built",
        "retry the code find command; its index refreshes automatically",
    )
    .with_retry())
}
