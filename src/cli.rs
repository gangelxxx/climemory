use std::collections::{BTreeMap, BTreeSet};

pub(crate) const VALUE_FLAGS: &[&str] = &[
    "agent",
    "dir",
    "parent",
    "slug",
    "note",
    "with-file",
    "path",
    "glob",
    "pattern",
    "limit",
    "offset",
    "context-lines",
    "before-context",
    "after-context",
    "kind",
    "symbol-kind",
    "depth",
];

#[derive(Clone, Debug, Default)]
pub struct Parsed {
    pub positionals: Vec<String>,
    pub values: BTreeMap<String, String>,
    pub bools: BTreeSet<String>,
    occurrences: BTreeMap<String, Vec<String>>,
    // Value-flag keys in command order across all flag names; lets callers
    // pair repeated flags of different names (--replace vs --replace-file)
    // by occurrence, which per-flag `occurrences` alone cannot express.
    value_flag_order: Vec<String>,
}

impl Parsed {
    pub fn parse(args: &[String]) -> crate::util::Result<Self> {
        let mut parsed = Self::default();
        let mut index = 0;
        // A bare `--` ends option parsing (known-issues item 431): every
        // later token is a positional, so positional text starting with `--`
        // (e.g. an add-item text) parses instead of dying as an unknown
        // option.
        let mut options_ended = false;
        while index < args.len() {
            let arg = &args[index];
            if options_ended {
                parsed.positionals.push(arg.clone());
            } else if arg == "--" {
                options_ended = true;
            } else if let Some(flag) = arg.strip_prefix("--") {
                if let Some((key, value)) = flag.split_once('=') {
                    if VALUE_FLAGS.contains(&key) && value.is_empty() {
                        return Err(missing_value(key));
                    }
                    push_value(&mut parsed, key, value)?;
                } else if VALUE_FLAGS.contains(&flag) {
                    index += 1;
                    let value = next_value(args, index, flag)?;
                    push_value(&mut parsed, flag, &value)?;
                } else {
                    parsed.bools.insert(flag.to_string());
                }
            } else if matches!(arg.as_str(), "-A" | "-B" | "-C") {
                let flag = match arg.as_str() {
                    "-A" => "after-context",
                    "-B" => "before-context",
                    _ => "context-lines",
                };
                index += 1;
                let value = next_value(args, index, flag)?;
                push_value(&mut parsed, flag, &value)?;
            } else if arg == "-h" {
                parsed.bools.insert("help".to_string());
            } else if let Some(name) = windows_style_flag(arg) {
                if parsed.positionals.is_empty() {
                    parsed.positionals.push(name.to_string());
                } else if slash_option_applies(&parsed.positionals, name) {
                    if VALUE_FLAGS.contains(&name) {
                        index += 1;
                        let value = next_value(args, index, name)?;
                        push_value(&mut parsed, name, &value)?;
                    } else {
                        parsed.bools.insert(name.to_string());
                    }
                } else {
                    // '/name' that names no valid option for the resolved
                    // command is positional text (a slash literal such as
                    // '/api/v1'), never an unknown option.
                    parsed.positionals.push(arg.clone());
                }
            } else {
                parsed.positionals.push(arg.clone());
            }
            index += 1;
        }
        Ok(parsed)
    }

    pub fn word(&self, index: usize) -> Option<&str> {
        self.positionals.get(index).map(String::as_str)
    }

    #[cfg(test)]
    pub fn command(&self) -> Option<&str> {
        self.positionals.first().map(String::as_str)
    }

    pub fn arg(&self, index: usize) -> Option<&str> {
        self.positionals.get(index + 1).map(String::as_str)
    }

