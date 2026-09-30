//! Thread-owned advisory memory and durable conversations with the primary model.
//! Bindings use stable thread IDs; provider sessions are disposable, dialogue is not.
mod lifecycle;
mod retrieval;
pub(crate) use lifecycle::create_memory;
mod document_blocks;
mod document_routing;
mod documents;
mod issue_scope;
mod preparation;
mod requirements;
mod runtime;
mod verification;
pub(crate) use retrieval::{note_limit, route, select};
pub(crate) use runtime::{response_schema, PROMPT};

use crate::cli::Parsed;
use crate::project::Project;
use crate::util::{atomic_write, fresh_id, iso_now, AppError, FileLock, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PROTOCOL: &str = "climemory/thread-dialogue-1";
const BINDING_FORMAT: &str = "climemory/thread-agent-2";
const LEGACY_BINDING_FORMAT: &str = "climemory/thread-agent-1";
const MAX_MEMORY: usize = 8000;
const COMPACT_MEMORY_LIMIT: usize = 1200;
const MAX_MESSAGE: usize = 16000;
const MAX_DEPTH: usize = 8;

/// Called under the dialogue and source locks by the session importer.
/// IDs belong to its own namespace; replaying a prepared commit is idempotent.
pub(crate) fn persist_session_binding(
    project: &Project,
    id: &str,
    parent: Option<String>,
    memory: String,
    revision: u64,
    updated: String,
) -> Result<()> {
    let profile = project
        .config
        .memory
        .chat_agent
        .as_ref()
        .unwrap_or(&project.config.memory.documents_agent);
    if !project.config.agent.profiles.contains_key(profile) || memory.chars().count() > MAX_MEMORY {
        return Err(AppError::new("invalid session memory binding"));
    }
    let binding = Binding {
        format: BINDING_FORMAT.into(),
        thread_id: id.into(),
        agent: Some(profile.clone()),
        provider: None,
        model: None,
        parent,
        memory,
        revision,
        last_dialogue: None,
        updated,
    };
    write_json(&binding_path(project, id)?, &binding)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    format: String,
    thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    // Read old bindings until an explicit bind --agent migrates them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    parent: Option<String>,
    memory: String,
    revision: u64,
    last_dialogue: Option<String>,
    updated: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    thread_id: String,
    request: String,
    history: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    document_scan: Option<documents::DocumentScan>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dialogue {
    #[serde(default)]
    read_only: bool,
    format: String,
    id: String,
    thread_id: String,
    task: String,
    phase: String,
    status: String,
    frames: Vec<Frame>,
    context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    document_requirements: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preparation: Option<Value>,
    question: Option<String>,
    report: Option<String>,
    error: Option<String>,
    pending_memory: Option<String>,
    pending_revision: Option<u64>,
    steps: usize,
    updated: String,
    events: Vec<Value>,
}

impl Dialogue {
    // Events survive popped parent frames and retries. Exclude thread identity so
    // equivalent document questions can still share a cache across owners.
    fn primary_clarifications(&self) -> Vec<Value> {
        let mut question = Value::Null;
        let mut answers = Vec::new();
        for event in &self.events {
            if event["event"] == "question" {
                question = event["text"].clone();
            } else if event["event"] == "reply" {
                answers
                    .push(json!({"speaker":"primary","question":question,"answer":event["text"]}));
                question = Value::Null;
            }
        }
        answers
    }
}

fn validate_id(id: &str) -> Result<()> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(AppError::new("invalid thread agent identity"));
    }
    Ok(())
}

fn validate_session(id: &str) -> Result<()> {
    validate_id(
        id.strip_prefix("ta-")
            .ok_or_else(|| AppError::new("invalid dialogue session"))?,
    )
}

fn directory(project: &Project, relative: &str) -> Result<PathBuf> {
    let root = fs::canonicalize(&project.data)?;
    let mut path = root.clone();
    for component in relative.split('/') {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta)
                if meta.file_type().is_symlink()
                    || !meta.is_dir()
                    || !fs::canonicalize(&path)?.starts_with(&root) =>
            {
                return Err(AppError::new(
                    "thread agent storage must stay inside project memory",
                ))
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    }
    Ok(path)
}

