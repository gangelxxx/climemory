//! Per-session accounting; request replacement makes checkpoints idempotent.
use super::*;
use std::{path::Path, time::Duration};

fn add(acc: &mut Value, value: &Value) -> Result<()> {
    match value {
        Value::Object(values) => {
            if !acc.is_object() {
                *acc = json!({});
            }
            for (key, v) in values {
                add(&mut acc[key], v)?;
            }
        }
        Value::Number(_) => {
            *acc = if let Some(n) = value.as_u64().filter(|_| !acc.is_f64()) {
                acc.as_u64()
                    .unwrap_or(0)
                    .checked_add(n)
                    .map(|v| json!(v))
                    .ok_or_else(|| AppError::new("session statistics counter overflow"))?
            } else {
                let total = acc.as_f64().unwrap_or(0.0) + value.as_f64().unwrap();
                if !total.is_finite() {
                    return Err(AppError::new("session statistics counter overflow"));
                }
                json!(total)
            };
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn persist(data_root: &Path, run_path: &Path, run: &Value) -> Result<()> {
    let dir = run_path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("session-statistics");
    Project::checked_path(data_root, &dir)?;
    fs::create_dir_all(&dir)?;
    for request in run["requests"].as_array().unwrap() {
        let mut sessions = Vec::new();
        if let Some(id) = run["session_id"].as_str().filter(|s| !s.is_empty()) {
            sessions.push(("external", id));
        }
        if let Some(id) = request["context_session"].as_str() {
            sessions.push(("context", id));
        }
        let calls: Vec<_> = run["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["correlation"]["request_id"] == request["request_id"])
            .cloned()
            .collect();
        let mut phases = BTreeMap::<String, Vec<Value>>::new();
        for c in &calls {
            phases
                .entry(c["phase"].as_str().unwrap_or("unknown").into())
                .or_default()
                .push(c.clone());
        }
        let phases: BTreeMap<_, _> = phases.into_iter().map(|(k, v)| (k, totals(&v))).collect();
        let mut agents = BTreeMap::<String, Vec<Value>>::new();
        let mut threads = BTreeMap::<String, Vec<Value>>::new();
        for c in &calls {
            agents
                .entry(json!([c["agent"], c["provider"], c["model"]]).to_string())
                .or_default()
                .push(c.clone());
            threads
                .entry(c["thread"].as_str().unwrap_or("unscoped").into())
                .or_default()
                .push(c.clone());
        }
        let agents: BTreeMap<_, _> = agents.into_iter().map(|(k, v)| (k, totals(&v))).collect();
        let threads: BTreeMap<_, _> = threads.into_iter().map(|(k, v)| (k, totals(&v))).collect();
        let mut breakdown = json!({});
        for name in ["operation", "retries", "analysis", "analysis_retries"] {
            breakdown[name] = totals(
                &calls
                    .iter()
                    .filter(|c| call_group(c) == name)
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
        for (kind, id) in sessions {
            let path = dir.join(format!("{kind}-{}.json", digest(id)));
            let lock = path.with_extension("lock");
            Project::checked_path(data_root, &path)?;
            Project::checked_path(data_root, &lock)?;
            let _lock = FileLock::acquire(&lock, Duration::from_secs(2))?;
            let mut state: Value = if path.exists() {
                serde_json::from_slice(&fs::read(&path)?)?
            } else {
                json!({"format":"climemory/session-statistics-1","session_kind":kind,"session_id":id,"requests":{},"primary_session_usage":null})
            };
            if !state["requests"].is_object()
                || (!state["primary_sessions"].is_null() && !state["primary_sessions"].is_object())
            {
                return Err(AppError::new("invalid session statistics"));
            }
            if state["primary_sessions"].is_null() {
                state["primary_sessions"] = json!({});
                // Legacy external summaries have an unambiguous snapshot owner.
                if kind == "external" && !state["primary_session_usage"].is_null() {
                    state["primary_sessions"][digest(id)] =
                        json!({"session_id":id,"snapshot":state["primary_session_usage"]});
                }
            }
            state["requests"][request["request_id"].as_str().unwrap()] = json!({
                "run_id":run["run_id"],"request":request,"statistics_file":run_path,"statistics_incomplete":run["statistics_incomplete"].as_bool().unwrap_or(false),
                "totals":totals(&calls),"phases":phases,"agents":agents,"threads":threads,"call_breakdown":breakdown
            });
            let mut combined = json!({});
            for entry in state["requests"].as_object().unwrap().values() {
                add(&mut combined, &entry["totals"])?;
            }
            state["totals"] = combined;
            for key in ["phases", "agents", "threads", "call_breakdown"] {
                let mut grouped = json!({});
                for entry in state["requests"].as_object().unwrap().values() {
                    add(&mut grouped, &entry[key])?;
                }
                state[key] = grouped;
            }
            state["request_count"] = json!(state["requests"].as_object().unwrap().len());
            state["updated_at"] = json!(iso_now());
            // A context topic may be resumed by unrelated external sessions.
            // Their cumulative primary counters must never replace one another.
            if let Some(external) = run["session_id"].as_str().filter(|s| !s.is_empty()) {
                let key = digest(external);
                if !run["primary_session_usage"].is_null()
                    && run["primary_session_usage"]["timestamp"].as_str()
                        >= state["primary_sessions"][&key]["snapshot"]["timestamp"].as_str()
                {
                    state["primary_sessions"][&key] =
                        json!({"session_id":external,"snapshot":run["primary_session_usage"]});
                }
                if kind == "external" {
                    state["primary_session_usage"] =
                        state["primary_sessions"][&key]["snapshot"].clone();
                }
            }
            if kind == "context" {
                state["primary_session_usage"] = Value::Null;
            }
            state["measurement"] = json!("Provider-reported agent tokens only; missing usage is not zero. Cached/reasoning tokens are subsets. External and context summaries overlap and must not be added together. Primary usage is a cumulative snapshot, never summed. Running means last observed; a killed process may remain running.");
            atomic_write(&path, &serde_json::to_vec_pretty(&state)?)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(id: &str, status: &str, usage: Value) -> Value {
        json!({"run_id":id,"session_id":"external-test","requests":[{"request_id":id,"context_session":"topic-test","status":status}],"calls":[{"correlation":{"request_id":id},"status":status,"usage":usage,"phase":"read","agent":"cheap","provider":"test","model":"test","thread":"t"}],"primary_session_usage":{"timestamp":"2026-09-25T00:00:00Z","cumulative":{"input_tokens":1000}}})
    }
    #[test]
    fn checkpoints_replace_unknown_usage_and_concurrent_requests_are_not_lost() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("runtime/statistics");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.json");
        let summary = d.path().join(format!(
            "runtime/session-statistics/external-{}.json",
            digest("external-test")
        ));
        persist(d.path(), &path, &run("a", "running", Value::Null)).unwrap();
        let state: Value = serde_json::from_slice(&fs::read(&summary).unwrap()).unwrap();
        assert_eq!(state["totals"]["running_calls"], 1);
        assert_eq!(
            state["totals"]["tokens"]["total_tokens"]["missing_calls"],
            1
        );
        assert!(state["totals"]["tokens"]["total_tokens"]["reported"].is_null());
        let done = run(
            "a",
            "completed",
            json!({"input_tokens":100,"cached_input_tokens":20,"output_tokens":10}),
        );
        persist(d.path(), &path, &done).unwrap();
        persist(d.path(), &path, &done).unwrap();
        std::thread::scope(|scope| {
            for n in 0..4 {
                let dir = &dir;
                let d = &d;
                scope.spawn(move || {
                    persist(
                        d.path(),
                        &dir.join(format!("{n}.json")),
                        &run(&format!("b{n}"), "error", Value::Null),
                    )
                    .unwrap()
                });
            }
        });
        let state: Value = serde_json::from_slice(&fs::read(summary).unwrap()).unwrap();
        assert_eq!(state["request_count"], 5);
        assert_eq!(state["totals"]["calls"], 5);
        assert_eq!(state["totals"]["running_calls"], 0);
        assert_eq!(state["totals"]["tokens"]["total_tokens"]["reported"], 110);
        assert_eq!(
            state["totals"]["tokens"]["total_tokens"]["missing_calls"],
            4
        );
        assert_eq!(
            state["primary_session_usage"]["cumulative"]["input_tokens"],
            1000
        );
    }
    #[test]
    fn context_keeps_primary_counters_separate_for_different_external_sessions() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("runtime/statistics");
        fs::create_dir_all(&dir).unwrap();
        let a = run("a", "completed", Value::Null);
        persist(d.path(), &dir.join("a.json"), &a).unwrap();
        let mut b = run("b", "completed", Value::Null);
        b["session_id"] = json!("another-external");
        b["primary_session_usage"]["cumulative"]["input_tokens"] = json!(25);
        persist(d.path(), &dir.join("b.json"), &b).unwrap();
        let path = d.path().join(format!(
            "runtime/session-statistics/context-{}.json",
            digest("topic-test")
        ));
        let state: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(state["primary_session_usage"].is_null());
        let sessions = state["primary_sessions"].as_object().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions[&digest("external-test")]["snapshot"]["cumulative"]["input_tokens"],
            1000
        );
        assert_eq!(
            sessions[&digest("another-external")]["snapshot"]["cumulative"]["input_tokens"],
            25
        );
    }

    #[test]
    fn overflow_is_reported_instead_of_restarting_the_sum_from_zero() {
        let mut value = json!(u64::MAX);
        assert!(add(&mut value, &json!(1)).is_err());
        assert_eq!(value, json!(u64::MAX));
        let mut cost = json!(0.5);
        add(&mut cost, &json!(0)).unwrap();
        assert_eq!(cost, json!(0.5));
        add(&mut cost, &json!(0.25)).unwrap();
        assert_eq!(cost, json!(0.75));
    }
    #[test]
    fn legacy_snapshot_survives_missing_usage_and_corrupt_maps_return_error() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("runtime/statistics");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.json");
        persist(d.path(), &path, &run("a", "completed", Value::Null)).unwrap();
        let summary = d.path().join(format!(
            "runtime/session-statistics/external-{}.json",
            digest("external-test")
        ));
        let mut legacy: Value = serde_json::from_slice(&fs::read(&summary).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("primary_sessions");
        fs::write(&summary, legacy.to_string()).unwrap();
        let mut missing = run("b", "completed", Value::Null);
        missing["primary_session_usage"] = Value::Null;
        persist(d.path(), &path, &missing).unwrap();
        let mut state: Value = serde_json::from_slice(&fs::read(&summary).unwrap()).unwrap();
        assert_eq!(
            state["primary_session_usage"]["cumulative"]["input_tokens"],
            1000
        );
        state["primary_sessions"] = json!("corrupt");
        fs::write(&summary, state.to_string()).unwrap();
        assert!(persist(d.path(), &path, &run("c", "completed", Value::Null)).is_err());
    }
}