    pub fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Every occurrence of a repeated value flag, in command order.
    pub fn values_all(&self, key: &str) -> &[String] {
        self.occurrences.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Value-flag keys in command order across all flag names.
    pub fn value_flag_order(&self) -> &[String] {
        &self.value_flag_order
    }

    pub fn required_value(&self, key: &str, example: &str) -> crate::util::Result<&str> {
        match self.value(key) {
            Some(value) if !value.is_empty() => Ok(value),
            // Flag present but empty: the value is the problem.
            Some(_) => Err(crate::util::AppError::with_hint(
                format!("--{key} requires a value"),
                example,
            )),
            // Flag absent entirely: name the omission, not the value, so the
            // caller adds the flag instead of re-checking its quoting.
            None => Err(crate::util::AppError::with_hint(
                format!("missing required --{key}"),
                example,
            )),
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.bools.contains(key)
    }
}

fn next_value(args: &[String], index: usize, flag: &str) -> crate::util::Result<String> {
    match args.get(index) {
        Some(value) if value.starts_with("--") => Err(missing_value_option_like(flag, value)),
        Some(value) => Ok(value.clone()),
        None => Err(missing_value(flag)),
    }
}

/// Value flags that legitimately repeat: the `--replace`/`--with` mutation
/// family is read pairwise via `values_all`/`value_flag_order`, `--replace-line`
/// repeats as independent needles applied in order in one mutation (registry
/// item 341), the comma-list flags are split on ',' at the read site, so a
/// repeat is equivalent to one longer list, `--pattern` is read via
/// `values_all` as independent needles batched into labeled windows
/// (known-issues item 259), `--diff-path` repeats to scope
/// --evidence-from-diff to several files (registry item 506), and
/// `--path`/`--glob` repeat as code grep scope selectors read in
/// command-line order via `value_flag_order` (the
/// code-grep-path-scopes-slash-literals Proposal). `state batch` reads
/// repeated --note via `values_all` as ordered `<kind>:<text>` entries; the
/// other state commands keep the single-value contract via a command-level
/// guard (same error text as the parse-level one). Repeating any
/// other value flag silently comma-joins into a value that cannot match
/// anything, so parse rejects it naming the flag (known-issues item 256).
const REPEATABLE_VALUE_FLAGS: &[&str] = &["path", "glob", "pattern"];

fn push_value(parsed: &mut Parsed, key: &str, value: &str) -> crate::util::Result<()> {
    if parsed.occurrences.contains_key(key) && !REPEATABLE_VALUE_FLAGS.contains(&key) {
        return Err(crate::util::AppError::with_hint(
            format!("--{key} accepts a single value but was passed more than once"),
            format!("cm <command> --{key}=<value>"),
        ));
    }
    parsed
        .occurrences
        .entry(key.to_string())
        .or_default()
        .push(value.to_string());
    parsed.value_flag_order.push(key.to_string());
    match parsed.values.get_mut(key) {
        Some(existing) => {
            existing.push(',');
            existing.push_str(value);
        }
        None => {
            parsed.values.insert(key.to_string(), value.to_string());
        }
    }
    Ok(())
}

fn missing_value(flag: &str) -> crate::util::AppError {
    crate::util::AppError::with_hint(
        format!("--{flag} requires a value"),
        format!("cm <command> --{flag}=<value>"),
    )
}

/// The token after a value flag looks like another option. Name the `=` form
/// in the message itself, not only in the hint: a value that legitimately
/// starts with `--` is otherwise rejected with no visible way forward. Also
/// name the `--` positional escape: when the flag token itself was meant as
/// positional text (a search query like '--area'), the requires-a-value
/// wording alone points at the wrong fix.
fn missing_value_option_like(flag: &str, token: &str) -> crate::util::AppError {
    crate::util::AppError::with_hint(
        format!(
            "--{flag} requires a value; the next token '{token}' looks like an option — use --{flag}=<value> for values starting with --, or `-- --{flag}` when '--{flag}' itself is positional text (e.g. a search query)"
        ),
        format!("cm <command> --{flag}=<value>"),
    )
}

#[cfg(windows)]
fn windows_style_flag(arg: &str) -> Option<&str> {
    arg.strip_prefix('/')
}

/// Command-aware Windows slash-option recognition (the
/// code-grep-path-scopes-slash-literals Proposal): a '/name' token parses as
/// a Windows-style option only when `name` is a valid option for the
/// resolved command; otherwise the token stays positional text, so a slash
/// literal like '/api/v1' needs no hidden '--' workaround while recognized
/// '/output', '/dir' forms keep working. The command is resolved from the
/// positionals already seen, longest registered prefix first (the
/// 'code <sub>'/'thread <sub>' dispatch keys go up to three words); shares
/// the help option registry with validate_command_options so parse-time
/// recognition and post-parse validation never disagree. 'help' parses as an
/// option on every command (validate_command_options skips it), so '/help'
/// keeps working anywhere; '--' remains the universal escape hatch.
/// Compiled on every platform: the call site sits inside the
/// windows_style_flag branch, which is inert but still type-checked off
/// Windows.
fn slash_option_applies(positionals: &[String], name: &str) -> bool {
    if name == "help" {
        return true;
    }
    (1..=positionals.len().min(3))
        .rev()
        .any(|len| crate::help::is_registered_option(&positionals[..len].join(" "), name))
}

#[cfg(not(windows))]
fn windows_style_flag(_arg: &str) -> Option<&str> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> crate::util::Result<Parsed> {
        Parsed::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }
    #[test]
    fn separator_and_repeated_search_scopes_preserve_text_and_order() {
        let p = parse(&[
            "code",
            "grep",
            "--path",
            "src",
            "--glob",
            "*.rs",
            "--path",
            "tests",
            "--",
            "--literal",
        ])
        .unwrap();
        assert_eq!(p.arg(1), Some("--literal"));
        assert_eq!(p.values_all("path"), ["src", "tests"]);
        assert_eq!(p.value_flag_order(), ["path", "glob", "path"]);
        crate::help::validate_command_options("code grep", &p).unwrap();
    }
    #[test]
    fn invalid_flags_are_rejected_without_ambiguous_values() {
        for args in [
            vec!["create", "Title", "--parent"],
            vec!["create", "Title", "--agent", "--help"],
            vec!["create", "Title", "--slug=a", "--slug=b"],
        ] {
            assert!(parse(&args).is_err());
        }
        for args in [
            vec!["context", "task", "--view=current"],
            vec!["code", "grep", "text", "--regex=false"],
            vec!["context", "task", "--budget-tokens=20"],
        ] {
            let p = parse(&args).unwrap();
            let command = if p.command() == Some("code") {
                "code grep"
            } else {
                "context"
            };
            assert!(crate::help::validate_command_options(command, &p).is_err());
        }
    }
}
