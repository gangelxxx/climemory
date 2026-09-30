//! One bounded retry policy at the shared provider boundary.
use crate::{agent_provider::*, config::AgentRetryConfig};
use serde_json::{json, Value};
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

pub(crate) struct RetryProvider {
    pub inner: Box<dyn Provider>,
    pub config: AgentRetryConfig,
    pub timeout: Duration,
}
impl Provider for RetryProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn run_step(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
    ) -> Result<StepResult, ProviderError> {
        self.run_step_with_schema(spec, cancel, sink, None)
    }
    fn run_step_with_schema(
        &self,
        spec: &StepSpec,
        cancel: &CancelFlag,
        sink: &mut (dyn FnMut(&ProviderEvent) + Send),
        schema: Option<Value>,
    ) -> Result<StepResult, ProviderError> {
        let start = Instant::now();
        let total = spec.limits.session_timeout.unwrap_or(self.timeout);
        // Never replay resumed sessions or tool-capable workers that may have
        // already performed an external action. HTTP workers cannot use tools.
        let safe = spec.session == SessionRequest::Fresh
            && spec.access == ProviderAccess::ReadOnly
            && (!spec.native_tools || matches!(self.inner.name(), "openai-compatible" | "ollama"));
        let attempts = if safe {
            self.config.max_attempts.clamp(1, 5)
        } else {
            1
        };
        for attempt in 1..=attempts {
            let _attempt = crate::agent_logs::install_attempt(Some(attempt));
            if cancel.load(Ordering::Relaxed) {
                return Err(ProviderError::Interrupted);
            }
            let remaining = total.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(ProviderError::TimedOut {
                    kind: ProviderTimeoutKind::Session,
                    session_id: None,
                });
            }
            let mut request = spec.clone();
            request.limits.session_timeout = Some(if attempts > 1 {
                remaining.min(Duration::from_secs(self.config.attempt_timeout_seconds))
            } else {
                remaining
            });
            crate::agent_logs::event(
                "attempt_started",
                json!({"attempt":attempt,"max_attempts":attempts,"remaining_ms":remaining.as_millis(),"timeout_ms":request.limits.session_timeout.unwrap().as_millis()}),
            );
            let attempt_started = Instant::now();
            let mut observed_tool = false;
            crate::statistics::track_attempt(attempt);
            let result = self.inner.run_step_with_schema(
                &request,
                cancel,
                &mut |event| {
                    observed_tool |= matches!(
                        event.kind,
                        ProviderEventKind::Command | ProviderEventKind::FileChange
                    );
                    sink(event);
                },
                schema.clone(),
            );
            crate::agent_logs::event(
                "attempt_finished",
                json!({"attempt":attempt,"elapsed_ms":attempt_started.elapsed().as_millis(),"status":if result.is_ok(){"completed"}else{"error"},"error":result.as_ref().err().map(|e|format!("{e:?}"))}),
            );
            if cancel.load(Ordering::Relaxed) {
                return Err(ProviderError::Interrupted);
            }
            if start.elapsed() >= total {
                return Err(ProviderError::TimedOut {
                    kind: ProviderTimeoutKind::Session,
                    session_id: None,
                });
            }
            let delay = match &result {
                Err(ProviderError::Transient { retry_after_ms, .. }) => {
                    Some(Duration::from_millis(retry_after_ms.unwrap_or(0)))
                }
                Err(ProviderError::TimedOut { .. }) => Some(Duration::ZERO),
                _ => None,
            };
            let Some(server_delay) = delay.filter(|_| attempt < attempts && !observed_tool) else {
                return result;
            };
            let backoff =
                Duration::from_millis(self.config.backoff_ms.saturating_mul(1 << (attempt - 1)));
            let delay = server_delay.max(backoff);
            if delay >= total.saturating_sub(start.elapsed()) {
                return result;
            }
            crate::agent_logs::event(
                "retry_scheduled",
                json!({"attempt":attempt,"next_attempt":attempt+1,"delay_ms":delay.as_millis(),"reason":result.as_ref().err().map(|e|format!("{e:?}"))}),
            );
            let waiting = Instant::now();
            while waiting.elapsed() < delay {
                if cancel.load(Ordering::Relaxed) {
                    return Err(ProviderError::Interrupted);
                }
                std::thread::sleep(
                    Duration::from_millis(20).min(delay.saturating_sub(waiting.elapsed())),
                );
            }
        }
        unreachable!("validated retry configuration")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    struct Script {
        calls: Calls,
        results: Mutex<std::collections::VecDeque<Result<StepResult, ProviderError>>>,
        tool: bool,
    }
    impl Provider for Script {
        fn name(&self) -> &'static str {
            "test"
        }
        fn run_step(
            &self,
            spec: &StepSpec,
            cancel: &CancelFlag,
            sink: &mut (dyn FnMut(&ProviderEvent) + Send),
        ) -> Result<StepResult, ProviderError> {
            self.run_step_with_schema(spec, cancel, sink, None)
        }
        fn run_step_with_schema(
            &self,
            spec: &StepSpec,
            _: &CancelFlag,
            sink: &mut (dyn FnMut(&ProviderEvent) + Send),
            schema: Option<Value>,
        ) -> Result<StepResult, ProviderError> {
            self.calls.lock().unwrap().push((spec.clone(), schema));
            if self.tool {
                sink(&ProviderEvent {
                    kind: ProviderEventKind::Command,
                    text: "tool".into(),
                    raw_kind: "tool".into(),
                    raw_json: "{}".into(),
                });
            }
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected replay")
        }
    }
    fn transient() -> Result<StepResult, ProviderError> {
        Err(ProviderError::Transient {
            detail: "503".into(),
            retry_after_ms: None,
        })
    }
    fn success() -> Result<StepResult, ProviderError> {
        Ok(StepResult {
            session_id: None,
            outcome: StepOutcome::Completed {
                summary: "done".into(),
            },
        })
    }
    fn spec() -> StepSpec {
        StepSpec {
            prompt: "request".into(),
            cwd: ".".into(),
            work_dir: ".".into(),
            session: SessionRequest::Fresh,
            model: Some("test".into()),
            reasoning_effort: None,
            result: StepResultKind::Completed,
            access: ProviderAccess::ReadOnly,
            native_tools: false,
            limits: ProviderExecutionLimits::default(),
            env: vec![],
        }
    }
    type Calls = Arc<Mutex<Vec<(StepSpec, Option<Value>)>>>;
    fn runner(
        results: Vec<Result<StepResult, ProviderError>>,
        tool: bool,
    ) -> (RetryProvider, Calls) {
        let calls = Arc::new(Mutex::new(vec![]));
        (
            RetryProvider {
                inner: Box::new(Script {
                    calls: calls.clone(),
                    results: Mutex::new(results.into()),
                    tool,
                }),
                config: AgentRetryConfig {
                    backoff_ms: 0,
                    ..Default::default()
                },
                timeout: Duration::from_secs(300),
            },
            calls,
        )
    }
    #[test]
    fn retries_preserve_schema_and_bound_each_attempt() {
        let (p, calls) = runner(vec![transient(), success()], false);
        p.run_step_with_schema(
            &spec(),
            &cancel_flag(),
            &mut |_| {},
            Some(json!({"type":"object"})),
        )
        .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for (s, schema) in calls.iter() {
            assert_eq!(s.prompt, "request");
            assert_eq!(*schema, Some(json!({"type":"object"})));
            assert_eq!(s.limits.session_timeout, Some(Duration::from_secs(180)));
        }
    }
    #[test]
    fn bounded_exhaustion_and_disable() {
        for limit in [1, 3] {
            let (mut p, calls) = runner((0..limit).map(|_| transient()).collect(), false);
            p.config.max_attempts = limit;
            assert!(p.run_step(&spec(), &cancel_flag(), &mut |_| {}).is_err());
            assert_eq!(calls.lock().unwrap().len(), limit as usize);
            if limit == 1 {
                assert!(
                    calls.lock().unwrap()[0].0.limits.session_timeout.unwrap()
                        > Duration::from_secs(290)
                );
            }
        }
    }
    #[test]
    fn permanent_errors_and_unsafe_requests_are_not_replayed() {
        for error in [
            ProviderError::Preparation {
                detail: "403".into(),
            },
            ProviderError::MalformedResult {
                reason: "bad JSON".into(),
            },
            ProviderError::Interrupted,
        ] {
            let (p, calls) = runner(vec![Err(error)], false);
            assert!(p.run_step(&spec(), &cancel_flag(), &mut |_| {}).is_err());
            assert_eq!(calls.lock().unwrap().len(), 1);
        }
        for mode in 0..4 {
            let (p, calls) = runner(vec![transient()], mode == 3);
            let mut s = spec();
            match mode {
                0 => s.native_tools = true,
                1 => s.session = SessionRequest::Resume("id".into()),
                2 => s.access = ProviderAccess::WorkspaceWrite,
                _ => (),
            }
            assert!(p.run_step(&s, &cancel_flag(), &mut |_| {}).is_err());
            assert_eq!(calls.lock().unwrap().len(), 1);
        }
    }
    #[test]
    fn retry_after_and_expired_deadline_do_not_launch_more_calls() {
        let (p, calls) = runner(
            vec![Err(ProviderError::Transient {
                detail: "429".into(),
                retry_after_ms: Some(400_000),
            })],
            false,
        );
        assert!(p.run_step(&spec(), &cancel_flag(), &mut |_| {}).is_err());
        assert_eq!(calls.lock().unwrap().len(), 1);
        let mut s = spec();
        s.limits.session_timeout = Some(Duration::ZERO);
        assert!(matches!(
            p.run_step(&s, &cancel_flag(), &mut |_| {}),
            Err(ProviderError::TimedOut { .. })
        ));
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
    #[test]
    fn cancellation_interrupts_backoff() {
        let (mut p, calls) = runner(vec![transient()], false);
        p.config.backoff_ms = 30_000;
        let cancel = cancel_flag();
        let other = cancel.clone();
        let observed = calls.clone();
        let worker = std::thread::spawn(move || {
            while observed.lock().unwrap().is_empty() {
                std::thread::yield_now();
            }
            other.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        assert!(matches!(
            p.run_step(&spec(), &cancel, &mut |_| {}),
            Err(ProviderError::Interrupted)
        ));
        worker.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
}
