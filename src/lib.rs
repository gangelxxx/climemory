mod agent_factory;
mod agent_http;
mod agent_logs;
mod agent_provider;
mod agent_retry;
mod build_info;
mod chat;
mod classifier;
mod cli;
mod code;
mod code_commands;
#[cfg_attr(
    all(test, not(feature = "code-index")),
    allow(dead_code, unused_imports)
)]
mod code_index;
mod config;
mod fake_codex;
mod fake_kimi;
mod feedback;
mod heartbeat;
mod help;
mod hooks;
mod mcp;
mod memory_app;
mod model;
mod output;
mod process;
mod project;
mod provider_test;
mod session_ingest;
mod statistics;
mod terms;
mod thread_agents;
mod token_estimate;
mod ui;
mod unified;
mod usage;
mod util;
use std::path::Path;
fn same_root(a: &Path, b: &Path) -> bool {
    std::fs::canonicalize(a)
        .ok()
        .zip(std::fs::canonicalize(b).ok())
        .is_some_and(|(a, b)| a == b)
}
fn guard_nested_mutation_root(_parsed: &cli::Parsed, root: &Path) -> util::Result<()> {
    if let Some(scope) = std::env::var_os("CM_CONTEXT_INTERNAL") {
        if scope == "1" || same_root(Path::new(&scope), root) {
            return Err(util::AppError::new(
                "memory agents cannot mutate their owning project through nested CLI calls",
            ));
        }
    }
    Ok(())
}
pub fn main_entry(legacy: bool) {
    util::init_console_utf8();
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if !legacy && args == ["--mcp"] {
        if let Err(error) = mcp::run() {
            eprintln!("CM MCP: {}", error.msg);
            std::process::exit(1);
        }
        return;
    }
    if let Some(result) = feedback::dispatch_worker(&args) {
        if let Err(error) = result {
            eprintln!("CM: {}", error.msg);
            std::process::exit(1);
        }
        return;
    }
    // Hooks have a strict JSON stdout protocol, independent of interactive UI.
    if !legacy && args.first().is_some_and(|arg| arg == "hooks") {
        if let Err(error) = hooks::run(&args[1..]) {
            eprintln!("CM hooks: {}", error.msg);
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|a| a == "--no-daemon")
        && args.get(1).is_some_and(|a| a == "exec")
        && std::env::var_os("CM_FAKE_CODEX_SCENARIO").is_some()
    {
        std::process::exit(fake_codex::run(&args[2..]));
    }
    if std::env::var_os("CM_FAKE_KIMI_SCENARIO").is_some() && args.iter().any(|a| a == "--print") {
        std::process::exit(fake_kimi::run(&args));
    }
    if !legacy {
        feedback::begin(&args);
    }
    let args = if legacy {
        args
    } else {
        match ui::parse(&args) {
            Ok(args) => args,
            Err(error) => {
                eprintln!("{}", ui::tr("ERROR", "ОШИБКА", "错误"));
                eprintln!("CM: {}", ui::error(&error.msg));
                feedback::finish(&Err(error));
                std::process::exit(1);
            }
        }
    };
    let statistics = (!legacy).then(|| statistics::begin(&args)).flatten();
    let heartbeat = (!legacy
        && !args.is_empty()
        && !matches!(args[0].as_str(), "help" | "--help" | "-h" | "init"))
    .then(heartbeat::Heartbeat::start);
    let result = if legacy {
        cli::Parsed::parse(&args).and_then(|p| memory_app::run(&p))
    } else {
        if !args.is_empty()
            && !args
                .first()
                .is_some_and(|message| unified::is_details_request(message))
            && !matches!(
                args[0].as_str(),
                "help" | "--help" | "-h" | "init" | "feedback" | "ingest-session"
            )
        {
            feedback::before_request();
        }
        chat::run(&args)
    };
    // Join the timer before printing the final outcome: no late RUNNING after ERROR.
    drop(heartbeat);
    if !legacy {
        if let Err(e) = &result {
            statistics::log_error(&e.msg);
        }
        feedback::finish(&result);
    }
    if let Some(statistics) = statistics {
        statistics.finish(result.is_ok());
    }
    if let Err(error) = result {
        if !legacy {
            eprintln!("{}", ui::tr("ERROR", "ОШИБКА", "错误"));
            eprintln!("CM: {}", ui::error(&error.msg));
            std::process::exit(1);
        }
        eprintln!(
            "{}",
            serde_json::json!({"record":"error","message":error.msg,"hint":error.hint,"diagnostic":error.details.diagnostic,"details":error.details.extra,"next_argv":error.details.retry.then_some(&args)})
        );
        std::process::exit(1);
    }
}
