use crate::cli::Parsed;
use crate::code_index;
use crate::output::write_search_records;
use crate::project::Project;
use crate::util::{AppError, Result};

pub fn run(parsed: &Parsed, project: &Project) -> Result<()> {
    if let Some(sub) = parsed.arg(0) {
        crate::help::validate_command_options(&format!("code {sub}"), parsed)?;
    }
    match parsed.arg(0) {
        Some("find") => find(parsed, project),
        Some("grep") => grep(parsed, project),
        Some(command) => Err(AppError::with_hint(
            format!("unknown code command '{command}'"),
            "cm help code",
        )),
        None => crate::help::print_focused(
            &["code"],
            parsed.value("output"),
            parsed.value("flag"),
            parsed.has("compact"),
        ),
    }
}

fn find(parsed: &Parsed, project: &Project) -> Result<()> {
    reject_extra_argument(parsed, 2, "cm code find <name>")?;
    let name = parsed
        .arg(1)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            AppError::with_hint(
                "code find requires a non-empty symbol name",
                "cm code find <name>",
            )
        })?;
    let presentation = search_presentation(parsed)?;
    let transitive = parsed.has("transitive");
    let depth = parse_bounded(parsed.value("depth"), 2, 1, 4, "--depth")?;
    if parsed.value("depth").is_some() && !transitive {
        return Err(AppError::with_hint(
            "--depth requires --transitive",
            "cm code find <name> --transitive --depth 2",
        ));
    }
    let freshness = code_index::FreshnessMode::parse(parsed.value("freshness"), "--freshness")?;
    // Definition disambiguation filters: --path repeats as a union of
    // substrings on the project-relative definition path ('/' separators on
    // every platform), --symbol-kind exact-matches the extracted definition
    // kind. Both filter definition rows only; --kind stays the edge filter.
    let path_filters: Vec<String> = parsed.values_all("path").to_vec();
    if path_filters.iter().any(|path| path.is_empty()) {
        return Err(AppError::with_hint(
            "--path must not be empty",
            "cm code find <name> --path src/main.rs",
        ));
    }
    let symbol_kind = parsed.value("symbol-kind");
    // Continuations preserve selection and advance to the next page.
    let continuation = code_index::FindContinuationSpec {
        kind: parsed.value("kind").map(str::to_string),
        paths: path_filters.clone(),
        symbol_kind: symbol_kind.map(str::to_string),
        transitive,
        explicit_depth: parsed.value("depth").map(|_| depth),
        explicit_freshness: parsed.value("freshness").map(str::to_string),
        full_root: Some(project.root.to_string_lossy().into_owned()),
    };
    let result = code_index::find(
        project,
        name,
        true,
        freshness,
        presentation.clone(),
        parsed.has("count"),
        parsed.value("kind"),
        &path_filters,
        symbol_kind,
        transitive,
        depth,
        Some(continuation),
    )?;
    let mut records = result.records(name);
    presentation.add_context(project, &mut records);
    write_search_records(&mut records, &[0])
}

