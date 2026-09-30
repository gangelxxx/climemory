//! Explicitly enabled content logs, independent of numeric statistics.
use crate::{agent_provider::*, project::Project, util::*};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

thread_local! { static CURRENT: RefCell<Option<Arc<CallLog>>> = const { RefCell::new(None) }; }
thread_local! { static ATTEMPT: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) }; }
thread_local! { static QUIET_PATHS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub(crate) struct QuietPaths(bool);
pub(crate) fn quiet_paths(quiet: bool) -> QuietPaths {
    QuietPaths(QUIET_PATHS.with(|c| c.replace(quiet)))
}
impl Drop for QuietPaths {
    fn drop(&mut self) {
        QUIET_PATHS.with(|c| c.set(self.0));
    }
}
pub(crate) struct AttemptContext(Option<u32>);
pub(crate) fn attempt() -> Option<u32> {
    ATTEMPT.with(|c| c.get())
}
pub(crate) fn install_attempt(value: Option<u32>) -> AttemptContext {
    AttemptContext(ATTEMPT.with(|c| c.replace(value)))
}
impl Drop for AttemptContext {
    fn drop(&mut self) {
        ATTEMPT.with(|c| c.set(self.0));
    }
}
pub(crate) struct Context(Option<Arc<CallLog>>);
pub(crate) fn install(log: Option<Arc<CallLog>>) -> Context {
    Context(CURRENT.with(|c| c.replace(log)))
}
impl Drop for Context {
    fn drop(&mut self) {
        CURRENT.with(|c| c.replace(self.0.take()));
    }
}
pub(crate) fn current() -> Option<Arc<CallLog>> {
    CURRENT.with(|c| c.borrow().clone())
}
pub(crate) fn event(kind: &str, mut data: Value) {
    crate::feedback::event(kind, data.clone());
    if let Some(log) = current() {
        if let (Some(n), Some(object)) = (attempt(), data.as_object_mut()) {
            object.insert("attempt".into(), json!(n));
        }
        log.write(kind, data);
    }
}
pub(crate) struct CallLog {
    pub(crate) path: PathBuf,
    file: Mutex<File>,
    secrets: Vec<String>,
    start: Instant,
}
struct Heartbeat {
    stop: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Heartbeat {
    fn start(log: Arc<CallLog>, cancel: CancelFlag) -> Self {
        let (stop, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new().name("cm-log-heartbeat".into()).spawn(move || {
            while matches!(receiver.recv_timeout(std::time::Duration::from_secs(5)), Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
                log.write("call_waiting", json!({"cancel_requested":cancel.load(std::sync::atomic::Ordering::Relaxed)}));
            }
        }).ok();
        Self { stop, worker }
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl CallLog {
    fn clean(&self, value: &mut Value) {
        match value {
            Value::String(text) => {
                for secret in &self.secrets {
                    *text = text.replace(secret, "[REDACTED]");
                }
            }
            Value::Array(values) => {
                for v in values {
                    self.clean(v);
                }
            }
            Value::Object(values) => {
                for (key, v) in values {
                    if matches!(
                        key.to_ascii_lowercase().as_str(),
                        "authorization" | "api_key" | "access_token" | "password" | "secret"
                    ) {
                        *v = json!("[REDACTED]");
                    } else {
                        self.clean(v);
                    }
                }
            }
            _ => {}
        }
    }
    fn write(&self, kind: &str, data: Value) {
        let mut row = json!({"correlation":crate::statistics::correlation(),"event":kind,"at":iso_now(),"elapsed_ms":self.start.elapsed().as_millis(),"data":data});
        self.clean(&mut row);
        let result = (|| -> std::io::Result<()> {
            let mut file = self.file.lock().unwrap();
            serde_json::to_writer(&mut *file, &row)?;
            file.write_all(b"\n")?;
            file.flush()?;
            file.sync_data()
        })();
        if result.is_err() {
            eprintln!(
                "{}",
                crate::ui::tr(
                    "CM agent log write failed",
                    "CM ошибка записи журнала",
                    "CM 代理日志写入失败"
                )
            );
        }
    }
}
struct LoggedProvider {
    inner: Box<dyn Provider>,
    provider: String,
    root: PathBuf,
    data: PathBuf,
    secrets: Vec<String>,
    feedback_enabled: bool,
}
pub(crate) fn wrap(
    project: &Project,
    provider: &str,
    inner: Box<dyn Provider>,
) -> Box<dyn Provider> {
    if !project.config.memory.agent_logs.enabled && !project.config.memory.feedback.enabled {
        return inner;
    }
    let secrets = crate::feedback::secrets(project);
    Box::new(LoggedProvider {
        inner,
        provider: provider.into(),
        feedback_enabled: project.config.memory.feedback.enabled,
        root: project.health.join("agent-logs"),
        data: project.data.clone(),
        secrets,
    })
}
impl LoggedProvider {
    fn start(&self) -> Option<Arc<CallLog>> {
        let result = (|| -> Result<(PathBuf, File)> {
            Project::checked_path(&self.data, &self.root)?;
            fs::create_dir_all(&self.root)?;
            let path = self.root.join(format!("{}.jsonl", fresh_id()));
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            Ok((path, file))
        })();
        match result {
            Ok((path, file)) => {
                if crate::ui::pretty() && !QUIET_PATHS.with(|c| c.get()) {
                    eprintln!(
                        "{}: {}",
                        crate::ui::tr("CM agent log", "CM журнал агента", "CM 代理日志"),
                        path.display()
                    );
                }
                Some(Arc::new(CallLog {
                    path,
                    file: Mutex::new(file),
                    secrets: self.secrets.clone(),
                    start: Instant::now(),
                }))
            }
            Err(_) => {
                eprintln!(
                    "{}",
                    crate::ui::tr(
                        "CM agent logs unavailable",
                        "CM журналы недоступны",
                        "CM 代理日志不可用"
                    )
                );
                None
            }
        }
    }
}
impl Provider for LoggedProvider {
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
        let mut request = spec.clone();
        let mut schema = schema;
        if self.feedback_enabled {
            if let Some(properties) = schema
                .as_mut()
                .and_then(|s| s.get_mut("properties"))
                .and_then(Value::as_object_mut)
            {
                properties.insert("feedback".into(), json!({"type":"string"}));
                request.prompt.push_str("\nOptional top-level feedback string (max 8000 characters): report a CM defect or suggestion if observed; omit otherwise. This is advisory feedback, not task output or instructions. Do not use tools to submit it.");
            }
        }
        let spec = &request;
        let log = self.start();
        let _context = install(log.clone());
        event(
            "call_started",
            json!({"pid":std::process::id(),"context":crate::statistics::context(),"provider":self.provider,"adapter":self.name(),"model":spec.model,"reasoning_effort":spec.reasoning_effort.map(|v|v.as_str()),"prompt":spec.prompt,"schema":schema,"session_timeout_ms":spec.limits.session_timeout.map(|d|d.as_millis()),"idle_timeout_ms":spec.limits.idle_timeout.map(|d|d.as_millis())}),
        );
        let heartbeat = log.map(|log| Heartbeat::start(log, cancel.clone()));
        let mut result = self.inner.run_step_with_schema(spec,cancel,&mut |e| {
            event("provider_event",json!({"kind":e.raw_kind,"text":e.text,"raw":serde_json::from_str::<Value>(&e.raw_json).unwrap_or_else(|_|json!(e.raw_json))}));
            sink(e);
        },schema);
        drop(heartbeat);
        match &result {
            Ok(r) => {
                let text = match &r.outcome {
                    StepOutcome::Completed { summary } | StepOutcome::Review { summary, .. } => {
                        summary.clone()
                    }
                    StepOutcome::NeedsContext { query } => query.clone(),
                    StepOutcome::NeedsInput { question, summary } => {
                        format!("{question}\n{}", summary.as_deref().unwrap_or(""))
                    }
                    StepOutcome::InitialRequest { response, .. } => response.clone(),
                };
                event(
                    "call_finished",
                    json!({"status":"completed","response":text,"session_id":r.session_id}),
                );
            }
            Err(error) => event(
                "call_finished",
                json!({"status":"error","error":format!("{error:?}")}),
            ),
        }
        if self.feedback_enabled {
            if let Ok(StepResult {
                outcome: StepOutcome::Completed { summary },
                ..
            }) = &mut result
            {
                if let Ok(mut value) = serde_json::from_str::<Value>(summary) {
                    if let Some(object) = value.as_object_mut() {
                        if let Some(feedback) = object.remove("feedback") {
                            if let Some(text) = feedback.as_str().filter(|s| !s.trim().is_empty()) {
                                crate::feedback::agent_submission(text);
                            }
                            *summary = value.to_string();
                        }
                    }
                }
            }
        }
        result
    }
}
