use super::*;

#[cfg(test)]
pub fn status(project: &Project) -> Result<CodeStatus> {
    let inventory = discover_inventory(project, None)?;
    status_inventory(project, &inventory)
}

/// Registry item 154: resolve a mutation-time `#<symbol>` code-embed anchor
/// to its line extent inside `path` (a project-relative source path). The
/// extent runs from the symbol's definition line through the line before the
/// next symbol in the same file, or to the file end. The index must be fresh:
/// a missing or stale index fails with an actionable `cm code index` hint,
/// zero matches hint `cm code context "<symbol>"`, and several same-name
/// definitions fail naming the candidate lines so the model disambiguates
/// with a `#Lstart-Lend` range. Nothing here writes: resolution only reads.
#[cfg(test)]
pub fn symbol_anchor_extent(project: &Project, path: &str, name: &str) -> Result<(usize, usize)> {
    let status = status(project)?;
    if !status.fresh {
        return Err(AppError::with_hint(
            format!(
                "source-code index is {}; cannot resolve symbol anchor '#{name}'",
                status.state
            ),
            format!("run `cm code index` first, or cite a line range: code:{path}#L10-L35"),
        ));
    }
    let connection = open_for_read(project)?;
    let mut statement = connection
        .prepare("SELECT line FROM source_code_symbols WHERE path=?1 AND name=?2 ORDER BY line")?;
    let lines = statement
        .query_map(params![path, name], |row| row.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    match lines.len() {
        0 => {
            // Registry item 413: name up to 3 indexed symbols of the cited
            // file (nearest by levenshtein, the topic_suggestions pattern) so
            // the repair is zero-round. The candidates ride the hint text
            // (survives the refs/parse.rs anchor re-wrap) and a structured
            // `indexed_symbols` extra on the model-mode error record.
            let mut candidates = connection.prepare(
                "SELECT name, line FROM source_code_symbols WHERE path=?1 ORDER BY name, line",
            )?;
            let mut nearest = candidates
                .query_map(params![path], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            // Same-name overloads are consecutive under ORDER BY name, line;
            // keep the earliest line so 3 slots name 3 distinct symbols.
            nearest.dedup_by(|current, previous| current.0 == previous.0);
            // Precompute the distance once per symbol (the topic_suggestions
            // pattern) instead of twice per comparison.
            let mut ranked = nearest
                .into_iter()
                .map(|(candidate, line)| {
                    (crate::util::levenshtein(name, &candidate), candidate, line)
                })
                .collect::<Vec<_>>();
            ranked.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
            ranked.truncate(3);
            let nearest = ranked
                .into_iter()
                .map(|(_, candidate, line)| (candidate, line))
                .collect::<Vec<_>>();
            let remedies = format!(
                "locate it with `cm code context \"{name}\"`, or cite a line range: code:{path}#L10-L35"
            );
            let hint = if nearest.is_empty() {
                remedies
            } else {
                format!(
                    "indexed symbols in {path}: {}; {}",
                    nearest
                        .iter()
                        .map(|(candidate, line)| format!("'{candidate}' (L{line})"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    remedies
                )
            };
            let mut error = AppError::with_hint(
                format!("no symbol named '{name}' is indexed in {path}"),
                hint,
            );
            if !nearest.is_empty() {
                error = error.with_extra(
                    "indexed_symbols",
                    json!(nearest
                        .iter()
                        .map(|(candidate, line)| json!({ "name": candidate, "line": line }))
                        .collect::<Vec<_>>()),
                );
            }
            Err(error)
        }
        1 => {
            let start = lines[0] as usize;
            let mut next = connection
                .prepare("SELECT MIN(line) FROM source_code_symbols WHERE path=?1 AND line>?2")?;
            let following = next
                .query_row(params![path, lines[0]], |row| row.get::<_, Option<i64>>(0))?
                .map(|line| line as usize);
            let end = match following {
                Some(line) => line - 1,
                None => {
                    fs::read_to_string(project.root.join(path)).map(|text| text.lines().count())?
                }
            };
            Ok((start, end.max(start)))
        }
        _ => Err(AppError::with_hint(
            format!(
                "symbol anchor '#{name}' is ambiguous in {path}: candidates at lines {}",
                lines
                    .iter()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!("disambiguate with a line range: code:{path}#L10-L35"),
        )),
    }
}

#[cfg(test)]
fn status_inventory(project: &Project, inventory: &SourceInventory) -> Result<CodeStatus> {
    let connection = open_for_read(project)?;
    status_inventory_from_connection(project, &connection, inventory)
}

pub(super) fn status_inventory_from_connection(
    project: &Project,
    connection: &Connection,
    inventory: &SourceInventory,
) -> Result<CodeStatus> {
    let transaction = connection.unchecked_transaction()?;
    let status = read_status_from_connection(&transaction, inventory)?;
    transaction.commit()?;
    if status.fresh && inventory.content_epoch.is_some() {
        write_scan_lease_best_effort(project, inventory);
    }
    Ok(status)
}

fn read_status_from_connection(
    connection: &Connection,
    inventory: &SourceInventory,
) -> Result<CodeStatus> {
    let corpus_epoch = meta(connection, "corpus_epoch")?;
    let counts = counts_for_read(connection)?;
    let stored_scan_epoch = meta(connection, "scan_epoch")?;
    let initialized = stored_scan_epoch.is_some();
    let metadata_fresh = stored_scan_epoch.as_deref() == Some(inventory.scan_epoch.as_str());
    let content_fresh = inventory
        .content_epoch
        .as_ref()
        .is_none_or(|current| corpus_epoch.as_ref() == Some(current));
    let fresh = metadata_fresh && content_fresh;
    Ok(CodeStatus {
        state: if !initialized {
            "missing"
        } else if fresh {
            "fresh"
        } else {
            "stale"
        },
        fresh,
        files: counts.0,
        symbols: counts.1,
        edges: counts.2,
        chunks: counts.3,
        corpus_epoch,
        stored_scan_epoch,
        current_scan_epoch: inventory.scan_epoch.clone(),
    })
}
