//! Process adapters used by memory agents. Dialogue state remains in CM.
//! Provider argv, output parsing, deadlines and cancellation are isolated here.
#![allow(dead_code)]

use crate::util::{atomic_write, AppError};
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

/// The executable override env var: the test hook (CM_CODEX_EXE = the cm
/// binary running its fake-Codex double) and the drift escape hatch for
/// non-standard Codex installs.
pub const CM_CODEX_EXE_ENV: &str = "CM_CODEX_EXE";
/// Kimi's corresponding executable override.
pub const CM_KIMI_EXE_ENV: &str = "CM_KIMI_EXE";
/// Claude Code's corresponding executable override.
pub const CM_CLAUDE_EXE_ENV: &str = "CM_CLAUDE_EXE";
/// Bound for the captured stderr tail of a failed provider run.
pub const MAX_STDERR_TAIL_CHARS: usize = 4_000;
/// Grace between the terminate request and the hard kill on cancellation.
const CANCEL_GRACE: Duration = Duration::from_secs(5);
const CANCEL_POLL: Duration = Duration::from_millis(50);

// --- Normalized types --------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderEventKind {
    SessionStarted,
    Message,
    Reasoning,
    Command,
    FileChange,
    Error,
    /// Anything the adapter does not specifically understand; raw_kind keeps
    /// the provider's own event type and raw_json preserves the payload in
    /// ignored operational storage.
    Other,
}

impl ProviderEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStarted => "session_started",
            Self::Message => "message",
            Self::Reasoning => "reasoning",
            Self::Command => "command",
            Self::FileChange => "file_change",
            Self::Error => "error",
            Self::Other => "other",
        }
    }
}

/// One normalized provider event. `text` is the complete user-visible
/// projection; `raw_json` is the complete input JSONL line (or non-JSON line)
/// retained only in ignored operational storage. `raw_kind` is the provider's
/// own event type string (`item.completed`, `turn.failed`, ...).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderEvent {
    pub kind: ProviderEventKind,
    pub text: String,
    pub raw_kind: String,
    pub raw_json: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewVerdict {
    Approved,
    ChangesRequested,
}

impl ReviewVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ChangesRequested => "changes_requested",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewFinding {
    pub severity: Option<String>,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub text: String,
}

/// The normalized result of one provider turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    /// Missing repository information is resolved by cm without routing the step.
    NeedsContext { query: String },
    /// A general model step: the provider exited cleanly; the summary is its
    /// final assistant message.
    Completed { summary: String },
    /// A review step: the schema-constrained terminal result.
    Review {
        verdict: ReviewVerdict,
        summary: String,
        findings: Vec<ReviewFinding>,
    },
    /// An interactive workflow step paused before routing so the operator can
    /// answer in the same provider conversation.
    NeedsInput {
        question: String,
        summary: Option<String>,
    },
    /// The read-only turn before agent chat creates any durable workflow.
    InitialRequest {
        action: InitialRequestAction,
        response: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialRequestAction {
    PrepareContext,
    CreateWorkflow,
    Answer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StepResult {
    /// The provider session id (None when the provider never started one).
    pub session_id: Option<String>,
    pub outcome: StepOutcome,
}

/// What one provider step needs.
#[derive(Clone, Debug)]
pub struct StepSpec {
    pub prompt: String,
    /// The project root the provider works in.
    pub cwd: PathBuf,
    pub session: SessionRequest,
    /// Optional provider-native model selector (`-m` / `--model`).
    pub model: Option<String>,
    /// Optional provider-native reasoning effort. Workflow validation limits
    /// this setting to Codex-backed model executors.
    pub reasoning_effort: Option<ModelReasoningEffort>,
    pub result: StepResultKind,
    /// Filesystem access granted to this turn. Review fan-out is always read-only.
    pub access: ProviderAccess,
    /// Permit provider-native tools. Codex suppresses its tool features when false.
    pub native_tools: bool,
    /// Operational process limits. The scheduler chooses them from provider config.
    pub limits: ProviderExecutionLimits,
    /// Adapter side-file directory for structured output schemas. Workflow
    /// steps use the task's agent-runs dir; pre-task routing uses its ignored
    /// request-routing directory.
    pub work_dir: PathBuf,
    /// Extra environment for the child process (test hooks only; production
    /// passes nothing and the child simply inherits the user's environment).
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionRequest {
    Fresh,
    Resume(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepResultKind {
    Completed,
    Review,
    Interactive,
    InitialRequest,
}

impl StepResultKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Review => "review",
            Self::Interactive => "interactive",
            Self::InitialRequest => "initial_request",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ModelReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProviderAccess {
    ReadOnly,
    #[default]
    WorkspaceWrite,
}

impl ProviderAccess {
    fn codex_sandbox(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProviderExecutionLimits {
    pub session_timeout: Option<Duration>,
    pub idle_timeout: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderTimeoutKind {
    Session,
    Idle,
}

impl ProviderTimeoutKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session timeout",
            Self::Idle => "idle timeout",
        }
    }
}

/// Cooperative cancellation: the caller (step 6 wires Ctrl+C) sets the flag;
/// the provider terminates the child within the bounded grace and returns
/// `ProviderError::Interrupted`.
pub type CancelFlag = Arc<AtomicBool>;

pub fn cancel_flag() -> CancelFlag {
    Arc::new(AtomicBool::new(false))
}

/// The provider failure cases. Raw payloads stay out; everything is a
/// bounded, actionable description.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderError {
    /// Retryable remote transport/service error, not a malformed model answer.
    Transient {
        detail: String,
        retry_after_ms: Option<u64>,
    },
    /// Host-side context preparation failed before the model was launched.
    Preparation { detail: String },
    /// The executable could not be started (not found, not permitted).
    Spawn { detail: String },
    /// The provider exited non-zero; the tail is bounded stderr.
    Exit {
        code: Option<i32>,
        stderr_tail: String,
        session_id: Option<String>,
    },
    /// The child exceeded a scheduler-supplied wall-clock or liveness limit.
    TimedOut {
        kind: ProviderTimeoutKind,
        session_id: Option<String>,
    },
    /// The terminal result was missing, unparseable, or contradictory.
    MalformedResult { reason: String },
    /// Cancellation stopped the run.
    Interrupted,
}

impl ProviderError {
    /// The scheduler-facing actionable error. The task state is never
    /// advanced on any of these.
    pub fn into_app_error(self, provider: &str) -> AppError {
        match self {
            Self::Transient { detail, .. } => AppError::new(format!("the {provider} provider is temporarily unavailable: {detail}")),
            Self::Preparation { detail } => AppError::with_hint(
                format!("context preparation failed before launching {provider}: {detail}"),
                "inspect cm thread context and cm semantic doctor diagnostics",
            ),
            Self::Spawn { detail } => AppError::with_hint(
                format!("could not start the {provider} provider: {detail}"),
                provider_install_hint(provider),
            ),
            Self::Exit {
                code, stderr_tail, ..
            } => AppError::with_hint(
                format!(
                    "the {provider} provider failed (exit {}): {}",
                    code.map(|code| code.to_string())
                        .unwrap_or_else(|| "by signal".to_string()),
                    stderr_tail
                ),
                "the task stays at its current state; resolve the provider problem and resume with `cm agent run <task>`".to_string(),
            ),
            Self::TimedOut { kind, .. } => AppError::with_hint(
                format!("the {provider} provider exceeded its {}", kind.as_str()),
                "the task stays at its current state; resume it after adjusting agent.providers timeout/retry settings".to_string(),
            ),
            Self::MalformedResult { reason } => AppError::with_hint(
                format!("the {provider} provider returned an unusable result: {reason}"),
                "the task stays at its current state; resume with `cm agent run <task>` — the step is retried".to_string(),
            ),
            Self::Interrupted => AppError::new("the provider run was interrupted"),
        }
    }
}

/// The internal provider interface implemented by every built-in and custom
/// adapter.
pub trait Provider: Send {
    fn name(&self) -> &'static str;
    /// Optional native output constraint; adapters without support retain host validation.
    fn run_step_with_schema(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
        _schema: Option<serde_json::Value>,
    ) -> std::result::Result<StepResult, ProviderError> {
        self.run_step(spec, cancel, sink)
    }
    fn run_step(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
    ) -> std::result::Result<StepResult, ProviderError>;
}

// --- Review result -------------------------------------------------------------

/// The JSON Schema passed to `codex --no-daemon exec --output-schema` for review steps:
/// the final message must be `{"verdict": "approved"|"changes_requested",
/// "summary": "...", "findings": [{"severity": string|null,
/// "path": string|null, "line": integer|null, "text": string}]}`.
pub fn review_output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdict", "summary", "findings"],
        "properties": {
            "verdict": { "type": "string", "enum": ["approved", "changes_requested", "needs_context"] },
            "summary": { "type": "string" },
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["severity", "path", "line", "text"],
                    "properties": {
                        "severity": { "type": ["string", "null"] },
                        "path": { "type": ["string", "null"] },
                        "line": { "type": ["integer", "null"] },
                        "text": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// Strict result for the read-only decision that precedes task creation in
/// `cm agent chat`.
pub fn initial_request_output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["action", "response"],
        "properties": {
            "action": { "type": "string", "enum": ["create_workflow", "answer", "prepare_context"] },
            "response": { "type": "string" }
        }
    })
}

/// Structured result for a model step that may need one more user answer.
pub fn interactive_output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["status", "summary", "question"],
        "properties": {
            "status": { "type": "string", "enum": ["completed", "needs_input", "needs_context"] },
            "summary": { "type": ["string", "null"] },
            "question": { "type": ["string", "null"] }
        }
    })
}

fn provider_install_hint(provider: &str) -> String {
    match provider {
        "codex" => format!("install Codex, or point {CM_CODEX_EXE_ENV} at its executable"),
        "kimi" => format!("install Kimi Code CLI, or point {CM_KIMI_EXE_ENV} at its executable"),
        "claude" => {
            format!("install Claude Code, or point {CM_CLAUDE_EXE_ENV} at its executable")
        }
        _ => format!("install the {provider} CLI and configure its executable"),
    }
}

#[derive(Deserialize)]
struct ReviewPayload {
    verdict: String,
    summary: String,
    #[serde(default)]
    findings: Vec<ReviewFindingPayload>,
}

