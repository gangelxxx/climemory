use super::*;

#[test]
fn session_freshness_reuses_recent_strict_scan_and_expires() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let source = temp.path().join("src/service.ts");
    fs::write(
        &source,
        "export function originalMarker() { return true; }\n",
    )
    .unwrap();
    index(&project, None).unwrap();

    let strict = context(
        &project,
        "original marker",
        true,
        FreshnessMode::Strict,
        1_000,
        20,
    )
    .unwrap();
    assert!(!strict.scan_reused);

    let reused = context(
        &project,
        "original marker",
        true,
        FreshnessMode::Session,
        1_000,
        20,
    )
    .unwrap();
    assert!(reused.scan_reused);
    assert_eq!(reused.action, "reused");
    assert!(reused.scan_age_ms.is_some());

    let connection = open_for_read(&project).unwrap();
    let scan_epoch = meta(&connection, "scan_epoch").unwrap().unwrap();
    drop(connection);
    atomic_write(
        &scan_lease_path(&project),
        &serde_json::to_vec(&ScanLease {
            scan_epoch,
            scanned_at_ms: 0,
        })
        .unwrap(),
    )
    .unwrap();
    assert!(status(&project).unwrap().fresh);
    let after_status = context(
        &project,
        "original marker",
        true,
        FreshnessMode::Session,
        1_000,
        20,
    )
    .unwrap();
    assert!(!after_status.scan_reused);
    assert_eq!(after_status.action, "unchanged");

    fs::write(
        &source,
        "export function originalMarker() { return true; }\nexport function recentMarker() { return true; }\n",
    )
    .unwrap();
    let still_reused = context(
        &project,
        "recent marker",
        true,
        FreshnessMode::Session,
        1_000,
        20,
    )
    .unwrap();
    assert!(still_reused.scan_reused);
    assert!(still_reused.evidence.is_empty());

    let strict = context(
        &project,
        "recent marker",
        true,
        FreshnessMode::Strict,
        1_000,
        20,
    )
    .unwrap();
    assert!(!strict.scan_reused);
    assert_eq!(strict.action, "reconciled");
    assert!(strict.evidence.iter().any(|item| {
        item.symbol.as_deref() == Some("recentMarker") || item.path == "src/service.ts"
    }));

    index(&project, Some(&source)).unwrap();
    let after_partial_index = context(
        &project,
        "recent marker",
        true,
        FreshnessMode::Session,
        1_000,
        20,
    )
    .unwrap();
    assert!(!after_partial_index.scan_reused);
    assert_eq!(after_partial_index.action, "unchanged");

    fs::write(
        &source,
        "export function expiredMarker() { return 'expired session scan'; }\n",
    )
    .unwrap();
    let connection = open_for_read(&project).unwrap();
    let scan_epoch = meta(&connection, "scan_epoch").unwrap().unwrap();
    drop(connection);
    let expired_lease = ScanLease {
        scan_epoch,
        scanned_at_ms: 0,
    };
    atomic_write(
        &scan_lease_path(&project),
        &serde_json::to_vec(&expired_lease).unwrap(),
    )
    .unwrap();
    let expired = context(
        &project,
        "expired marker",
        true,
        FreshnessMode::Session,
        1_000,
        20,
    )
    .unwrap();
    assert!(!expired.scan_reused);
    assert_eq!(expired.action, "reconciled");
    assert!(expired
        .evidence
        .iter()
        .any(|item| item.path == "src/service.ts"));
}

#[test]
fn strict_stale_context_reuses_its_discovered_inventory() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let source = temp.path().join("src/service.rs");
    fs::write(&source, "pub fn original() {}\n").unwrap();
    index(&project, None).unwrap();
    fs::write(&source, "pub fn changed() {}\n").unwrap();

    let before = DISCOVERY_RUNS.with(std::cell::Cell::get);
    let result = context(&project, "changed", true, FreshnessMode::Strict, 1_000, 20).unwrap();
    let after = DISCOVERY_RUNS.with(std::cell::Cell::get);

    assert_eq!(result.action, "reconciled");
    assert_eq!(after - before, 2);
}

#[test]
fn scoped_noop_index_preserves_the_scan_epoch() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let source = temp.path().join("src/lib.rs");
    fs::write(&source, "pub fn alpha() {}\n").unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    assert!(meta(&connection, "scan_epoch").unwrap().is_some());
    drop(connection);

    index(&project, Some(&source)).unwrap();
    let connection = open(&project).unwrap();
    assert!(
        meta(&connection, "scan_epoch").unwrap().is_some(),
        "a scoped no-op index must not degrade global freshness"
    );
}
