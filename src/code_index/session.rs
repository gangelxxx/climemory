use super::*;

pub(super) fn recent_status(
    project: &Project,
    connection: &Connection,
) -> Result<Option<(CodeStatus, u64)>> {
    let Some(stored_scan_epoch) = meta(connection, "scan_epoch")? else {
        return Ok(None);
    };
    let Some(lease) = read_scan_lease(project)? else {
        return Ok(None);
    };
    if lease.scan_epoch != stored_scan_epoch {
        return Ok(None);
    }
    let Some(age) = unix_time_ms().checked_sub(lease.scanned_at_ms) else {
        return Ok(None);
    };
    if age > SESSION_FRESHNESS_MS {
        return Ok(None);
    }
    let counts = counts_for_read(connection)?;
    Ok(Some((
        CodeStatus {
            state: "fresh",
            fresh: true,
            files: counts.0,
            symbols: counts.1,
            edges: counts.2,
            chunks: counts.3,
            corpus_epoch: meta(connection, "corpus_epoch")?,
            stored_scan_epoch: Some(stored_scan_epoch.clone()),
            current_scan_epoch: stored_scan_epoch,
        },
        age,
    )))
}

pub(super) fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub(super) fn scan_lease_path(project: &Project) -> PathBuf {
    project.health.join(SESSION_LEASE_FILE)
}

fn write_scan_lease(project: &Project, scan_epoch: &str, scanned_at_ms: u64) -> Result<()> {
    let lease = ScanLease {
        scan_epoch: scan_epoch.to_string(),
        scanned_at_ms,
    };
    atomic_write(&scan_lease_path(project), &serde_json::to_vec(&lease)?)
}

// The lease only accelerates session freshness; failing to write it must not
// fail an otherwise successful command.
pub(super) fn write_scan_lease_best_effort(project: &Project, inventory: &SourceInventory) {
    if let Err(error) = write_scan_lease(project, &inventory.scan_epoch, inventory.discovered_at_ms)
    {
        eprintln!(
            "warning: scan lease was not written (session freshness reuse disabled): {}",
            error.msg
        );
    }
}

fn read_scan_lease(project: &Project) -> Result<Option<ScanLease>> {
    let bytes = match fs::read(scan_lease_path(project)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

pub(super) fn invalidate_scan_lease(project: &Project) -> Result<()> {
    match fs::remove_file(scan_lease_path(project)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