fn checked_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
            return Err(AppError::new("thread agent storage requires regular files"))
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    Ok(())
}

fn binding_path(project: &Project, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    let path = directory(project, "thread-agents")?.join(format!("{id}.json"));
    checked_file(&path)?;
    Ok(path)
}

fn session_path(project: &Project, id: &str) -> Result<PathBuf> {
    validate_session(id)?;
    let path = directory(project, "agent-runs/thread-dialogues")?.join(format!("{id}.json"));
    checked_file(&path)?;
    Ok(path)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&read_state_bytes(path)?)?)
}

fn read_state_bytes(path: &Path) -> Result<Vec<u8>> {
    checked_file(path)?;
    if fs::metadata(path)?.len() > 2_000_000 {
        return Err(AppError::new("thread agent state exceeds 2 MB"));
    }
    Ok(fs::read(path)?)
}

fn load_binding(project: &Project, id: &str) -> Result<Binding> {
    Ok(load_binding_snapshot(project, id)?.0)
}

fn load_binding_snapshot(project: &Project, id: &str) -> Result<(Binding, Vec<u8>)> {
    let raw = read_state_bytes(&binding_path(project, id)?)?;
    let b: Binding = serde_json::from_slice(&raw)?;
    let valid_layout = match b.format.as_str() {
        BINDING_FORMAT => b.agent.is_some() && b.provider.is_none() && b.model.is_none(),
        LEGACY_BINDING_FORMAT => b.agent.is_none() && b.provider.is_some(),
        _ => false,
    };
    if !valid_layout || b.thread_id != id || b.memory.chars().count() > MAX_MEMORY {
        return Err(AppError::new("invalid thread agent binding"));
    }
    if let Some(parent) = &b.parent {
        validate_id(parent)?;
    }
    Ok((b, raw))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    checked_file(path)?;
    fs::create_dir_all(path.parent().unwrap())?;
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() > 2_000_000 {
        return Err(AppError::new("thread agent state exceeds 2 MB"));
    }
    atomic_write(path, &bytes)
}

fn save(project: &Project, d: &mut Dialogue) -> Result<()> {
    d.updated = iso_now();
    write_json(&session_path(project, &d.id)?, d)
}

fn load_session(project: &Project, id: &str) -> Result<Dialogue> {
    let d: Dialogue = read_json(&session_path(project, id)?)?;
    if d.format != PROTOCOL || d.id != id || d.frames.is_empty() || d.frames.len() > MAX_DEPTH {
        return Err(AppError::new("invalid thread dialogue"));
    }
    validate_id(&d.thread_id)?;
    for frame in &d.frames {
        validate_id(&frame.thread_id)?;
    }
    Ok(d)
}

fn bounded(text: &str, max: usize, label: &str) -> Result<String> {
    if text.trim().is_empty() || text.chars().count() > max || text.contains('\0') {
        return Err(AppError::new(format!(
            "{label} must contain 1..{max} characters"
        )));
    }
    Ok(text.trim().into())
}

fn message(parsed: &Parsed) -> Result<String> {
    let value = match (parsed.arg(3), parsed.value("with-file")) {
        (Some(text), None) => text.to_string(),
        (None, Some(path)) => crate::util::decode_body_bytes(&fs::read(path)?, path)?,
        _ => {
            return Err(AppError::new(
                "provide one message argument or --with-file PATH",
            ))
        }
    };
    bounded(&value, MAX_MESSAGE, "message")
}

fn argv(project: &Project, action: &str, identity: &str) -> Value {
    json!([
        if action == "status" || action == "get" {
            "read"
        } else {
            action
        },
        identity,
        "--dir",
        project.root
    ])
}

