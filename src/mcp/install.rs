use crate::{project::Project, util::*};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use toml_edit::{value, Array, DocumentMut, Item, Table};

const MARKER: &str = "# Managed by Climemory MCP.\n";
fn update(text: &str, root: &Path, exe: &Path, timeout: u64) -> Result<String> {
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|e| AppError::new(format!("invalid .codex/config.toml: {e}")))?;
    if doc.get("mcp_servers").is_none() {
        doc["mcp_servers"] = Item::Table(Table::new());
    }
    let servers = doc["mcp_servers"]
        .as_table_like_mut()
        .ok_or_else(|| AppError::new("mcp_servers must be a table"))?;
    if let Some(existing) = servers.get("cm") {
        let managed = existing
            .as_table()
            .and_then(|t| t.decor().prefix())
            .and_then(|d| d.as_str())
            .is_some_and(|s| s.contains(MARKER.trim()));
        let legacy = existing
            .get("args")
            .and_then(Item::as_array)
            .is_some_and(|args| {
                args.get(0).and_then(|v| v.as_str()).is_some_and(|s| {
                    Path::new(s) == root.join("memory/runtime/integration/cm_mcp.py")
                }) && args.get(1).and_then(|v| v.as_str()) == Some("--project")
                    && args
                        .get(2)
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| Path::new(s) == root)
            });
        if !managed && !legacy {
            return Err(AppError::new("mcp_servers.cm already exists and is not managed by CM; existing settings were preserved"));
        }
    } else {
        servers.insert("cm", Item::Table(Table::new()));
    }
    let server = servers
        .get_mut("cm")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| AppError::new("mcp_servers.cm must be a table"))?;
    let prefix = server
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_owned();
    if !prefix.contains(MARKER.trim()) {
        server.decor_mut().set_prefix(format!("{prefix}{MARKER}"));
    }
    server["command"] = value(
        exe.to_str()
            .ok_or_else(|| AppError::new("CM executable path must be UTF-8"))?,
    );
    server["args"] = value(Array::from_iter(["--mcp"]));
    server["cwd"] = value(
        root.to_str()
            .ok_or_else(|| AppError::new("project path must be UTF-8"))?,
    );
    if !server.contains_key("enabled") {
        server["enabled"] = value(true);
    }
    if !server.contains_key("tool_timeout_sec") {
        server["tool_timeout_sec"] = value(timeout.saturating_add(45).min(i64::MAX as u64) as i64);
    }
    if !server.contains_key("tools") {
        server["tools"] = Item::Table(Table::new());
    }
    let tools = server["tools"]
        .as_table_like_mut()
        .ok_or_else(|| AppError::new("MCP tools must be a table"))?;
    if tools.get("ask").is_none() {
        tools.insert("ask", Item::Table(Table::new()));
    }
    let ask = tools
        .get_mut("ask")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| AppError::new("MCP ask settings must be a table"))?;
    if ask.get("approval_mode").is_none() {
        ask.insert("approval_mode", value("approve"));
    }
    Ok(doc.to_string())
}
fn path(root: &Path) -> Result<PathBuf> {
    let path = root.join(".codex/config.toml");
    Project::checked_path(root, &path)?;
    Ok(path)
}
fn read(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}
pub(crate) fn preflight(root: &Path) -> Result<()> {
    update(&read(&path(root)?)?, root, &std::env::current_exe()?, 300)?;
    Ok(())
}
pub(crate) fn configure(root: &Path) -> Result<()> {
    let project = Project::open(root)?;
    let path = path(root)?;
    fs::create_dir_all(path.parent().unwrap())?;
    let lock = path.with_file_name("climemory-mcp-install.lock");
    Project::checked_path(root, &lock)?;
    let _lock = FileLock::acquire(&lock, Duration::from_secs(2))?;
    let text = update(
        &read(&path)?,
        root,
        &std::env::current_exe()?,
        project.config.memory.timeout_seconds,
    )?;
    atomic_write(&path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_update_preserves_other_settings_and_disable_is_sticky() {
        let root = Path::new("/test");
        let first = update(
            "# User comment\nmodel = 'user-model'\n[mcp_servers.other]\ncommand = 'other'\n",
            root,
            Path::new("/test/cm"),
            300,
        )
        .unwrap();
        let second = update(&first, root, Path::new("/test/cm"), 300).unwrap();
        assert_eq!(first, second);
        assert!(second.contains("# User comment"));
        let commented = second.replace(MARKER, &format!("# Keep my comment\n{MARKER}"));
        assert_eq!(
            update(&commented, root, Path::new("/test/cm"), 300).unwrap(),
            commented
        );
        let disabled = second.replace("enabled = true", "enabled = false");
        assert!(update(&disabled, root, Path::new("/new/cm"), 300)
            .unwrap()
            .contains("enabled = false"));
        assert!(update(
            "[mcp_servers.cm]\ncommand='other'\n",
            root,
            Path::new("/test/cm"),
            300
        )
        .is_err());
        assert!(update("invalid toml [", root, Path::new("/test/cm"), 300).is_err());
    }
}
