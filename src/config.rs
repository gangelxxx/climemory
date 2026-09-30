use crate::util::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "format")]
    pub format: String,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryConfig {
    pub unified: UnifiedConfig,
    #[serde(skip_serializing, rename = "documents_prefilter")]
    _retired_documents_prefilter: Option<bool>,
    pub feedback: FeedbackConfig,
    pub timeouts: TaskTimeouts,
    pub agent_retries: AgentRetryConfig,
    pub agent_logs: StatisticsConfig,
    pub statistics: StatisticsConfig,
    pub cache: CacheConfig,
    pub mode: MemoryMode,
    pub chat_agent: Option<String>,
    pub documents_agent: String,
    pub verification_agent: Option<String>,
    pub preparation_agent: Option<String>,
    pub budget_tokens: usize,
    pub timeout_seconds: u64,
    pub max_steps: usize,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            unified: UnifiedConfig::default(),
            _retired_documents_prefilter: None,
            feedback: FeedbackConfig::default(),
            timeouts: TaskTimeouts::default(),
            agent_retries: AgentRetryConfig::default(),
            agent_logs: StatisticsConfig::default(),
            statistics: StatisticsConfig::default(),
            cache: CacheConfig::default(),
            mode: MemoryMode::Threads,
            chat_agent: None,
            documents_agent: "agent_medium".into(),
            verification_agent: Some("agent_medium".into()),
            preparation_agent: None,
            budget_tokens: 6000,
            timeout_seconds: 120,
            max_steps: 16,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UnifiedConfig {
    // Accepted only for old configuration files; never selects a retrieval path.
    #[serde(skip_serializing, rename = "enabled")]
    _retired_enabled: Option<bool>,
    pub concurrency: usize,
    pub max_candidates: usize,
}
impl Default for UnifiedConfig {
    fn default() -> Self {
        Self {
            _retired_enabled: None,
            concurrency: 3,
            max_candidates: 8,
        }
    }
}
/// Optional phase budgets; absent values preserve the legacy overall limit.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskTimeouts {
    #[serde(skip_serializing, rename = "coordinator_seconds")]
    _retired_coordinator_seconds: Option<u64>,
    #[serde(skip_serializing, rename = "documents_seconds")]
    _retired_documents_seconds: Option<u64>,
    pub ingest_seconds: Option<u64>,
    pub provider_test_seconds: Option<u64>,
}
impl MemoryConfig {
    pub fn phase_timeout(&self, phase: &str) -> u64 {
        let value = match phase {
            "ingest" => self.timeouts.ingest_seconds,
            "probe" => self.timeouts.provider_test_seconds,
            _ => None,
        };
        value
            .unwrap_or(self.timeout_seconds)
            .min(self.timeout_seconds)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeedbackConfig {
    pub background: bool,
    pub request_budget_seconds: u64,
    pub retry_cooldown_seconds: u64,
    pub enabled: bool,
    pub agent: String,
    pub timeout_seconds: u64,
}
impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            background: true,
            request_budget_seconds: 10,
            retry_cooldown_seconds: 300,
            enabled: false,
            agent: "agent_low".into(),
            timeout_seconds: 60,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentRetryConfig {
    pub max_attempts: u32,
    pub attempt_timeout_seconds: u64,
    pub backoff_ms: u64,
}
impl Default for AgentRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            attempt_timeout_seconds: 180,
            backoff_ms: 500,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatisticsConfig {
    pub enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub enabled: bool,
}
impl Default for CacheConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryMode {
    #[default]
    Threads,
    DocsOnly,
    ReadOnly,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<crate::classifier::ClassifierConfig>,
    #[serde(default)]
    pub providers: BTreeMap<String, AgentProviderConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, AgentProfile>,
}
fn format() -> String {
    "climemory/memory-1".into()
}
impl Default for Config {
    fn default() -> Self {
        Self {
            format: format(),
            memory: MemoryConfig::default(),
            agent: AgentConfig {
                proxy: ProxyConfig::default(),
                classifier: None,
                providers: BTreeMap::new(),
                profiles: default_agent_profiles(),
            },
        }
    }
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut config: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        config.agent.proxy.validate()?;
        if [
            config.memory.timeouts.ingest_seconds,
            config.memory.timeouts.provider_test_seconds,
        ]
        .into_iter()
        .flatten()
        .any(|n| !(1..=600).contains(&n))
            || !(1..=60).contains(&config.memory.feedback.request_budget_seconds)
            || config.memory.feedback.retry_cooldown_seconds > 86400
        {
            return Err(AppError::new(
                "invalid task timeout or feedback request budget",
            ));
        }
        if config.memory.feedback.enabled
            && (!(1..=300).contains(&config.memory.feedback.timeout_seconds)
                || !config
                    .agent
                    .profiles
                    .contains_key(&config.memory.feedback.agent))
        {
            return Err(AppError::new(
                "memory.feedback requires an existing agent profile and timeout_seconds in 1..300",
            ));
        }
        if let Some(classifier) = &mut config.agent.classifier {
            classifier.proxy = config.agent.proxy.clone();
        }
        let retries = &config.memory.agent_retries;
        if !(1..=5).contains(&retries.max_attempts)
            || !(1..=600).contains(&retries.attempt_timeout_seconds)
            || retries.backoff_ms > 30000
        {
            return Err(AppError::new("invalid agent retry limits"));
        }
        if !matches!(
            config.format.as_str(),
            "climemory/memory-1" | "climemory/threads-1"
        ) {
            return Err(AppError::new("unsupported memory config format"));
        }
        if !(256..=50000).contains(&config.memory.budget_tokens)
            || !(1..=8).contains(&config.memory.unified.concurrency)
            || !(1..=32).contains(&config.memory.unified.max_candidates)
            || !(1..=600).contains(&config.memory.timeout_seconds)
            || !(1..=32).contains(&config.memory.max_steps)
        {
            return Err(AppError::new("invalid memory limits"));
        }
        for (name, profile) in &config.agent.profiles {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
            {
                return Err(AppError::new("invalid agent profile name"));
            }
            let adapter = config
                .agent
                .provider_adapter(&profile.provider)
                .ok_or_else(|| AppError::new("unknown profile provider"))?;
            if profile.model.as_ref().is_none_or(|m| {
                m.trim().is_empty()
                    || m.trim() != m
                    || m.chars().count() > 200
                    || m.chars().any(char::is_control)
            }) {
                return Err(AppError::new("agent profiles require an explicit model"));
            }
            if profile.reasoning_effort.is_some() && !adapter.supports_reasoning_effort() {
                return Err(AppError::new("provider does not support reasoning_effort"));
            }
        }
        Ok(config)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::util::atomic_write(path, &serde_json::to_vec_pretty(self)?)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<crate::agent_provider::ModelReasoningEffort>,
}

fn default_agent_profiles() -> BTreeMap<String, AgentProfile> {
    use crate::agent_provider::ModelReasoningEffort;
    [
        ("agent_high", ModelReasoningEffort::High),
        ("agent_medium", ModelReasoningEffort::Medium),
        ("agent_low", ModelReasoningEffort::Low),
    ]
    .into_iter()
    .map(|(name, effort)| {
        (
            name.into(),
            AgentProfile {
                provider: "codex".into(),
                model: Some("gpt-5.5".into()),
                reasoning_effort: Some(effort),
            },
        )
    })
    .collect()
}

/// OpenRouter upstream selection, forwarded as the request `provider` object.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRouting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<ProviderSort>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSort {
    Price,
    Throughput,
    Latency,
}

impl ProviderRouting {
    pub fn validate(&self, adapter: AgentProviderAdapter) -> Result<()> {
        if adapter != AgentProviderAdapter::OpenaiCompatible {
            return Err(AppError::new(
                "provider routing requires the openai-compatible adapter",
            ));
        }
        for (name, list) in [
            ("order", &self.order),
            ("only", &self.only),
            ("ignore", &self.ignore),
        ] {
            if let Some(list) = list {
                if list.is_empty()
                    || list.iter().any(|slug| {
                        slug.trim().is_empty()
                            || slug.trim() != slug
                            || slug.chars().any(char::is_control)
                    })
                {
                    return Err(AppError::new(format!(
                        "provider routing {name} requires a nonempty list of provider slugs"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Response protocol requested from an OpenAI-compatible HTTP endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpResponseFormat {
    JsonObject,
    JsonSchema,
}

impl HttpResponseFormat {
    pub fn validate(self, adapter: AgentProviderAdapter) -> Result<()> {
        if adapter != AgentProviderAdapter::OpenaiCompatible {
            return Err(AppError::new("response_format requires the openai-compatible adapter; Ollama already uses its native format schema"));
        }
        Ok(())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct AgentProviderConfig {
    /// Omitted means the legacy JSON-object protocol; schema enforcement is opt-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<HttpResponseFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ProviderRouting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub allow_remote_content: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Optional inline HTTP credential; never include its value in diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Adapter dialect. The legacy provider names `codex` and `kimi`
    /// infer their matching adapter when this field is omitted. Every custom
    /// provider name must select an adapter explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<AgentProviderAdapter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Fixed arguments inserted before adapter-generated arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

impl std::fmt::Debug for AgentProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentProviderConfig")
            .field("response_format", &self.response_format)
            .field("routing", &self.routing)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("reasoning_enabled", &self.reasoning_enabled)
            .field("endpoint", &self.endpoint)
            .field("allow_remote_content", &self.allow_remote_content)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("adapter", &self.adapter)
            .field("executable", &self.executable)
            .field("args", &self.args)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProviderAdapter {
    Codex,
    Kimi,
    Claude,
    Jsonl,
    Ollama,
    #[serde(rename = "openai-compatible")]
    OpenaiCompatible,
}

impl AgentProviderAdapter {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Kimi => "kimi",
            Self::Claude => "claude",
            Self::Jsonl => "jsonl",
            Self::Ollama => "ollama",
            Self::OpenaiCompatible => "openai-compatible",
        }
    }

    pub fn supports_reasoning_effort(self) -> bool {
        matches!(self, Self::Codex | Self::OpenaiCompatible)
    }
}

impl AgentConfig {
    /// Resolve a logical provider name to its adapter. Codex and Kimi remain
    /// implicit registry entries for backwards compatibility.
    pub fn provider_adapter(&self, name: &str) -> Option<AgentProviderAdapter> {
        self.providers
            .get(name)
            .and_then(|settings| settings.adapter)
            .or(match name {
                "codex" => Some(AgentProviderAdapter::Codex),
                "kimi" => Some(AgentProviderAdapter::Kimi),
                _ => None,
            })
    }

    pub fn provider_names(&self) -> Vec<String> {
        let mut names = self.providers.keys().cloned().collect::<BTreeSet<_>>();
        names.insert("codex".to_string());
        names.insert("kimi".to_string());
        names.into_iter().collect()
    }
}

pub(crate) fn endpoint_is_loopback(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint).ok().is_some_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            })
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub url: String,
}
impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://localhost:10809".into(),
        }
    }
}
impl ProxyConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let url =
            reqwest::Url::parse(&self.url).map_err(|_| AppError::new("invalid agent.proxy.url"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(AppError::new("agent.proxy.url must be an HTTP(S) proxy URL without credentials, path, query or fragment"));
        }
        Ok(())
    }
    pub(crate) fn apply(
        &self,
        builder: reqwest::blocking::ClientBuilder,
    ) -> Result<reqwest::blocking::ClientBuilder> {
        self.validate()?;
        if self.enabled {
            let proxy =
                reqwest::Proxy::all(&self.url).map_err(|_| AppError::new("invalid agent proxy"))?;
            Ok(builder.no_proxy().proxy(proxy))
        } else {
            Ok(builder)
        }
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };

    #[test]
    fn explicit_proxy_routes_loopback_without_contacting_origin() {
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        origin.set_nonblocking(true).unwrap();
        proxy.set_nonblocking(true).unwrap();
        let config = ProxyConfig {
            enabled: true,
            url: format!("http://{}", proxy.local_addr().unwrap()),
        };
        let target = format!("http://{}/test", origin.local_addr().unwrap());
        let expected = target.clone();
        let worker = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut stream = loop {
                if let Ok((stream, _)) = proxy.accept() {
                    break stream;
                }
                assert!(start.elapsed() < Duration::from_secs(3));
                std::thread::sleep(Duration::from_millis(10));
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut data = Vec::new();
            let mut byte = [0];
            while !data.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                data.push(byte[0]);
            }
            assert!(String::from_utf8(data)
                .unwrap()
                .starts_with(&format!("GET {expected} HTTP/1.1")));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let client = config
            .apply(reqwest::blocking::Client::builder().timeout(Duration::from_secs(2)))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(client.get(target).send().unwrap().text().unwrap(), "ok");
        worker.join().unwrap();
        assert!(matches!(origin.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
        assert!(ProxyConfig {
            enabled: false,
            url: "unused".into()
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn failed_proxy_never_falls_back_to_origin() {
        let origin = TcpListener::bind("127.0.0.1:0").unwrap();
        origin.set_nonblocking(true).unwrap();
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = ProxyConfig {
            enabled: true,
            url: format!("http://{}", dead.local_addr().unwrap()),
        };
        drop(dead);
        let client = proxy
            .apply(reqwest::blocking::Client::builder().timeout(Duration::from_millis(300)))
            .unwrap()
            .build()
            .unwrap();
        assert!(client
            .get(format!("http://{}/", origin.local_addr().unwrap()))
            .send()
            .is_err());
        assert!(matches!(origin.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }
}

#[cfg(test)]
mod task_timeout_tests {
    use super::*;
    #[test]
    fn phase_limits_preserve_defaults_and_cannot_extend_overall_budget() {
        let mut config = MemoryConfig::default();
        assert_eq!(config.phase_timeout("probe"), 120);
        config.timeouts.provider_test_seconds = Some(30);
        config.timeouts.ingest_seconds = Some(300);
        assert_eq!(config.phase_timeout("probe"), 30);
        assert_eq!(config.phase_timeout("ingest"), 120);
        config.timeout_seconds = 5;
        assert_eq!(config.phase_timeout("probe"), 5);
    }
    #[test]
    fn invalid_phase_budgets_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        for value in [0, 601] {
            std::fs::write(
                &file,
                serde_json::json!({"memory":{"timeouts":{"ingest_seconds":value}}}).to_string(),
            )
            .unwrap();
            assert!(Config::load(&file).is_err());
        }
    }
}
