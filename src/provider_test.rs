//! Explicit connectivity probes: no project memory is sent to providers.
use crate::{
    agent_provider::*,
    project::Project,
    util::{AppError, Result},
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, IsTerminal, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

// Count the visible static response, not an undisplayed JSON report or spinner frames.
macro_rules! prettyln {
    () => { prettyln!("") };
    ($($arg:tt)*) => {{
        let text = format!($($arg)*);
        crate::statistics::output(&text);
        println!("{text}");
    }};
}

pub(crate) fn run(pretty: bool) -> Result<()> {
    if std::env::var_os("CM_CHAT_INTERNAL").is_some()
        || std::env::var_os("CM_CONTEXT_INTERNAL").is_some()
    {
        return Err(AppError::new("memory workers cannot test providers"));
    }
    let _quiet = crate::agent_logs::quiet_paths(pretty);
    if pretty {
        prettyln!(
            "\n  CM · {}\n",
            crate::ui::tr(
                "Provider connectivity check",
                "Проверка подключения к провайдерам",
                "供应商连接检查"
            )
        );
    }
    let project = Project::open(&crate::chat::project_root()?)?;
    let directory = project
        .health
        .join("provider-tests")
        .join(crate::util::fresh_id());
    Project::checked_path(&project.data, &directory)?;
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join("AGENTS.md"),
        "Connectivity test only. Do not read files or use tools. Return the requested JSON.\n",
    )?;
    if pretty {
        prettyln!(
            "  1. {}: {}\n  2. {}\n",
            crate::ui::tr(
                "Configuration loaded · profiles",
                "Конфигурация загружена · профилей",
                "配置已加载 · 配置项"
            ),
            project.config.agent.profiles.len(),
            crate::ui::tr(
                "Sending test requests",
                "Отправка тестовых запросов",
                "发送测试请求"
            )
        );
    }
    let mut rows = Vec::new();
    let mut covered = BTreeSet::new();
    let mut secrets = Vec::new();
    for settings in project.config.agent.providers.values() {
        secrets.extend(
            settings
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        );
        if let Some(key) = &settings.api_key_env {
            secrets.extend(std::env::var(key).ok().filter(|s| !s.is_empty()));
        }
    }
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for (index, (name, profile)) in project.config.agent.profiles.iter().enumerate() {
        if pretty {
            prettyln!(
                "  [{}/{}] {} · {} · {}",
                index + 1,
                project.config.agent.profiles.len(),
                name,
                terminal_text(&profile.provider),
                terminal_text(profile.model.as_deref().unwrap_or("—"))
            );
        }
        let spinner = Spinner::start(pretty);
        covered.insert(profile.provider.clone());
        let start = Instant::now();
        let _scope = crate::statistics::Scope::new(name, "test_providers", None);
        let result = (|| -> Result<()> {
            let provider = crate::agent_factory::build_provider(&project, &profile.provider, None)?;
            let mut used_tools = false;
            let result = provider.run_step_with_schema(&StepSpec {
                prompt: "Connectivity test. Return exactly {\"status\":\"ok\"}. Do not use tools or read files.".into(),
                cwd: directory.clone(), work_dir: directory.clone(),
                session: SessionRequest::Fresh, model: profile.model.clone(),
                reasoning_effort: profile.reasoning_effort,
                result: StepResultKind::Completed, access: ProviderAccess::ReadOnly,
                native_tools: false,
                limits: ProviderExecutionLimits { session_timeout: Some(Duration::from_secs(project.config.memory.phase_timeout("probe"))), idle_timeout: None },
                env: vec![("CM_CHAT_INTERNAL".into(), "1".into()), ("CM_CONTEXT_INTERNAL".into(), project.root.to_string_lossy().into())],
            }, &cancel_flag(), &mut |event| {
                if matches!(event.kind, ProviderEventKind::Command | ProviderEventKind::FileChange) { used_tools = true; }
            }, Some(json!({"type":"object","additionalProperties":false,"required":["status"],"properties":{"status":{"type":"string","enum":["ok"]}}})))
                .map_err(|e| AppError::new(format!("{e:?}")))?;
            if used_tools {
                return Err(AppError::new("provider used forbidden native tools"));
            }
            match result.outcome {
                StepOutcome::Completed { summary }
                    if serde_json::from_str::<serde_json::Value>(&summary).ok()
                        == Some(json!({"status":"ok"})) =>
                {
                    Ok(())
                }
                _ => Err(AppError::new(
                    "provider responded but did not return the expected test JSON",
                )),
            }
        })();
        drop(spinner);
        let error = result
            .as_ref()
            .err()
            .map(|e| redact_error(&e.msg, &secrets));
        if pretty {
            prettyln!(
                "    {} · {:.2} {}",
                if result.is_ok() {
                    crate::ui::tr(
                        "✓ OK — response verified",
                        "✓ OK — ответ проверен",
                        "✓ 成功 — 响应已验证",
                    )
                } else {
                    crate::ui::tr("✗ ERROR", "✗ ОШИБКА", "✗ 错误")
                },
                start.elapsed().as_secs_f64(),
                crate::ui::tr("s", "с", "秒")
            );
            if let Some(error) = &error {
                prettyln!("    {}", terminal_text(error));
            }
            prettyln!();
        }
        rows.push(json!({"provider":profile.provider,"profile":name,"model":profile.model,"status":if result.is_ok(){"ok"}else{"error"},"elapsed_ms":start.elapsed().as_millis(),"error":error}));
    }
    for name in project
        .config
        .agent
        .providers
        .keys()
        .filter(|name| !covered.contains(*name))
    {
        if pretty {
            prettyln!(
                "  ○ {} · {}\n",
                terminal_text(name),
                crate::ui::tr(
                    "NOT TESTED: no model profile",
                    "НЕ ПРОВЕРЕН: нет профиля с моделью",
                    "未测试：没有模型配置"
                )
            );
        }
        rows.push(json!({"provider":name,"profile":null,"model":null,"status":"not_tested","error":"No agent profile selects this provider; configure an explicit model in a profile."}));
    }
    let ok =
        rows.iter().any(|r| r["status"] == "ok") && !rows.iter().any(|r| r["status"] == "error");
    let output = json!({"status":if ok {"ok"} else {"error"},"checks":rows,"complete":rows.iter().all(|r|r["status"]=="ok")}).to_string();
    if pretty {
        let passed = rows.iter().filter(|r| r["status"] == "ok").count();
        let failed = rows.iter().filter(|r| r["status"] == "error").count();
        prettyln!(
            "  3. {} {passed} · {} {failed} · {} {}\n",
            crate::ui::tr("Summary: passed", "Итог: успешно", "总结：成功"),
            crate::ui::tr("failed", "ошибок", "失败"),
            crate::ui::tr("not tested", "не проверено", "未测试"),
            rows.len() - passed - failed
        );
        if project.config.memory.agent_logs.enabled {
            prettyln!(
                "  {}: {}",
                crate::ui::tr("Detailed logs", "Подробные логи", "详细日志"),
                project.health.join("agent-logs").display()
            );
        }
    } else {
        crate::statistics::output(&output);
        println!("{output}");
    }
    if ok {
        Ok(())
    } else {
        Err(AppError::new(
            "some configured provider checks failed or could not run",
        ))
    }
}

