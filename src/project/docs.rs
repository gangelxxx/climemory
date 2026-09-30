//! User-owned reference documents. This module only reads, never writes.
use super::*;
use serde_json::{json, Value};
use std::io::Read;

const MAX_BYTES: usize = 4_000_000;
const MAX_ENTRIES: usize = 1024;

impl Project {
    pub fn user_documents(&self) -> Result<Vec<Value>> {
        fn visit(
            root: &Path,
            path: &Path,
            remaining: &mut usize,
            entries: &mut usize,
            result: &mut Vec<Value>,
        ) -> Result<()> {
            *entries += 1;
            if *entries > MAX_ENTRIES {
                return Err(AppError::new("memory/docs exceeds 1024 filesystem entries; no documents were omitted silently"));
            }
            let meta = fs::symlink_metadata(path)?;
            if meta.file_type().is_symlink() {
                return Err(AppError::new(format!(
                    "user documents must not contain symlinks: {}",
                    path.display()
                )));
            }
            if meta.is_dir() {
                let mut children = fs::read_dir(path)?
                    .take(MAX_ENTRIES + 1)
                    .map(|e| e.map(|e| e.path()))
                    .collect::<std::io::Result<Vec<_>>>()?;
                children.sort();
                for child in children {
                    visit(root, &child, remaining, entries, result)?;
                }
            } else if meta.is_file() {
                let name = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let mut raw = Vec::new();
                fs::File::open(path)?
                    .take((*remaining + 1) as u64)
                    .read_to_end(&mut raw)?;
                if raw.len() > *remaining {
                    return Err(AppError::new("memory/docs exceeds the 4000000-byte total limit; no user documents were omitted"));
                }
                *remaining -= raw.len();
                let text = std::str::from_utf8(&raw).map_err(|_| {
                    AppError::new(format!("user document must be UTF-8 text: {name}"))
                })?;
                if text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
                {
                    return Err(AppError::new(format!(
                        "user document contains non-text control characters: {name}"
                    )));
                }
                result.push(json!({"path":name,"revision":crate::util::digest(&raw),
                    "text":text.trim_start_matches('\u{feff}'),"start_line":1,"end_line":text.lines().count().max(1),"authority":"user_reference","read_only":true}));
            } else {
                return Err(AppError::new(format!(
                    "user document must be a regular file: {}",
                    path.display()
                )));
            }
            Ok(())
        }
        let path = self.data.join("docs");
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
                return Err(AppError::new("memory/docs must be a real directory"));
            }
            _ => {}
        }
        let mut result = Vec::new();
        let mut remaining = MAX_BYTES;
        let mut entries = 0;
        visit(&self.root, &path, &mut remaining, &mut entries, &mut result)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_complete_sorted_and_refreshed_without_writes() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("memory")).unwrap();
        Config::default()
            .save(&temp.path().join("memory/config.json"))
            .unwrap();
        let project = Project::open(temp.path()).unwrap();
        assert!(project.user_documents().unwrap().is_empty());
        let docs = project.data.join("docs");
        fs::create_dir_all(docs.join("nested")).unwrap();
        fs::write(docs.join("ui.md"), "Blue buttons").unwrap();
        fs::write(docs.join("nested/requirements"), "Offline support").unwrap();
        let first = project.user_documents().unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["path"], "memory/docs/nested/requirements");
        assert_eq!(first[1]["text"], "Blue buttons");
        fs::write(docs.join("ui.md"), "Green buttons").unwrap();
        assert_ne!(first, project.user_documents().unwrap());
        fs::write(docs.join("invalid.pdf"), [0xff, 0]).unwrap();
        assert!(project.user_documents().is_err());
        fs::remove_file(docs.join("invalid.pdf")).unwrap();
        fs::write(docs.join("large.txt"), vec![b'x'; MAX_BYTES + 1]).unwrap();
        assert!(project.user_documents().is_err());
    }
}
