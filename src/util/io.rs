use super::*;

/// Decode a piped/redirected body to Unicode (Proposal windows-utf8-cli-io):
/// UTF-8 is the byte contract on every platform, with or without a BOM;
/// BOM-marked UTF-16LE/BE (legacy Windows PowerShell output) is transcoded
/// explicitly; anything else fails BEFORE any mutation with an
/// encoding-specific remedy. `source` names the origin ("stdin" or
/// "--with-file 'path'") for the error text.
pub fn decode_body_bytes(bytes: &[u8], source: &str) -> Result<String> {
    const UTF8_REMEDY: &str =
        "save the body as UTF-8 (in PowerShell: Set-Content body.md -Encoding utf8) and retry";
    const UTF16_REMEDY: &str =
        "the bytes look like UTF-16 without a BOM; save the body as UTF-8 or keep the BOM \
         (PowerShell `>` / Out-File output already carries one)";
    let text = match bytes {
        // UTF-8 BOM: strip it; normalize_newlines strips a leading U+FEFF
        // too, but an explicit strip keeps the validation error offsets
        // honest and the intent visible.
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8(rest.to_vec()).map_err(|error| {
            AppError::with_hint(
                format!("{source} body is not valid UTF-8 after its BOM: {error}"),
                UTF8_REMEDY,
            )
        })?,
        // BOM-marked UTF-16LE (Windows PowerShell `>` / Out-File default).
        [0xFF, 0xFE, rest @ ..] => decode_utf16(rest, false, source)?,
        // BOM-marked UTF-16BE.
        [0xFE, 0xFF, rest @ ..] => decode_utf16(rest, true, source)?,
        _ => String::from_utf8(bytes.to_vec()).map_err(|error| {
            AppError::with_hint(
                format!("{source} body is not valid UTF-8: {error}"),
                UTF8_REMEDY,
            )
        })?,
    };
    // A BOM-less UTF-16 body whose text contains any ASCII (the markdown
    // scaffolding of a real body guarantees it) is byte-valid UTF-8 riddled
    // with NULs; storing it would corrupt the source silently. Reject with
    // the encoding-specific remedy. (Pure non-ASCII BOM-less UTF-16 carries
    // no NULs and is indistinguishable from legitimate text — undetectable
    // without false positives.)
    if text.contains('\0') {
        return Err(AppError::with_hint(
            format!("{source} body contains NUL bytes"),
            UTF16_REMEDY,
        ));
    }
    Ok(text)
}

/// Transcode BOM-marked UTF-16 body bytes (LE unless `big_endian`).
fn decode_utf16(rest: &[u8], big_endian: bool, source: &str) -> Result<String> {
    if !rest.len().is_multiple_of(2) {
        return Err(AppError::with_hint(
            format!("{source} UTF-16 body has an odd byte count"),
            "the pipe truncated the body mid-code-unit; re-run with the complete body",
        ));
    }
    let units: Vec<u16> = rest
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if big_endian {
                u16::from_be_bytes([pair[0], pair[1]])
            } else {
                u16::from_le_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    String::from_utf16(&units).map_err(|error| {
        AppError::with_hint(
            format!("{source} UTF-16 body has an unpaired surrogate: {error}"),
            "save the body as UTF-8 and retry",
        )
    })
}

/// Proposal windows-utf8-cli-io: point an attached Windows console at UTF-8
/// before argument parsing or diagnostics, so Cyrillic input echoes and
/// output renders predictably. Redirected handles keep the byte contract
/// (UTF-8 bytes regardless of the console code page) and are never touched.
/// No-op off Windows.
#[cfg(windows)]
pub fn init_console_utf8() {
    use std::os::windows::io::AsRawHandle;
    const CP_UTF8: u32 = 65001;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut u32) -> i32;
        fn SetConsoleCP(code_page: u32) -> i32;
        fn SetConsoleOutputCP(code_page: u32) -> i32;
    }
    fn is_console(handle: *mut core::ffi::c_void) -> bool {
        let mut mode = 0u32;
        unsafe { GetConsoleMode(handle, &mut mode) != 0 }
    }
    if is_console(std::io::stdin().as_raw_handle()) {
        unsafe {
            SetConsoleCP(CP_UTF8);
        }
    }
    if is_console(std::io::stdout().as_raw_handle())
        || is_console(std::io::stderr().as_raw_handle())
    {
        unsafe {
            SetConsoleOutputCP(CP_UTF8);
        }
    }
}

