//! Shared provider construction for memory dialogues.
use crate::agent_provider::*;
use crate::config::AgentProviderAdapter;
use crate::project::Project;
use crate::util::{AppError, Result};
use std::path::{Path, PathBuf};

pub(crate) fn build_provider(
    project: &Project,
    provider: &str,
    override_exe: Option<&Path>,
) -> Result<Box<dyn Provider>> {
    let inner = build_unmeasured(project, provider, override_exe)?;
    let measured: Box<dyn Provider> = Box::new(crate::statistics::MeasuredProvider {
        inner,
        provider: provider.into(),
    });
    let resilient = Box::new(crate::agent_retry::RetryProvider {
        inner: measured,
        config: project.config.memory.agent_retries.clone(),
        timeout: std::time::Duration::from_secs(project.config.memory.timeout_seconds),
    });
    Ok(crate::agent_logs::wrap(project, provider, resilient))
}

fn build_unmeasured(
    project: &Project,
    provider: &str,
    override_exe: Option<&Path>,
) -> Result<Box<dyn Provider>> {
    let settings = project.config.agent.providers.get(provider);
    let configured_exe = settings
        .and_then(|settings| settings.executable.as_deref())
        .map(PathBuf::from);
    let exe = override_exe.map(Path::to_path_buf).or(configured_exe);
    let args = settings
        .map(|settings| settings.args.clone())
        .unwrap_or_default();
    let adapter = project
        .config
        .agent
        .provider_adapter(provider)
        .ok_or_else(|| {
            AppError::with_hint(
                format!(
                    "unknown agent provider '{provider}'; available providers: {}",
                    project.config.agent.provider_names().join(", ")
                ),
                "configure agent.providers.<name>.adapter in memory/config.json",
            )
        })?;
    if let Some(routing) = settings.and_then(|settings| settings.routing.as_ref()) {
        routing.validate(adapter)?;
    }
    if let Some(format) = settings.and_then(|settings| settings.response_format) {
        format.validate(adapter)?;
    }
    if project.config.agent.proxy.enabled
        && !matches!(
            adapter,
            AgentProviderAdapter::Ollama | AgentProviderAdapter::OpenaiCompatible
        )
    {
        return Err(AppError::new("agent.proxy requires an HTTP provider adapter; external CLI providers cannot guarantee proxy routing"));
    }
    match adapter {
        AgentProviderAdapter::Ollama | AgentProviderAdapter::OpenaiCompatible => Ok(Box::new(
            crate::agent_http::HttpProvider::new(
                settings
                    .expect("HTTP providers are explicitly configured")
                    .clone(),
                adapter,
            )?
            .with_proxy(project.config.agent.proxy.clone())?,
        )),
        AgentProviderAdapter::Codex => Ok(Box::new(match exe {
            Some(exe) => CodexProvider::with_exe_and_args(exe, args),
            None if args.is_empty() => CodexProvider::from_env(),
            None => CodexProvider::with_exe_and_args(
                crate::agent_provider::env_provider_executable(
                    crate::agent_provider::CM_CODEX_EXE_ENV,
                    "codex",
                ),
                args,
            ),
        })),
        AgentProviderAdapter::Kimi => Ok(Box::new(match exe {
            Some(exe) => KimiProvider::with_exe_and_args(exe, args),
            None if args.is_empty() => KimiProvider::from_env(),
            None => KimiProvider::with_exe_and_args(
                crate::agent_provider::env_provider_executable(
                    crate::agent_provider::CM_KIMI_EXE_ENV,
                    "kimi",
                ),
                args,
            ),
        })),
        AgentProviderAdapter::Claude => Ok(Box::new(match exe {
            Some(exe) => ClaudeProvider::with_exe_and_args(exe, args),
            None if args.is_empty() => ClaudeProvider::from_env(),
            None => ClaudeProvider::with_exe_and_args(
                crate::agent_provider::env_provider_executable(
                    crate::agent_provider::CM_CLAUDE_EXE_ENV,
                    "claude",
                ),
                args,
            ),
        })),
        AgentProviderAdapter::Jsonl => {
            let exe = exe.ok_or_else(|| {
                AppError::new(format!(
                    "agent provider '{provider}' uses adapter 'jsonl' but has no executable"
                ))
            })?;
            Ok(Box::new(JsonlProvider::new(exe, args)))
        }
    }
}
