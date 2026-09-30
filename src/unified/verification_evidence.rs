//! Complete redundant selection metadata only from independently validated originals.
use super::*;

pub(super) fn complete_selection(a: &mut Assembly, reviewed: &BTreeSet<String>) -> Result<()> {
    for (references, category) in std::iter::once((&a.select, "invalid verification references"))
        .chain(
            a.aspects
                .iter()
                .map(|aspect| (&aspect.evidence, "aspect lacks verified evidence")),
        )
        .chain(
            a.conflicts
                .iter()
                .map(|conflict| (&conflict.evidence, "invalid conflict evidence")),
        )
    {
        if references.iter().any(|id| !reviewed.contains(id)) {
            return Err(AppError::new(format!(
                "unified protocol: {category}: originals not supplied to this call: {:?}",
                references
                    .iter()
                    .filter(|id| !reviewed.contains(*id))
                    .take(8)
                    .collect::<Vec<_>>()
            )));
        }
    }
    let selected: BTreeSet<_> = a
        .select
        .iter()
        .chain(a.aspects.iter().flat_map(|aspect| &aspect.evidence))
        .chain(a.conflicts.iter().flat_map(|conflict| &conflict.evidence))
        .cloned()
        .collect();
    // `select` repeats citations already present on aspects/conflicts. An omitted
    // duplicate is not a reason to regenerate the answer. Validate against this
    // call's originals, never the broader index or an unopened thread's passport.
    a.select = selected.into_iter().collect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(select: Value, aspect: Value, conflict: Value) -> Assembly {
        serde_json::from_value(json!({
            "answer":"A reported discrepancy remains.", "select":select, "need":[],
            "aspects":[{"question":"What is required?", "status":"found", "evidence":aspect}],
            "conflicts":[{"kind":"implementation_discrepancy", "description":"Reported differently", "evidence":conflict}]
        })).unwrap()
    }

    #[test]
    fn omitted_duplicate_citations_are_restored_without_changing_claims() {
        let mut a = answer(
            json!(["doc:1"]),
            json!(["doc:2"]),
            json!(["memo:1", "doc:2"]),
        );
        let reviewed = ["doc:1", "doc:2", "memo:1"].map(str::to_owned).into();
        complete_selection(&mut a, &reviewed).unwrap();
        assert_eq!(a.select, ["doc:1", "doc:2", "memo:1"]);
        assert_eq!(a.aspects[0].evidence, ["doc:2"]);
        assert_eq!(a.conflicts[0].evidence, ["memo:1", "doc:2"]);
        assert_eq!(a.aspects[0].status, "found");
    }

    #[test]
    fn unopened_or_fabricated_citations_are_rejected_in_every_location() {
        let reviewed = ["doc:1".to_owned()].into();
        for (select, aspect, conflict) in [
            (json!(["unopened:1"]), json!(["doc:1"]), json!(["doc:1"])),
            (json!(["doc:1"]), json!(["unopened:1"]), json!(["doc:1"])),
            (json!(["doc:1"]), json!(["doc:1"]), json!(["unopened:1"])),
        ] {
            let mut a = answer(select, aspect, conflict);
            let before = a.select.clone();
            assert!(complete_selection(&mut a, &reviewed).is_err());
            assert_eq!(a.select, before);
        }
    }
}
