//! Opt-in process-local accounting. No prompts, response text or credentials stored.
use crate::{agent_provider::*, project::Project, util::*};
use serde_json::{json, Value};
use std::{cell::RefCell, collections::BTreeMap, fs, path::PathBuf, sync::Mutex, time::Instant};

mod lifecycle;
mod sessions;

static RUN: Mutex<Option<Run>> = Mutex::new(None);
thread_local! { static CONTEXT: RefCell<Value> = const { RefCell::new(Value::Null) }; }
struct Run {
    path: PathBuf,
    lease: Option<lifecycle::Lease>,
    data_root: PathBuf,
    secrets: Vec<String>,
    content_logs: bool,
    started: Instant,
    data: Value,
}
static COUNTS: Mutex<[u64; 4]> = Mutex::new([0; 4]);
pub(crate) fn track_attempt(attempt: u32) {
    let analysis = context()["phase"] == "error_analysis";
    let index = usize::from(analysis) * 2 + usize::from(attempt > 1);
    COUNTS.lock().unwrap()[index] += 1;
}
pub(crate) fn call_counts() -> Value {
    {
        let c = COUNTS.lock().unwrap();
        json!({"operation":c[0],"retries":c[1],"analysis":c[2],"analysis_retries":c[3],"total":c.iter().sum::<u64>()})
    }
}
fn call_group(row: &Value) -> &'static str {
    match (
        row["phase"] == "error_analysis",
        row["attempt"].as_u64().unwrap_or(1) > 1,
    ) {
        (false, false) => "operation",
        (false, true) => "retries",
        (true, false) => "analysis",
        (true, true) => "analysis_retries",
    }
}
pub(crate) struct Guard;
pub(crate) struct Scope(Value);
pub(crate) fn context() -> Value {
    CONTEXT.with(|c| c.borrow().clone())
}
impl Scope {
    pub fn new(agent: &str, phase: &str, thread: Option<&str>) -> Self {
        Self(CONTEXT.with(|c| c.replace(json!({"agent":agent,"phase":phase,"thread":thread}))))
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CONTEXT.with(|c| {
            c.replace(self.0.take());
        });
    }
}

pub(crate) fn correlation() -> Value {
    RUN.lock().unwrap().as_ref().map(|r| {
        let request = r.data["requests"].as_array().unwrap().last().unwrap();
        json!({"run_id":r.data["run_id"],"request_id":request["request_id"],"external_session_id":r.data["session_id"],"context_session":request["context_session"]})
    }).unwrap_or(Value::Null)
}
pub(crate) fn bind_context(id: &str) {
    update(|d| {
        d["requests"].as_array_mut().unwrap().last_mut().unwrap()["context_session"] = json!(id)
    });
    checkpoint();
}
pub(crate) fn event(kind: &str, data: Value) {
    update(|d| {
        let request = d["requests"].as_array().unwrap().last().unwrap()["request_id"].clone();
        d["events"]
            .as_array_mut()
            .unwrap()
            .push(json!({"at":iso_now(),"request_id":request,"event":kind,"data":data}));
    });
    checkpoint();
}
fn checkpoint() {
    if let Some(run) = RUN.lock().unwrap().as_mut() {
        persist(run);
    }
}
fn persist(run: &mut Run) -> bool {
    run.data["totals"] = totals(run.data["calls"].as_array().unwrap());
    run.data["call_counts"] = call_counts();
    let result = (|| -> Result<()> {
        atomic_write(&run.path, &serde_json::to_vec_pretty(&run.data)?)?;
        sessions::persist(&run.data_root, &run.path, &run.data)
    })();
    let success = result.is_ok();
    if let Err(e) = result {
        eprintln!(
            "{}: {}",
            crate::ui::tr(
                "CM statistics write failed",
                "CM ошибка записи статистики",
                "CM 统计写入失败"
            ),
            e.msg
        );
    }
    success
}
fn content_event(kind: &str, text: &str) {
    use std::io::Write;
    let active = RUN.lock().unwrap();
    let Some(run) = active.as_ref().filter(|r| r.content_logs) else {
        return;
    };
    let request = run.data["requests"].as_array().unwrap().last().unwrap();
    let mut row = json!({"at":iso_now(),"event":kind,"run_id":run.data["run_id"],"request_id":request["request_id"],"external_session_id":run.data["session_id"],"context_session":request["context_session"],"text":text});
    crate::feedback::clean(&mut row, &run.secrets);
    let result = (|| -> Result<()> {
        let dir = run
            .path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("request-logs");
        Project::checked_path(&run.data_root, &dir)?;
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.jsonl", run.data["run_id"].as_str().unwrap()));
        Project::checked_path(&run.data_root, &path)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        serde_json::to_writer(&mut file, &row)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    })();
    if result.is_err() {
        eprintln!(
            "{}",
            crate::ui::tr(
                "CM request log write failed",
                "CM ошибка записи журнала запросов",
                "CM 请求日志写入失败"
            )
        );
    }
}
fn request() -> Value {
    json!({"request_id":fresh_id(),"started_at":iso_now(),"status":"running","context_session":null,"input_chars":0,"input_bytes":0,"output_chars":0,"output_bytes":0})
}
fn update(f: impl FnOnce(&mut Value)) {
    if let Some(run) = RUN.lock().unwrap().as_mut() {
        f(&mut run.data);
    }
}
pub(crate) fn enabled() -> bool {
    RUN.lock().unwrap().is_some()
}
pub(crate) fn begin(args: &[String]) -> Option<Guard> {
    let project = Project::open(&crate::chat::project_root().ok()?).ok()?;
    begin_project(args, &project, None)
}

