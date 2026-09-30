use crate::{
    cli::Parsed,
    util::{AppError, Result},
};
use serde_json::json;
pub const COMMANDS: &[(&str, &str, &[&str])] = &[
    (
        "route",
        "route <task> - select relevant thread context with the optional classifier",
        &[],
    ),
    (
        "context",
        "context <task> - retrieve advisory memory locally",
        &[],
    ),
    (
        "read",
        "read <thread-or-session> - inspect saved memory or dialogue",
        &[],
    ),
    (
        "create",
        "create <title> [--parent <thread>] - create a topic with an agent",
        &["parent", "agent", "slug", "note", "with-file"],
    ),
    (
        "ask",
        "ask <thread> <task> - consult its agent",
        &["with-file"],
    ),
    (
        "reply",
        "reply <session> <answer> - answer an agent question",
        &["with-file"],
    ),
    (
        "report",
        "report <session> <result> - save what changed and why",
        &["with-file"],
    ),
    ("pending", "pending - list unfinished dialogues", &[]),
    (
        "cancel",
        "cancel <session> - close an unfinished dialogue",
        &[],
    ),
    (
        "retry",
        "retry <session> - continue an interrupted dialogue",
        &[],
    ),
    (
        "bind",
        "bind <thread> --agent <profile> [--parent <thread-or-none>] - configure ownership",
        &["agent", "parent"],
    ),
    (
        "init",
        "init [directory] - initialize memory and model instructions",
        &[],
    ),
    ("version", "version - show binary identity", &[]),
    (
        "code grep",
        "code grep <text> - search repository text",
        &[
            "regex",
            "ignore-case",
            "path",
            "glob",
            "pattern",
            "count",
            "count-by-file",
            "invert-match",
            "multiline",
            "limit",
            "offset",
            "context-lines",
            "before-context",
            "after-context",
        ],
    ),
    (
        "code find",
        "code find <symbol> - find definitions and callers",
        &[
            "path",
            "kind",
            "symbol-kind",
            "count",
            "limit",
            "offset",
            "transitive",
            "depth",
        ],
    ),
];
pub fn is_registered_option(command: &str, flag: &str) -> bool {
    flag == "dir"
        || flag == "help"
        || COMMANDS
            .iter()
            .any(|(name, _, flags)| *name == command && flags.contains(&flag))
}
pub fn validate_command_options(command: &str, parsed: &Parsed) -> Result<()> {
    if !COMMANDS.iter().any(|(name, _, _)| *name == command) {
        return Err(AppError::with_hint(
            format!("unknown command '{command}'"),
            "cm help",
        ));
    }
    for flag in parsed.values.keys().chain(parsed.bools.iter()) {
        if !is_registered_option(command, flag) {
            return Err(AppError::with_hint(
                format!("unknown option '--{flag}' for '{command}'"),
                format!("cm help {command}"),
            ));
        }
        if parsed.values.contains_key(flag) != crate::cli::VALUE_FLAGS.contains(&flag.as_str()) {
            return Err(AppError::new(format!("invalid value form for '--{flag}'")));
        }
    }
    Ok(())
}
pub fn print_focused(path: &[&str], _: Option<&str>, _: Option<&str>, _: bool) -> Result<()> {
    let key = path.join(" ");
    let rows: Vec<_> = COMMANDS
        .iter()
        .filter(|(name, _, _)| {
            key.is_empty() || *name == key || name.starts_with(&format!("{key} "))
        })
        .map(|(name, usage, flags)| json!({"command":name,"usage":usage,"options":flags}))
        .collect();
    if rows.is_empty() {
        return Err(AppError::new(format!("unknown help topic '{key}'")));
    }
    crate::output::write_records(&[
        json!({"record":"help","commands":rows,"global_options":["dir","help"],"output":"JSONL","configuration":"memory/config.json: agent.profiles, agent.providers, memory limits","workflow":"context -> ask -> reply if asked -> implement and test -> report; create a child thread for a distinct topic"}),
    ])
}
