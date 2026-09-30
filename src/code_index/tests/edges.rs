use super::*;

#[test]
fn one_file_change_preserves_unrelated_materialized_edges() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let changed = temp.path().join("src/a.rs");
    fs::write(
        &changed,
        "pub fn target_a() {}\npub fn caller_a() { target_a(); }\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("src/b.rs"),
        "pub fn target_b() {}\npub fn caller_b() { target_b(); }\n",
    )
    .unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let unrelated_before: i64 = connection
        .query_row(
            "SELECT id FROM source_code_edges WHERE src_path='src/b.rs' AND dst_raw='target_b'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);

    fs::write(
        &changed,
        "pub fn target_a_v2() {}\npub fn caller_a() { target_a_v2(); }\n",
    )
    .unwrap();
    let stats = index(&project, None).unwrap();
    assert_eq!(stats.indexed, 1);
    let connection = open(&project).unwrap();
    let unrelated_after: i64 = connection
        .query_row(
            "SELECT id FROM source_code_edges WHERE src_path='src/b.rs' AND dst_raw='target_b'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let changed_edges: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_code_edges WHERE src_path='src/a.rs' AND dst_raw='target_a_v2' AND dst_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unrelated_after, unrelated_before);
    assert_eq!(changed_edges, 1);
}

#[test]
fn removing_an_ambiguous_definition_re_resolves_existing_callers() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/a.rs"), "pub fn shared_target() {}\n").unwrap();
    let removed = temp.path().join("src/b.rs");
    fs::write(&removed, "pub fn shared_target() {}\n").unwrap();
    fs::write(
        temp.path().join("src/c.rs"),
        "pub fn caller() { shared_target(); }\n",
    )
    .unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let before: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_code_edges WHERE dst_raw='shared_target' AND dst_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, 0);
    drop(connection);

    fs::remove_file(removed).unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let after: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_code_edges WHERE dst_raw='shared_target' AND dst_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, 1);
}