fn dialogue_metrics(d: &Dialogue) -> Value {
    let calls: Vec<_> = d
        .events
        .iter()
        .filter(|e| e["event"] == "model_call")
        .collect();
    let mut phases = std::collections::BTreeMap::<String, usize>::new();
    for call in &calls {
        *phases
            .entry(call["phase"].as_str().unwrap_or("unknown").into())
            .or_default() += 1;
    }
    json!({"scope":"entire_dialogue","provider_usage":crate::usage::summarize(&d.events),"model_attempts":d.steps,"instrumented_attempts":calls.len(),
        "model_elapsed_ms":calls.iter().filter_map(|e|e["elapsed_ms"].as_u64()).sum::<u64>(),
        "failed_attempts":calls.iter().filter(|e|e["status"] == "error").count(),"calls_by_phase":phases,
        "automatic_retries":d.events.iter().filter(|e|e["event"] == "automatic_retry").count(),
        "cache_reuses":d.events.iter().filter(|e|e["event"] == "document_cache_reused").count()})
}

fn continuation(d: &Dialogue) -> Value {
    if d.status != "error" {
        return Value::Null;
    }
    let events: Vec<_> = d
        .events
        .iter()
        .rev()
        .take_while(|e| e["event"] != "command_started")
        .collect();
    let reason = events
        .iter()
        .find(|e| e["event"] == "continuation_needed")
        .and_then(|e| e["reason"].as_str())
        .unwrap_or("response_or_state_error");
    let checkpoint = events.iter().find(|e| e["event"] == "work_checkpoint");
    if reason == "extraction_repair_exhausted" {
        return json!({"reason":reason,"checkpoint":checkpoint,"action":"cancel",
            "requires_change":true,
            "guidance":"Follow next_argv to cancel this dialogue. Then narrow the document question or change the document agent profile and start a new ask. An unchanged retry cannot repair the exhausted extraction."});
    }
    json!({"reason":reason,"checkpoint":checkpoint,"action":"retry",
        "guidance":"Inspect the error, then follow next_argv. Retry resumes saved work with a fresh command budget; it does not restart completed document chunks. Total future stages are not predicted."})
}

fn dialogue_record(project: &Project, d: &Dialogue) -> Value {
    let action = match d.status.as_str() {
        "question" => "reply",
        "awaiting_report" => "report",
        "error" if continuation(d)["action"] == "cancel" => "cancel",
        "error" | "running" => "retry",
        _ => "status",
    };
    json!({"record":"thread_dialogue", "session":d.id, "thread_id":d.thread_id,
        "status":d.status, "phase":d.phase, "task":d.task, "context":d.prepared_context.as_ref().or(d.context.as_ref()),"document_requirements":if d.prepared_context.is_some() { None } else { d.document_requirements.as_ref().map(documents::public_packet) },"preparation":d.preparation,
        "question":d.question, "speaking_thread":d.frames.last().map(|f| &f.thread_id),
        "read_only": d.read_only,
        "report_required": !d.read_only && !matches!(d.status.as_str(), "complete" | "cancelled"),
        "error":d.error, "continuation":continuation(d), "metrics":dialogue_metrics(d), "steps":d.steps, "updated":d.updated,
        "next_argv":argv(project, action, &d.id), "authority":"advisory"})
}

fn memory_record(project: &Project, b: &Binding) -> Result<Value> {
    let thread = project.resolve_thread(&b.thread_id)?;
    Ok(json!({"thread_id":b.thread_id, "slug":thread.meta.slug,
        "title":thread.meta.title,"summary":thread.meta.summary,
        "area":thread.meta.area,"tags":thread.meta.tags,
        "agent":b.agent,"parent":b.parent,"memory":b.memory,"authority":"advisory",
        "archive_handle":(!thread.historical.is_empty()).then(||format!("archive:{}",b.thread_id))}))
}

fn binding_record(project: &Project, b: &Binding) -> Result<Value> {
    let mut record = memory_record(project, b)?;
    let resolved = resolve_settings(&project.config.agent, b);
    let settings = resolved.as_ref().ok();
    let diagnostics = json!({"record":"thread_agent",
        "provider":settings.map(|s|&s.provider),"model":settings.and_then(|s|s.model.as_ref()),
        "reasoning_effort":settings.and_then(|s|s.reasoning_effort),
        "legacy_binding":b.format==LEGACY_BINDING_FORMAT,"configuration_error":resolved.as_ref().err().map(|e|&e.msg),
        "revision":b.revision,"last_dialogue":b.last_dialogue,"updated":b.updated,
        "ask_argv":argv(project,"ask",record["slug"].as_str().unwrap())});
    record
        .as_object_mut()
        .unwrap()
        .extend(diagnostics.as_object().unwrap().clone());
    Ok(record)
}

