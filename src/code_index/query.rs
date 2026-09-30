use super::*;

/// Read-path metadata produced alongside the query result by [`fresh_read`].
pub(super) struct FreshReadMeta {
    pub status: CodeStatus,
    pub action: &'static str,
    pub scan_reused: bool,
    pub scan_age_ms: Option<u64>,
    #[cfg(test)]
    pub discovery_ms: u128,
    #[cfg(test)]
    pub content_scan_ms: u128,
}

/// Shared freshness entry for the code read paths (context, find): session
/// mode serves a recent strict scan via the lease and never touches the
/// writer lock; strict mode re-discovers and re-fingerprints the inventory
/// under the writer lock and, with `auto_index`, reconciles a stale index
/// in-process (otherwise it fails with the recovery argv). The query closure
/// runs on the read connection while the freshness guarantees hold (inside
/// the session transaction, or under the held writer lock).
pub(super) fn fresh_read<T>(
    project: &Project,
    auto_index: bool,
    freshness_mode: FreshnessMode,
    query: impl FnOnce(&Connection) -> Result<T>,
) -> Result<(T, FreshReadMeta)> {
    let mut connection = Some(open_for_read(project)?);
    if freshness_mode == FreshnessMode::Session {
        let transaction = connection
            .as_ref()
            .expect("read connection is available")
            .unchecked_transaction()?;
        if let Some((status, age)) = recent_status(project, &transaction)? {
            #[cfg(test)]
            pause_before_context_query(&project.root);
            let result = query(&transaction)?;
            transaction.commit()?;
            return Ok((
                result,
                FreshReadMeta {
                    status,
                    action: "reused",
                    scan_reused: true,
                    scan_age_ms: Some(age),
                    #[cfg(test)]
                    discovery_ms: 0,
                    #[cfg(test)]
                    content_scan_ms: 0,
                },
            ));
        }
        transaction.commit()?;
    }

    let index_lock = Some(project.code_index_lock()?);
    let mut inventory = discover_inventory(project, None)?;
    fingerprint_inventory_contents(&mut inventory)?;
    #[cfg(test)]
    let discovery_ms = inventory.discovery_ms;
    #[cfg(test)]
    let content_scan_ms = inventory.content_scan_ms;
    let before = status_inventory_from_connection(
        project,
        connection.as_ref().expect("read connection is available"),
        &inventory,
    )?;
    let (status, action) = if before.fresh {
        (before, "unchanged")
    } else if auto_index {
        debug_assert!(index_lock.is_some());
        invalidate_scan_lease(project)?;
        drop(connection.take());
        let stats = index_inventory_locked(project, inventory)?;
        connection = Some(open_for_read(project)?);
        (
            CodeStatus {
                state: stats.state,
                fresh: stats.complete,
                files: stats.files,
                symbols: stats.symbols,
                edges: stats.edges,
                chunks: stats.chunks,
                corpus_epoch: Some(stats.corpus_epoch.clone()),
                stored_scan_epoch: stats.complete.then(|| stats.scan_epoch.clone()),
                current_scan_epoch: stats.scan_epoch,
            },
            stats.action,
        )
    } else {
        return Err(AppError::with_hint(
            format!(
                "source-code index is {}; a fresh index is required",
                before.state
            ),
            "retry the code find command; its index refreshes automatically",
        )
        .with_retry());
    };
    #[cfg(test)]
    pause_before_context_query(&project.root);
    let connection = connection.expect("read connection restored after reconciliation");
    let result = query(&connection)?;
    drop(index_lock);
    Ok((
        result,
        FreshReadMeta {
            status,
            action,
            scan_reused: false,
            scan_age_ms: None,
            #[cfg(test)]
            discovery_ms,
            #[cfg(test)]
            content_scan_ms,
        },
    ))
}

#[cfg(test)]
pub fn context(
    project: &Project,
    query: &str,
    auto_index: bool,
    freshness_mode: FreshnessMode,
    budget_tokens: usize,
    limit: usize,
) -> Result<CodeContext> {
    let (candidates, meta) = fresh_read(project, auto_index, freshness_mode, |connection| {
        search_candidates(connection, query, limit.saturating_mul(8).max(40))
    })?;
    build_context(
        query,
        meta.status,
        meta.action,
        freshness_mode,
        meta.scan_reused,
        meta.scan_age_ms,
        meta.discovery_ms,
        meta.content_scan_ms,
        candidates,
        budget_tokens,
        limit,
    )
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn build_context(
    query: &str,
    status: CodeStatus,
    action: &'static str,
    freshness_mode: FreshnessMode,
    scan_reused: bool,
    scan_age_ms: Option<u64>,
    discovery_ms: u128,
    content_scan_ms: u128,
    candidates: Vec<CodeEvidence>,
    budget_tokens: usize,
    limit: usize,
) -> Result<CodeContext> {
    let available = candidates.len();
    let evidence = candidates.into_iter().take(limit).collect::<Vec<_>>();
    let mut context = CodeContext {
        status,
        action,
        omitted: available.saturating_sub(evidence.len()),
        evidence,
        estimated_tokens: 0,
        freshness_mode: freshness_mode.label(),
        scan_reused,
        scan_age_ms,
        discovery_ms,
        content_scan_ms,
    };
    // Standalone model output adds final batch statistics after this layer. Keep a
    // small deterministic reserve so --budget-tokens remains a bound on the whole
    // code JSONL batch, not merely on evidence bodies.
    let target = budget_tokens.saturating_sub(48);
    loop {
        let stats = output_stats(&context.records(query))?;
        context.estimated_tokens = stats.estimated_tokens;
        if stats.estimated_tokens <= target {
            break;
        }
        if context.evidence.is_empty() {
            return Err(AppError::with_hint(
                format!(
                    "--budget-tokens {budget_tokens} is too small for the code context summary"
                ),
                "cm code context \"<task>\" --budget-tokens 1000",
            ));
        }
        context.evidence.pop();
        context.omitted += 1;
    }
    Ok(context)
}
