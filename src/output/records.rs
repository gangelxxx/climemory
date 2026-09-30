use super::*;

pub fn serialize_records(records: &[Value]) -> Result<String> {
    let mut text = String::new();
    for value in records {
        text.push_str(&serde_json::to_string(value)?);
        text.push('\n');
    }
    Ok(text)
}

pub fn output_stats(records: &[Value]) -> Result<OutputStats> {
    let text = serialize_records(records)?;
    Ok(OutputStats {
        chars: text.chars().count(),
        bytes: text.len(),
        estimated_tokens: estimate_tokens(&text),
    })
}

/// Upper bound on the annotate fixpoint iterations; stats digits stabilize
/// after one or two inserts in practice, the bound only guards against
/// pathological oscillation.
const MAX_ANNOTATE_ITERATIONS: usize = 8;

/// Multi-summary variant of `annotate_summary` (registry item 296): one shared
/// fixpoint writes the same batch stats into every summary record, so no
/// summary publishes numbers computed before a later summary gained its stats
/// fields.
pub fn annotate_summaries(records: &mut [Value], summary_indexes: &[usize]) -> Result<OutputStats> {
    annotate_summaries_with_limit(records, summary_indexes, MAX_ANNOTATE_ITERATIONS)
}

/// Iteratively writes the batch stats into the summary record until an insert
/// stops changing them. On non-convergence within `limit` iterations the
/// freshest stats are still published and the summary carries
/// `output_stats_converged: false`; absence of that field means the published
/// numbers are exact. The returned stats always describe the final serialized
/// batch.
pub(super) fn annotate_summaries_with_limit(
    records: &mut [Value],
    summary_indexes: &[usize],
    limit: usize,
) -> Result<OutputStats> {
    let write_stats = |records: &mut [Value], stats: &OutputStats| -> Result<()> {
        for &summary_index in summary_indexes {
            let summary = records
                .get_mut(summary_index)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| AppError::new("output summary must be a JSON object"))?;
            summary.insert("output_chars".to_string(), json!(stats.chars));
            summary.insert("output_bytes".to_string(), json!(stats.bytes));
            summary.insert(
                "estimated_tokens".to_string(),
                json!(stats.estimated_tokens),
            );
        }
        Ok(())
    };
    let mut last = None;
    for _ in 0..limit {
        let stats = output_stats(records)?;
        if last == Some(stats) {
            return Ok(stats);
        }
        write_stats(records, &stats)?;
        last = Some(stats);
    }
    // Non-convergence: write the signal first so the recomputed stats account
    // for it, then publish the freshest numbers instead of leaving the summary
    // one iteration behind.
    for &summary_index in summary_indexes {
        records
            .get_mut(summary_index)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| AppError::new("output summary must be a JSON object"))?
            .insert("output_stats_converged".to_string(), json!(false));
    }
    let stats = output_stats(records)?;
    write_stats(records, &stats)?;
    output_stats(records)
}

pub fn write_records(records: &[Value]) -> Result<()> {
    if super::is_capturing() {
        super::CAPTURE.with(|c| c.borrow_mut().as_mut().unwrap().extend_from_slice(records));
        return Ok(());
    }
    let text = serialize_records(records)?;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}