pub(crate) fn begin_project(
    args: &[String],
    project: &Project,
    session: Option<&str>,
) -> Option<Guard> {
    *COUNTS.lock().unwrap() = [0; 4];
    if args
        .first()
        .is_some_and(|s| matches!(s.as_str(), "help" | "init" | "--help" | "-h"))
    {
        return None;
    }
    if !project.config.memory.statistics.enabled {
        return None;
    }
    let setup = (|| -> Result<PathBuf> {
        let dir = project.health.join("statistics");
        Project::checked_path(&project.data, &dir)?;
        fs::create_dir_all(&dir)?;
        if let Err(error) = lifecycle::recover(&project.data, &dir) {
            eprintln!(
                "{}: {}",
                crate::ui::tr(
                    "CM statistics recovery failed",
                    "CM ошибка восстановления статистики",
                    "CM 统计恢复失败"
                ),
                error.msg
            );
        }
        Ok(dir.join(format!("{}.json", fresh_id())))
    })();
    let path = match setup {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{}: {}",
                crate::ui::tr(
                    "CM statistics unavailable",
                    "CM статистика недоступна",
                    "CM 统计不可用"
                ),
                e.msg
            );
            return None;
        }
    };
    let lease = lifecycle::start(&project.data, &path).ok();
    let data = json!({"run_id":path.file_stem().unwrap().to_string_lossy(),"requests":[request()],"events":[],"format":"climemory/statistics-1","version":crate::build_info::BINARY_VERSION,"started_at":iso_now(),"status":"running","lifecycle_tracked":lease.is_some(),
        "command":match args.first().map(String::as_str) { Some("--feedback-worker") => "diagnostic_analysis", Some("hooks") => "hooks", Some("ingest-session") => "ingest-session", Some("-test_providers") => "test_providers", Some("feedback") => "feedback", _ => "chat" },
        "session_id":session.map(str::to_owned).or_else(||std::env::var("CODEX_THREAD_ID").or_else(|_|std::env::var("CODEX_SESSION_ID")).ok()),
        "exchange":{"requests":0,"input_chars":0,"input_bytes":0,"responses":0,"output_chars":0,"output_bytes":0,"errors":0,"input_tokens":null,"output_tokens":null},
        "calls":[],"cache":{},"source_events":0,"source_chars":0,"source_by_kind":{},"primary_session_usage":null,
        "measurement":"Characters are Unicode scalar counts. Exchange tokens are unavailable without the primary tokenizer. Prompt/schema sizes exclude provider-injected instructions. Provider token counts may include that overhead; cached/reasoning tokens are subsets, not additional totals. Primary session usage is a cumulative snapshot from newly consumed Codex records, not CM call usage; do not sum snapshots."});
    if let Err(e) = atomic_write(&path, &serde_json::to_vec_pretty(&data).ok()?) {
        eprintln!(
            "{}: {}",
            crate::ui::tr(
                "CM statistics unavailable",
                "CM статистика недоступна",
                "CM 统计不可用"
            ),
            e.msg
        );
        return None;
    }
    *RUN.lock().unwrap() = Some(Run {
        path,
        lease,
        data_root: project.data.clone(),
        secrets: crate::feedback::secrets(project),
        content_logs: project.config.memory.agent_logs.enabled,
        started: Instant::now(),
        data,
    });
    content_event("cli_request", &json!(args).to_string());
    if let Some(v) = crate::session_ingest::current_usage_snapshot() {
        primary(&v);
    }
    Some(Guard)
}
impl Guard {
    pub fn finish(self, success: bool) {
        if let Some(v) = crate::session_ingest::current_usage_snapshot() {
            primary(&v);
        }
        if let Some(mut run) = RUN.lock().unwrap().take() {
            run.data["status"] = json!(if success { "complete" } else { "error" });
            for r in run.data["requests"].as_array_mut().unwrap() {
                if r["status"] == "running" {
                    r["status"] = json!(if success { "complete" } else { "error" });
                }
            }
            run.data["elapsed_ms"] = json!(run.started.elapsed().as_millis());
            let calls = run.data["calls"].as_array().unwrap();
            run.data["totals"] = totals(calls);
            run.data["call_counts"] = call_counts();
            let calls = run.data["calls"].as_array().unwrap();
            let mut breakdown = json!({});
            for name in ["operation", "retries", "analysis", "analysis_retries"] {
                let selected: Vec<_> = calls
                    .iter()
                    .filter(|c| call_group(c) == name)
                    .cloned()
                    .collect();
                breakdown[name] = totals(&selected);
            }
            run.data["call_breakdown"] = breakdown;
            run.data["outcome"] = outcome(&run.data);
            let mut groups = BTreeMap::<(String, String, String, String), Vec<Value>>::new();
            for c in run.data["calls"].as_array().unwrap() {
                groups
                    .entry((
                        c["agent"].as_str().unwrap_or("unknown").into(),
                        c["phase"].as_str().unwrap_or("unknown").into(),
                        c["provider"].as_str().unwrap_or("unknown").into(),
                        c["model"].as_str().unwrap_or("unknown").into(),
                    ))
                    .or_default()
                    .push(c.clone());
            }
            run.data["agents"]=json!(groups.into_iter().map(|((agent,phase,provider,model),calls)|json!({"agent":agent,"phase":phase,"provider":provider,"model":model,"totals":totals(&calls)})).collect::<Vec<_>>());
            let persisted = persist(&mut run);
            // Independent new file: a locked checkpoint cannot hide completion.
            let receipt = json!({"format":"climemory/statistics-completion-1",
                "run_id":run.data["run_id"],"status":run.data["status"],
                "finished_at":iso_now(),"statistics_persisted":persisted,
                "outcome":run.data["outcome"],"totals":run.data["totals"]});
            let receipt_path = run
                .path
                .parent()
                .unwrap()
                .with_file_name("statistics-completions")
                .join(run.path.file_name().unwrap());
            if let Err(error) =
                Project::checked_path(&run.data_root, &receipt_path).and_then(|_| {
                    atomic_write(&receipt_path, &serde_json::to_vec_pretty(&receipt).unwrap())
                })
            {
                eprintln!(
                    "{}: {}",
                    crate::ui::tr(
                        "CM statistics completion write failed",
                        "CM ошибка записи завершения статистики",
                        "CM 统计完成记录写入失败"
                    ),
                    error.msg
                );
            }
            if persisted && receipt_path.exists() {
                if let Some(lease) = run.lease.take() {
                    lease.finish();
                }
            }
            if crate::ui::pretty() {
                eprintln!(
                    "{}: {}",
                    crate::ui::tr("CM statistics", "CM статистика", "CM 统计"),
                    run.path.display()
                );
            }
        }
    }
}
fn totals(calls: &[Value]) -> Value {
    let counters = crate::usage::summarize(
        &calls
            .iter()
            .map(|c| {
                let mut c = c.clone();
                c["event"] = json!("model_call");
                c["provider"] = json!("all");
                c["model"] = json!("all");
                c
            })
            .collect::<Vec<_>>(),
    );
    let mut v = json!({"calls":calls.len(),"running_calls":calls.iter().filter(|c|c["status"]=="running").count(),"failed_calls":calls.iter().filter(|c|c["status"]=="error").count(),"tokens":counters.get(0).map(|v|v["counters"].clone()).unwrap_or_else(||json!({}))});
    if calls.is_empty() {
        for key in [
            "input_tokens",
            "cached_input_tokens",
            "cache_write_input_tokens",
            "output_tokens",
            "reasoning_output_tokens",
            "cost_usd",
        ] {
            v["tokens"][key] = json!({"reported":0,"measured_calls":0,"missing_calls":0});
        }
    }
    for key in ["total_tokens", "uncached_input_tokens"] {
        let values: Vec<_> = calls
            .iter()
            .filter_map(|c| {
                let input = c["usage"]["input_tokens"].as_u64()?;
                if key == "total_tokens" {
                    input.checked_add(c["usage"]["output_tokens"].as_u64()?)
                } else {
                    input.checked_sub(c["usage"]["cached_input_tokens"].as_u64()?)
                }
            })
            .collect();
        v["tokens"][key] = json!({"reported":if calls.is_empty(){Some(0)} else if values.is_empty(){None} else {values.iter().try_fold(0u64,|a,b|a.checked_add(*b))},"measured_calls":values.len(),"missing_calls":calls.len()-values.len()});
    }
    for key in [
        "input_chars",
        "input_bytes",
        "schema_chars",
        "schema_bytes",
        "output_chars",
        "output_bytes",
        "event_text_chars",
        "elapsed_ms",
    ] {
        let values = calls
            .iter()
            .filter_map(|c| c[key].as_u64())
            .collect::<Vec<_>>();
        v[key] = json!({"measured":if calls.is_empty(){Some(0)}else if values.is_empty(){None}else{values.iter().try_fold(0u64,|a,b|a.checked_add(*b))},"missing_calls":calls.len()-values.len()});
    }
    v
}
pub(crate) fn input(text: &str) {
    update(|d| {
        let requests = d["requests"].as_array_mut().unwrap();
        if requests.last().unwrap()["status"] != "running" {
            requests.push(request());
        }
        let r = requests.last_mut().unwrap();
        r["input_chars"] = json!(text.chars().count());
        r["input_bytes"] = json!(text.len());
    });
    content_event("external_input", text);
    crate::feedback::event("external_input", json!({"text":text}));
    update(|d| {
        let e = &mut d["exchange"];
        for (k, n) in [
            ("requests", 1),
            ("input_chars", text.chars().count()),
            ("input_bytes", text.len()),
        ] {
            e[k] = json!(e[k].as_u64().unwrap() + n as u64);
        }
    });
}
pub(crate) fn output(text: &str) {
    update(|d| {
        let r = d["requests"].as_array_mut().unwrap().last_mut().unwrap();
        r["status"] = json!("complete");
        r["finished_at"] = json!(iso_now());
        if let Ok(v) = serde_json::from_str::<Value>(text) {
            r["result_status"] = v["status"].clone();
        }
        r["output_chars"] = json!(text.chars().count());
        r["output_bytes"] = json!(text.len());
    });
    content_event("external_output", text);
    crate::feedback::event("external_output", json!({"text":text}));
    update(|d| {
        let e = &mut d["exchange"];
        for (k, n) in [
            ("responses", 1),
            ("output_chars", text.chars().count()),
            ("output_bytes", text.len()),
        ] {
            e[k] = json!(e[k].as_u64().unwrap() + n as u64);
        }
    });
}
pub(crate) fn log_error(message: &str) {
    content_event("command_error", message);
    checkpoint();
}
pub(crate) fn error() {
    update(|d| {
        d["requests"].as_array_mut().unwrap().last_mut().unwrap()["status"] = json!("error")
    });
    update(|d| d["exchange"]["errors"] = json!(d["exchange"]["errors"].as_u64().unwrap() + 1));
}
pub(crate) fn cache(layer: &str, hit: bool) {
    cache_status(layer, if hit { "hits" } else { "misses" });
}
pub(crate) fn cache_status(layer: &str, status: &str) {
    update(|d| {
        let key = format!("{layer}_{status}");
        d["cache"][&key] = json!(d["cache"][&key].as_u64().unwrap_or(0) + 1);
        let r = d["requests"].as_array_mut().unwrap().last_mut().unwrap();
        if !r["cache"].is_object() {
            r["cache"] = json!({});
        }
        r["cache"][&key] = json!(r["cache"][&key].as_u64().unwrap_or(0) + 1);
    });
}
pub(crate) fn source(events: usize, chars: usize) {
    update(|d| {
        d["source_events"] = json!(d["source_events"].as_u64().unwrap() + events as u64);
        d["source_chars"] = json!(d["source_chars"].as_u64().unwrap() + chars as u64);
    });
}
pub(crate) fn source_kind(kind: &str, chars: usize) {
    update(|d| {
        let v = &mut d["source_by_kind"][kind];
        if v.is_null() {
            *v = json!({"events":0,"chars":0});
        }
        v["events"] = json!(v["events"].as_u64().unwrap() + 1);
        v["chars"] = json!(v["chars"].as_u64().unwrap() + chars as u64);
    });
}
pub(crate) fn primary(v: &Value) {
    if v["type"] != "event_msg" || v["payload"]["type"] != "token_count" {
        return;
    }
    let raw = &v["payload"]["info"]["total_token_usage"];
    if !raw.is_object() {
        return;
    }
    let mut clean = json!({});
    for k in [
        "input_tokens",
        "cached_input_tokens",
        "cache_write_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ] {
        clean[k] = raw[k].as_u64().map(|v| json!(v)).unwrap_or(Value::Null);
    }
    let timestamp = v["timestamp"].as_str().filter(|s| s.len() < 64);
    update(|d| {
        if timestamp >= d["primary_session_usage"]["timestamp"].as_str() {
            d["primary_session_usage"] = json!({"timestamp":timestamp,"cumulative":clean,"scope":"Codex session as reported, not an incremental cost and not attributable solely to CM"});
        }
    });
}
pub(crate) fn record(mut row: Value) {
    if !enabled() {
        return;
    }
    row["correlation"] = correlation();
    row["agent_log"] = crate::agent_logs::current()
        .map(|l| l.path.to_string_lossy().into_owned())
        .map(Value::String)
        .unwrap_or(Value::Null);
    row["attempt"] = json!(crate::agent_logs::attempt().unwrap_or(1));
    let context = CONTEXT.with(|c| c.borrow().clone());
    for k in ["agent", "phase", "thread"] {
        if row[k].is_null() {
            row[k] = context[k].clone();
        }
    }
    let mut active = RUN.lock().unwrap();
    if let Some(run) = active.as_mut() {
        let calls = run.data["calls"].as_array_mut().unwrap();
        if let Some(previous) = calls.iter_mut().find(|c| c["call_id"] == row["call_id"]) {
            *previous = row;
        } else {
            calls.push(row);
        }
        persist(run);
    }
}

