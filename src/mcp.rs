//! Native stdio MCP transport. Each request owns one ordinary, isolated CM child.
mod install;
pub(crate) use install::{configure, preflight};

use crate::{project::Project, util::*};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, BufRead, Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT: u64 = 1_048_576;
type Pending = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

fn send(value: Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}
fn error(id: Value, code: i32, message: &str) {
    send(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}));
}
fn question(params: &Value) -> Result<&str> {
    if params["name"] != "ask" {
        return Err(AppError::new("unknown tool"));
    }
    let args = params["arguments"]
        .as_object()
        .ok_or_else(|| AppError::new("expected question"))?;
    let text = args
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if args.len() != 1
        || text.is_empty()
        || text.chars().count() > 8000
        || text.contains('\0')
        || text.starts_with('-')
        || matches!(
            text,
            "init" | "help" | "ingest-session" | "hooks" | "feedback"
        )
    {
        return Err(AppError::new(
            "expected a memory question of 1..8000 characters, not a CLI command",
        ));
    }
    Ok(text)
}
fn ask(root: &Path, text: &str, cancel: &AtomicBool) -> Result<Value> {
    let project = Project::open(root)?;
    let dir = project.health.join("mcp");
    Project::checked_path(&project.data, &dir)?;
    fs::create_dir_all(&dir)?;
    let id = fresh_id();
    let stdout = dir.join(format!("{id}.stdout.log"));
    let stderr = dir.join(format!("{id}.stderr.log"));
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg(text)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&stdout)?,
        )
        .stderr(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&stderr)?,
        );
    crate::process::hide_window(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let started = Instant::now();
    let mut child = command.spawn()?;
    let budget = Duration::from_secs(project.config.memory.timeout_seconds.saturating_add(30));
    let result = (|| -> Result<Value> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(AppError::new("MCP request cancelled"));
            }
            if started.elapsed() >= budget {
                return Err(AppError::new("MCP request timed out"));
            }
            if fs::metadata(&stdout)?.len() > MAX_OUTPUT {
                return Err(AppError::new("MCP output limit exceeded"));
            }
            if let Some(status) = child.try_wait()? {
                let mut bytes = Vec::new();
                fs::File::open(&stdout)?
                    .take(MAX_OUTPUT + 1)
                    .read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_OUTPUT {
                    return Err(AppError::new("MCP output limit exceeded"));
                }
                let output = String::from_utf8(bytes)
                    .map_err(|_| AppError::new("CM output is not UTF-8"))?;
                let empty = output.trim().is_empty();
                return Ok(
                    json!({"content":[{"type":"text","text":if empty {format!("CM returned no evidence; inspect {}",stderr.display())}else{output}}],"isError":!status.success() || empty}),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    if !matches!(child.try_wait(), Ok(Some(_))) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        crate::agent_provider::terminate_child(&mut child);
    }
    let receipt = json!({"request_id":id,"elapsed_ms":started.elapsed().as_millis(),
        "cancelled":cancel.load(Ordering::Relaxed),"error":result.as_ref().err().map(|e|&e.msg),
        "is_error":result.as_ref().map(|v|v["isError"].clone()).unwrap_or(json!(true)),
        "stdout_log":stdout,"stderr_log":stderr});
    if let Err(e) = atomic_write(
        &dir.join(format!("{id}.json")),
        &serde_json::to_vec(&receipt)?,
    ) {
        eprintln!("CM MCP receipt write failed: {}", e.msg);
    }
    result
}

pub(crate) fn run() -> Result<()> {
    if [
        "CM_CHAT_INTERNAL",
        "CM_CONTEXT_INTERNAL",
        "CM_DOCS_INTERNAL",
    ]
    .iter()
    .any(|k| std::env::var_os(k).is_some())
    {
        return Err(AppError::new("CM workers cannot start MCP servers"));
    }
    let root = crate::chat::project_root()?;
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let mut workers = Vec::new();
    let result = (|| -> Result<()> {
        let mut input = io::stdin().lock();
        loop {
            let mut line = String::new();
            if input.by_ref().take(65537).read_line(&mut line)? == 0 {
                break;
            }
            if line.len() > 65536 {
                return Err(AppError::new("MCP input limit exceeded"));
            }
            let request: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => {
                    error(Value::Null, -32700, "Invalid JSON");
                    continue;
                }
            };
            let id = request["id"].clone();
            if request["jsonrpc"] != "2.0" {
                error(id, -32600, "Invalid request");
                continue;
            }
            if id.is_null() {
                if request["method"] == "notifications/cancelled" {
                    if let Some(flag) = pending
                        .lock()
                        .unwrap()
                        .get(&request["params"]["requestId"].to_string())
                    {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
                continue;
            }
            if !id.is_string() && !id.is_i64() && !id.is_u64() {
                error(Value::Null, -32600, "Invalid request ID");
                continue;
            }
            let value = match request["method"].as_str() {
                Some("initialize") => {
                    json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"cm","version":crate::build_info::BINARY_VERSION}})
                }
                Some("ping") => json!({}),
                Some("tools/list") => {
                    json!({"tools":[{"name":"ask","description":"Read project memory. Batch related questions; reuse @context:ID. Waits for final evidence; no polling. Reported claims are not independent proof.","inputSchema":{"type":"object","required":["question"],"additionalProperties":false,"properties":{"question":{"type":"string","minLength":1,"maxLength":8000}}},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":true}}]})
                }
                Some("tools/call") => {
                    let text = match question(&request["params"]) {
                        Ok(q) => q.to_owned(),
                        Err(e) => {
                            error(id, -32602, &e.msg);
                            continue;
                        }
                    };
                    let key = id.to_string();
                    let flag = Arc::new(AtomicBool::new(false));
                    {
                        let mut map = pending.lock().unwrap();
                        if map.len() >= 4 || map.contains_key(&key) {
                            error(id, -32600, "Too many active requests or duplicate ID");
                            continue;
                        }
                        map.insert(key.clone(), flag.clone());
                    }
                    let (root, pending) = (root.clone(), pending.clone());
                    workers.push(thread::spawn(move || {
                        let value = ask(&root, &text, &flag).unwrap_or_else(
                            |e| json!({"isError":true,"content":[{"type":"text","text":e.msg}]}),
                        );
                        send(json!({"jsonrpc":"2.0","id":id,"result":value}));
                        pending.lock().unwrap().remove(&key);
                    }));
                    // Reap finished workers so a long-lived server retains no per-call handles.
                    let mut i = 0;
                    while i < workers.len() {
                        if workers[i].is_finished() {
                            let _ = workers.swap_remove(i).join();
                        } else {
                            i += 1;
                        }
                    }
                    continue;
                }
                _ => {
                    error(id, -32601, "Method not found");
                    continue;
                }
            };
            send(json!({"jsonrpc":"2.0","id":id,"result":value}));
        }
        Ok(())
    })();
    for flag in pending.lock().unwrap().values() {
        flag.store(true, Ordering::Relaxed);
    }
    for worker in workers {
        let _ = worker.join();
    }
    result
}
