use super::*;

const LABEL: &str = "Climemory automatic memory";
const EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "Stop", "PreCompact"];

pub(super) fn configure(enable: bool) -> Result<()> {
    let root = crate::chat::project_root()?;
    let dir = root.join(".codex");
    Project::checked_path(&root, &dir)?;
    fs::create_dir_all(&dir)?;
    let path = checked(&dir, "hooks.json")?;
    let _lock = FileLock::acquire(
        &checked(&dir, "climemory-install.lock")?,
        Duration::from_secs(2),
    )?;
    let mut config: Value = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        json!({})
    };
    update(&mut config, &std::env::current_exe()?, enable)?;
    atomic_write(&path, &serde_json::to_vec_pretty(&config)?)?;
    println!(
        "{}",
        json!({"status":if enable {"installed"} else {"uninstalled"},"path":path,"note":"Codex must trust project hooks before running them. Existing hooks were preserved."})
    );
    Ok(())
}

fn update(config: &mut Value, exe: &Path, enable: bool) -> Result<()> {
    let object = config
        .as_object_mut()
        .ok_or_else(|| AppError::new("hooks.json must be an object"))?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| AppError::new("hooks must be an object"))?;
    let exe = exe
        .to_str()
        .ok_or_else(|| AppError::new("hook executable path must be UTF-8"))?;
    let shell_path = if cfg!(windows) {
        exe.replace('\\', "/")
    } else {
        exe.to_owned()
    };
    let command = format!("'{}' hooks codex", shell_path.replace('\'', "'\"'\"'"));
    let windows = format!("& '{}' hooks codex", exe.replace('\'', "''"));
    let encoded = base64(
        &windows
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    for event in EVENTS {
        let groups = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| AppError::new(format!("{event} hooks must be an array")))?;
        // Remove just our handlers, including when somebody has grouped them
        // with an unrelated hook. Never drop the entire matcher group.
        groups.retain_mut(|group| {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = handlers.len();
                handlers.retain(|handler| handler["statusMessage"] != LABEL);
                return before == handlers.len() || !handlers.is_empty();
            }
            true
        });
        if enable {
            groups.push(json!({"hooks":[{"type":"command","command":command,
                "commandWindows":format!("powershell.exe -NoProfile -NonInteractive -EncodedCommand {encoded}"),
                "statusMessage":LABEL,"timeout":if *event == "UserPromptSubmit" {120} else {10},
                "additionalContextLimit":2500}]}));
        }
    }
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let value = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(TABLE[((value >> 18) & 63) as usize] as char);
        out.push(TABLE[((value >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn install_is_idempotent_and_uninstall_preserves_other_handlers() {
        let other = json!({"type":"command","command":"echo existing"});
        let mut config = json!({"description":"keep","hooks":{"Stop":[{"hooks":[other.clone()]}]}});
        update(&mut config, Path::new("C:/space and 'quotes'/cm.exe"), true).unwrap();
        let once = config.clone();
        update(&mut config, Path::new("C:/space and 'quotes'/cm.exe"), true).unwrap();
        assert_eq!(config, once);
        config["hooks"]["Stop"][1]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(other.clone());
        update(&mut config, Path::new("cm"), false).unwrap();
        assert_eq!(config["description"], "keep");
        assert_eq!(config["hooks"]["Stop"][0]["hooks"][0], other);
        assert_eq!(config["hooks"]["Stop"][1]["hooks"][0], other);
    }
    #[test]
    fn encoding_and_invalid_config() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert!(update(&mut json!({"hooks":[]}), Path::new("cm"), true).is_err());
    }
}
