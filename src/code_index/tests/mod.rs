use super::*;
use crate::config::Config;
use tempfile::TempDir;

/// Positive hook waits are deadlock guards, not race assertions: the hooked
/// event must eventually arrive, so the bound only catches a real hang.
/// Keep it generous — under parallel `cargo test` load a 1-2s wall-clock
/// bound false-reds the routine gate (known-issues item 205).
const HOOK_WAIT: Duration = Duration::from_secs(30);

fn project(temp: &TempDir) -> Project {
    let data = temp.path().join("memory");
    fs::create_dir_all(data.join("threads")).unwrap();
    Config::default().save(&data.join("config.json")).unwrap();
    Project::open(temp.path()).unwrap()
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .expect("git must be available for repository discovery tests");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
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

#[cfg(feature = "code-index")]
mod concurrency;
#[cfg(feature = "code-index")]
mod context;
mod discovery;
#[cfg(feature = "code-index")]
mod edges;
#[cfg(feature = "code-index")]
mod freshness;
#[cfg(feature = "code-index")]
mod storage;
