//! Reinitializing climemory preserves user data and managed integrations.
use std::{fs, path::Path, process::Command};

fn run(root: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_cm"))
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn reinitialization_preserves_config_and_managed_markers_without_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run(root, &["init"]);
    let config_before = fs::read(root.join("memory/config.json")).unwrap();
    let agents = root.join("AGENTS.md");
    let previous = fs::read_to_string(&agents).unwrap();
    fs::write(
        &agents,
        format!("User instructions before.\n{previous}\nUser instructions after.\n"),
    )
    .unwrap();
    fs::write(root.join("memory/docs/keep.md"), "User requirements.").unwrap();
    run(root, &["init"]);
    let updated = fs::read_to_string(&agents).unwrap();
    assert_eq!(
        updated
            .matches("<!-- >>> climemory model instructions >>> -->")
            .count(),
        1
    );
    assert!(updated.starts_with("User instructions before."));
    assert!(updated.ends_with("User instructions after.\n"));
    let mcp = fs::read_to_string(root.join(".codex/config.toml")).unwrap();
    assert!(mcp.contains("# Managed by Climemory MCP."));
    assert_eq!(
        fs::read(root.join("memory/config.json")).unwrap(),
        config_before
    );
    assert_eq!(
        fs::read_to_string(root.join("memory/docs/keep.md")).unwrap(),
        "User requirements."
    );
    run(root, &["init"]);
    assert_eq!(fs::read_to_string(agents).unwrap(), updated);
}

#[test]
fn hooks_replace_and_remove_managed_handlers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run(root, &["init"]);
    run(root, &["hooks", "install", "codex"]);
    let path = root.join(".codex/hooks.json");
    let old = fs::read_to_string(&path).unwrap();
    run(root, &["hooks", "install", "codex"]);
    let updated = fs::read_to_string(&path).unwrap();
    assert_eq!(updated.matches("Climemory automatic memory").count(), 4);
    fs::write(&path, old).unwrap();
    run(root, &["hooks", "uninstall", "codex"]);
    assert!(!fs::read_to_string(path)
        .unwrap()
        .contains("automatic memory"));
}
