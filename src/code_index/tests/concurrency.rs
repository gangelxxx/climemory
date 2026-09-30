use super::*;

#[test]
fn source_change_before_publication_never_marks_the_old_inventory_fresh() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let source = temp.path().join("src/service.rs");
    let original = "pub fn original() {}\n";
    fs::write(&source, original).unwrap();
    let initial = index(&project, None).unwrap();
    let original_modified = fs::metadata(&source).unwrap().modified().unwrap();
    fs::write(&source, "pub fn prepared_version() {}\n").unwrap();

    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    PRE_PUBLISH_HOOKS.lock().unwrap().push(PrePublishHook {
        root: project.root.clone(),
        reached: reached_tx,
        resume: resume_rx,
    });
    let worker_project = project.clone();
    let worker = std::thread::spawn(move || index(&worker_project, None));

    reached_rx
        .recv_timeout(HOOK_WAIT)
        .expect("indexing should pause before freshness publication");
    fs::write(&source, "pub fn final_version() {}\n").unwrap();
    resume_tx.send(()).unwrap();
    let error = worker
        .join()
        .unwrap()
        .expect_err("an unstable source snapshot must not be published");
    assert!(error.msg.contains("changed while"));
    assert!(error.details.retry);
    assert!(!status(&project).unwrap().fresh);

    fs::write(&source, original).unwrap();
    fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .unwrap();
    let reverted = status(&project).unwrap();
    assert_eq!(reverted.current_scan_epoch, initial.scan_epoch);
    assert!(reverted.stored_scan_epoch.is_none());
    assert!(!reverted.fresh);

    fs::write(&source, "pub fn final_version() {}\n").unwrap();
    let retry = index(&project, None).unwrap();
    assert!(retry.complete);
    let context = context(
        &project,
        "final_version",
        false,
        FreshnessMode::Strict,
        1_000,
        20,
    )
    .unwrap();
    assert!(context
        .evidence
        .iter()
        .any(|item| item.path == "src/service.rs"));
}

#[test]
fn strict_metadata_reconciliation_revalidates_before_publication() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    let source = temp.path().join("service.rs");
    fs::write(&source, "pub fn original() {}\n").unwrap();
    index(&project, None).unwrap();
    let modified = fs::metadata(&source).unwrap().modified().unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified + Duration::from_secs(2)))
        .unwrap();

    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    PRE_PUBLISH_HOOKS.lock().unwrap().push(PrePublishHook {
        root: project.root.clone(),
        reached: reached_tx,
        resume: resume_rx,
    });
    let worker_project = project.clone();
    let worker = std::thread::spawn(move || {
        context(
            &worker_project,
            "original",
            true,
            FreshnessMode::Strict,
            1_000,
            20,
        )
    });

    reached_rx
        .recv_timeout(HOOK_WAIT)
        .expect("strict metadata reconciliation should revalidate its snapshot");
    fs::write(&source, "pub fn replacement() {}\n").unwrap();
    resume_tx.send(()).unwrap();
    let error = worker
        .join()
        .unwrap()
        .expect_err("a changed strict snapshot must not be published");
    assert!(error.msg.contains("source files changed while"));
    assert!(!status(&project).unwrap().fresh);
}

#[test]
fn direct_index_waits_for_its_writer_lock_before_discovery() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/initial.rs"), "pub fn initial() {}\n").unwrap();

    let held_lock = project.code_index_lock().unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    DISCOVERY_HOOKS
        .lock()
        .unwrap()
        .push((project.root.clone(), started_tx));
    let worker_project = project.clone();
    let worker = std::thread::spawn(move || index(&worker_project, None));

    assert!(
        started_rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "discovery ran before the code-index writer lock was acquired"
    );
    fs::write(temp.path().join("src/late.rs"), "pub fn late() {}\n").unwrap();
    drop(held_lock);

    started_rx
        .recv_timeout(HOOK_WAIT)
        .expect("discovery should start after the writer lock is released");
    let stats = worker.join().unwrap().unwrap();
    assert_eq!(stats.files, 2);
    assert_eq!(stats.indexed, 2);
}

