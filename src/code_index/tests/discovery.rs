use super::*;

#[test]
fn source_reads_never_follow_a_replacement_link() {
    let temp = TempDir::new().unwrap();
    let source_path = temp.path().join("source.rs");
    let target = temp.path().join("outside.rs");
    fs::write(&source_path, "pub fn original() {}\n").unwrap();
    fs::write(&target, "pub fn outside() {}\n").unwrap();
    let source = source_file(temp.path(), &source_path).unwrap().unwrap();
    fs::remove_file(&source_path).unwrap();
    if !symlink_or_skip(&target, &source_path) {
        return;
    }

    assert!(read_source(&source).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "pub fn outside() {}\n");
}

#[test]
fn source_reads_reject_metadata_changed_after_discovery() {
    let temp = TempDir::new().unwrap();
    let source_path = temp.path().join("source.rs");
    fs::write(&source_path, "pub fn original() {}\n").unwrap();
    let source = source_file(temp.path(), &source_path).unwrap().unwrap();
    fs::write(&source_path, "pub fn replacement_with_another_size() {}\n").unwrap();

    let error = read_source(&source).unwrap_err();
    assert!(error.msg.contains("changed while it was being read"));
}

#[test]
fn only_the_root_managed_memory_directory_is_skipped() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src/memory")).unwrap();
    fs::write(
        temp.path().join("src/memory/cache.rs"),
        "pub fn cache() {}\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("memory/ignored.rs"),
        "pub fn ignored() {}\n",
    )
    .unwrap();
    let (sources, complete) = discover_sources(&project, None).unwrap();
    assert!(complete);
    assert!(sources
        .iter()
        .any(|source| source.path == "src/memory/cache.rs"));
    assert!(!sources
        .iter()
        .any(|source| source.path == "memory/ignored.rs"));
}

#[test]
fn discovery_respects_repository_ignore_rules() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::create_dir_all(temp.path().join("ignored/generated")).unwrap();
    fs::write(temp.path().join(".gitignore"), "ignored/\n").unwrap();
    fs::write(temp.path().join("src/kept.rs"), "pub fn kept() {}\n").unwrap();
    fs::write(
        temp.path().join("ignored/generated/dropped.rs"),
        "pub fn dropped() {}\n",
    )
    .unwrap();

    let (sources, complete) = discover_sources(&project, None).unwrap();
    assert!(complete);
    assert_eq!(
        sources
            .iter()
            .map(|source| source.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/kept.rs"]
    );
}