fn grep(parsed: &Parsed, project: &Project) -> Result<()> {
    // Registry item 536: --pattern repeats to batch several needles into one
    // scan with per-needle labeled groups (the thread-get --pattern
    // precedent); it never mixes with the positional literal.
    let patterns = parsed.values_all("pattern");
    if patterns.is_empty() {
        reject_extra_argument(parsed, 2, "cm code grep <literal>")?;
    } else {
        reject_extra_argument(
            parsed,
            1,
            "cm code grep --pattern <needle> [--pattern <needle>...] (the positional literal and --pattern do not mix)",
        )?;
    }
    let ignore_case = parsed.has("ignore-case");
    let queries: Vec<String> = if patterns.is_empty() {
        let query = parsed
            .arg(1)
            .filter(|query| !query.is_empty())
            .ok_or_else(|| {
                AppError::with_hint(
                    "code grep requires a non-empty literal or --regex pattern",
                    "cm code grep <literal>",
                )
            })?;
        vec![query.to_string()]
    } else {
        if patterns.iter().any(|needle| needle.is_empty()) {
            return Err(AppError::with_hint(
                "--pattern must not be empty",
                "cm code grep --pattern <needle>",
            ));
        }
        // Duplicate needles would emit identical labeled groups; dedupe on
        // the effective needle (the literal-mode lowercase fold; --regex
        // --ignore-case folds more, so fold-equivalent regex needles still
        // emit their own groups), keeping first-occurrence order.
        let mut seen = std::collections::HashSet::new();
        patterns
            .iter()
            .filter(|needle| {
                seen.insert(if ignore_case {
                    needle.to_lowercase()
                } else {
                    needle.to_string()
                })
            })
            .cloned()
            .collect()
    };
    let regex_mode = parsed.has("regex");
    let presentation = search_presentation(parsed)?;
    // Continuations preserve selection and advance to the next page.
    let continuation = code_index::GrepContinuationSpec {
        pattern_mode: !patterns.is_empty(),
        full_root: Some(project.root.to_string_lossy().into_owned()),
    };
    // Proposal code-grep-path-scopes-slash-literals: --path/--glob repeat as
    // scope selectors and interleave in command-line order (value_flag_order
    // pairs each flag name with its occurrence), so the scan layer compiles
    // one gitignore-precedence filter — a negated glob can carve matches out
    // of a path-selected subtree and a later positive glob re-includes them.
    let mut path_values = parsed.values_all("path").iter();
    let mut glob_values = parsed.values_all("glob").iter();
    let mut selectors = Vec::new();
    for flag in parsed.value_flag_order() {
        match flag.as_str() {
            "path" => {
                let path = path_values
                    .next()
                    .expect("value_flag_order pairs with values_all occurrences");
                if path.is_empty() {
                    return Err(AppError::with_hint(
                        "--path must not be empty",
                        "cm code grep <literal> --path src/main.rs",
                    ));
                }
                selectors.push(code_index::GrepSelector::Path(path.clone()));
            }
            "glob" => selectors.push(code_index::GrepSelector::Glob(
                glob_values
                    .next()
                    .expect("value_flag_order pairs with values_all occurrences")
                    .clone(),
            )),
            _ => {}
        }
    }
    let scope = code_index::GrepScope::new(selectors);
    let result = code_index::grep(
        project,
        &queries,
        ignore_case,
        regex_mode,
        &scope,
        presentation.clone(),
        parsed.has("count"),
        code_index::GrepModes {
            invert_match: parsed.has("invert-match"),
            multiline: parsed.has("multiline"),
            count_by_file: parsed.has("count-by-file"),
        },
        Some(continuation),
    )?;
    let mut records = result.records(ignore_case, &scope);
    presentation.add_context(project, &mut records);
    // One shared fixpoint annotates every per-needle summary/count record
    // (the item-296 multi-summary pattern); a single needle annotates record
    // 0 exactly as before.
    let summary_indexes = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(
                record["record"].as_str(),
                Some("code_grep_summary") | Some("code_grep_count")
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    write_search_records(&mut records, &summary_indexes)
}

fn search_presentation(parsed: &Parsed) -> Result<code_index::SearchPresentation> {
    let limit = Some(parse_bounded(
        parsed.value("limit"),
        50,
        1,
        1000,
        "--limit",
    )?);
    let context = parse_bounded(parsed.value("context-lines"), 0, 0, 50, "--context-lines")?;
    Ok(code_index::SearchPresentation {
        limit,
        offset: parse_bounded(parsed.value("offset"), 0, 0, usize::MAX, "--offset")?,
        sort: code_index::SearchSort::parse(parsed.value("sort"))?,
        reverse: parsed.has("reverse"),
        before: parse_bounded(
            parsed.value("before-context"),
            context,
            0,
            50,
            "--before-context",
        )?,
        after: parse_bounded(
            parsed.value("after-context"),
            context,
            0,
            50,
            "--after-context",
        )?,
    })
}

fn parse_bounded(
    raw: Option<&str>,
    default: usize,
    minimum: usize,
    maximum: usize,
    flag: &str,
) -> Result<usize> {
    let value = match raw {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| AppError::new(format!("{flag} must be an integer")))?,
        None => default,
    };
    if !(minimum..=maximum).contains(&value) {
        return Err(AppError::new(format!(
            "{flag} must be between {minimum} and {maximum}"
        )));
    }
    Ok(value)
}

fn reject_extra_argument(parsed: &Parsed, first_extra: usize, usage: &str) -> Result<()> {
    if let Some(argument) = parsed.arg(first_extra) {
        return Err(AppError::with_hint(
            format!("unexpected argument '{argument}'"),
            usage,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_values_reject_invalid_and_out_of_range_input() {
        assert_eq!(parse_bounded(None, 20, 1, 200, "--limit").unwrap(), 20);
        assert!(parse_bounded(Some("x"), 20, 1, 200, "--limit").is_err());
        assert!(parse_bounded(Some("0"), 20, 1, 200, "--limit").is_err());
    }
}