#[test]
fn batched_definition_removals_use_the_combined_resolution_transition() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let definitions = (0..3)
        .map(|index| temp.path().join(format!("src/definition_{index}.rs")))
        .collect::<Vec<_>>();
    for definition in &definitions {
        fs::write(definition, "pub fn shared_target() {}\n").unwrap();
    }
    fs::write(
        temp.path().join("src/caller.rs"),
        "pub fn caller() { shared_target(); }\n",
    )
    .unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let initially_resolved: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_code_edges
             WHERE dst_raw='shared_target' AND dst_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(initially_resolved, 0);
    drop(connection);

    fs::remove_file(&definitions[0]).unwrap();
    fs::remove_file(&definitions[1]).unwrap();
    let reduced = index(&project, None).unwrap();
    assert_eq!(reduced.removed, 2);
    let connection = open(&project).unwrap();
    let resolved_path: String = connection
        .query_row(
            "SELECT definition.path
             FROM source_code_edges AS edge
             JOIN source_code_symbols AS definition ON definition.id=edge.dst_id
             WHERE edge.dst_raw='shared_target'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(resolved_path, "src/definition_2.rs");
    drop(connection);

    fs::remove_file(&definitions[2]).unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let remaining_edges: i64 = connection
        .query_row(
            "SELECT count(*) FROM source_code_edges WHERE dst_raw='shared_target'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining_edges, 0);
}

#[test]
fn tsx_calls_are_owned_by_the_deepest_enclosing_callable() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    // Line order is not an ownership rule: every call below must attach to the
    // DEEPEST ENCLOSING callable, not the nearest preceding definition. Under
    // the old rule the useMemo/useCallback/useEffect bodies (lines 10-14) and
    // the JSX inline handler (line 15) were all owned by `handleClick`, and
    // the top-level `boot()` by the nearest preceding definition (`boot`).
    fs::write(
        temp.path().join("src/view.tsx"),
        "import { useEffect, useMemo, useCallback } from 'react';\n\
         \n\
         function useData() {\n\
         \x20   fetchData();\n\
         }\n\
         export function View() {\n\
         \x20   const handleClick = () => {\n\
         \x20       trackClick();\n\
         \x20   };\n\
         \x20   const data = useMemo(() => compute(), []);\n\
         \x20   const persist = useCallback(() => save(), []);\n\
         \x20   useEffect(() => {\n\
         \x20       loadData();\n\
         \x20   }, []);\n\
         \x20   return <button onClick={() => inlineSave()} />;\n\
         }\n\
         export function Sibling() {\n\
         \x20   siblingHelper();\n\
         }\n\
         function fetchData() {}\n\
         function trackClick() {}\n\
         function compute() {}\n\
         function save() {}\n\
         function loadData() {}\n\
         function siblingHelper() {}\n\
         function inlineSave() {}\n\
         function boot() {}\n\
         boot();\n",
    )
    .unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let owner_of = |dst_raw: &str, line: i64| -> Option<String> {
        connection
            .query_row(
                &format!(
                    "SELECT owner.name FROM source_code_edges AS edge \
                     LEFT JOIN source_code_symbols AS owner ON owner.id=edge.src_id \
                     WHERE edge.dst_raw='{dst_raw}' AND edge.line={line}"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(
        owner_of("fetchData", 4).as_deref(),
        Some("useData"),
        "custom hook"
    );
    assert_eq!(
        owner_of("trackClick", 8).as_deref(),
        Some("handleClick"),
        "named variable-bound arrow"
    );
    assert_eq!(
        owner_of("compute", 10).as_deref(),
        Some("View"),
        "same-line useMemo callback"
    );
    assert_eq!(
        owner_of("save", 11).as_deref(),
        Some("View"),
        "useCallback anonymous callback"
    );
    assert_eq!(
        owner_of("loadData", 13).as_deref(),
        Some("View"),
        "nested useEffect callback"
    );
    assert_eq!(
        owner_of("inlineSave", 15).as_deref(),
        Some("View"),
        "JSX inline handler"
    );
    assert_eq!(owner_of("siblingHelper", 18).as_deref(), Some("Sibling"));
    assert_eq!(
        owner_of("boot", 28),
        None,
        "top-level call keeps a NULL owner even when its target resolves"
    );
}

#[test]
fn non_callable_definitions_never_own_a_call() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    // Only callables own calls: a const/static initializer range is not a
    // callable, so `make()` in the static initializer keeps a NULL owner even
    // though the constant's range contains the reference line.
    fs::write(
        temp.path().join("src/a.rs"),
        "pub static TABLE: i32 = make();\npub fn make() -> i32 { 1 }\n",
    )
    .unwrap();
    index(&project, None).unwrap();
    let connection = open(&project).unwrap();
    let owner: Option<String> = connection
        .query_row(
            "SELECT owner.name FROM source_code_edges AS edge \
             LEFT JOIN source_code_symbols AS owner ON owner.id=edge.src_id \
             WHERE edge.dst_raw='make' AND edge.line=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        owner, None,
        "a constant initializer is outside any callable"
    );
}

#[test]
fn repeated_partial_index_reuses_initialized_derived_storage() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    let source = temp.path().join("src/scoped.rs");
    fs::write(
        &source,
        "pub fn scoped_target() {}\npub fn scoped_caller() { scoped_target(); }\n",
    )
    .unwrap();

    let first = index(&project, Some(&source)).unwrap();
    assert!(!first.complete);
    let connection = open(&project).unwrap();
    let edge_before: i64 = connection
        .query_row(
            "SELECT id FROM source_code_edges WHERE dst_raw='scoped_target'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);

    let second = index(&project, Some(&source)).unwrap();
    assert_eq!(second.action, "unchanged");
    let connection = open(&project).unwrap();
    let edge_after: i64 = connection
        .query_row(
            "SELECT id FROM source_code_edges WHERE dst_raw='scoped_target'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(edge_after, edge_before);
}