#[test]
fn strict_context_holds_the_writer_lock_through_its_evidence_query() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/lib.rs"), "pub fn evidence() {}\n").unwrap();
    index(&project, None).unwrap();

    let (query_reached_tx, query_reached_rx) = std::sync::mpsc::channel();
    let (query_resume_tx, query_resume_rx) = std::sync::mpsc::channel();
    CONTEXT_QUERY_HOOKS.lock().unwrap().push(ContextQueryHook {
        root: project.root.clone(),
        reached: query_reached_tx,
        resume: query_resume_rx,
    });
    let context_project = project.clone();
    let context_worker = std::thread::spawn(move || {
        context(
            &context_project,
            "evidence",
            false,
            FreshnessMode::Strict,
            1_000,
            20,
        )
    });
    query_reached_rx
        .recv_timeout(HOOK_WAIT)
        .expect("context should pause before querying evidence");

    let (discovery_tx, discovery_rx) = std::sync::mpsc::channel();
    DISCOVERY_HOOKS
        .lock()
        .unwrap()
        .push((project.root.clone(), discovery_tx));
    let index_project = project.clone();
    let index_worker = std::thread::spawn(move || index(&index_project, None));
    assert!(
        discovery_rx
            .recv_timeout(Duration::from_millis(150))
            .is_err(),
        "a writer started discovery before the context evidence query finished"
    );

    query_resume_tx.send(()).unwrap();
    let result = context_worker.join().unwrap().unwrap();
    assert!(result.evidence.iter().any(|item| item.path == "src/lib.rs"));
    discovery_rx
        .recv_timeout(HOOK_WAIT)
        .expect("writer discovery should start after the context releases its lock");
    index_worker.join().unwrap().unwrap();
}

#[test]
fn session_context_reads_status_and_evidence_from_one_database_snapshot() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let source = temp.path().join("src/lib.rs");
    fs::write(&source, "pub fn original_marker() {}\n").unwrap();
    index(&project, None).unwrap();
    context(
        &project,
        "original_marker",
        false,
        FreshnessMode::Strict,
        1_000,
        20,
    )
    .expect("a strict scan should establish the session freshness lease");

    let (query_reached_tx, query_reached_rx) = std::sync::mpsc::channel();
    let (query_resume_tx, query_resume_rx) = std::sync::mpsc::channel();
    CONTEXT_QUERY_HOOKS.lock().unwrap().push(ContextQueryHook {
        root: project.root.clone(),
        reached: query_reached_tx,
        resume: query_resume_rx,
    });
    let context_project = project.clone();
    let context_worker = std::thread::spawn(move || {
        context(
            &context_project,
            "concurrent_marker",
            true,
            FreshnessMode::Session,
            1_000,
            20,
        )
    });
    query_reached_rx
        .recv_timeout(HOOK_WAIT)
        .expect("session context should pause inside its read transaction");

    fs::write(&source, "pub fn concurrent_marker() {}\n").unwrap();
    let writer_project = project.clone();
    std::thread::spawn(move || index(&writer_project, None))
        .join()
        .unwrap()
        .expect("WAL writer should complete beside a session read snapshot");

    query_resume_tx.send(()).unwrap();
    let reused = context_worker.join().unwrap().unwrap();
    assert!(reused.scan_reused);
    assert!(reused.evidence.is_empty());

    let current = context(
        &project,
        "concurrent_marker",
        false,
        FreshnessMode::Strict,
        1_000,
        20,
    )
    .unwrap();
    assert!(current
        .evidence
        .iter()
        .any(|item| item.path == "src/lib.rs"));
}

#[test]
fn code_index_does_not_contend_with_memory_source_lock() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/lib.rs"), "pub fn indexed() {}\n").unwrap();

    let _thread_index_lock = project.source_lock().unwrap();
    let stats = index(&project, None).unwrap();
    assert_eq!(stats.indexed, 1);
}

#[test]
fn fresh_context_is_sqlite_read_only_and_reuses_one_connection() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn read_only_marker() {}\n",
    )
    .unwrap();
    index(&project, None).unwrap();

    CONNECTION_OPENS.with(|opens| opens.set(0));
    let writer = Connection::open(project.store_path()).unwrap();
    writer.busy_timeout(Duration::from_millis(50)).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let result = context(
        &project,
        "read_only_marker",
        true,
        FreshnessMode::Strict,
        1_000,
        20,
    )
    .unwrap();
    writer.execute_batch("ROLLBACK").unwrap();

    assert_eq!(result.action, "unchanged");
    assert!(result.evidence.iter().any(|item| item.path == "src/lib.rs"));
    CONNECTION_OPENS.with(|opens| assert_eq!(opens.get(), 1));
}