fn redact_error(message: &str, secrets: &[String]) -> String {
    // ProviderError diagnostics use Debug formatting, which escapes quotes and
    // control characters. Mask both forms, longest first for overlapping keys.
    let mut variants = Vec::new();
    for secret in secrets.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        variants.push(secret.to_owned());
        let escaped = format!("{secret:?}");
        variants.push(escaped[1..escaped.len() - 1].to_owned());
    }
    variants.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut clean = message.to_owned();
    for secret in variants {
        clean = clean.replace(&secret, "[REDACTED]");
    }
    clean.chars().take(1000).collect()
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

struct Spinner {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Spinner {
    fn start(pretty: bool) -> Self {
        if !pretty {
            return Self {
                stop: None,
                worker: None,
            };
        }
        if !io::stdout().is_terminal() {
            prettyln!(
                "    {}…",
                crate::ui::tr("Waiting for response", "Ожидание ответа", "等待响应")
            );
            return Self {
                stop: None,
                worker: None,
            };
        }
        let (stop, receiver) = mpsc::channel();
        let waiting = crate::ui::tr("Waiting for response", "Ожидание ответа", "等待响应");
        let seconds = crate::ui::tr("s", "с", "秒");
        let worker = thread::spawn(move || {
            let started = Instant::now();
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut i = 0;
            loop {
                let mut out = io::stdout().lock();
                let _ = write!(
                    out,
                    "\r    {} {waiting} · {:.1} {seconds}   ",
                    frames[i % frames.len()],
                    started.elapsed().as_secs_f64()
                );
                let _ = out.flush();
                drop(out);
                if !matches!(
                    receiver.recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
                i += 1;
            }
            let mut out = io::stdout().lock();
            let _ = write!(out, "\r{}\r", " ".repeat(65));
            let _ = out.flush();
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
        }
    }
}
impl Drop for Spinner {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_redaction_handles_trimmed_overlapping_and_escaped_keys() {
        let secrets = vec![" token ".into(), "token-long".into(), "key\"quoted".into()];
        assert_eq!(
            redact_error("token-long token", &secrets),
            "[REDACTED] [REDACTED]"
        );
        assert_eq!(
            redact_error(&format!("{:?}", "key\"quoted"), &secrets),
            "\"[REDACTED]\""
        );
        assert_eq!(redact_error("key\"quoted", &secrets), "[REDACTED]");
        assert_eq!(redact_error(&"Ж".repeat(1100), &[]).chars().count(), 1000);
    }
    #[test]
    fn terminal_labels_cannot_inject_control_sequences() {
        assert_eq!(
            terminal_text("provider\x1b[2J\r\nnext"),
            "provider [2J  next"
        );
    }
}
