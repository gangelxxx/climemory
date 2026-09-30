//! Recovery uses an OS-held lease, not PID liveness (PIDs can be reused).
use crate::{project::Project, util::*};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub(super) struct Lease {
    marker: PathBuf,
    _lock: FileLock,
}
impl Lease {
    pub(super) fn finish(self) {
        let _ = fs::remove_file(&self.marker);
    }
}
pub(super) fn start(root: &Path, path: &Path) -> Result<Lease> {
    let active = path.parent().unwrap().with_file_name("statistics-active");
    Project::checked_path(root, &active)?;
    fs::create_dir_all(&active)?;
    let marker = active.join(path.file_name().unwrap());
    let lock = FileLock::acquire(&marker.with_extension("lock"), Duration::ZERO)?;
    atomic_write(&marker, b"{}")?;
    Ok(Lease {
        marker,
        _lock: lock,
    })
}
pub(super) fn recover(root: &Path, dir: &Path) -> Result<()> {
    let active = dir.with_file_name("statistics-active");
    Project::checked_path(root, &active)?;
    if !active.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(active)? {
        let marker = entry?.path();
        if marker.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        Project::checked_path(root, &marker)?;
        let lock_path = marker.with_extension("lock");
        Project::checked_path(root, &lock_path)?;
        let Ok(_lock) = FileLock::acquire(&lock_path, Duration::ZERO) else {
            continue;
        };
        let path = dir.join(marker.file_name().unwrap());
        Project::checked_path(root, &path)?;
        let completion = dir
            .with_file_name("statistics-completions")
            .join(marker.file_name().unwrap());
        Project::checked_path(root, &completion)?;
        if path.exists() {
            let mut data: Value = serde_json::from_slice(&fs::read(&path)?)?;
            if completion.exists() {
                let receipt: Value = serde_json::from_slice(&fs::read(&completion)?)?;
                for field in ["status", "outcome", "totals"] {
                    data[field] = receipt[field].clone();
                }
                data["statistics_incomplete"] = json!(true);
            } else if data["status"] == "running" {
                data["status"] = json!("interrupted");
                data["interruption"] = json!("Process lease released without a completion receipt; final usage may be missing.");
            }
            let status = data["status"].clone();
            if let Some(requests) = data["requests"].as_array_mut() {
                for request in requests {
                    if request["status"] == "running" {
                        request["status"] = status.clone();
                    }
                }
            }
            if let Some(calls) = data["calls"].as_array_mut() {
                for call in calls {
                    if call["status"] == "running" {
                        call["status"] = json!("interrupted");
                    }
                }
            }
            data["statistics_incomplete"] = json!(true);
            atomic_write(&path, &serde_json::to_vec_pretty(&data)?)?;
            if data["requests"].is_array() && data["calls"].is_array() {
                super::sessions::persist(root, &path, &data)?;
            }
        }
        fs::remove_file(marker)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_recovers_completed_run_with_incomplete_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let stats = dir.path().join("statistics");
        let path = stats.join("run.json");
        let lease = start(dir.path(), &path).unwrap();
        atomic_write(&path, br#"{"status":"running"}"#).unwrap();
        atomic_write(
            &dir.path().join("statistics-completions/run.json"),
            br#"{"status":"complete","totals":{"calls":1},"outcome":{}}"#,
        )
        .unwrap();
        drop(lease);
        recover(dir.path(), &stats).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value["status"], "complete");
        assert_eq!(value["statistics_incomplete"], true);
        assert_eq!(value["totals"]["calls"], 1);
    }
    #[test]
    fn active_lease_is_not_interrupted_and_abandoned_lease_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let stats = dir.path().join("statistics");
        let path = stats.join("run.json");
        let lease = start(dir.path(), &path).unwrap();
        atomic_write(&path, br#"{"status":"running"}"#).unwrap();
        recover(dir.path(), &stats).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("running"));
        drop(lease);
        recover(dir.path(), &stats).unwrap();
        assert!(fs::read_to_string(path).unwrap().contains("interrupted"));
    }
}
