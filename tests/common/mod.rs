#![allow(dead_code)]
use assert_cmd::Command;
use std::path::{Path, PathBuf};
pub fn run(root: &Path, args: &[&str], stdin: &str) -> assert_cmd::assert::Assert {
    Command::cargo_bin("cm-internal-tests")
        .unwrap()
        .arg("--dir")
        .arg(root)
        .args(args)
        .write_stdin(stdin.to_string())
        .assert()
}
pub fn run_raw(args: &[&str], stdin: &str) -> assert_cmd::assert::Assert {
    Command::cargo_bin("cm-internal-tests")
        .unwrap()
        .args(args)
        .write_stdin(stdin.to_string())
        .assert()
}
pub fn init(root: &Path) {
    run_raw(&["init", root.to_str().unwrap()], "").success();
}
pub fn thread_path(root: &Path, id: &str) -> PathBuf {
    root.join("memory/threads")
        .join(&id[..2])
        .join(format!("{id}.md"))
}