/// See the Windows variant; off Windows the UTF-8 byte contract needs no
/// console setup.
#[cfg(not(windows))]
pub fn init_console_utf8() {}

pub fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_start_matches('\u{feff}')
        .to_string()
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::new(format!("path has no parent: {}", path.display())))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("write"),
        fresh_id()
    ));
    write_tmp(&tmp, bytes)?;
    if let Err(error) = replace_file(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// Registry item 564: a mid-write/sync failure removes the tmp file instead
/// of leaking it (previously only a replace failure cleaned up, so repeated
/// failed init re-runs accumulated tmp files under the target dir). An OPEN
/// failure (e.g. a create_new collision) removes nothing — that tmp is not
/// ours. The file is closed before the remove: Windows cannot delete an
/// open file.
fn write_tmp(tmp: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(tmp)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(tmp);
            Err(error.into())
        }
    }
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let from_w: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_w: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // A reader without FILE_SHARE_DELETE can briefly prevent an atomic rename.
    // Keep the old destination intact; never fall back to truncating it.
    for attempt in 0..7 {
        let ok = unsafe {
            MoveFileExW(
                from_w.as_ptr(),
                to_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if attempt == 6 || !matches!(error.raw_os_error(), Some(5 | 32 | 33)) {
            return Err(error.into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10 << attempt));
    }
    unreachable!()
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    Ok(())
}

#[cfg(feature = "code-index")]
pub fn preview(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        flat
    } else {
        let mut out = flat
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn atomic_replace_retries_readers_but_preserves_old_file_on_timeout() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("statistics.json");
        atomic_write(&path, b"old").unwrap();
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(70));
            drop(reader);
        });
        atomic_write(&path, b"new").unwrap();
        release.join().unwrap();
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        assert!(atomic_write(&path, b"lost").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        drop(reader);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn decode_body_bytes_accepts_utf8_with_and_without_bom() {
        let cyrillic = "Привет, мир";
        assert_eq!(
            decode_body_bytes(cyrillic.as_bytes(), "stdin").unwrap(),
            cyrillic
        );
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(cyrillic.as_bytes());
        assert_eq!(decode_body_bytes(&bom, "stdin").unwrap(), cyrillic);
    }

    #[test]
    fn decode_body_bytes_transcodes_bom_marked_utf16() {
        let cyrillic = "Привет, мир";
        for big_endian in [false, true] {
            let mut bytes = if big_endian {
                vec![0xFE, 0xFF]
            } else {
                vec![0xFF, 0xFE]
            };
            for unit in cyrillic.encode_utf16() {
                let pair = if big_endian {
                    unit.to_be_bytes()
                } else {
                    unit.to_le_bytes()
                };
                bytes.extend_from_slice(&pair);
            }
            assert_eq!(
                decode_body_bytes(&bytes, "--with-file 'body.md'").unwrap(),
                cyrillic
            );
        }
    }

    #[test]
    fn decode_body_bytes_rejects_malformed_input_before_mutation() {
        // Lone 0xFF is never valid UTF-8.
        let error = decode_body_bytes(b"abc\xFFdef", "stdin").unwrap_err();
        assert!(error.msg.contains("stdin body is not valid UTF-8"));
        assert!(error.hint.is_some());
        // BOM-less UTF-16 slips past UTF-8 validation as NUL-riddled text
        // (any ASCII scaffolding guarantees the NULs; see decode_body_bytes).
        let mut bomless = Vec::new();
        for unit in "### Goal\n\nПривет".encode_utf16() {
            bomless.extend_from_slice(&unit.to_le_bytes());
        }
        let error = decode_body_bytes(&bomless, "stdin").unwrap_err();
        assert!(error.msg.contains("NUL bytes"));
        // Odd-count BOM-marked UTF-16 was truncated mid-code-unit.
        let error = decode_body_bytes(&[0xFF, 0xFE, 0x41], "stdin").unwrap_err();
        assert!(error.msg.contains("odd byte count"));
    }

    /// Registry item 564: a failed `atomic_write` leaves no tmp file behind,
    /// whichever post-create stage failed. The target path is an existing
    /// directory, so the replace stage fails deterministically on every
    /// platform (a file cannot replace a directory).
    #[test]
    fn atomic_write_leaves_no_tmp_file_behind_on_failure() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target-is-a-directory");
        fs::create_dir_all(&target).unwrap();
        assert!(atomic_write(&target, b"data").is_err());
        let leftovers: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp leftovers: {leftovers:?}");
    }
}