pub(crate) struct MeasuredProvider {
    pub inner: Box<dyn Provider>,
    pub provider: String,
}
impl Provider for MeasuredProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn run_step(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
    ) -> std::result::Result<StepResult, ProviderError> {
        self.run_step_with_schema(spec, cancel, sink, None)
    }
    fn run_step_with_schema(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
        schema: Option<Value>,
    ) -> std::result::Result<StepResult, ProviderError> {
        if !enabled() {
            return self.inner.run_step_with_schema(spec, cancel, sink, schema);
        }
        let start = Instant::now();
        let call_id = fresh_id();
        let started_at = iso_now();
        let encoded = schema.as_ref().map(Value::to_string).unwrap_or_default();
        let mut meter = crate::usage::Meter::default();
        let mut response_metadata = json!({});
        let mut events = 0usize;
        let adapter = self.inner.name();
        record(
            json!({"call_id":call_id,"started_at":started_at,"event":"model_call","provider":self.provider,"adapter":adapter,"model":spec.model,"status":"running","input_chars":spec.prompt.chars().count(),"input_bytes":spec.prompt.len(),"schema_chars":encoded.chars().count(),"schema_bytes":encoded.len()}),
        );
        let result = self.inner.run_step_with_schema(
            spec,
            cancel,
            &mut |e| {
                events += e.text.chars().count();
                observe_usage(&mut meter, &e.raw_json, adapter);
                if e.raw_kind == "cm_usage" {
                    if let Ok(value) = serde_json::from_str::<Value>(&e.raw_json) {
                        for field in ["upstream_provider", "generation_id", "response_model"] {
                            response_metadata[field] = value["usage"][field].clone();
                        }
                    }
                }
                sink(e);
            },
            schema,
        );
        let text = result.as_ref().ok().map(|r| match &r.outcome {
            StepOutcome::Completed { summary } | StepOutcome::Review { summary, .. } => {
                summary.clone()
            }
            StepOutcome::NeedsContext { query } => query.clone(),
            StepOutcome::NeedsInput { question, summary } => {
                format!("{}{}", question, summary.as_deref().unwrap_or(""))
            }
            StepOutcome::InitialRequest { response, .. } => response.clone(),
        });
        let mut row = json!({"event":"model_call","provider":self.provider,"adapter":self.inner.name(),"model":spec.model,"status":if result.is_ok(){"completed"}else{"error"},"input_chars":spec.prompt.chars().count(),"input_bytes":spec.prompt.len(),"schema_chars":encoded.chars().count(),"schema_bytes":encoded.len(),"output_chars":text.as_ref().map(|s|s.chars().count()),"output_bytes":text.as_ref().map(String::len),"event_text_chars":events,"elapsed_ms":start.elapsed().as_millis()});
        row["call_id"] = json!(call_id);
        row["started_at"] = json!(started_at);
        row["finished_at"] = json!(iso_now());
        row["error_kind"] = result
            .as_ref()
            .err()
            .map(|e| {
                json!(match e {
                    ProviderError::Transient { .. } => "transient",
                    ProviderError::Preparation { .. } => "preparation",
                    ProviderError::Spawn { .. } => "spawn",
                    ProviderError::Exit { .. } => "exit",
                    ProviderError::TimedOut { .. } => "timeout",
                    ProviderError::MalformedResult { .. } => "malformed_result",
                    ProviderError::Interrupted => "interrupted",
                })
            })
            .unwrap_or(Value::Null);
        for field in ["upstream_provider", "generation_id", "response_model"] {
            row[field] = response_metadata[field].clone();
        }
        meter.attach(&mut row);
        record(row);
        result
    }
}
fn observe_usage(meter: &mut crate::usage::Meter, raw: &str, adapter: &str) {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    if v["type"] == "cm_usage"
        || (adapter == "codex" && v["type"] == "turn.completed")
        || (adapter == "jsonl" && v["type"] == "result")
    {
        meter.observe(&v["usage"], false);
    } else if adapter == "claude" && v["type"] == "result" {
        let u = &v["usage"];
        let mut normalized = u.clone();
        normalized["input_tokens"] = u["input_tokens"]
            .as_u64()
            .and_then(|n| n.checked_add(u["cache_read_input_tokens"].as_u64().unwrap_or(0)))
            .and_then(|n| n.checked_add(u["cache_creation_input_tokens"].as_u64().unwrap_or(0)))
            .map(|n| json!(n))
            .unwrap_or(Value::Null);
        normalized["cached_input_tokens"] = u["cache_read_input_tokens"].clone();
        normalized["cache_write_input_tokens"] = u["cache_creation_input_tokens"].clone();
        normalized["cost"] = v["total_cost_usd"].clone();
        meter.observe(&normalized, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_is_provider_reported_and_cached_tokens_are_not_added_twice() {
        let mut codex = crate::usage::Meter::default();
        observe_usage(
            &mut codex,
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":20}}"#,
            "codex",
        );
        assert_eq!(codex.usage["input_tokens"], 100);
        let mut claude = crate::usage::Meter::default();
        observe_usage(
            &mut claude,
            r#"{"type":"result","usage":{"input_tokens":20,"cache_read_input_tokens":80,"cache_creation_input_tokens":10,"output_tokens":15},"total_cost_usd":0.02}"#,
            "claude",
        );
        assert_eq!(claude.usage["input_tokens"], 110);
        assert_eq!(claude.usage["cached_input_tokens"], 80);
        assert_eq!(claude.usage["cost_usd"], 0.02);
        let mut missing = crate::usage::Meter::default();
        observe_usage(
            &mut missing,
            r#"{"role":"assistant","content":"text"}"#,
            "kimi",
        );
        assert_eq!(missing.records, 0);
        let mut row = json!({"status":"completed"});
        codex.attach(&mut row);
        let total = totals(&[row, json!({"status":"error"})]);
        assert_eq!(total["tokens"]["input_tokens"]["reported"], 100);
        assert_eq!(total["tokens"]["input_tokens"]["missing_calls"], 1);
        assert_eq!(totals(&[])["tokens"]["input_tokens"]["reported"], 0);
    }
}

fn outcome(data: &Value) -> Value {
    let completeness = data["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["result_status"].as_str())
        .collect::<Vec<_>>();
    let coverage = if completeness.contains(&"partial") {
        "partial"
    } else if completeness.contains(&"error") {
        "error"
    } else if !completeness.is_empty() && completeness.iter().all(|s| *s == "complete") {
        "complete"
    } else {
        "not_applicable"
    };
    let counters = &data["totals"]["tokens"];
    let known = |name: &str| counters[name]["missing_calls"].as_u64() == Some(0);
    let diagnostics = data["call_breakdown"]["analysis"]["failed_calls"]
        .as_u64()
        .unwrap_or(0)
        + data["call_breakdown"]["analysis_retries"]["failed_calls"]
            .as_u64()
            .unwrap_or(0);
    json!({"execution_status":data["status"],"answer_completeness":coverage,
        "diagnostic_failed_calls":diagnostics,
        "token_accounting_complete":known("input_tokens") && known("output_tokens"),
        "cost_accounting_complete":known("cost_usd")})
}
#[cfg(test)]
mod outcome_tests {
    use super::*;
    #[test]
    fn partial_answer_and_missing_usage_are_not_execution_failure() {
        let value = json!({"status":"complete","requests":[{"result_status":"partial"}],
            "totals":{"tokens":{"input_tokens":{"missing_calls":1},"output_tokens":{"missing_calls":0},"cost_usd":{"missing_calls":1}}},
            "call_breakdown":{"analysis":{"failed_calls":1}}});
        let o = outcome(&value);
        assert_eq!(o["execution_status"], "complete");
        assert_eq!(o["answer_completeness"], "partial");
        assert_eq!(o["diagnostic_failed_calls"], 1);
        assert_eq!(o["token_accounting_complete"], false);
        assert_eq!(o["cost_accounting_complete"], false);
    }
}
