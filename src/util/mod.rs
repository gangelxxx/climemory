use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod ids;
mod locks;
#[cfg(test)]
use locks::*;
mod error;
mod io;
pub use error::AppError;
pub(crate) use ids::closest_value;
#[cfg(test)]
use ids::iso_utc_millis;
#[cfg(test)]
pub(crate) use ids::levenshtein;
#[allow(unused_imports)]
pub use ids::{digest, fresh_id, iso_now, iso_utc, now_epoch, Result};
#[cfg(feature = "code-index")]
pub use io::preview;
pub use io::{atomic_write, decode_body_bytes, init_console_utf8, normalize_newlines};
pub use locks::FileLock;
pub(crate) use locks::{open_regular_file_no_follow, opened_file_matches_path};
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const FILE_LOCK_HELPER_ROOT: &str = "CLIMEMORY_TEST_FILE_LOCK_HELPER_ROOT";
    const LEGACY_LOCK_HELPER_ROOT: &str = "CLIMEMORY_TEST_LEGACY_LOCK_HELPER_ROOT";
    const OWNED_LOCK_HELPER_ROOT: &str = "CLIMEMORY_TEST_OWNED_LOCK_HELPER_ROOT";
    const OWNED_LOCK_HELPER_KIND: &str = "CLIMEMORY_TEST_OWNED_LOCK_KIND";
    const OWNED_LOCK_HELPER_PROJECT: &str = "CLIMEMORY_TEST_OWNED_LOCK_PROJECT";
    const OWNED_LOCK_HELPER_BUILD: &str = "CLIMEMORY_TEST_OWNED_LOCK_BUILD";

    #[test]
    fn sha256_is_stable_and_cryptographic_length() {
        assert_eq!(
            digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn timestamp_shape_is_iso_utc() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn millisecond_timestamp_shape_is_fixed_width_iso_utc() {
        assert_eq!(iso_utc_millis(0, 5), "1970-01-01T00:00:00.005Z");
        assert_eq!(iso_utc_millis(0, 500), "1970-01-01T00:00:00.500Z");
    }

    #[test]
    fn millisecond_timestamps_order_chronologically_within_a_second() {
        let earlier = iso_utc_millis(1_234_567_890, 42);
        let later = iso_utc_millis(1_234_567_890, 43);
        assert!(later > earlier);
        assert!(iso_utc_millis(1_234_567_891, 0) > later);
    }

    #[test]
    fn stdin_normalization_removes_a_windows_utf8_bom() {
        assert_eq!(normalize_newlines("\u{feff}### Goal\r\n"), "### Goal\n");
    }

    #[test]
    fn file_lock_child_process() {
        let Some(root) = std::env::var_os(FILE_LOCK_HELPER_ROOT).map(PathBuf::from) else {
            return;
        };
        let _owner = FileLock::acquire(&root.join("code-index.lock"), Duration::ZERO).unwrap();
        fs::write(root.join("ready"), b"").unwrap();
        let started = Instant::now();
        while !root.join("release").exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "parent did not release the file-lock helper"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn legacy_file_lock_child_process() {
        let Some(root) = std::env::var_os(LEGACY_LOCK_HELPER_ROOT).map(PathBuf::from) else {
            return;
        };
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("code-index.lock"))
            .unwrap();
        writeln!(marker, "{} legacy", std::process::id()).unwrap();
        marker.sync_all().unwrap();
        fs::write(root.join("ready"), b"").unwrap();
        let started = Instant::now();
        while !root.join("release").exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "parent did not release the legacy file-lock helper"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(marker);
        fs::remove_file(root.join("code-index.lock")).unwrap();
    }

    #[test]
    fn owned_file_lock_child_process() {
        let Some(root) = std::env::var_os(OWNED_LOCK_HELPER_ROOT).map(PathBuf::from) else {
            return;
        };
        let owner = ProcessLockOwner {
            kind: std::env::var(OWNED_LOCK_HELPER_KIND)
                .ok()
                .and_then(|kind| LockOwnerKind::parse(&kind))
                .unwrap_or(LockOwnerKind::HealthWorker),
            project: std::env::var(OWNED_LOCK_HELPER_PROJECT).ok(),
            build: std::env::var(OWNED_LOCK_HELPER_BUILD).unwrap_or_else(|_| "-".to_string()),
        };
        let _owner = FileLock::acquire_with_policy(
            &root.join("code-index.lock"),
            Duration::ZERO,
            Duration::from_millis(20),
            &owner,
            HealthLockRetryPolicy::default(),
        )
        .unwrap();
        fs::write(root.join("ready"), b"").unwrap();
        let started = Instant::now();
        while !root.join("release").exists() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "parent did not release the owned file-lock helper"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn test_lock_owner(kind: LockOwnerKind, project: &str) -> ProcessLockOwner {
        ProcessLockOwner {
            kind,
            project: Some(project.to_string()),
            build: "-".to_string(),
        }
    }

    fn fast_health_retry() -> HealthLockRetryPolicy {
        HealthLockRetryPolicy {
            budget: Duration::from_secs(5),
            max_retries: 500,
            backoff_initial: Duration::from_millis(5),
            backoff_max: Duration::from_millis(20),
        }
    }

    fn spawn_owned_lock_helper(
        root: &Path,
        kind: &str,
        project: &str,
        build: &str,
    ) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("util::tests::owned_file_lock_child_process")
            .arg("--nocapture")
            .env(OWNED_LOCK_HELPER_ROOT, root)
            .env(OWNED_LOCK_HELPER_KIND, kind)
            .env(OWNED_LOCK_HELPER_PROJECT, project)
            .env(OWNED_LOCK_HELPER_BUILD, build)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn await_owned_helper_ready(child: &mut std::process::Child, root: &Path) {
        let started = Instant::now();
        while !root.join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "owned file-lock helper exited before acquiring the lock"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "owned file-lock helper did not acquire the lock"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn lock_timeout_error_names_the_marker_owner() {
        // Feedback item 11: the generic lock timeout names the owner.
        // (A self-owned marker reads as Stale by design, so the live v1
        // owner here is a child process, like the other lock tests.)
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let mut child = spawn_owned_lock_helper(temp.path(), "health-worker", "project-a", "-");
        await_owned_helper_ready(&mut child, temp.path());
        let error = lock_timeout_error(&path);
        assert!(
            error.msg.contains("owner: health-worker project project-a"),
            "v1 owner rides the timeout error: {}",
            error.msg
        );
        assert!(error.msg.contains("marker age"), "age rides: {}", error.msg);
        child.kill().unwrap();
        let _ = child.wait().unwrap();
        fs::remove_file(&path).unwrap();
        // No marker: the plain opaque message survives.
        let error = lock_timeout_error(&path);
        assert_eq!(error.msg, "another climemory writer holds the project lock");
    }

    #[test]
    fn lock_marker_v1_roundtrips_and_stays_legible_to_legacy_readers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let owner = ProcessLockOwner {
            kind: LockOwnerKind::HealthWorker,
            project: Some("project-a".to_string()),
            build: "build-a".to_string(),
        };
        let marker = create_lock_marker_as(&path, &owner).unwrap().unwrap();
        let bytes = fs::read(&path).unwrap();
        // Pre-v1 builds parse the owner pid from the short first line.
        assert_eq!(legacy_marker_owner(&bytes), Some(std::process::id()));
        assert!(bytes.iter().position(|byte| *byte == b'\n').unwrap() < 128);
        assert_eq!(
            marker_owner(&bytes),
            MarkerOwner::V1 {
                kind: LockOwnerKind::HealthWorker,
                project: Some("project-a".to_string()),
                build: "build-a".to_string(),
            }
        );
        // Pre-v1 and malformed extension lines stay legacy owners.
        assert_eq!(marker_owner(b"4242 legacy\n"), MarkerOwner::Legacy);
        assert_eq!(marker_owner(b"1"), MarkerOwner::Legacy);
        assert_eq!(
            marker_owner(b"1 x\ncm-lock/2 health-worker p b\n"),
            MarkerOwner::Legacy
        );
        assert_eq!(
            marker_owner(b"1 x\ncm-lock/1 nobody p b\n"),
            MarkerOwner::Legacy
        );
        drop(marker);
        fs::remove_file(&path).unwrap();

        // A marker without a registered project never matches a contender.
        let owner = ProcessLockOwner {
            kind: LockOwnerKind::HealthWorker,
            project: None,
            build: "build-a".to_string(),
        };
        let marker = create_lock_marker_as(&path, &owner).unwrap().unwrap();
        let bytes = fs::read(&path).unwrap();
        let parsed = marker_owner(&bytes);
        assert_eq!(
            parsed,
            MarkerOwner::V1 {
                kind: LockOwnerKind::HealthWorker,
                project: None,
                build: "build-a".to_string(),
            }
        );
        assert!(!is_retriable_health_lock(
            &parsed,
            &test_lock_owner(LockOwnerKind::Foreground, "project-a")
        ));
        drop(marker);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn file_lock_waits_through_a_short_same_project_health_lock_and_commits_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let mut child = spawn_owned_lock_helper(temp.path(), "health-worker", "project-a", "-");
        await_owned_helper_ready(&mut child, temp.path());

        // Release the health holder mid-wait: the contender must wait through
        // the short hold and then acquire exactly once — the retry lives
        // inside acquisition, before any commit point, so nothing replays.
        let release = temp.path().join("release");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            fs::write(release, b"").unwrap();
        });
        let started = Instant::now();
        let lock = FileLock::acquire_with_policy(
            &path,
            Duration::from_millis(25),
            Duration::from_millis(20),
            &test_lock_owner(LockOwnerKind::Foreground, "project-a"),
            fast_health_retry(),
        )
        .expect("a foreground command waits through a short health-worker lock");
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "the contender acquired before the health worker released: {:?}",
            started.elapsed()
        );
        drop(lock);
        releaser.join().unwrap();
        child.wait().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn file_lock_fails_a_long_health_lock_at_the_bound_with_owner_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let mut child = spawn_owned_lock_helper(temp.path(), "health-worker", "project-a", "-");
        await_owned_helper_ready(&mut child, temp.path());

        let policy = HealthLockRetryPolicy {
            budget: Duration::from_millis(300),
            ..fast_health_retry()
        };
        let started = Instant::now();
        let error = FileLock::acquire_with_policy(
            &path,
            Duration::from_millis(25),
            Duration::from_millis(20),
            &test_lock_owner(LockOwnerKind::Foreground, "project-a"),
            policy,
        )
        .err()
        .expect("a long health-worker lock exhausts the bounded retry");
        assert!(
            error
                .msg
                .contains("cm health worker holds the project lock"),
            "unexpected error: {}",
            error.msg
        );
        assert!(
            error.msg.contains("waited"),
            "missing waited duration: {}",
            error.msg
        );
        assert!(
            error.msg.contains("retries"),
            "missing retry count: {}",
            error.msg
        );
        assert_eq!(
            error.hint.as_deref(),
            Some("retry the same command; the health worker releases its lock shortly")
        );
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the documented bound was not enforced: {:?}",
            started.elapsed()
        );
        child.kill().unwrap();
        let _ = child.wait().unwrap();

        // The crashed holder leaves its marker; stale recovery still works.
        let lock = FileLock::acquire_with_policy(
            &path,
            Duration::ZERO,
            Duration::from_millis(20),
            &test_lock_owner(LockOwnerKind::Foreground, "project-a"),
            policy,
        )
        .unwrap();
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn file_lock_keeps_the_normal_bound_for_unrelated_lock_owners() {
        // A live unrelated foreground process, a health worker from another
        // project, and a health worker from another build all keep the normal
        // bounded wait — none are silently retried under the health policy.
        for (kind, project, build) in [
            ("foreground", "project-a", "-"),
            ("health-worker", "project-b", "-"),
            ("health-worker", "project-a", "other-build"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("code-index.lock");
            let mut child = spawn_owned_lock_helper(temp.path(), kind, project, build);
            await_owned_helper_ready(&mut child, temp.path());

            let started = Instant::now();
            let error = FileLock::acquire_with_policy(
                &path,
                Duration::from_millis(50),
                Duration::from_millis(20),
                &test_lock_owner(LockOwnerKind::Foreground, "project-a"),
                fast_health_retry(),
            )
            .err()
            .unwrap_or_else(|| panic!("a live {kind} owner ({project}/{build}) must block"));
            // Since feedback item 11 the timeout names the live marker owner.
            assert!(
                error
                    .msg
                    .starts_with("another climemory writer holds the project lock (owner: "),
                "owner diagnostics ride the bounded-wait error: {}",
                error.msg
            );
            assert!(
                error.msg.contains(kind),
                "the owner kind {kind} is named: {}",
                error.msg
            );
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "a {kind} owner ({project}/{build}) was retried under the health policy"
            );
            child.kill().unwrap();
            let _ = child.wait().unwrap();
        }
    }

    #[test]
    fn file_lock_health_contender_never_retries_a_health_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let mut child = spawn_owned_lock_helper(temp.path(), "health-worker", "project-a", "-");
        await_owned_helper_ready(&mut child, temp.path());

        let started = Instant::now();
        let error = FileLock::acquire_with_policy(
            &path,
            Duration::from_millis(50),
            Duration::from_millis(20),
            &test_lock_owner(LockOwnerKind::HealthWorker, "project-a"),
            fast_health_retry(),
        )
        .err()
        .expect("a second health worker keeps the bounded wait");
        // Since feedback item 11 the timeout names the live marker owner.
        assert!(
            error.msg.contains("owner: health-worker project project-a"),
            "owner diagnostics ride the bounded-wait error: {}",
            error.msg
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a health-worker contender retried behind a health-worker lock"
        );
        child.kill().unwrap();
        let _ = child.wait().unwrap();
    }

    #[test]
    fn file_lock_queued_foreground_burst_all_acquires_after_a_short_health_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let mut child = spawn_owned_lock_helper(temp.path(), "health-worker", "project-a", "-");
        await_owned_helper_ready(&mut child, temp.path());

        let release = temp.path().join("release");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            fs::write(release, b"").unwrap();
        });
        let mut contenders = Vec::new();
        for _ in 0..3 {
            let path = path.clone();
            contenders.push(std::thread::spawn(move || {
                // After the health holder releases, the burst serializes as
                // ordinary foreground handoff, so the normal wait must cover
                // the siblings' hold times — only the health-worker phase is
                // retried under the owner-aware policy.
                let lock = FileLock::acquire_with_policy(
                    &path,
                    Duration::from_secs(2),
                    Duration::from_millis(20),
                    &test_lock_owner(LockOwnerKind::Foreground, "project-a"),
                    fast_health_retry(),
                )
                .expect("every queued foreground contender acquires the lock");
                drop(lock);
            }));
        }
        for contender in contenders {
            contender.join().unwrap();
        }
        releaser.join().unwrap();
        child.wait().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn marker_publication_preserves_an_owner_created_after_inspection() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        assert_eq!(
            legacy_marker_state(&path).unwrap(),
            LegacyMarkerState::Missing
        );

        let legacy_marker = "4242 legacy\n";
        fs::write(&path, legacy_marker).unwrap();

        assert!(create_lock_marker_as(&path, &process_lock_owner())
            .unwrap()
            .is_none());
        assert_eq!(fs::read_to_string(path).unwrap(), legacy_marker);
    }

    #[test]
    fn malformed_legacy_markers_expire_instead_of_becoming_permanent_owners() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let old = SystemTime::now()
            .checked_sub(Duration::from_secs(5 * 60))
            .unwrap();

        for marker in ["0 legacy\n", "1"] {
            fs::write(&path, marker).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
            assert_eq!(
                legacy_marker_state(&path).unwrap(),
                LegacyMarkerState::Stale
            );
        }

        fs::write(&path, "1").unwrap();
        assert!(matches!(
            legacy_marker_state(&path).unwrap(),
            LegacyMarkerState::Active(MarkerOwner::Legacy)
        ));

        let future = SystemTime::now()
            .checked_add(Duration::from_secs(5 * 60))
            .unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(future))
            .unwrap();
        assert_eq!(
            legacy_marker_state(&path).unwrap(),
            LegacyMarkerState::Stale
        );
    }

    #[test]
    fn file_lock_survives_an_old_marker_and_recovers_after_owner_crash() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("util::tests::file_lock_child_process")
            .arg("--nocapture")
            .env(FILE_LOCK_HELPER_ROOT, temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !temp.path().join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "file-lock helper exited before acquiring the lock"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "file-lock helper did not acquire the lock"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let old = SystemTime::now()
            .checked_sub(Duration::from_secs(5 * 60))
            .unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        let contender = FileLock::acquire(&path, Duration::from_millis(25));
        child.kill().unwrap();
        let _ = child.wait().unwrap();

        let error = contender
            .err()
            .expect("a live OS lock must outlive marker freshness");
        // The live owner pid makes the marker Active despite its age, so the
        // feedback item 11 owner diagnostics ride the error.
        assert!(
            error
                .msg
                .starts_with("another climemory writer holds the project lock (owner: "),
            "owner diagnostics ride: {}",
            error.msg
        );
        assert_eq!(
            error.hint.as_deref(),
            Some("retry the same command after the other cm process exits")
        );
        assert!(
            path.exists(),
            "a crashed owner leaves its diagnostic marker"
        );
        let next = FileLock::acquire(&path, Duration::ZERO).unwrap();
        assert!(path.exists());
        drop(next);
        assert!(!path.exists());
        assert!(temp.path().join("code-index.lock.guard").exists());
    }

    #[test]
    fn file_lock_honors_a_live_legacy_owner_and_recovers_after_its_crash() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("util::tests::legacy_file_lock_child_process")
            .arg("--nocapture")
            .env(LEGACY_LOCK_HELPER_ROOT, temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !temp.path().join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "legacy file-lock helper exited before publishing its marker"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "legacy file-lock helper did not publish its marker"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let error = FileLock::acquire(&path, Duration::from_millis(25))
            .err()
            .expect("a live legacy owner must block the advisory protocol");
        assert!(
            error
                .msg
                .contains(&format!("owner: legacy marker pid {}", child.id())),
            "the legacy owner pid rides: {}",
            error.msg
        );
        child.kill().unwrap();
        let _ = child.wait().unwrap();
        assert!(path.exists());

        let lock = FileLock::acquire(&path, Duration::ZERO).unwrap();
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn file_lock_heartbeat_keeps_its_marker_fresh_for_legacy_waiters() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let lock =
            FileLock::acquire_with_heartbeat(&path, Duration::ZERO, Duration::from_millis(20))
                .unwrap();
        let old = SystemTime::now()
            .checked_sub(Duration::from_secs(5 * 60))
            .unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        let started = Instant::now();
        loop {
            let fresh = fs::metadata(&path)
                .unwrap()
                .modified()
                .unwrap()
                .elapsed()
                .is_ok_and(|age| age < Duration::from_secs(1));
            if fresh {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "lock heartbeat did not refresh the marker"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn file_lock_drop_preserves_a_replacement_marker() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("code-index.lock");
        let lock = FileLock::acquire(&path, Duration::ZERO).unwrap();

        fs::remove_file(&path).unwrap();
        fs::write(&path, "replacement owner\n").unwrap();
        drop(lock);

        assert_eq!(fs::read_to_string(path).unwrap(), "replacement owner\n");
    }

    fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "file symlinks are unsupported",
            ))
        }
    }

    fn symlink_or_skip(target: &Path, link: &Path) -> bool {
        match create_file_symlink(target, link) {
            Ok(()) => true,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ) || error.raw_os_error() == Some(1314) =>
            {
                false
            }
            Err(error) => panic!("could not create test symlink: {error}"),
        }
    }

    #[test]
    fn lock_marker_open_never_follows_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("sentinel.txt");
        let marker = temp.path().join("code-index.lock");
        fs::write(&target, "1 sentinel\n").unwrap();
        if !symlink_or_skip(&target, &marker) {
            return;
        }

        assert!(open_lock_marker(&marker).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "1 sentinel\n");
    }

    #[test]
    fn file_lock_replaces_a_marker_symlink_without_touching_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("sentinel.txt");
        let marker = temp.path().join("code-index.lock");
        fs::write(&target, "keep me").unwrap();
        if !symlink_or_skip(&target, &marker) {
            return;
        }

        let lock = FileLock::acquire(&marker, Duration::ZERO).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
        assert!(fs::symlink_metadata(&marker).unwrap().is_file());
        drop(lock);
        assert!(!marker.exists());
    }

    #[test]
    fn file_lock_rejects_a_guard_symlink_without_touching_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("sentinel.txt");
        let marker = temp.path().join("code-index.lock");
        fs::write(&target, "keep me").unwrap();
        if !symlink_or_skip(&target, &lock_guard_path(&marker)) {
            return;
        }

        let error = FileLock::acquire(&marker, Duration::ZERO)
            .err()
            .expect("a guard symlink must be rejected");
        assert!(error.msg.contains("lock guard"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep me");
        assert!(!marker.exists());
    }
}