#[test]
fn git_discovery_matches_the_walker_and_submodules_force_fallback() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::create_dir_all(temp.path().join("ignored")).unwrap();
    fs::create_dir_all(temp.path().join("target/generated")).unwrap();
    fs::write(temp.path().join(".gitignore"), "ignored/\n").unwrap();
    fs::write(temp.path().join("src/tracked.rs"), "pub fn tracked() {}\n").unwrap();
    fs::write(
        temp.path().join("src/untracked.rs"),
        "pub fn untracked() {}\n",
    )
    .unwrap();
    fs::write(temp.path().join("src/deleted.rs"), "pub fn deleted() {}\n").unwrap();
    fs::write(temp.path().join("ignored/no.rs"), "pub fn ignored() {}\n").unwrap();
    fs::write(
        temp.path().join("target/generated/no.rs"),
        "pub fn generated() {}\n",
    )
    .unwrap();
    fs::write(temp.path().join(".hidden.rs"), "pub fn hidden() {}\n").unwrap();
    git(temp.path(), &["init", "--quiet"]);
    git(temp.path(), &["add", "src/tracked.rs", "src/deleted.rs"]);
    git(
        temp.path(),
        &[
            "add",
            "-f",
            "ignored/no.rs",
            "target/generated/no.rs",
            ".hidden.rs",
        ],
    );
    fs::remove_file(temp.path().join("src/deleted.rs")).unwrap();

    assert!(!git_discovery_is_worthwhile(temp.path()));
    let mut git_paths = git_candidate_paths(temp.path())
        .unwrap()
        .into_iter()
        .filter_map(|path| source_file(temp.path(), &path).unwrap())
        .map(|source| source.path)
        .collect::<Vec<_>>();
    git_paths.sort();
    assert_eq!(git_paths, vec!["src/tracked.rs", "src/untracked.rs"]);

    let mut walked = walk_candidate_paths(temp.path(), temp.path(), None)
        .unwrap()
        .into_iter()
        .filter_map(|path| source_file(temp.path(), &path).unwrap())
        .map(|source| source.path)
        .collect::<Vec<_>>();
    walked.sort();
    assert_eq!(walked, git_paths);

    let auto = discover_inventory(&project, None).unwrap();
    assert_eq!(auto.discovery_backend, DiscoveryBackend::Walk);

    fs::write(temp.path().join(".gitmodules"), "# conservative fallback\n").unwrap();
    assert!(git_candidate_paths(temp.path()).is_none());
    let fallback = discover_inventory(&project, None).unwrap();
    assert_eq!(fallback.discovery_backend, DiscoveryBackend::Walk);
    assert_eq!(
        fallback
            .sources
            .iter()
            .map(|source| source.path.as_str())
            .collect::<Vec<_>>(),
        git_paths.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

#[test]
fn grep_inventory_admits_text_files_under_both_backends() {
    let temp = TempDir::new().unwrap();
    let project = project(&temp);
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join(".gitignore"), "ignored.md\n").unwrap();
    fs::write(temp.path().join("src/tracked.rs"), "pub fn tracked() {}\n").unwrap();
    fs::write(temp.path().join("README.md"), "# docs\n").unwrap();
    fs::write(temp.path().join("config.yaml"), "key: value\n").unwrap();
    fs::write(temp.path().join("notes.txt"), "not admitted\n").unwrap();
    fs::write(temp.path().join("ignored.md"), "ignored doc\n").unwrap();
    git(temp.path(), &["init", "--quiet"]);
    git(temp.path(), &["add", "src/tracked.rs"]);

    // The grep flavor admits UTF-8 text config/doc files alongside
    // recognized source files; gitignored text files stay excluded and
    // non-listed extensions (*.txt) stay out under both backends.
    let expected = vec!["README.md", "config.yaml", "src/tracked.rs"];
    let mut walked = walk_candidate_paths(temp.path(), temp.path(), None)
        .unwrap()
        .into_iter()
        .filter_map(|path| source_file_admitting(temp.path(), &path, true).unwrap())
        .map(|source| source.path)
        .collect::<Vec<_>>();
    walked.sort();
    assert_eq!(walked, expected);

    let mut git_paths = git_candidate_paths(temp.path())
        .unwrap()
        .into_iter()
        .filter_map(|path| source_file_admitting(temp.path(), &path, true).unwrap())
        .map(|source| source.path)
        .collect::<Vec<_>>();
    git_paths.sort();
    assert_eq!(git_paths, expected);

    // The source-only flavor is unchanged: text files stay out.
    let inventory = discover_inventory(&project, None).unwrap();
    assert_eq!(
        inventory
            .sources
            .iter()
            .map(|source| source.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/tracked.rs"]
    );
    let grep_inventory = discover_inventory_grep(&project, None).unwrap();
    assert_eq!(
        grep_inventory
            .sources
            .iter()
            .map(|source| source.path.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    // Grep-admitted text files carry the sentinel language.
    assert!(
        grep_inventory
            .sources
            .iter()
            .find(|source| source.path == "README.md")
            .unwrap()
            .language
            == "text"
    );
}

#[test]
fn dot_ignore_control_files_force_the_complete_walker_fallback() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::create_dir_all(temp.path().join("dot-ignored")).unwrap();
    fs::write(temp.path().join(".gitignore"), ".ignore\n").unwrap();
    fs::write(temp.path().join(".ignore"), "dot-ignored/\n").unwrap();
    fs::write(temp.path().join("src/kept.rs"), "pub fn kept() {}\n").unwrap();
    fs::write(
        temp.path().join("dot-ignored/dropped.rs"),
        "pub fn dropped() {}\n",
    )
    .unwrap();
    git(temp.path(), &["init", "--quiet"]);
    git(temp.path(), &["add", "src/kept.rs"]);

    assert!(git_candidate_paths(temp.path()).is_none());
    let walked = walk_candidate_paths(temp.path(), temp.path(), None)
        .unwrap()
        .into_iter()
        .filter_map(|path| source_file(temp.path(), &path).unwrap())
        .map(|source| source.path)
        .collect::<Vec<_>>();
    assert_eq!(walked, vec!["src/kept.rs"]);
}

#[test]
fn git_ignore_classification_drains_large_output_while_writing_input() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join(".gitignore"), "ignored/\n").unwrap();
    git(temp.path(), &["init", "--quiet"]);
    let paths = (0..20_000)
        .map(|index| format!("ignored/source_{index:05}.rs"))
        .collect::<Vec<_>>();

    let ignored = git_ignored_paths(temp.path(), &paths).unwrap();

    assert_eq!(ignored.len(), paths.len());
    assert!(paths.iter().all(|path| ignored.contains(path)));
}