#[derive(Deserialize)]
struct ReviewFindingPayload {
    severity: Option<String>,
    path: Option<String>,
    line: Option<u32>,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitialRequestPayload {
    action: String,
    response: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InteractivePayload {
    status: String,
    summary: Option<String>,
    question: Option<String>,
}

fn parse_interactive_result(text: &str) -> std::result::Result<StepOutcome, ProviderError> {
    let payload: InteractivePayload =
        serde_json::from_str(text.trim()).map_err(|error| ProviderError::MalformedResult {
            reason: format!("the final message is not the interactive result JSON ({error})"),
        })?;
    match payload.status.as_str() {
        "needs_context" => {
            if payload.summary.is_some() {
                return Err(ProviderError::MalformedResult {
                    reason: "an interactive context request requires summary null".into(),
                });
            }
            context_request(payload.question.as_deref().unwrap_or(""))
        }
        "completed" => {
            let summary = payload.summary.unwrap_or_default().trim().to_string();
            if summary.is_empty() {
                return Err(ProviderError::MalformedResult {
                    reason: "an interactive completed result requires a non-empty summary"
                        .to_string(),
                });
            }
            if payload
                .question
                .as_deref()
                .is_some_and(|question| !question.trim().is_empty())
            {
                return Err(ProviderError::MalformedResult {
                    reason: "an interactive completed result must not include a question"
                        .to_string(),
                });
            }
            Ok(StepOutcome::Completed { summary })
        }
        "needs_input" => {
            let question = payload.question.unwrap_or_default().trim().to_string();
            if question.is_empty() {
                return Err(ProviderError::MalformedResult {
                    reason: "an interactive needs_input result requires a non-empty question"
                        .to_string(),
                });
            }
            let summary = payload
                .summary
                .map(|summary| summary.trim().to_string())
                .filter(|summary| !summary.is_empty());
            Ok(StepOutcome::NeedsInput { question, summary })
        }
        other => Err(ProviderError::MalformedResult {
            reason: format!(
                "unknown interactive status '{other}'; expected completed or needs_input"
            ),
        }),
    }
}

fn parse_initial_request_result(text: &str) -> std::result::Result<StepOutcome, ProviderError> {
    let payload: InitialRequestPayload =
        serde_json::from_str(text.trim()).map_err(|error| ProviderError::MalformedResult {
            reason: format!("the final message is not the initial request decision JSON ({error})"),
        })?;
    let action = match payload.action.as_str() {
        "create_workflow" => InitialRequestAction::CreateWorkflow,
        "answer" => InitialRequestAction::Answer,
        "prepare_context" => InitialRequestAction::PrepareContext,
        other => {
            return Err(ProviderError::MalformedResult {
                reason: format!(
                    "unknown initial request action '{other}'; expected create_workflow, answer or prepare_context"
                ),
            });
        }
    };
    let response = payload.response.trim().to_string();
    if response.is_empty() {
        return Err(ProviderError::MalformedResult {
            reason: "the initial request response must not be empty".to_string(),
        });
    }
    Ok(StepOutcome::InitialRequest { action, response })
}

/// Parse and validate the schema-constrained review result. Missing,
/// malformed, or contradictory (an `approved` verdict that still carries a
/// blocking-severity finding) results are `ProviderError::MalformedResult`.
fn parse_review_result(text: &str) -> std::result::Result<StepOutcome, ProviderError> {
    let payload: ReviewPayload =
        serde_json::from_str(text.trim()).map_err(|error| ProviderError::MalformedResult {
            reason: format!("the final message is not the review result JSON ({error})"),
        })?;
    if payload.verdict == "needs_context" {
        if !payload.findings.is_empty() {
            return Err(ProviderError::MalformedResult {
                reason: "a context request cannot contain review findings".into(),
            });
        }
        return context_request(&payload.summary);
    }
    let verdict = match payload.verdict.as_str() {
        "approved" => ReviewVerdict::Approved,
        "changes_requested" => ReviewVerdict::ChangesRequested,
        other => {
            return Err(ProviderError::MalformedResult {
                reason: format!(
                    "unknown review verdict '{other}'; expected approved or changes_requested"
                ),
            });
        }
    };
    if payload.summary.trim().is_empty() {
        return Err(ProviderError::MalformedResult {
            reason: "the review summary must not be empty".to_string(),
        });
    }
    let findings = payload
        .findings
        .into_iter()
        .map(|finding| {
            if finding.text.trim().is_empty() {
                return Err(ProviderError::MalformedResult {
                    reason: "a review finding has an empty text".to_string(),
                });
            }
            Ok(ReviewFinding {
                severity: finding.severity,
                path: finding.path,
                line: finding.line,
                text: finding.text,
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if verdict == ReviewVerdict::Approved
        && findings
            .iter()
            .any(|finding| finding.severity.as_deref() == Some("blocking"))
    {
        return Err(ProviderError::MalformedResult {
            reason: "contradictory review result: verdict 'approved' with blocking finding(s)"
                .to_string(),
        });
    }
    Ok(StepOutcome::Review {
        verdict,
        summary: payload.summary,
        findings,
    })
}

// --- Codex adapter ---------------------------------------------------------------

/// The Codex CLI adapter. `exe` resolution: `CM_CODEX_EXE` first (test hook
/// and drift escape hatch), else a directly spawnable `codex` command on
/// PATH. Windows resolution includes npm's `codex.cmd` shim because spawning
/// the bare extensionless name does not apply PATHEXT in every parent process.
pub struct CodexProvider {
    exe: PathBuf,
    args: Vec<String>,
}

impl CodexProvider {
    pub fn from_env() -> Self {
        Self {
            exe: env_provider_executable(CM_CODEX_EXE_ENV, "codex"),
            args: Vec::new(),
        }
    }

    pub fn with_exe(exe: PathBuf) -> Self {
        Self {
            exe,
            args: Vec::new(),
        }
    }

    pub fn with_exe_and_args(exe: PathBuf, args: Vec<String>) -> Self {
        Self { exe, args }
    }
}

/// The executable for a built-in adapter: the CM_*_EXE override when set,
/// else the command resolved on PATH. Shared by the from_env constructors
/// and the configured-args path so fixed `args` never disable the override.
pub(crate) fn env_provider_executable(env_var: &str, command: &str) -> PathBuf {
    std::env::var_os(env_var)
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve_command_executable(command))
}

/// Resolve a shell-free command before passing it to `Command::new`.
/// Windows callers do not consistently apply PATHEXT, and GUI-launched
/// terminals often omit Rust's conventional per-user bin directory.
pub(crate) fn resolve_command_executable(command: &str) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(exe) = resolve_windows_command(
            command,
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("PATHEXT").as_deref(),
            std::env::var_os("USERPROFILE").as_deref(),
        ) {
            return exe;
        }
    }
    PathBuf::from(command)
}

#[cfg(windows)]
fn resolve_windows_command(
    command: &str,
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
    user_profile: Option<&OsStr>,
) -> Option<PathBuf> {
    let command_path = Path::new(command);
    if command_path.components().count() != 1 {
        return command_path.is_file().then(|| command_path.to_path_buf());
    }
    let mut directories = path
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let is_cargo = command_path
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("cargo"));
    if is_cargo {
        if let Some(profile) = user_profile {
            let cargo_bin = PathBuf::from(profile).join(".cargo").join("bin");
            if !directories.iter().any(|directory| directory == &cargo_bin) {
                directories.push(cargo_bin);
            }
        }
    }
    let extensions = if command_path.extension().is_some() {
        vec![String::new()]
    } else {
        pathext
            .and_then(OsStr::to_str)
            .map(|value| {
                value
                    .split(';')
                    .filter(|extension| !extension.is_empty())
                    .map(|extension| {
                        if extension.starts_with('.') {
                            extension.to_ascii_lowercase()
                        } else {
                            format!(".{extension}").to_ascii_lowercase()
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|extensions| !extensions.is_empty())
            .unwrap_or_else(|| vec![".com".into(), ".exe".into(), ".bat".into(), ".cmd".into()])
    };
    directories.into_iter().find_map(|directory| {
        extensions.iter().find_map(|extension| {
            let candidate = directory.join(format!("{command}{extension}"));
            candidate.is_file().then_some(candidate)
        })
    })
}

/// THE argv construction site — every Codex CLI shape lives in this one
/// function so provider CLI drift has exactly one repair site (pinned by
/// unit tests). The prompt is NEVER an argv element: `-` reads it from
/// stdin, sidestepping the Windows ~32k argv limit entirely.
///
/// Shapes:
/// - fresh:  `codex --no-daemon exec --sandbox <sandbox> --json --skip-git-repo-check -C <cwd>
///            [--output-schema <file>] -`
/// - resume: `codex --no-daemon exec --sandbox <sandbox> resume <session-id> --json
///            --skip-git-repo-check -`
///
/// Completed-result workflow steps use the explicit workspace-write sandbox;
/// review steps (plain model reviews and all fanout turns) and the initial
/// request router pass read-only. The sandbox is a parent `exec` option, so
/// it precedes `resume`. `--output-schema` remains fresh-only because review
/// and routing turns always start fresh. `-C` is fresh-only: a resumed session
/// keeps its original working directory, and the child process itself is still
/// spawned with the workspace root as cwd (the agent worktree when one is
/// prepared, otherwise the project root).
fn codex_argv(
    cwd: &Path,
    session: &SessionRequest,
    model: Option<&str>,
    reasoning_effort: Option<ModelReasoningEffort>,
    schema: Option<&Path>,
    sandbox: &str,
) -> Vec<String> {
    let mut argv = vec!["--no-daemon".to_string(), "exec".to_string()];
    argv.push("--sandbox".to_string());
    argv.push(sandbox.to_string());
    if let Some(model) = model {
        argv.push("--model".to_string());
        argv.push(model.to_string());
    }
    if let Some(reasoning_effort) = reasoning_effort {
        argv.push("--config".to_string());
        argv.push(format!(
            "model_reasoning_effort=\"{}\"",
            reasoning_effort.as_str()
        ));
    }
    if let SessionRequest::Resume(session_id) = session {
        argv.push("resume".to_string());
        argv.push(session_id.clone());
    }
    argv.push("--json".to_string());
    argv.push("--skip-git-repo-check".to_string());
    if let SessionRequest::Fresh = session {
        argv.push("-C".to_string());
        argv.push(cwd.to_string_lossy().into_owned());
        if let Some(schema) = schema {
            argv.push("--output-schema".to_string());
            argv.push(schema.to_string_lossy().into_owned());
        }
    }
    argv.push("-".to_string());
    argv
}

/// Per-process isolation for JSON protocol workers; no global config edits.
fn codex_no_tools_args() -> Vec<String> {
    let mut args = vec!["--ignore-user-config".into(), "--ephemeral".into()];
    for setting in [
        "features.shell_tool=false",
        "features.unified_exec=false",
        "features.multi_agent=false",
        "features.apps=false",
        "features.plugins=false",
        "features.hooks=false",
        "features.browser_use=false",
        "features.in_app_browser=false",
        "features.image_generation=false",
        "features.view_image=false",
        "features.code_mode=false",
        "features.code_mode_host=false",
        "features.sleep_tool=false",
        "features.skill_search=false",
        "features.tool_suggest=false",
        "features.skip_host_skill_discovery=true",
        "mcp_servers.cm.enabled=false",
        "web_search=\"disabled\"",
        "project_doc_max_bytes=0",
        "project_root_markers=[\"AGENTS.md\"]",
    ] {
        args.extend(["--config".into(), setting.into()]);
    }
    args
}

// Windows restricted tokens can fail native profile discovery even when the
// caller's USERPROFILE is inherited. Use that caller-owned default explicitly;
// never search other profiles, create a new home, or replace a configured home.
#[cfg(windows)]
fn codex_home_fallback(configured: Option<&OsStr>, profile: Option<&OsStr>) -> Option<PathBuf> {
    if configured.is_some() {
        return None;
    }
    let profile = Path::new(profile?);
    let home = profile.join(".codex");
    (profile.is_absolute() && home.is_dir()).then_some(home)
}

/// Stateful parser for the Codex JSONL event stream: captures the session
/// id (thread.started), tracks the last agent_message (the final summary /
/// review payload), and normalizes each line into one ProviderEvent.
/// Defensive: unknown event kinds pass through as `other` with their raw type
/// name and complete raw JSON so the operational journal remains replayable.
#[derive(Default)]
struct CodexStreamParser {
    session_id: Option<String>,
    final_message: Option<String>,
}

impl CodexStreamParser {
    fn on_line(&mut self, line: &str) -> Option<ProviderEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // Not JSON at all: keep a bounded preview for diagnostics.
            return Some(ProviderEvent {
                kind: ProviderEventKind::Other,
                text: trimmed.to_string(),
                raw_kind: "non_json".to_string(),
                raw_json: trimmed.to_string(),
            });
        };
        let raw_kind = value
            .get("type")
            .and_then(|kind| kind.as_str())
            .unwrap_or("unknown")
            .to_string();
        let event = |kind, text: String| ProviderEvent {
            kind,
            text,
            raw_kind: raw_kind.clone(),
            raw_json: trimmed.to_string(),
        };
        match raw_kind.as_str() {
            "thread.started" => {
                let session_id = value
                    .get("thread_id")
                    .and_then(|id| id.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !session_id.is_empty() {
                    self.session_id = Some(session_id.clone());
                }
                Some(event(ProviderEventKind::SessionStarted, session_id))
            }
            "item.completed" | "item.started" => {
                let item = value.get("item").cloned().unwrap_or_default();
                let item_type = item
                    .get("type")
                    .and_then(|kind| kind.as_str())
                    .unwrap_or("unknown");
                let text_of = |key: &str| {
                    item.get(key)
                        .and_then(|text| text.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                match item_type {
                    "agent_message" => {
                        let text = text_of("text");
                        if raw_kind == "item.completed" {
                            self.final_message = Some(text.clone());
                        }
                        Some(event(ProviderEventKind::Message, text))
                    }
                    "reasoning" => Some(event(ProviderEventKind::Reasoning, text_of("text"))),
                    "command_execution" => {
                        let command = text_of("command");
                        let exit = item
                            .get("exit_code")
                            .and_then(|code| code.as_i64())
                            .map(|code| format!(" (exit {code})"))
                            .unwrap_or_default();
                        Some(event(
                            ProviderEventKind::Command,
                            format!("{command}{exit}"),
                        ))
                    }
                    "file_change" | "file_changes" => {
                        let paths = item
                            .get("changes")
                            .and_then(|changes| changes.as_array())
                            .map(|changes| {
                                changes
                                    .iter()
                                    .filter_map(|change| {
                                        change.get("path").and_then(|path| path.as_str())
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        Some(event(ProviderEventKind::FileChange, paths))
                    }
                    "error" => Some(event(ProviderEventKind::Error, text_of("message"))),
                    _ => Some(ProviderEvent {
                        kind: ProviderEventKind::Other,
                        text: String::new(),
                        raw_kind: format!("{raw_kind}:{item_type}"),
                        raw_json: trimmed.to_string(),
                    }),
                }
            }
            "error" | "turn.failed" => {
                let message = value
                    .get("message")
                    .and_then(|message| message.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        value
                            .get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(|message| message.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                Some(event(ProviderEventKind::Error, message))
            }
            _ => Some(ProviderEvent {
                kind: ProviderEventKind::Other,
                text: String::new(),
                raw_kind,
                raw_json: trimmed.to_string(),
            }),
        }
    }
}

/// Per-line stdout stream parser shared by the CLI provider scaffolding.
trait StreamLineParser {
    fn feed_line(&mut self, line: &str) -> Option<ProviderEvent>;
}

impl StreamLineParser for CodexStreamParser {
    fn feed_line(&mut self, line: &str) -> Option<ProviderEvent> {
        self.on_line(line)
    }
}

impl StreamLineParser for KimiStreamParser {
    fn feed_line(&mut self, line: &str) -> Option<ProviderEvent> {
        self.on_line(line)
    }
}

/// Shared spawn/stream/cancel scaffolding for the CLI providers: the
/// prompt rides stdin on a writer thread (a large prompt never deadlocks
/// against a full pipe buffer), stderr accumulates into a bounded tail, each
/// stdout line feeds the provider's stream parser, the cancellation poll
/// terminates the child (bounded grace, then kill), and the configured
/// session/idle timeouts fire through the same activity-tracked pipes.
/// Returns the process outcome: exit status, bounded stderr tail, and how
/// the run ended (completed, interrupted, or which timeout fired).
fn pump_provider_child(
    child: &mut Child,
    prompt: String,
    parser: &mut (impl StreamLineParser + Send),
    cancel: &CancelFlag,
    sink: &mut (dyn FnMut(&ProviderEvent) + Send),
    limits: ProviderExecutionLimits,
) -> std::result::Result<ProviderProcessOutcome, ProviderError> {
    let mut stdin = child.stdin.take().expect("stdin was piped");
    std::thread::spawn(move || {
        let _ = stdin.write_all(prompt.as_bytes());
        // Closing stdin is the prompt's end marker.
        drop(stdin);
    });

    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let started = Instant::now();
    // A launcher can exit while descendants retain its pipes. Keep cancellation
    // active through stream draining; scoped joins would wait indefinitely.
    let (lines, receiver) = std::sync::mpsc::sync_channel(16);
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let tail = Arc::clone(&stderr_tail);
    let stderr_activity = Arc::clone(&last_activity);
    let stderr_done = Arc::new(AtomicBool::new(false));
    let done = stderr_done.clone();
    std::thread::spawn(move || {
        let text = read_bounded_tail(
            ActivityReader::new(stderr, stderr_activity),
            MAX_STDERR_TAIL_CHARS,
        );
        *tail.lock().unwrap() = text;
        done.store(true, Ordering::Release);
    });
    let stdout_activity = Arc::clone(&last_activity);
    std::thread::spawn(move || {
        for line in BufReader::new(ActivityReader::new(stdout, stdout_activity)).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let mut stdout_done = false;
    let mut stop_observed = None;
    let termination = {
        // Cancellation poll: terminate (bounded grace), then kill.
        loop {
            if cancel.load(Ordering::Relaxed) {
                stop_observed = Some(Instant::now());
                terminate_child(child);
                break ProviderTermination::Interrupted;
            }
            if limits
                .session_timeout
                .is_some_and(|timeout| started.elapsed() >= timeout)
            {
                stop_observed = Some(Instant::now());
                terminate_child(child);
                break ProviderTermination::TimedOut(ProviderTimeoutKind::Session);
            }
            if limits.idle_timeout.is_some_and(|timeout| {
                last_activity
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .elapsed()
                    >= timeout
            }) {
                stop_observed = Some(Instant::now());
                terminate_child(child);
                break ProviderTermination::TimedOut(ProviderTimeoutKind::Idle);
            }
            match receiver.recv_timeout(CANCEL_POLL) {
                Ok(line) => {
                    if let Some(event) = parser.feed_line(&line) {
                        sink(&event);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => stdout_done = true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if matches!(child.try_wait(), Ok(Some(_)) | Err(_))
                && stdout_done
                && stderr_done.load(Ordering::Acquire)
            {
                break ProviderTermination::Completed;
            }
            if stdout_done {
                std::thread::sleep(CANCEL_POLL);
            }
        }
    };
    if let Some(observed) = stop_observed {
        eprintln!(
            "cm: provider shutdown {}",
            serde_json::json!({
                "reason":format!("{termination:?}"),
                "signal_observed_ms":observed.duration_since(started).as_millis(),
                "cleanup_ms":observed.elapsed().as_millis(),
                "stdout_drained":stdout_done,"stderr_drained":stderr_done.load(Ordering::Acquire)
            })
        );
    }
    let status = child
        .try_wait()
        .map_err(|error| ProviderError::Spawn {
            detail: format!("could not inspect the provider process: {error}"),
        })?
        .ok_or_else(|| ProviderError::Spawn {
            detail: "provider process did not exit within the bounded shutdown budget".into(),
        })?;
    let stderr_tail = stderr_tail
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    Ok(ProviderProcessOutcome {
        status,
        stderr_tail,
        termination,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderTermination {
    Completed,
    Interrupted,
    TimedOut(ProviderTimeoutKind),
}

struct ProviderProcessOutcome {
    status: std::process::ExitStatus,
    stderr_tail: String,
    termination: ProviderTermination,
}

struct ActivityReader<R> {
    inner: R,
    last_activity: Arc<Mutex<Instant>>,
}

impl<R> ActivityReader<R> {
    fn new(inner: R, last_activity: Arc<Mutex<Instant>>) -> Self {
        Self {
            inner,
            last_activity,
        }
    }
}

impl<R: Read> Read for ActivityReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0 {
            *self
                .last_activity
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
        }
        Ok(read)
    }
}

impl Provider for CodexProvider {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn run_step(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
    ) -> std::result::Result<StepResult, ProviderError> {
        self.run_constrained_step(spec, cancel, sink, None)
    }

    fn run_step_with_schema(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
        schema: Option<serde_json::Value>,
    ) -> std::result::Result<StepResult, ProviderError> {
        self.run_constrained_step(spec, cancel, sink, schema)
    }
}

impl CodexProvider {
    fn run_constrained_step(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
        output_schema: Option<serde_json::Value>,
    ) -> std::result::Result<StepResult, ProviderError> {
        let schema = output_schema
            .map(|schema| ("context-output-schema.json", schema))
            .or_else(|| match spec.result {
                StepResultKind::Completed => None,
                StepResultKind::Review => {
                    Some(("review-output-schema.json", review_output_schema()))
                }
                StepResultKind::Interactive => Some((
                    "interactive-output-schema.json",
                    interactive_output_schema(),
                )),
                StepResultKind::InitialRequest => Some((
                    "initial-request-output-schema.json",
                    initial_request_output_schema(),
                )),
            });
        let sandbox = if spec.result == StepResultKind::InitialRequest {
            ProviderAccess::ReadOnly.codex_sandbox()
        } else {
            spec.access.codex_sandbox()
        };
        let schema_path = schema
            .map(|(name, schema)| {
                let path = spec.work_dir.join(name);
                std::fs::create_dir_all(&spec.work_dir).map_err(|error| ProviderError::Spawn {
                    detail: format!(
                        "cannot create the agent work directory {}: {error}",
                        spec.work_dir.display()
                    ),
                })?;
                let schema = serde_json::to_string_pretty(&schema)
                    .expect("the provider output schema serializes");
                atomic_write(&path, schema.as_bytes()).map_err(|error| ProviderError::Spawn {
                    detail: format!(
                        "cannot write the provider output schema {}: {error}",
                        path.display()
                    ),
                })?;
                Ok(path)
            })
            .transpose()?;
        let mut argv = codex_argv(
            &spec.cwd,
            &spec.session,
            spec.model.as_deref(),
            spec.reasoning_effort,
            schema_path.as_deref(),
            sandbox,
        );
        if !spec.native_tools {
            argv.splice(2..2, codex_no_tools_args());
        }
        let mut command = Command::new(&self.exe);
        crate::process::hide_window(&mut command);
        command
            .args(&self.args)
            .args(&argv)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.envs(spec.env.iter().map(|(key, value)| (key, value)));
        #[cfg(windows)]
        {
            let inherited_home = std::env::var_os("CODEX_HOME");
            let inherited_profile = std::env::var_os("USERPROFILE");
            let value = |key: &str| {
                spec.env
                    .iter()
                    .rev()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                    .map(|(_, v)| OsStr::new(v))
            };
            if let Some(home) = codex_home_fallback(
                value("CODEX_HOME").or(inherited_home.as_deref()),
                value("USERPROFILE").or(inherited_profile.as_deref()),
            ) {
                command.env("CODEX_HOME", home);
            }
        }
        let mut child = command.spawn().map_err(|error| {
            let detail = if error.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "executable '{}' was not found; install Codex or set {CM_CODEX_EXE_ENV} to its path",
                    self.exe.display()
                )
            } else {
                format!("could not spawn '{}': {error}", self.exe.display())
            };
            ProviderError::Spawn { detail }
        })?;

        // The prompt rides stdin (the `-` argv slot) on the shared pump's
        // writer thread so a large prompt never deadlocks against a full
        // pipe buffer.
        let mut parser = CodexStreamParser::default();
        let prompt = if spec.result == StepResultKind::Interactive
            && matches!(spec.session, SessionRequest::Resume(_))
        {
            schema_constrained_prompt(spec)
        } else {
            spec.prompt.clone()
        };
        let outcome =
            pump_provider_child(&mut child, prompt, &mut parser, cancel, sink, spec.limits)?;
        let session_id = parser.session_id.clone();
        match outcome.termination {
            ProviderTermination::Interrupted => return Err(ProviderError::Interrupted),
            ProviderTermination::TimedOut(kind) => {
                return Err(ProviderError::TimedOut { kind, session_id })
            }
            ProviderTermination::Completed => {}
        }
        if !outcome.status.success() {
            return Err(ProviderError::Exit {
                code: outcome.status.code(),
                stderr_tail: outcome.stderr_tail,
                session_id,
            });
        }
        let Some(final_message) = parser.final_message.clone() else {
            return Err(ProviderError::MalformedResult {
                reason: "the provider exited cleanly but produced no final assistant message"
                    .to_string(),
            });
        };
        let outcome = match spec.result {
            StepResultKind::Completed => parse_completed_result(&final_message)?,
            StepResultKind::Review => parse_review_result(&final_message)?,
            StepResultKind::Interactive => parse_interactive_result(&final_message)?,
            StepResultKind::InitialRequest => parse_initial_request_result(&final_message)?,
        };
        Ok(StepResult {
            session_id,
            outcome,
        })
    }
}

// --- Kimi adapter ----------------------------------------------------------

/// Kimi Code CLI adapter. Kimi's prompt mode is non-interactive; read-only
/// steps additionally use plan mode, which restricts the available tools.
/// Initial-request routing remains unsupported until its provider contract is
/// enabled independently.
pub struct KimiProvider {
    exe: PathBuf,
    args: Vec<String>,
}

impl KimiProvider {
    pub fn from_env() -> Self {
        Self {
            exe: env_provider_executable(CM_KIMI_EXE_ENV, "kimi"),
            args: Vec::new(),
        }
    }

    pub fn with_exe(exe: PathBuf) -> Self {
        Self {
            exe,
            args: Vec::new(),
        }
    }

    pub fn with_exe_and_args(exe: PathBuf, args: Vec<String>) -> Self {
        Self { exe, args }
    }
}

/// Kimi's non-interactive print-mode contract. The prompt rides stdin, which
/// keeps the full context pack clear of Windows' command-line size ceiling.
fn kimi_argv(session: &SessionRequest, model: Option<&str>, access: ProviderAccess) -> Vec<String> {
    let mut argv = Vec::new();
    if let SessionRequest::Resume(session_id) = session {
        argv.push("--session".to_string());
        argv.push(session_id.clone());
    }
    if let Some(model) = model {
        argv.push("--model".to_string());
        argv.push(model.to_string());
    }
    if access == ProviderAccess::ReadOnly {
        // `--plan` also forces plan mode back on when resuming a session, so
        // a read-only CM step cannot inherit a writable Kimi session mode.
        // The CLI persists plan mode into the resumed session and has no flag
        // to disable it again, which is why workflow validation rejects Kimi
        // review steps with session 'reuse': one read-only resume would leave
        // the shared session read-only for every later writable step.
        argv.push("--plan".to_string());
    }
    argv.extend([
        "--print".to_string(),
        "--input-format".to_string(),
        "text".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ]);
    argv
}

fn kimi_prompt(spec: &StepSpec) -> std::result::Result<String, ProviderError> {
    match spec.result {
        StepResultKind::Completed => Ok(spec.prompt.clone()),
        StepResultKind::Review => {
            let schema = serde_json::to_string(&review_output_schema())
                .expect("the review schema serializes");
            Ok(format!(
                "{}\n\nYour final assistant message MUST contain only one JSON object matching this schema, without Markdown fences or commentary:\n{}",
                spec.prompt, schema
            ))
        }
        StepResultKind::Interactive => {
            let schema = serde_json::to_string(&interactive_output_schema())
                .expect("the interactive schema serializes");
            Ok(format!(
                "{}\n\nYour final assistant message MUST contain only one JSON object matching this schema, without Markdown fences or commentary:\n{}",
                spec.prompt, schema
            ))
        }
        StepResultKind::InitialRequest => Err(ProviderError::MalformedResult {
            reason:
                "Kimi initial-request routing is not enabled; use Codex for initial request routing"
                    .to_string(),
        }),
    }
}

#[derive(Default)]
struct KimiStreamParser {
    session_id: Option<String>,
    final_message: Option<String>,
}

impl KimiStreamParser {
    fn on_line(&mut self, line: &str) -> Option<ProviderEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return Some(ProviderEvent {
                kind: ProviderEventKind::Other,
                text: trimmed.to_string(),
                raw_kind: "non_json".to_string(),
                raw_json: trimmed.to_string(),
            });
        };
        let role = value
            .get("role")
            .and_then(|role| role.as_str())
            .unwrap_or("unknown");
        let event_type = value
            .get("type")
            .and_then(|kind| kind.as_str())
            .unwrap_or_default();
        let raw_kind = if event_type.is_empty() {
            role.to_string()
        } else {
            format!("{role}:{event_type}")
        };
        let event = |kind, text: String| ProviderEvent {
            kind,
            text,
            raw_kind: raw_kind.clone(),
            raw_json: trimmed.to_string(),
        };
        if role == "meta" && event_type == "session.resume_hint" {
            let session_id = value
                .get("session_id")
                .and_then(|id| id.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if !session_id.is_empty() {
                self.session_id = Some(session_id.clone());
            }
            return Some(event(ProviderEventKind::SessionStarted, session_id));
        }
        match role {
            "assistant" => {
                let content = kimi_content_text(value.get("content"));
                let tool_calls = value
                    .get("tool_calls")
                    .and_then(|calls| calls.as_array())
                    .filter(|calls| !calls.is_empty());
                if let Some(tool_calls) = tool_calls {
                    // An assistant turn may carry both content and tool
                    // calls; the content is still a candidate final message
                    // (it can be the last assistant text of the run).
                    if !content.is_empty() {
                        self.final_message = Some(content.clone());
                    }
                    let description = kimi_tool_calls_text(tool_calls);
                    let kind = if kimi_tools_change_files(tool_calls) {
                        ProviderEventKind::FileChange
                    } else {
                        ProviderEventKind::Command
                    };
                    Some(event(
                        kind,
                        if content.is_empty() {
                            description
                        } else {
                            format!("{content}\n{description}")
                        },
                    ))
                } else {
                    if !content.is_empty() {
                        self.final_message = Some(content.clone());
                    }
                    Some(event(ProviderEventKind::Message, content))
                }
            }
            "tool" => Some(event(
                ProviderEventKind::Command,
                kimi_content_text(value.get("content")),
            )),
            "error" => Some(event(
                ProviderEventKind::Error,
                kimi_content_text(value.get("content")),
            )),
            _ => Some(event(
                ProviderEventKind::Other,
                kimi_content_text(value.get("content")),
            )),
        }
    }
}

/// Kimi-only structured-result tolerance: the prompt forbids Markdown
/// fences, but a whole-payload ```json fence is unambiguous, so strip exactly
/// one outer fence before the strict parse. Anything else (leading
/// commentary, inner fences) stays malformed.
fn kimi_unfenced_result_text(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some((_, body)) = rest.split_once('\n') else {
        return trimmed;
    };
    match body.trim_end().strip_suffix("```") {
        Some(inner) => inner.trim(),
        None => trimmed,
    }
}

fn kimi_content_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(|text| text.as_str())
                    .or_else(|| block.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

fn kimi_tool_name(call: &serde_json::Value) -> &str {
    call.get("function")
        .and_then(|function| function.get("name"))
        .and_then(|name| name.as_str())
        .or_else(|| call.get("name").and_then(|name| name.as_str()))
        .unwrap_or("tool")
}

fn kimi_tools_change_files(calls: &[serde_json::Value]) -> bool {
    calls.iter().any(|call| {
        let name = kimi_tool_name(call).to_ascii_lowercase();
        ["write", "edit", "patch", "replace", "create"]
            .iter()
            .any(|needle| name.contains(needle))
    })
}

fn kimi_tool_calls_text(calls: &[serde_json::Value]) -> String {
    calls
        .iter()
        .map(|call| {
            let name = kimi_tool_name(call);
            let arguments = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .or_else(|| call.get("arguments"))
                .map(|arguments| match arguments {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            if arguments.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {arguments}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Provider for KimiProvider {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn run_step(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
    ) -> std::result::Result<StepResult, ProviderError> {
        let prompt = kimi_prompt(spec)?;
        let argv = kimi_argv(&spec.session, spec.model.as_deref(), spec.access);
        let mut command = Command::new(&self.exe);
        crate::process::hide_window(&mut command);
        command
            .args(&self.args)
            .args(&argv)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.envs(spec.env.iter().map(|(key, value)| (key, value)));
        let mut child = command.spawn().map_err(|error| {
            let detail = if error.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "executable '{}' was not found; install Kimi Code CLI or set {CM_KIMI_EXE_ENV} to its path",
                    self.exe.display()
                )
            } else {
                format!("could not spawn '{}': {error}", self.exe.display())
            };
            ProviderError::Spawn { detail }
        })?;

        let mut parser = KimiStreamParser::default();
        let outcome =
            pump_provider_child(&mut child, prompt, &mut parser, cancel, sink, spec.limits)?;
        let session_id = parser.session_id.clone();
        match outcome.termination {
            ProviderTermination::Interrupted => return Err(ProviderError::Interrupted),
            ProviderTermination::TimedOut(kind) => {
                return Err(ProviderError::TimedOut { kind, session_id })
            }
            ProviderTermination::Completed => {}
        }
        if !outcome.status.success() {
            return Err(ProviderError::Exit {
                code: outcome.status.code(),
                stderr_tail: outcome.stderr_tail,
                session_id,
            });
        }
        let Some(final_message) = parser.final_message else {
            return Err(ProviderError::MalformedResult {
                reason: "the provider exited cleanly but produced no final assistant message"
                    .to_string(),
            });
        };
        let outcome = match spec.result {
            StepResultKind::Completed => parse_completed_result(&final_message)?,
            StepResultKind::Review => {
                parse_review_result(kimi_unfenced_result_text(&final_message))?
            }
            StepResultKind::Interactive => {
                parse_interactive_result(kimi_unfenced_result_text(&final_message))?
            }
            StepResultKind::InitialRequest => unreachable!("rejected before spawn"),
        };
        Ok(StepResult {
            session_id,
            outcome,
        })
    }
}

// --- Claude Code adapter ---------------------------------------------------

/// Claude Code print-mode adapter. CM keeps the CLI dialect here and consumes
/// only documented stream-json records; the scheduler sees the same normalized
/// provider surface as Codex and Kimi.
pub struct ClaudeProvider {
    exe: PathBuf,
    args: Vec<String>,
}

impl ClaudeProvider {
    pub fn from_env() -> Self {
        Self {
            exe: env_provider_executable(CM_CLAUDE_EXE_ENV, "claude"),
            args: Vec::new(),
        }
    }

    pub fn with_exe_and_args(exe: PathBuf, args: Vec<String>) -> Self {
        Self { exe, args }
    }
}

fn claude_argv(
    session: &SessionRequest,
    model: Option<&str>,
    access: ProviderAccess,
) -> Vec<String> {
    let mut argv = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
    ];
    if let Some(model) = model {
        argv.push("--model".to_string());
        argv.push(model.to_string());
    }
    if let SessionRequest::Resume(session_id) = session {
        argv.push("--resume".to_string());
        argv.push(session_id.clone());
    }
    match access {
        ProviderAccess::ReadOnly => {
            argv.push("--permission-mode".to_string());
            argv.push("plan".to_string());
        }
        ProviderAccess::WorkspaceWrite => {
            // Print mode cannot prompt, so edit tools stay denied unless an
            // auto-accept mode is given; acceptEdits is the minimal
            // write-enabling mode (--dangerously-skip-permissions stays
            // unused).
            argv.push("--permission-mode".to_string());
            argv.push("acceptEdits".to_string());
        }
    }
    argv
}

fn schema_constrained_prompt(spec: &StepSpec) -> String {
    let schema = match spec.result {
        StepResultKind::Completed => return spec.prompt.clone(),
        StepResultKind::Review => review_output_schema(),
        StepResultKind::Interactive => interactive_output_schema(),
        StepResultKind::InitialRequest => initial_request_output_schema(),
    };
    format!(
        "{}\n\nYour final response MUST contain only one JSON object matching this schema, without Markdown fences or commentary:\n{}",
        spec.prompt,
        serde_json::to_string(&schema).expect("the provider result schema serializes")
    )
}

#[derive(Default)]
struct ClaudeStreamParser {
    session_id: Option<String>,
    final_message: Option<String>,
    protocol_error: Option<String>,
    saw_result: bool,
}

impl ClaudeStreamParser {
    fn on_line(&mut self, line: &str) -> Option<ProviderEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                self.protocol_error = Some(format!("invalid stream-json record: {error}"));
                return Some(ProviderEvent {
                    kind: ProviderEventKind::Error,
                    text: "invalid Claude stream-json record".to_string(),
                    raw_kind: "non_json".to_string(),
                    raw_json: trimmed.to_string(),
                });
            }
        };
        let raw_kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        if let Some(session_id) = value
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            self.session_id = Some(session_id.to_string());
        }
        let event = |kind, text: String| ProviderEvent {
            kind,
            text,
            raw_kind: raw_kind.clone(),
            raw_json: trimmed.to_string(),
        };
        match raw_kind.as_str() {
            "system"
                if value.get("subtype").and_then(serde_json::Value::as_str) == Some("init") =>
            {
                Some(event(
                    ProviderEventKind::SessionStarted,
                    self.session_id.clone().unwrap_or_default(),
                ))
            }
            "assistant" => {
                let text = value
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .map(claude_content_text)
                    .unwrap_or_default();
                if !text.is_empty() {
                    self.final_message = Some(text.clone());
                }
                Some(event(ProviderEventKind::Message, text))
            }
            "result" => {
                self.saw_result = true;
                let text = value
                    .get("result")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if value
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    self.protocol_error = Some(if text.is_empty() {
                        "Claude reported an error result".to_string()
                    } else {
                        text.clone()
                    });
                    return Some(event(ProviderEventKind::Error, text));
                }
                if !text.is_empty() {
                    self.final_message = Some(text.clone());
                }
                Some(event(ProviderEventKind::Message, text))
            }
            _ => Some(event(ProviderEventKind::Other, String::new())),
        }
    }
}

fn claude_content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => {
            // Prefer real text blocks: thinking content must not leak into a
            // step summary when a terminal result record is empty.
            let block_text = |kind: &str| {
                blocks
                    .iter()
                    .filter_map(|block| {
                        (block.get("type").and_then(serde_json::Value::as_str) == Some(kind))
                            .then(|| block.get("text").and_then(serde_json::Value::as_str))
                            .flatten()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let text = block_text("text");
            if text.is_empty() {
                block_text("thinking")
            } else {
                text
            }
        }
        _ => String::new(),
    }
}

impl StreamLineParser for ClaudeStreamParser {
    fn feed_line(&mut self, line: &str) -> Option<ProviderEvent> {
        self.on_line(line)
    }
}

impl Provider for ClaudeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn run_step(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
    ) -> std::result::Result<StepResult, ProviderError> {
        let access = if spec.result == StepResultKind::InitialRequest {
            ProviderAccess::ReadOnly
        } else {
            spec.access
        };
        let argv = claude_argv(&spec.session, spec.model.as_deref(), access);
        let mut command = Command::new(&self.exe);
        crate::process::hide_window(&mut command);
        command
            .args(&self.args)
            .args(&argv)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(spec.env.iter().map(|(key, value)| (key, value)));
        let mut child = command.spawn().map_err(|error| ProviderError::Spawn {
            detail: format!("could not spawn '{}': {error}", self.exe.display()),
        })?;
        let mut parser = ClaudeStreamParser::default();
        let process = pump_provider_child(
            &mut child,
            schema_constrained_prompt(spec),
            &mut parser,
            cancel,
            sink,
            spec.limits,
        )?;
        let session_id = parser.session_id.clone();
        match process.termination {
            ProviderTermination::Interrupted => return Err(ProviderError::Interrupted),
            ProviderTermination::TimedOut(kind) => {
                return Err(ProviderError::TimedOut { kind, session_id })
            }
            ProviderTermination::Completed => {}
        }
        if !process.status.success() {
            return Err(ProviderError::Exit {
                code: process.status.code(),
                stderr_tail: process.stderr_tail,
                session_id,
            });
        }
        if let Some(reason) = parser.protocol_error {
            return Err(ProviderError::MalformedResult { reason });
        }
        if !parser.saw_result {
            return Err(ProviderError::MalformedResult {
                reason: "Claude exited cleanly without a terminal result record".to_string(),
            });
        }
        let final_message = parser
            .final_message
            .ok_or_else(|| ProviderError::MalformedResult {
                reason: "Claude's terminal result contained no response text".to_string(),
            })?;
        Ok(StepResult {
            session_id,
            outcome: parse_step_outcome(spec.result, &final_message)?,
        })
    }
}

// --- Versioned custom JSONL adapter ---------------------------------------

pub const CUSTOM_PROVIDER_REQUEST_SCHEMA: &str = "climemory/provider-request-1";
pub const CUSTOM_PROVIDER_EVENT_SCHEMA: &str = "climemory/provider-event-1";

pub struct JsonlProvider {
    exe: PathBuf,
    args: Vec<String>,
}

impl JsonlProvider {
    pub fn new(exe: PathBuf, args: Vec<String>) -> Self {
        Self { exe, args }
    }
}

#[derive(Serialize)]
struct JsonlProviderRequest<'a> {
    schema: &'static str,
    prompt: &'a str,
    cwd: String,
    session: JsonlSessionRequest<'a>,
    model: Option<&'a str>,
    result: &'static str,
    access: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum JsonlSessionRequest<'a> {
    Fresh,
    Resume { id: &'a str },
}

#[derive(Default)]
struct JsonlStreamParser {
    session_id: Option<String>,
    final_message: Option<String>,
    protocol_error: Option<String>,
    saw_result: bool,
}

impl JsonlStreamParser {
    fn on_line(&mut self, line: &str) -> Option<ProviderEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        if self.saw_result {
            self.protocol_error = Some("received a record after the terminal result".to_string());
        }
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                self.protocol_error = Some(format!("invalid JSONL record: {error}"));
                return Some(ProviderEvent {
                    kind: ProviderEventKind::Error,
                    text: "invalid custom provider JSONL record".to_string(),
                    raw_kind: "non_json".to_string(),
                    raw_json: trimmed.to_string(),
                });
            }
        };
        if value
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|schema| schema != CUSTOM_PROVIDER_EVENT_SCHEMA)
        {
            self.protocol_error = Some(format!(
                "event schema must be '{CUSTOM_PROVIDER_EVENT_SCHEMA}'"
            ));
        }
        let raw_kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let text = value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let event = |kind, text: String| ProviderEvent {
            kind,
            text,
            raw_kind: raw_kind.clone(),
            raw_json: trimmed.to_string(),
        };
        match raw_kind.as_str() {
            "session" => {
                let id = value
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default();
                if id.is_empty() {
                    self.protocol_error =
                        Some("a session event requires a non-empty id".to_string());
                } else {
                    self.session_id = Some(id.to_string());
                }
                Some(event(ProviderEventKind::SessionStarted, id.to_string()))
            }
            "message" => Some(event(ProviderEventKind::Message, text)),
            "reasoning" => Some(event(ProviderEventKind::Reasoning, text)),
            "command" => Some(event(ProviderEventKind::Command, text)),
            "file_change" => Some(event(ProviderEventKind::FileChange, text)),
            "error" => Some(event(ProviderEventKind::Error, text)),
            "result" => {
                if self.saw_result {
                    self.protocol_error =
                        Some("received more than one terminal result".to_string());
                }
                self.saw_result = true;
                if let Some(id) = value
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                {
                    self.session_id = Some(id.to_string());
                }
                if text.trim().is_empty() {
                    self.protocol_error =
                        Some("the terminal result requires non-empty text".to_string());
                } else {
                    self.final_message = Some(text.clone());
                }
                Some(event(ProviderEventKind::Message, text))
            }
            other => {
                self.protocol_error = Some(format!("unknown custom provider event type '{other}'"));
                Some(event(ProviderEventKind::Other, text))
            }
        }
    }
}

impl StreamLineParser for JsonlStreamParser {
    fn feed_line(&mut self, line: &str) -> Option<ProviderEvent> {
        self.on_line(line)
    }
}

impl Provider for JsonlProvider {
    fn name(&self) -> &'static str {
        "jsonl"
    }

    fn run_step(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
    ) -> std::result::Result<StepResult, ProviderError> {
        let session = match &spec.session {
            SessionRequest::Fresh => JsonlSessionRequest::Fresh,
            SessionRequest::Resume(id) => JsonlSessionRequest::Resume { id },
        };
        let request = JsonlProviderRequest {
            schema: CUSTOM_PROVIDER_REQUEST_SCHEMA,
            prompt: &spec.prompt,
            cwd: spec.cwd.to_string_lossy().to_string(),
            session,
            model: spec.model.as_deref(),
            result: spec.result.as_str(),
            access: spec.access.as_str(),
        };
        let mut input =
            serde_json::to_string(&request).map_err(|error| ProviderError::MalformedResult {
                reason: format!("cannot serialize the custom provider request: {error}"),
            })?;
        input.push('\n');
        let mut command = Command::new(&self.exe);
        crate::process::hide_window(&mut command);
        command
            .args(&self.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(spec.env.iter().map(|(key, value)| (key, value)));
        let mut child = command.spawn().map_err(|error| ProviderError::Spawn {
            detail: format!("could not spawn '{}': {error}", self.exe.display()),
        })?;
        let mut parser = JsonlStreamParser::default();
        let process =
            pump_provider_child(&mut child, input, &mut parser, cancel, sink, spec.limits)?;
        let session_id = parser.session_id.clone();
        match process.termination {
            ProviderTermination::Interrupted => return Err(ProviderError::Interrupted),
            ProviderTermination::TimedOut(kind) => {
                return Err(ProviderError::TimedOut { kind, session_id })
            }
            ProviderTermination::Completed => {}
        }
        if !process.status.success() {
            return Err(ProviderError::Exit {
                code: process.status.code(),
                stderr_tail: process.stderr_tail,
                session_id,
            });
        }
        if let Some(reason) = parser.protocol_error {
            return Err(ProviderError::MalformedResult { reason });
        }
        if !parser.saw_result {
            return Err(ProviderError::MalformedResult {
                reason: "the custom provider exited without a terminal result event".to_string(),
            });
        }
        let final_message = parser
            .final_message
            .ok_or_else(|| ProviderError::MalformedResult {
                reason: "the custom provider terminal result was empty".to_string(),
            })?;
        Ok(StepResult {
            session_id,
            outcome: parse_step_outcome(spec.result, &final_message)?,
        })
    }
}

fn parse_step_outcome(
    kind: StepResultKind,
    text: &str,
) -> std::result::Result<StepOutcome, ProviderError> {
    match kind {
        StepResultKind::Completed => parse_completed_result(text),
        StepResultKind::Review => parse_review_result(text),
        StepResultKind::Interactive => parse_interactive_result(text),
        StepResultKind::InitialRequest => parse_initial_request_result(text),
    }
}

fn context_request(query: &str) -> std::result::Result<StepOutcome, ProviderError> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 2000 {
        return Err(ProviderError::MalformedResult {
            reason: "context request requires 1..2000 characters describing missing information"
                .into(),
        });
    }
    Ok(StepOutcome::NeedsContext {
        query: query.into(),
    })
}

fn parse_completed_result(text: &str) -> std::result::Result<StepOutcome, ProviderError> {
    // Normalize only the control payload. Ordinary completed output must keep
    // its original formatting, including code fences and surrounding whitespace.
    let payload = kimi_unfenced_result_text(text);
    let parsed = serde_json::from_str::<serde_json::Value>(payload);
    if parsed.is_err() && payload.starts_with('{') && payload.contains("\"needs_context\"") {
        return Err(ProviderError::MalformedResult {
            reason: "malformed needs_context JSON; workflow was not completed".into(),
        });
    }
    if let Ok(value) = parsed {
        if value["status"] == "needs_context" {
            return context_request(value["query"].as_str().unwrap_or(""));
        }
    }
    Ok(StepOutcome::Completed {
        summary: text.into(),
    })
}

#[cfg(test)]
mod context_request_tests {
    use super::*;

    #[test]
    fn fenced_context_requests_never_complete_and_normal_output_is_preserved() {
        assert_eq!(
            parse_completed_result(
                "```json\n{\"status\":\"needs_context\",\"query\":\"retry callers\"}\n```"
            )
            .unwrap(),
            StepOutcome::NeedsContext {
                query: "retry callers".into()
            }
        );
        assert!(
            parse_completed_result("```json\n{\"status\":\"needs_context\",\"query\": }\n```")
                .is_err()
        );
        let summary = "  ```rust\nfn main() {}\n```\n";
        assert_eq!(
            parse_completed_result(summary).unwrap(),
            StepOutcome::Completed {
                summary: summary.into()
            }
        );
    }

    #[test]
    fn all_workflow_result_kinds_return_discovery_to_cm() {
        for (kind, payload) in [
            (
                StepResultKind::Completed,
                r#"{"status":"needs_context","query":"retry callers"}"#,
            ),
            (
                StepResultKind::Interactive,
                r#"{"status":"needs_context","question":"retry callers","summary":null}"#,
            ),
            (
                StepResultKind::Review,
                r#"{"verdict":"needs_context","summary":"retry callers","findings":[]}"#,
            ),
        ] {
            assert_eq!(
                parse_step_outcome(kind, payload).unwrap(),
                StepOutcome::NeedsContext {
                    query: "retry callers".into()
                }
            );
        }
        assert!(parse_completed_result(r#"{"status":"needs_context","query":" "}"#).is_err());
        assert!(parse_completed_result(r#"{"status":"needs_context","query": }"#).is_err());
        assert!(parse_interactive_result(
            r#"{"status":"needs_context","question":"retry callers","summary":"already completed"}"#
        )
        .is_err());
        assert!(parse_review_result(r#"{"verdict":"needs_context","summary":"retry callers","findings":[{"text":"premature finding"}]}"#).is_err());
        assert_eq!(
            parse_completed_result("Implemented retries").unwrap(),
            StepOutcome::Completed {
                summary: "Implemented retries".into()
            }
        );
    }
}

/// Graceful-then-hard termination: SIGTERM, up to CANCEL_GRACE to exit, then
/// kill. Windows bounds the tree-kill helper and fallback cleanup by the same
/// grace period; the caller never follows this with an unbounded wait.
pub(crate) fn terminate_child(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("taskkill");
        command
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(0x08000000); // CREATE_NO_WINDOW
        terminate_windows(child, command, CANCEL_GRACE);
    }
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(_) => return,
            }
            if started.elapsed() >= CANCEL_GRACE {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

// A bounded helper wait keeps a slow taskkill from consuming an unlimited
// cancellation budget. Never select processes by executable name.
#[cfg(windows)]
fn terminate_windows(child: &mut Child, mut command: Command, budget: Duration) {
    let started = Instant::now();
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let mut helper = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
    let mut tree_confirmed = false;
    if let Some(process) = helper.as_mut() {
        let deadline = started + budget / 2;
        while matches!(process.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        tree_confirmed = process
            .try_wait()
            .ok()
            .flatten()
            .is_some_and(|s| s.success());
        let _ = process.kill();
    }
    let helper_ms = started.elapsed().as_millis();
    let _ = child.kill();
    let deadline = started + budget;
    while Instant::now() < deadline {
        let child_done = !matches!(child.try_wait(), Ok(None));
        let helper_done = helper
            .as_mut()
            .is_none_or(|p| !matches!(p.try_wait(), Ok(None)));
        if child_done && helper_done {
            eprintln!(
                "cm: Windows provider cleanup {}",
                serde_json::json!({
                    "tree_helper_ms":helper_ms,"total_ms":started.elapsed().as_millis(),
                    "tree_exit_confirmed":tree_confirmed
                })
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Own the helper until it is reaped without extending the caller's deadline.
    if let Some(mut helper) = helper {
        std::thread::spawn(move || {
            let _ = helper.wait();
        });
    }
    eprintln!("cm: provider shutdown exceeded its bounded cleanup budget");
}

/// Read a stream to end, retaining only the last `cap` chars.
pub(crate) fn read_bounded_tail(reader: impl Read, cap: usize) -> String {
    let mut reader = reader;
    let mut retained = Vec::new();
    let mut chunk = [0u8; 8_192];
    while let Ok(read) = reader.read(&mut chunk) {
        if read == 0 {
            break;
        }
        retained.extend_from_slice(&chunk[..read]);
        if retained.len() > cap * 2 {
            let keep = retained.split_off(retained.len() - cap);
            retained = keep;
        }
    }
    let text = String::from_utf8_lossy(&retained);
    text.chars()
        .rev()
        .take(cap)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn codex_home_uses_only_existing_caller_profile_and_preserves_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().as_os_str();
        assert_eq!(super::codex_home_fallback(None, Some(profile)), None);
        std::fs::create_dir(temp.path().join(".codex")).unwrap();
        assert_eq!(
            super::codex_home_fallback(None, Some(profile)),
            Some(temp.path().join(".codex"))
        );
        for value in ["", "custom", "C:\\explicit-home"] {
            assert_eq!(
                super::codex_home_fallback(Some(std::ffi::OsStr::new(value)), Some(profile)),
                None
            );
        }
        assert_eq!(super::codex_home_fallback(None, None), None);
        assert_eq!(
            super::codex_home_fallback(None, Some(std::ffi::OsStr::new("relative"))),
            None
        );
    }
    use super::*;

    #[cfg(windows)]
    #[test]
    fn shutdown_sleep_fixture() {
        if std::env::var_os("CM_TEST_SHUTDOWN_SLEEP").is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[cfg(windows)]
    #[test]
    fn slow_shutdown_helper_is_bounded_and_does_not_kill_unrelated_process() {
        fn sleeper() -> Command {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "agent_provider::tests::shutdown_sleep_fixture"])
                .env("CM_TEST_SHUTDOWN_SLEEP", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command
        }
        let mut child = sleeper().spawn().unwrap();
        let mut unrelated = sleeper().spawn().unwrap();
        let started = Instant::now();
        terminate_windows(&mut child, sleeper(), Duration::from_millis(400));
        let elapsed = started.elapsed();
        let stopped = child.try_wait().unwrap().is_some();
        let untouched = unrelated.try_wait().unwrap().is_none();
        let _ = unrelated.kill();
        let _ = unrelated.wait();
        assert!(stopped);
        assert!(untouched);
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    }

    #[test]
    #[expect(
        clippy::zombie_processes,
        reason = "The fixture deliberately exits before its descendant to test inherited pipes; the parent test releases that descendant."
    )]
    fn inherited_pipe_fixture() {
        match std::env::var("CM_TEST_INHERITED_PIPE").as_deref() {
            Ok("launcher") => {
                let _ = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "agent_provider::tests::inherited_pipe_fixture",
                        "--nocapture",
                    ])
                    .env("CM_TEST_INHERITED_PIPE", "holder")
                    .spawn()
                    .unwrap();
            }
            Ok("holder") => {
                let started = Instant::now();
                let release = std::env::var("CM_TEST_PIPE_RELEASE").unwrap();
                while !Path::new(&release).exists() && started.elapsed() < Duration::from_secs(10) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = std::fs::write(Path::new(&release).with_extension("done"), "done");
            }
            _ => {}
        }
    }

    #[test]
    fn cancellation_remains_active_after_launcher_exit_with_inherited_pipes() {
        let temp = tempfile::tempdir().unwrap();
        let release = temp.path().join("release");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent_provider::tests::inherited_pipe_fixture",
                "--nocapture",
            ])
            .env("CM_TEST_INHERITED_PIPE", "launcher")
            .env("CM_TEST_PIPE_RELEASE", &release)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // try_wait leaves stdin owned by Child for the pump to take.
        while child.try_wait().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
        let cancel = cancel_flag();
        let signal = cancel.clone();
        let notifier = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            signal.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let outcome = pump_provider_child(
            &mut child,
            String::new(),
            &mut CodexStreamParser::default(),
            &cancel,
            &mut |_| {},
            ProviderExecutionLimits::default(),
        )
        .unwrap();
        std::fs::write(&release, "release").unwrap();
        notifier.join().unwrap();
        assert_eq!(outcome.termination, ProviderTermination::Interrupted);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "stream draining ignored cancellation"
        );
        let cleanup = Instant::now();
        while !release.with_extension("done").exists() && cleanup.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // --- Pure-function tests ------------------------------------------------

    #[test]
    fn codex_argv_pins_workflow_review_resume_and_routing_shapes() {
        let cwd = Path::new("C:\\project");
        assert_eq!(
            codex_argv(
                cwd,
                &SessionRequest::Fresh,
                None,
                None,
                None,
                "workspace-write",
            ),
            vec![
                "--no-daemon",
                "exec",
                "--sandbox",
                "workspace-write",
                "--json",
                "--skip-git-repo-check",
                "-C",
                "C:\\project",
                "-",
            ]
        );
        assert_eq!(
            codex_argv(
                cwd,
                &SessionRequest::Fresh,
                None,
                None,
                Some(Path::new("C:\\schema.json")),
                "workspace-write",
            ),
            vec![
                "--no-daemon",
                "exec",
                "--sandbox",
                "workspace-write",
                "--json",
                "--skip-git-repo-check",
                "-C",
                "C:\\project",
                "--output-schema",
                "C:\\schema.json",
                "-",
            ]
        );
        // Resume: no -C, no --output-schema; the session id names the thread.
        assert_eq!(
            codex_argv(
                cwd,
                &SessionRequest::Resume("sess-1".to_string()),
                Some("gpt-test"),
                Some(ModelReasoningEffort::High),
                None,
                "workspace-write",
            ),
            vec![
                "--no-daemon",
                "exec",
                "--sandbox",
                "workspace-write",
                "--model",
                "gpt-test",
                "--config",
                "model_reasoning_effort=\"high\"",
                "resume",
                "sess-1",
                "--json",
                "--skip-git-repo-check",
                "-",
            ]
        );
        assert_eq!(
            codex_argv(
                cwd,
                &SessionRequest::Fresh,
                None,
                None,
                Some(Path::new("C:\\route-schema.json")),
                "read-only",
            ),
            vec![
                "--no-daemon",
                "exec",
                "--sandbox",
                "read-only",
                "--json",
                "--skip-git-repo-check",
                "-C",
                "C:\\project",
                "--output-schema",
                "C:\\route-schema.json",
                "-",
            ]
        );
    }

    #[test]
    fn kimi_argv_and_stream_parser_pin_session_model_and_activity() {
        assert_eq!(
            kimi_argv(
                &SessionRequest::Resume("session-k1".to_string()),
                Some("kimi-code/kimi-for-coding"),
                ProviderAccess::WorkspaceWrite,
            ),
            vec![
                "--session",
                "session-k1",
                "--model",
                "kimi-code/kimi-for-coding",
                "--print",
                "--input-format",
                "text",
                "--output-format",
                "stream-json",
            ]
        );
        assert_eq!(
            kimi_argv(&SessionRequest::Fresh, None, ProviderAccess::ReadOnly),
            vec![
                "--plan",
                "--print",
                "--input-format",
                "text",
                "--output-format",
                "stream-json",
            ]
        );

        let mut parser = KimiStreamParser::default();
        let tool = parser
            .on_line(
                r#"{"role":"assistant","content":[],"tool_calls":[{"function":{"name":"WriteFile","arguments":"{\"path\":\"x\"}"}}]}"#,
            )
            .unwrap();
        assert_eq!(tool.kind, ProviderEventKind::FileChange);
        let message = parser
            .on_line(r#"{"role":"assistant","content":[{"type":"text","text":"done"}]}"#)
            .unwrap();
        assert_eq!(message.kind, ProviderEventKind::Message);
        assert_eq!(parser.final_message.as_deref(), Some("done"));
        let session = parser
            .on_line(r#"{"role":"meta","type":"session.resume_hint","session_id":"session-k1"}"#)
            .unwrap();
        assert_eq!(session.kind, ProviderEventKind::SessionStarted);
        assert_eq!(parser.session_id.as_deref(), Some("session-k1"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_resolution_finds_npm_command_shims() {
        let temp = tempfile::tempdir().unwrap();
        let shim = temp.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").unwrap();
        // An extensionless sibling must not mask the directly spawnable shim.
        std::fs::write(temp.path().join("codex"), "#!/bin/sh\n").unwrap();
        let path = std::env::join_paths([temp.path()]).unwrap();
        let pathext = OsStr::new(".EXE;.CMD");

        assert_eq!(
            resolve_windows_command("codex", Some(path.as_os_str()), Some(pathext), None),
            Some(shim)
        );
        assert_eq!(
            resolve_windows_command("missing", Some(path.as_os_str()), Some(pathext), None),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolution_falls_back_to_the_user_cargo_bin() {
        let temp = tempfile::tempdir().unwrap();
        let cargo_bin = temp.path().join(".cargo").join("bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        let cargo = cargo_bin.join("cargo.exe");
        std::fs::write(&cargo, b"").unwrap();
        let empty_path = std::env::join_paths([temp.path().join("elsewhere")]).unwrap();

        assert_eq!(
            resolve_windows_command(
                "cargo",
                Some(empty_path.as_os_str()),
                Some(OsStr::new(".EXE;.CMD")),
                Some(temp.path().as_os_str()),
            ),
            Some(cargo)
        );
    }

    #[test]
    fn stream_normalization_maps_known_and_unknown_events() {
        let mut parser = CodexStreamParser::default();
        macro_rules! event {
            ($line:expr) => {
                parser.on_line($line).expect("an event")
            };
        }

        let session = event!(r#"{"type":"thread.started","thread_id":"sess-9"}"#);
        assert_eq!(session.kind, ProviderEventKind::SessionStarted);
        assert_eq!(session.text, "sess-9");
        assert_eq!(parser.session_id.as_deref(), Some("sess-9"));

        let turn = event!(r#"{"type":"turn.started"}"#);
        assert_eq!(turn.kind, ProviderEventKind::Other);
        assert_eq!(turn.raw_kind, "turn.started");

        let reasoning = event!(
            r#"{"type":"item.completed","item":{"id":"i1","type":"reasoning","text":"thinking"}}"#
        );
        assert_eq!(reasoning.kind, ProviderEventKind::Reasoning);
        assert_eq!(reasoning.text, "thinking");

        let command = event!(
            r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"bash -lc ls","exit_code":0}}"#
        );
        assert_eq!(command.kind, ProviderEventKind::Command);
        assert_eq!(command.text, "bash -lc ls (exit 0)");

        let change = event!(
            r#"{"type":"item.completed","item":{"id":"i3","type":"file_change","changes":[{"path":"src/a.rs"},{"path":"src/b.rs"}]}}"#
        );
        assert_eq!(change.kind, ProviderEventKind::FileChange);
        assert_eq!(change.text, "src/a.rs, src/b.rs");

        let message = event!(
            r#"{"type":"item.completed","item":{"id":"i4","type":"agent_message","text":"all done"}}"#
        );
        assert_eq!(message.kind, ProviderEventKind::Message);
        assert_eq!(parser.final_message.as_deref(), Some("all done"));

        let error = event!(r#"{"type":"error","message":"boom"}"#);
        assert_eq!(error.kind, ProviderEventKind::Error);
        assert_eq!(error.text, "boom");
        let failed = event!(r#"{"type":"turn.failed","error":{"message":"budget"}}"#);
        assert_eq!(failed.kind, ProviderEventKind::Error);
        assert_eq!(failed.text, "budget");

        // Unknown kinds and non-JSON lines stay lossless in the operational
        // transcript even when their normalized presentation is empty.
        let unknown_line = r#"{"type":"mcp.tool_call","x":1}"#;
        let unknown = event!(unknown_line);
        assert_eq!(unknown.kind, ProviderEventKind::Other);
        assert_eq!(unknown.raw_kind, "mcp.tool_call");
        assert_eq!(unknown.raw_json, unknown_line);
        assert!(unknown.text.is_empty());
        let item_unknown =
            event!(r#"{"type":"item.completed","item":{"type":"web_search","query":"q"}}"#);
        assert_eq!(item_unknown.raw_kind, "item.completed:web_search");
        let non_json = event!("this is not json");
        assert_eq!(non_json.kind, ProviderEventKind::Other);
        assert_eq!(non_json.raw_kind, "non_json");
        assert_eq!(non_json.text, "this is not json");
        assert_eq!(non_json.raw_json, "this is not json");
        let long_text = "x".repeat(4000 + 250);
        let long_line = serde_json::json!({
            "type": "item.completed",
            "item": {"type": "agent_message", "text": long_text}
        })
        .to_string();
        let long = event!(&long_line);
        assert_eq!(long.text.chars().count(), 4000 + 250);
        assert_eq!(long.raw_json, long_line);
        assert!(parser.on_line("").is_none());
    }

    #[test]
    fn review_output_schema_is_pinned() {
        assert_eq!(
            review_output_schema(),
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["verdict", "summary", "findings"],
                "properties": {
                    "verdict": { "type": "string", "enum": ["approved", "changes_requested", "needs_context"] },
                    "summary": { "type": "string" },
                    "findings": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["severity", "path", "line", "text"],
                            "properties": {
                                "severity": { "type": ["string", "null"] },
                                "path": { "type": ["string", "null"] },
                                "line": { "type": ["integer", "null"] },
                                "text": { "type": "string" }
                            }
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn initial_request_schema_and_results_are_strict() {
        assert_eq!(
            initial_request_output_schema(),
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["action", "response"],
                "properties": {
                    "action": { "type": "string", "enum": ["create_workflow", "answer", "prepare_context"] },
                    "response": { "type": "string" }
                }
            })
        );

        let create = parse_initial_request_result(
            r#"{"action":"create_workflow","response":"This needs repository work."}"#,
        )
        .unwrap();
        assert_eq!(
            create,
            StepOutcome::InitialRequest {
                action: InitialRequestAction::CreateWorkflow,
                response: "This needs repository work.".to_string(),
            }
        );
        assert!(matches!(parse_initial_request_result(
            r#"{"action":"prepare_context","response":"Explain repository agent configuration"}"#
        ).unwrap(), StepOutcome::InitialRequest { action: InitialRequestAction::PrepareContext, .. }));
        let answer = parse_initial_request_result(
            r#"{"action":"answer","response":"The current mode opens a neutral home."}"#,
        )
        .unwrap();
        assert_eq!(
            answer,
            StepOutcome::InitialRequest {
                action: InitialRequestAction::Answer,
                response: "The current mode opens a neutral home.".to_string(),
            }
        );

        for invalid in [
            r#"{"action":"later","response":"x"}"#,
            r#"{"action":"answer","response":"  "}"#,
            r#"{"action":"answer"}"#,
            r#"{"action":"answer","response":"x","extra":true}"#,
        ] {
            assert!(matches!(
                parse_initial_request_result(invalid),
                Err(ProviderError::MalformedResult { .. })
            ));
        }
    }

    fn malformed(text: &str) -> String {
        match parse_review_result(text) {
            Err(ProviderError::MalformedResult { reason }) => reason,
            other => panic!("expected MalformedResult for {text:?}, got {other:?}"),
        }
    }

    #[test]
    fn review_result_parsing_validates_the_terminal_payload() {
        let approved = parse_review_result(
            r#"{"verdict":"approved","summary":"clean","findings":[{"severity":"minor","path":null,"line":null,"text":"nit: naming"}]}"#,
        )
        .unwrap();
        let StepOutcome::Review {
            verdict,
            summary,
            findings,
        } = approved
        else {
            panic!("expected a review outcome");
        };
        assert_eq!(verdict, ReviewVerdict::Approved);
        assert_eq!(summary, "clean");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity.as_deref(), Some("minor"));

        let changes = parse_review_result(
            r#"{"verdict":"changes_requested","summary":"two problems","findings":[{"severity":"blocking","path":"src/a.rs","line":12,"text":"race"}]}"#,
        )
        .unwrap();
        let StepOutcome::Review {
            verdict, findings, ..
        } = changes
        else {
            panic!("expected a review outcome");
        };
        assert_eq!(verdict, ReviewVerdict::ChangesRequested);
        assert_eq!(findings[0].path.as_deref(), Some("src/a.rs"));
        assert_eq!(findings[0].line, Some(12));

        assert!(malformed("not json").contains("not the review result JSON"));
        assert!(
            malformed(r#"{"summary":"x","findings":[]}"#).contains("not the review result JSON")
        );
        assert!(
            malformed(r#"{"verdict":"lgtm","summary":"x","findings":[]}"#)
                .contains("unknown review verdict 'lgtm'")
        );
        assert!(
            malformed(r#"{"verdict":"approved","summary":"  ","findings":[]}"#)
                .contains("summary must not be empty")
        );
        assert!(malformed(
            r#"{"verdict":"changes_requested","summary":"x","findings":[{"text":" "}]}"#
        )
        .contains("empty text"));
        assert!(malformed(
            r#"{"verdict":"approved","summary":"x","findings":[{"severity":"blocking","text":"must fix"}]}"#
        )
        .contains("contradictory"));
    }

    #[test]
    fn read_bounded_tail_keeps_only_the_last_chars() {
        let text = "x".repeat(10_000);
        let tail = read_bounded_tail(text.as_bytes(), 100);
        assert_eq!(tail.len(), 100);
        assert!(tail.chars().all(|c| c == 'x'));
        assert_eq!(read_bounded_tail("short".as_bytes(), 100), "short");
    }

    // --- Process-level tests through the fake Codex ----------------------------

    /// The real cm binary carrying the fake-Codex double. Unit tests in a
    /// bin target run inside the libtest harness (current_exe is NOT cm), so
    /// locate the sibling binary cargo built next to the harness.
    fn cm_exe() -> PathBuf {
        let current = std::env::current_exe().unwrap();
        let debug = current.parent().and_then(Path::parent).unwrap();
        let exe = debug.join(if cfg!(windows) { "cm.exe" } else { "cm" });
        assert!(
            exe.is_file(),
            "the fake-codex tests spawn the real binary; run `cargo build` first ({})",
            exe.display()
        );
        exe
    }

    fn write_scenario(dir: &Path, calls: serde_json::Value) -> String {
        write_scenario_named(dir, "scenario", calls)
    }

    fn write_scenario_named(dir: &Path, name: &str, calls: serde_json::Value) -> String {
        let scenario = dir.join(format!("{name}.json"));
        std::fs::write(
            &scenario,
            serde_json::json!({
                "state_file": dir.join(format!("{name}-state.json")),
                "calls": calls,
            })
            .to_string(),
        )
        .unwrap();
        scenario.to_string_lossy().into_owned()
    }

    fn spec(
        root: &Path,
        session: SessionRequest,
        result: StepResultKind,
        scenario: &str,
    ) -> StepSpec {
        StepSpec {
            prompt: "the bounded context pack".to_string(),
            cwd: root.to_path_buf(),
            session,
            model: None,
            reasoning_effort: None,
            result,
            access: ProviderAccess::WorkspaceWrite,
            native_tools: true,
            limits: ProviderExecutionLimits::default(),
            work_dir: root.join("agent-run"),
            env: vec![("CM_FAKE_CODEX_SCENARIO".to_string(), scenario.to_string())],
        }
    }

    fn run(
        spec: &StepSpec,
    ) -> (
        std::result::Result<StepResult, ProviderError>,
        Vec<ProviderEvent>,
    ) {
        let provider = CodexProvider::with_exe(cm_exe());
        let cancel = cancel_flag();
        let mut events = Vec::new();
        let result = provider.run_step(spec, &cancel, &mut |event| events.push(event.clone()));
        (result, events)
    }

    #[test]
    fn run_step_streams_normalized_events_and_completes() {
        let temp = tempfile::TempDir::new().unwrap();
        let scenario = write_scenario(
            temp.path(),
            serde_json::json!([{
                "session_id": "sess-1",
                "expect_sandbox": "workspace-write",
                "events": [
                    {"type": "turn.started"},
                    {"type": "item.completed", "item": {"id": "i1", "type": "reasoning", "text": "thinking"}},
                    {"type": "item.completed", "item": {"id": "i2", "type": "command_execution", "command": "cargo test", "exit_code": 0}}
                ],
                "final_message": "implemented the change",
                "exit_code": 0
            }]),
        );
        let (result, events) = run(&spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Completed,
            &scenario,
        ));
        let result = result.unwrap();
        assert_eq!(result.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            result.outcome,
            StepOutcome::Completed {
                summary: "implemented the change".to_string()
            }
        );
        let kinds = events.iter().map(|event| event.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                ProviderEventKind::SessionStarted,
                ProviderEventKind::Other,
                ProviderEventKind::Reasoning,
                ProviderEventKind::Command,
                ProviderEventKind::Message,
            ]
        );
        // The review schema file is only written for review steps.
        assert!(!temp
            .path()
            .join("agent-run/review-output-schema.json")
            .exists());
    }

    #[test]
    fn run_step_enforces_session_and_idle_timeouts() {
        for (name, limits, expected) in [
            (
                "session-timeout",
                ProviderExecutionLimits {
                    session_timeout: Some(Duration::from_millis(50)),
                    idle_timeout: None,
                },
                ProviderTimeoutKind::Session,
            ),
            (
                "idle-timeout",
                ProviderExecutionLimits {
                    session_timeout: None,
                    idle_timeout: Some(Duration::from_millis(50)),
                },
                ProviderTimeoutKind::Idle,
            ),
        ] {
            let temp = tempfile::TempDir::new().unwrap();
            let scenario = write_scenario_named(
                temp.path(),
                name,
                serde_json::json!([{
                    "delay_ms": 1_000,
                    "session_id": "too-late",
                    "final_message": "too late",
                    "exit_code": 0
                }]),
            );
            let mut request = spec(
                temp.path(),
                SessionRequest::Fresh,
                StepResultKind::Completed,
                &scenario,
            );
            request.limits = limits;
            let (result, _) = run(&request);
            assert_eq!(
                result.unwrap_err(),
                ProviderError::TimedOut {
                    kind: expected,
                    session_id: None,
                }
            );
        }
    }

    #[test]
    fn run_step_resume_validates_the_session_and_reports_mismatch() {
        let temp = tempfile::TempDir::new().unwrap();
        let scenario = write_scenario(
            temp.path(),
            serde_json::json!([{
                "session_id": "sess-1",
                "expect_resume_session": "sess-1",
                "final_message": "continued",
                "exit_code": 0
            }]),
        );
        let (result, _) = run(&spec(
            temp.path(),
            SessionRequest::Resume("sess-1".to_string()),
            StepResultKind::Completed,
            &scenario,
        ));
        assert!(result.is_ok(), "{result:?}");

        // A fresh session where the script demands a resume: the fake exits
        // 3, surfaced as a provider exit failure with its stderr tail.
        let scenario = write_scenario_named(
            temp.path(),
            "mismatch",
            serde_json::json!([{
                "expect_resume_session": "sess-1",
                "exit_code": 3
            }]),
        );
        let (result, _) = run(&spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Completed,
            &scenario,
        ));
        assert_eq!(
            result.unwrap_err(),
            ProviderError::Exit {
                code: Some(3),
                stderr_tail: "fake-codex: expected `resume sess-1`, got None".to_string(),
                session_id: None,
            }
        );
    }

    #[test]
    fn completed_turn_passes_native_schema_without_parsing_as_review() {
        let temp = tempfile::TempDir::new().unwrap();
        let reply = r#"{"response":{"action":"done","selected":["code:real"]}}"#;
        let scenario = write_scenario(
            temp.path(),
            serde_json::json!([{
                "session_id":"context-schema", "expect_output_schema":true,
                "final_message":reply, "exit_code":0
            }]),
        );
        let turn = spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Completed,
            &scenario,
        );
        let schema = serde_json::json!({"type":"object","required":["response"],
            "properties":{"response":{"type":"object"}}});
        let result = CodexProvider::with_exe(cm_exe())
            .run_step_with_schema(&turn, &cancel_flag(), &mut |_| {}, Some(schema.clone()))
            .unwrap();
        assert!(matches!(result.outcome, StepOutcome::Completed { summary } if summary == reply));
        let written: serde_json::Value = serde_json::from_slice(
            &std::fs::read(turn.work_dir.join("context-output-schema.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written, schema);
    }

    #[test]
    fn run_step_review_parses_the_schema_constrained_result() {
        let temp = tempfile::TempDir::new().unwrap();
        let review = serde_json::json!({
            "verdict": "changes_requested",
            "summary": "one blocking problem",
            "findings": [{"severity": "blocking", "path": "src/a.rs", "line": 7, "text": "off by one"}]
        });
        let scenario = write_scenario(
            temp.path(),
            serde_json::json!([{
                "session_id": "sess-r",
                "expect_output_schema": true,
                "final_message": serde_json::to_string(&review).unwrap(),
                "exit_code": 0
            }]),
        );
        let (result, _) = run(&spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Review,
            &scenario,
        ));
        let result = result.unwrap();
        let StepOutcome::Review {
            verdict, findings, ..
        } = result.outcome
        else {
            panic!("expected a review outcome");
        };
        assert_eq!(verdict, ReviewVerdict::ChangesRequested);
        assert_eq!(findings.len(), 1);
        // The adapter wrote the schema side file into the work dir.
        assert!(temp
            .path()
            .join("agent-run/review-output-schema.json")
            .is_file());

        // A malformed review payload fails the step without advancing.
        let scenario = write_scenario_named(
            temp.path(),
            "malformed",
            serde_json::json!([{
                "final_message": "this is not the review json",
                "exit_code": 0
            }]),
        );
        let (result, _) = run(&spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Review,
            &scenario,
        ));
        assert!(matches!(
            result.unwrap_err(),
            ProviderError::MalformedResult { reason } if reason.contains("not the review result JSON")
        ));
    }

    #[test]
    fn run_step_initial_request_parses_the_schema_constrained_decision() {
        let temp = tempfile::TempDir::new().unwrap();
        let decision = serde_json::json!({
            "action": "answer",
            "response": "This can be answered without creating a workflow."
        });
        let scenario = write_scenario(
            temp.path(),
            serde_json::json!([{
                "session_id": "sess-route",
                "expect_output_schema": true,
                "expect_sandbox": "read-only",
                "final_message": serde_json::to_string(&decision).unwrap(),
                "exit_code": 0
            }]),
        );
        let (result, _) = run(&spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::InitialRequest,
            &scenario,
        ));
        assert_eq!(
            result.unwrap().outcome,
            StepOutcome::InitialRequest {
                action: InitialRequestAction::Answer,
                response: "This can be answered without creating a workflow.".to_string(),
            }
        );
        assert!(temp
            .path()
            .join("agent-run/initial-request-output-schema.json")
            .is_file());
        assert!(!temp
            .path()
            .join("agent-run/review-output-schema.json")
            .exists());
    }

    #[test]
    fn run_step_nonzero_exit_carries_a_bounded_stderr_tail() {
        let temp = tempfile::TempDir::new().unwrap();
        let long = "e".repeat(MAX_STDERR_TAIL_CHARS + 500);
        let scenario = write_scenario(
            temp.path(),
            serde_json::json!([{
                "stderr": long,
                "exit_code": 7
            }]),
        );
        let (result, _) = run(&spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Completed,
            &scenario,
        ));
        match result.unwrap_err() {
            ProviderError::Exit {
                code, stderr_tail, ..
            } => {
                assert_eq!(code, Some(7));
                assert_eq!(stderr_tail.chars().count(), MAX_STDERR_TAIL_CHARS);
            }
            other => panic!("expected an exit failure, got {other:?}"),
        }
    }

    #[test]
    fn run_step_spawn_failure_is_actionable() {
        let temp = tempfile::TempDir::new().unwrap();
        let provider = CodexProvider::with_exe(temp.path().join("no-such-codex"));
        let cancel = cancel_flag();
        let spec = StepSpec {
            prompt: "x".to_string(),
            cwd: temp.path().to_path_buf(),
            session: SessionRequest::Fresh,
            model: None,
            reasoning_effort: None,
            result: StepResultKind::Completed,
            access: ProviderAccess::WorkspaceWrite,
            native_tools: true,
            limits: ProviderExecutionLimits::default(),
            work_dir: temp.path().to_path_buf(),
            env: Vec::new(),
        };
        match provider.run_step(&spec, &cancel, &mut |_| {}) {
            Err(ProviderError::Spawn { detail }) => {
                assert!(detail.contains("was not found"), "{detail}");
                assert!(detail.contains(CM_CODEX_EXE_ENV), "{detail}");
            }
            other => panic!("expected a spawn failure, got {other:?}"),
        }
    }

    #[test]
    fn kimi_run_step_resumes_selected_model_and_parses_review() {
        let temp = tempfile::TempDir::new().unwrap();
        let scenario = write_scenario_named(
            temp.path(),
            "kimi-scenario",
            serde_json::json!([
                {
                    "session_id": "session-k1",
                    "expect_model": "kimi-code/test",
                    "expect_prompt_contains": ["bounded context"],
                    "events": [{
                        "role": "assistant",
                        "content": [],
                        "tool_calls": [{"function": {"name": "Shell", "arguments": "cargo test"}}]
                    }],
                    "final_message": "implemented with Kimi"
                },
                {
                    "session_id": "session-k1",
                    "expect_resume_session": "session-k1",
                    "expect_model": "kimi-code/test",
                    "expect_plan": true,
                    "expect_prompt_contains": ["final assistant message MUST contain only one JSON object"],
                    "final_message": r#"{"verdict":"approved","summary":"clean","findings":[]}"#
                }
            ]),
        );
        let provider = KimiProvider::with_exe(cm_exe());
        let cancel = cancel_flag();
        let mut events = Vec::new();
        let mut first = StepSpec {
            prompt: "bounded context".to_string(),
            cwd: temp.path().to_path_buf(),
            session: SessionRequest::Fresh,
            model: Some("kimi-code/test".to_string()),
            reasoning_effort: None,
            result: StepResultKind::Completed,
            access: ProviderAccess::WorkspaceWrite,
            native_tools: true,
            limits: ProviderExecutionLimits::default(),
            work_dir: temp.path().join("agent-run"),
            env: vec![("CM_FAKE_KIMI_SCENARIO".to_string(), scenario.clone())],
        };
        let completed = provider
            .run_step(&first, &cancel, &mut |event| events.push(event.clone()))
            .unwrap();
        assert_eq!(completed.session_id.as_deref(), Some("session-k1"));
        assert!(matches!(
            completed.outcome,
            StepOutcome::Completed { ref summary } if summary == "implemented with Kimi"
        ));
        assert!(events
            .iter()
            .any(|event| event.kind == ProviderEventKind::Command));

        first.session = SessionRequest::Resume("session-k1".to_string());
        first.result = StepResultKind::Review;
        first.access = ProviderAccess::ReadOnly;
        let reviewed = provider.run_step(&first, &cancel, &mut |_| {}).unwrap();
        assert!(matches!(
            reviewed.outcome,
            StepOutcome::Review {
                verdict: ReviewVerdict::Approved,
                ..
            }
        ));

        first.result = StepResultKind::InitialRequest;
        assert!(matches!(
            provider.run_step(&first, &cancel, &mut |_| {}),
            Err(ProviderError::MalformedResult { .. })
        ));
    }

    #[test]
    fn kimi_run_step_keeps_a_final_message_that_rides_tool_calls() {
        let temp = tempfile::TempDir::new().unwrap();
        // The run's only assistant text arrives on a turn that also carries
        // tool calls; it is still the final message.
        let scenario = write_scenario_named(
            temp.path(),
            "kimi-scenario",
            serde_json::json!([{
                "session_id": "session-k2",
                "events": [{
                    "role": "assistant",
                    "content": [{"type": "text", "text": "implemented with Kimi"}],
                    "tool_calls": [{"function": {"name": "Shell", "arguments": "cargo test"}}]
                }]
            }]),
        );
        let provider = KimiProvider::with_exe(cm_exe());
        let cancel = cancel_flag();
        let spec = StepSpec {
            prompt: "bounded context".to_string(),
            cwd: temp.path().to_path_buf(),
            session: SessionRequest::Fresh,
            model: None,
            reasoning_effort: None,
            result: StepResultKind::Completed,
            access: ProviderAccess::WorkspaceWrite,
            native_tools: true,
            limits: ProviderExecutionLimits::default(),
            work_dir: temp.path().join("agent-run"),
            env: vec![("CM_FAKE_KIMI_SCENARIO".to_string(), scenario)],
        };
        let completed = provider.run_step(&spec, &cancel, &mut |_| {}).unwrap();
        assert!(matches!(
            completed.outcome,
            StepOutcome::Completed { ref summary } if summary == "implemented with Kimi"
        ));
    }

    #[test]
    fn kimi_review_result_tolerates_one_whole_payload_code_fence() {
        // Exactly one outer fence strips; a bare payload and leading
        // commentary pass through untouched.
        let fenced =
            "```json\n{\"verdict\":\"approved\",\"summary\":\"clean\",\"findings\":[]}\n```";
        assert!(matches!(
            parse_review_result(kimi_unfenced_result_text(fenced)),
            Ok(StepOutcome::Review {
                verdict: ReviewVerdict::Approved,
                ..
            })
        ));
        let bare = "{\"verdict\":\"approved\"}";
        assert_eq!(kimi_unfenced_result_text(bare), bare);
        let commentary = "Here is the result:\n```json\n{}\n```";
        assert_eq!(kimi_unfenced_result_text(commentary), commentary);

        // End-to-end through the fake stream.
        let temp = tempfile::TempDir::new().unwrap();
        let scenario = write_scenario_named(
            temp.path(),
            "kimi-scenario",
            serde_json::json!([{
                "session_id": "session-k3",
                "expect_plan": true,
                "final_message": fenced
            }]),
        );
        let provider = KimiProvider::with_exe(cm_exe());
        let cancel = cancel_flag();
        let spec = StepSpec {
            prompt: "review".to_string(),
            cwd: temp.path().to_path_buf(),
            session: SessionRequest::Fresh,
            model: None,
            reasoning_effort: None,
            result: StepResultKind::Review,
            access: ProviderAccess::ReadOnly,
            native_tools: true,
            limits: ProviderExecutionLimits::default(),
            work_dir: temp.path().join("agent-run"),
            env: vec![("CM_FAKE_KIMI_SCENARIO".to_string(), scenario)],
        };
        let reviewed = provider.run_step(&spec, &cancel, &mut |_| {}).unwrap();
        assert!(matches!(
            reviewed.outcome,
            StepOutcome::Review {
                verdict: ReviewVerdict::Approved,
                ..
            }
        ));
    }

    #[test]
    fn run_step_cancellation_interrupts_within_the_grace() {
        let temp = tempfile::TempDir::new().unwrap();
        let scenario = write_scenario(
            temp.path(),
            serde_json::json!([{
                "delay_ms": 30000,
                "session_id": "sess-slow",
                "final_message": "never",
                "exit_code": 0
            }]),
        );
        let provider = CodexProvider::with_exe(cm_exe());
        let cancel = cancel_flag();
        let trigger = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            trigger.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let spec = spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Completed,
            &scenario,
        );
        let result = provider.run_step(&spec, &cancel, &mut |_| {});
        let elapsed = started.elapsed();
        assert_eq!(result.unwrap_err(), ProviderError::Interrupted);
        assert!(
            elapsed < Duration::from_secs(15),
            "cancellation took {elapsed:?} (grace is {CANCEL_GRACE:?})"
        );
    }

    #[test]
    fn fake_codex_scripts_multiple_calls_via_the_state_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let scenario = write_scenario(
            temp.path(),
            serde_json::json!([
                {"session_id": "sess-1", "final_message": "first", "exit_code": 0},
                {"session_id": "sess-1", "final_message": "second", "exit_code": 0}
            ]),
        );
        let (first, _) = run(&spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Completed,
            &scenario,
        ));
        assert_eq!(
            first.unwrap().outcome,
            StepOutcome::Completed {
                summary: "first".to_string()
            }
        );
        let (second, _) = run(&spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Completed,
            &scenario,
        ));
        assert_eq!(
            second.unwrap().outcome,
            StepOutcome::Completed {
                summary: "second".to_string()
            }
        );
        // The script is exhausted: the fake exits 5.
        let (third, _) = run(&spec(
            temp.path(),
            SessionRequest::Fresh,
            StepResultKind::Completed,
            &scenario,
        ));
        assert!(matches!(
            third.unwrap_err(),
            ProviderError::Exit { code: Some(5), .. }
        ));
    }

    #[test]
    fn claude_argv_and_stream_parser_pin_model_permissions_and_session() {
        assert_eq!(
            claude_argv(
                &SessionRequest::Fresh,
                Some("sonnet"),
                ProviderAccess::ReadOnly
            ),
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "sonnet",
                "--permission-mode",
                "plan"
            ]
        );
        let resumed = claude_argv(
            &SessionRequest::Resume("claude-session".to_string()),
            None,
            ProviderAccess::WorkspaceWrite,
        );
        assert!(resumed
            .windows(2)
            .any(|pair| pair == ["--resume", "claude-session"]));
        // Print mode cannot prompt: write steps auto-accept edits, never
        // bypass permissions wholesale.
        assert!(resumed
            .windows(2)
            .any(|pair| pair == ["--permission-mode", "acceptEdits"]));
        assert!(!resumed.iter().any(|arg| arg.contains("dangerously")));

        let mut parser = ClaudeStreamParser::default();
        let init = parser
            .on_line(r#"{"type":"system","subtype":"init","session_id":"claude-session"}"#)
            .unwrap();
        assert_eq!(init.kind, ProviderEventKind::SessionStarted);
        parser
            .on_line(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            )
            .unwrap();
        // Text blocks win over thinking blocks, so thinking never leaks into
        // a step summary; a thinking-only message still yields its text.
        let event = parser
            .on_line(
                r#"{"type":"assistant","message":{"content":[{"type":"thinking","text":"hmm"},{"type":"text","text":"real answer"}]}}"#,
            )
            .unwrap();
        assert_eq!(event.text, "real answer");
        let event = parser
            .on_line(
                r#"{"type":"assistant","message":{"content":[{"type":"thinking","text":"only thinking"}]}}"#,
            )
            .unwrap();
        assert_eq!(event.text, "only thinking");
        parser
            .on_line(
                r#"{"type":"result","is_error":false,"result":"done","session_id":"claude-session"}"#,
            )
            .unwrap();
        assert!(parser.saw_result);
        assert_eq!(parser.session_id.as_deref(), Some("claude-session"));
        assert_eq!(parser.final_message.as_deref(), Some("done"));
        assert!(parser.protocol_error.is_none());
    }

    #[test]
    fn custom_jsonl_protocol_is_versioned_strict_and_keeps_terminal_result() {
        let request = JsonlProviderRequest {
            schema: CUSTOM_PROVIDER_REQUEST_SCHEMA,
            prompt: "continue",
            cwd: "C:\\project".to_string(),
            session: JsonlSessionRequest::Resume {
                id: "custom-session",
            },
            model: Some("local-model"),
            result: "interactive",
            access: "workspace_write",
        };
        let request = serde_json::to_value(request).unwrap();
        assert_eq!(request["schema"], CUSTOM_PROVIDER_REQUEST_SCHEMA);
        assert_eq!(request["session"]["mode"], "resume");
        assert_eq!(request["session"]["id"], "custom-session");
        assert_eq!(request["model"], "local-model");
        assert_eq!(request["result"], "interactive");
        assert_eq!(request["access"], "workspace_write");

        let mut parser = JsonlStreamParser::default();
        parser
            .on_line(
                r#"{"schema":"climemory/provider-event-1","type":"session","id":"custom-session"}"#,
            )
            .unwrap();
        parser
            .on_line(r#"{"schema":"climemory/provider-event-1","type":"message","text":"working"}"#)
            .unwrap();
        parser
            .on_line(r#"{"schema":"climemory/provider-event-1","type":"result","text":"finished"}"#)
            .unwrap();
        assert_eq!(parser.session_id.as_deref(), Some("custom-session"));
        assert_eq!(parser.final_message.as_deref(), Some("finished"));
        assert!(parser.saw_result);
        assert!(parser.protocol_error.is_none());

        parser
            .on_line(r#"{"schema":"climemory/provider-event-1","type":"message","text":"late"}"#)
            .unwrap();
        assert_eq!(
            parser.protocol_error.as_deref(),
            Some("received a record after the terminal result")
        );
    }

    #[test]
    fn interactive_result_requires_a_consistent_status_payload() {
        assert_eq!(
            parse_interactive_result(
                r#"{"status":"needs_input","summary":"Need one value","question":"Which target?"}"#
            )
            .unwrap(),
            StepOutcome::NeedsInput {
                question: "Which target?".to_string(),
                summary: Some("Need one value".to_string())
            }
        );
        assert_eq!(
            parse_interactive_result(
                r#"{"status":"completed","summary":"Implemented","question":null}"#
            )
            .unwrap(),
            StepOutcome::Completed {
                summary: "Implemented".to_string()
            }
        );
        assert!(parse_interactive_result(
            r#"{"status":"needs_input","summary":null,"question":""}"#
        )
        .is_err());
    }
}