fn resolve_settings(
    config: &crate::config::AgentConfig,
    binding: &Binding,
) -> Result<crate::config::AgentProfile> {
    let settings = if let Some(name) = &binding.agent {
        config.profiles.get(name).cloned().ok_or_else(|| AppError::new(format!(
            "thread agent profile '{name}' is missing from memory/config.json agent.profiles; restore it or rebind with --agent")))?
    } else {
        crate::config::AgentProfile {
            provider: binding
                .provider
                .clone()
                .ok_or_else(|| AppError::new("legacy binding is missing provider"))?,
            model: binding.model.clone(),
            reasoning_effort: None,
        }
    };
    if config.provider_adapter(&settings.provider).is_none() {
        return Err(AppError::new(
            "bound agent provider is no longer configured",
        ));
    }
    if settings.model.as_ref().is_none_or(|model| {
        model.trim().is_empty()
            || model != model.trim()
            || model.chars().count() > 200
            || model.chars().any(char::is_control)
    }) {
        return Err(AppError::new(
            "thread agent requires an explicit model; configure agent.profiles.<name>.model and rebind legacy bindings with --agent",
        ));
    }
    Ok(settings)
}

pub fn list(project: &Project) -> Result<Vec<Value>> {
    binding_ids(project)?
        .iter()
        .map(|id| binding_record(project, &load_binding(project, id)?))
        .collect()
}

/// Read-only source inventory for the unified derived index.
pub(crate) fn source_inventory(project: &Project) -> Result<Vec<Value>> {
    binding_ids(project)?
        .iter()
        .map(|id| {
            let (binding, raw) = load_binding_snapshot(project, id)?;
            let mut record = memory_record(project, &binding)?;
            record["revision"] = json!(crate::util::digest(&raw));
            record["path"] = json!(format!("memory/thread-agents/{id}.json"));
            Ok(record)
        })
        .collect()
}

fn list_memories(
    project: &Project,
    mut snapshots: Option<&mut std::collections::BTreeMap<String, Vec<u8>>>,
) -> Result<Vec<Value>> {
    binding_ids(project)?
        .iter()
        .map(|id| {
            let (binding, raw) = load_binding_snapshot(project, id)?;
            let record = memory_record(project, &binding)?;
            // Context retains bytes until selection; discovery needs only the record.
            if let Some(snapshots) = snapshots.as_mut() {
                snapshots.insert(binding.thread_id, raw);
            }
            Ok(record)
        })
        .collect()
}

fn binding_ids(project: &Project) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let path = directory(project, "thread-agents")?;
    if path.exists() {
        let mut paths = fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        for path in paths {
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            result.push(id.to_owned());
        }
    }
    Ok(result)
}

pub fn pending(project: &Project) -> Result<Vec<Value>> {
    pending_records(project, usize::MAX, |d| dialogue_record(project, d))
        .map(|(_, records)| records)
}

/// Validate every session, but materialize only the requested pending projection.
fn pending_records(
    project: &Project,
    limit: usize,
    project_record: impl Fn(&Dialogue) -> Value,
) -> Result<(usize, Vec<Value>)> {
    let path = directory(project, "agent-runs/thread-dialogues")?;
    let mut result = Vec::new();
    let mut count = 0;
    if path.exists() {
        let mut paths = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        for path in paths {
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let d = load_session(project, id)?;
            if !matches!(d.status.as_str(), "complete" | "cancelled")
                && (project.config.memory.mode != crate::config::MemoryMode::ReadOnly
                    || d.read_only)
            {
                count += 1;
                if result.len() < limit {
                    result.push(project_record(&d));
                }
            }
        }
    }
    Ok((count, result))
}

/// Small read-only discovery packet. Full memory/history stays behind read handles.
pub fn context_hint(
    project: &Project,
    bindings: &[Value],
    filters: &std::collections::BTreeMap<String, String>,
) -> Option<Value> {
    let pending = pending_records(project, 8, |d| {
        json!({
            "session":d.id,"thread_id":d.thread_id,"status":d.status,
            "task":d.task.chars().take(160).collect::<String>(),
            "status_argv":argv(project,"status",&d.id)
        })
    });
    let (pending_count, brief_pending, pending_error) = match pending {
        Ok((count, records)) => (Some(count), records, None),
        Err(error) => (None, Vec::new(), Some(error.msg)),
    };
    if bindings.is_empty() && pending_count == Some(0) {
        return None;
    }
    let limit = note_limit(filters);
    let brief_bindings = bindings.iter().take(limit).map(|b|json!({
        "thread_id":b["thread_id"],"slug":b["slug"],"agent":b["agent"],"parent":b["parent"],"ask_argv":argv(project,"ask",b["slug"].as_str().unwrap()),"selection_reason":b["selection_reason"]
    })).collect::<Vec<_>>();
    let mut hint = json!({"bindings":brief_bindings,"pending":brief_pending,
        "bindings_omitted":bindings.len().saturating_sub(limit),"pending_omitted":pending_count.map(|count| count.saturating_sub(8)),

        "pending_argv":["pending","--dir",project.root],
        "guidance":"Ask the relevant bound agent for context. Answer its clarifications from known facts or ask the user. Report completed work to the same session, or cancel it."});
    if project.config.memory.mode == crate::config::MemoryMode::ReadOnly {
        hint["guidance"] = json!("Ask relevant agents and answer clarifications. Context delivery completes a read-only consultation; no report is required.");
    }
    if let Some(error) = pending_error {
        hint["pending_error"] = json!(error);
        hint["pending_complete"] = json!(false);
    }
    Some(hint)
}

/// The current thread counts toward the depth limit, even before it is saved.
fn validate_parent_chain(project: &Project, thread_id: &str, parent: Option<&str>) -> Result<()> {
    let mut seen = BTreeSet::from([thread_id.to_owned()]);
    let mut cursor = parent.map(str::to_owned);
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) || seen.len() > MAX_DEPTH {
            return Err(AppError::new(format!(
                "thread agent parent cycle or depth exceeds {MAX_DEPTH}"
            )));
        }
        cursor = load_binding(project, &id)?.parent;
    }
    Ok(())
}

fn bind(parsed: &Parsed, project: &Project, identity: &str) -> Result<Value> {
    let doc = project.resolve_thread(identity)?;
    let path = binding_path(project, &doc.meta.id)?;
    let old = if path.exists() {
        Some(load_binding(project, &doc.meta.id)?)
    } else {
        None
    };
    if old.as_ref().is_some_and(|b| b.agent.is_none()) && parsed.value("agent").is_none() {
        return Err(AppError::new(
            "legacy binding: select --agent PROFILE to migrate without losing memory",
        ));
    }
    let agent = parsed
        .value("agent")
        .map(str::to_owned)
        .or_else(|| old.as_ref().and_then(|b| b.agent.clone()))
        .unwrap_or_else(|| "agent_medium".into());
    if !project.config.agent.profiles.contains_key(&agent) {
        return Err(AppError::new(format!(
            "unknown agent profile '{agent}'; configure it in memory/config.json agent.profiles"
        )));
    }
    let parent = match parsed.value("parent") {
        Some("none") => None,
        Some(name) => Some(project.resolve_thread(name)?.meta.id),
        None => old.as_ref().and_then(|b| b.parent.clone()),
    };
    validate_parent_chain(project, &doc.meta.id, parent.as_deref())?;
    let mut b = old.unwrap_or(Binding {
        format: BINDING_FORMAT.into(),
        thread_id: doc.meta.id,
        agent: Some(agent.clone()),
        provider: None,
        model: None,
        parent: None,
        memory: String::new(),
        revision: 0,
        last_dialogue: None,
        updated: iso_now(),
    });
    if b.revision > 0 && b.agent.as_ref() == Some(&agent) && b.parent == parent {
        return binding_record(project, &b);
    }
    b.format = BINDING_FORMAT.into();
    b.agent = Some(agent);
    b.provider = None;
    b.model = None;
    b.parent = parent;
    b.revision += 1;
    b.updated = iso_now();
    write_json(&path, &b)?;
    binding_record(project, &b)
}

pub fn run(parsed: &Parsed, project: &Project) -> Result<()> {
    let action = parsed.arg(1).unwrap_or("help");
    if action == "help" {
        return crate::help::print_focused(
            &["thread", "agent"],
            parsed.value("output"),
            None,
            false,
        );
    }
    let read_only = matches!(action, "list" | "pending" | "status" | "get");
    if !read_only {
        crate::guard_nested_mutation_root(parsed, &project.root)?;
    }
    if parsed.arg(4).is_some()
        || (parsed.arg(3).is_some() && !matches!(action, "ask" | "reply" | "report"))
    {
        return Err(AppError::new("unexpected thread agent positional argument"));
    }
    if matches!(action, "list" | "pending") {
        if parsed.arg(2).is_some() {
            return Err(AppError::new("list/pending takes no identity"));
        }
        let records = if action == "list" {
            list(project)?
        } else {
            pending(project)?
        };
        let mut output =
            vec![json!({"record":"thread_agent_summary","kind":action,"count":records.len()})];
        output.extend(records);
        return crate::output::write_records(&output);
    }
    let identity = parsed
        .arg(2)
        .ok_or_else(|| AppError::new("thread agent requires a thread or session identity"))?;
    if action == "get" {
        let doc = project.resolve_thread(identity)?;
        return crate::output::write_records(&[binding_record(
            project,
            &load_binding(project, &doc.meta.id)?,
        )?]);
    }
    if action == "status" {
        let (id, offset) = match identity.split_once('@') {
            Some((id, raw)) => (
                id,
                raw.parse::<usize>()
                    .map_err(|_| AppError::new("invalid dialogue offset"))?,
            ),
            None => (identity, 0),
        };
        let dialogue = load_session(project, id)?;
        if offset > dialogue.events.len() {
            return Err(AppError::new("dialogue offset exceeds history"));
        }
        let mut records = vec![dialogue_record(project, &dialogue)];
        if dialogue.prepared_context.is_some() {
            records.push(json!({"record":"unprepared_context","session":id,"context":dialogue.context,"document_requirements":dialogue.document_requirements}));
        } else {
            records[0]["document_requirements"] = json!(dialogue.document_requirements);
        }
        let mut next = offset;
        for event in dialogue.events.iter().skip(offset).take(8) {
            records
                .push(json!({"record":"dialogue_event","session":id,"index":next,"event":event}));
            next += 1;
        }
        if next < dialogue.events.len() {
            records[0]["history_next_argv"] = argv(project, "read", &format!("{id}@{next}"));
        }
        records[0]["history_offset"] = json!(offset);
        records[0]["history_total"] = json!(dialogue.events.len());
        return crate::output::write_records(&records);
    }
    let dir = directory(project, "agent-runs/thread-dialogues")?;
    fs::create_dir_all(&dir)?;
    let _lock = FileLock::acquire(&dir.join("execution.lock"), Duration::from_millis(100))?;
    if action == "bind" {
        return crate::output::write_records(&[bind(parsed, project, identity)?]);
    }
    let mut d = if action == "ask" {
        let doc = project.resolve_thread(identity)?;
        load_binding(project, &doc.meta.id)?;
        let task = message(parsed)?;
        Dialogue {
            read_only: project.config.memory.mode == crate::config::MemoryMode::ReadOnly,
            format: PROTOCOL.into(),
            id: format!("ta-{}", fresh_id()),
            thread_id: doc.meta.id.clone(),
            task: task.clone(),
            phase: "context".into(),
            status: "running".into(),
            frames: vec![Frame {
                thread_id: doc.meta.id,
                request: task,
                history: Vec::new(),
                document_scan: None,
            }],
            context: None,
            document_requirements: None,
            question: None,
            prepared_context: None,
            preparation: None,
            report: None,
            error: None,
            pending_memory: None,
            pending_revision: None,
            steps: 0,
            updated: iso_now(),
            events: Vec::new(),
        }
    } else {
        load_session(project, identity)?
    };
    if project.config.memory.mode == crate::config::MemoryMode::ReadOnly && !d.read_only {
        return Err(AppError::new(
            "cannot resume a writable dialogue in read_only mode; start a new ask",
        ));
    }
    if d.read_only && (action == "report" || d.phase != "context" || d.pending_memory.is_some()) {
        return Err(AppError::new(
            "read-only consultations cannot update memory",
        ));
    }
    match action {
        "ask" => {}
        "reply" if d.status == "question" => {
            let text = message(parsed)?;
            d.frames
                .last_mut()
                .unwrap()
                .history
                .push(json!({"speaker":"primary","answer":text}));
            d.events.push(
                json!({"event":"reply","text":text,"thread_id":d.frames.last().unwrap().thread_id}),
            );
            d.question = None;
        }
        "report" => {
            let text = message(parsed)?;
            if d.report.as_ref() == Some(&text) && d.status == "complete" {
                return crate::output::write_records(&[dialogue_record(project, &d)]);
            }
            if d.status != "awaiting_report" {
                return Err(AppError::new("dialogue is not awaiting a result report"));
            }
            d.phase = "report".into();
            d.report = Some(text.clone());
            d.frames[0]
                .history
                .push(json!({"speaker":"primary","result_report":text,"authority":"reported"}));
            d.events.push(json!({"event":"report","text":text}));
        }
        "retry" if matches!(d.status.as_str(), "error" | "running") => {}
        "cancel" if !matches!(d.status.as_str(), "complete" | "cancelled") => {
            if d.pending_memory.is_some() {
                return Err(AppError::new(
                    "report commit is pending; retry to finish it before cancelling",
                ));
            }
            d.status = "cancelled".into();
            d.events.push(json!({"event":"cancelled"}));
            save(project, &mut d)?;
            return crate::output::write_records(&[dialogue_record(project, &d)]);
        }
        _ => {
            return Err(AppError::new(
                "command is not valid for this dialogue state",
            ))
        }
    }
    d.status = "running".into();
    if d.pending_memory.is_none() {
        d.document_requirements = None;
        d.prepared_context = None;
        d.preparation = None;
    }
    d.error = None;
    save(project, &mut d)?;
    if !crate::output::is_capturing() {
        eprintln!("cm: thread agent session {}", d.id);
    }
    d.events.push(json!({"event":"command_started"}));
    if let Err(error) = runtime::drive(project, parsed, &mut d) {
        if let Some(reason) = error
            .details
            .extra
            .as_ref()
            .and_then(|e| e.get("recovery_reason"))
        {
            d.events
                .push(json!({"event":"continuation_needed","reason":reason}));
        }
        d.status = "error".into();
        d.error = Some(error.msg);
    }
    save(project, &mut d)?;
    let record = dialogue_record(project, &d);
    if !crate::output::is_capturing() {
        eprintln!("cm: next_argv {}", record["next_argv"]);
    }
    crate::output::write_records(&[record])
}

#[cfg(test)]
mod rename_tests {
    use super::*;

    #[test]
    fn old_binding_layouts_keep_original_bytes_and_read_as_current_formats() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("memory")).unwrap();
        crate::config::Config::default()
            .save(&dir.path().join("memory/config.json"))
            .unwrap();
        let project = Project::open(dir.path()).unwrap();
        fs::create_dir(project.data.join("thread-agents")).unwrap();
        let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        for (format, agent, provider) in [
            ("climemory/thread-agent-1", None, Some("codex")),
            ("climemory/thread-agent-2", Some("agent_low"), None),
        ] {
            let value = json!({"format":format,"thread_id":id,"agent":agent,"provider":provider,
                "model":null,"parent":null,"memory":"Keep this fact.","revision":4,
                "last_dialogue":null,"updated":"2026-09-30T00:00:00Z"});
            let bytes = serde_json::to_vec(&value).unwrap();
            fs::write(binding_path(&project, id).unwrap(), &bytes).unwrap();
            let (binding, original) = load_binding_snapshot(&project, id).unwrap();
            assert_eq!(binding.format, format);
            assert_eq!(binding.memory, "Keep this fact.");
            assert_eq!(original, bytes);
        }
    }
}
