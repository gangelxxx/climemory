mod docs;
use crate::config::Config;
use crate::model::ThreadDoc;
use crate::util::{AppError, FileLock, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    pub data: PathBuf,
    pub health: PathBuf,
    pub config: Config,
    catalog: Arc<Mutex<Option<Vec<ThreadDoc>>>>,
}
pub fn is_initialized_root(root: &Path) -> bool {
    root.join("memory/config.json").is_file()
}
impl Project {
    pub fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let data = root.join("memory");
        if fs::symlink_metadata(&data)?.file_type().is_symlink() {
            return Err(AppError::new("memory must be a real directory"));
        }
        Self::checked_path(&data, &data.join("config.json"))?;
        let config = Config::load(&data.join("config.json"))?;
        Ok(Self {
            root,
            health: data.join("runtime"),
            data,
            config,
            catalog: Arc::new(Mutex::new(None)),
        })
    }
    pub fn store_path(&self) -> PathBuf {
        self.data.join("code-index.db")
    }
    pub fn code_index_lock(&self) -> Result<FileLock> {
        Self::checked_path(&self.data, &self.health)?;
        fs::create_dir_all(&self.health)?;
        FileLock::acquire(
            &self.health.join("code-index.lock"),
            Duration::from_secs(30),
        )
    }
    pub fn source_lock(&self) -> Result<FileLock> {
        Self::checked_path(&self.data, &self.health)?;
        fs::create_dir_all(&self.health)?;
        FileLock::acquire(&self.health.join("source.lock"), Duration::from_secs(3))
    }
    pub fn load_threads(&self) -> Result<Vec<ThreadDoc>> {
        fn visit(path: &Path, result: &mut Vec<ThreadDoc>) -> Result<()> {
            let meta = match fs::symlink_metadata(path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            if meta.file_type().is_symlink() {
                return Err(AppError::new("thread storage must not contain symlinks"));
            }
            if meta.is_dir() {
                for entry in fs::read_dir(path)? {
                    visit(&entry?.path(), result)?;
                }
            } else if path.extension().is_some_and(|e| e == "md") {
                if meta.len() > 4_000_000 {
                    return Err(AppError::new("thread source exceeds 4 MB"));
                }
                result.push(ThreadDoc::parse(&fs::read_to_string(path)?)?);
            }
            Ok(())
        }
        if let Some(docs) = self.catalog.lock().unwrap().as_ref() {
            return Ok(docs.clone());
        }
        let mut result = Vec::new();
        visit(&self.data.join("threads"), &mut result)?;
        result.sort_by(|a, b| a.meta.slug.cmp(&b.meta.slug));
        *self.catalog.lock().unwrap() = Some(result.clone());
        Ok(result)
    }
    pub fn resolve_thread(&self, identity: &str) -> Result<ThreadDoc> {
        let identity = identity.strip_prefix("thread:").unwrap_or(identity);
        if self.catalog.lock().unwrap().is_none() {
            self.load_threads()?;
        }
        let catalog = self.catalog.lock().unwrap();
        let mut matches = catalog
            .as_ref()
            .unwrap()
            .iter()
            .filter(|d| d.meta.id == identity || d.meta.slug == identity);
        let doc = matches
            .next()
            .ok_or_else(|| AppError::new(format!("unknown thread '{identity}'")))?;
        if matches.next().is_some() {
            return Err(AppError::new("ambiguous thread identity"));
        }
        Ok(doc.clone())
    }
    pub fn checked_path(root: &Path, path: &Path) -> Result<()> {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| AppError::new("storage path is outside memory"))?;
        let mut current = root.to_path_buf();
        for component in relative.components() {
            if !matches!(component, std::path::Component::Normal(_)) {
                return Err(AppError::new("invalid storage path"));
            }
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(AppError::new("storage paths must not contain symlinks"))
                }
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        Ok(())
    }
    pub fn persist(&self, doc: &ThreadDoc) -> Result<()> {
        let doc = ThreadDoc::parse(&doc.render())?;
        let dir = self.data.join("threads").join(&doc.meta.id[..2]);
        let path = dir.join(format!("{}.md", doc.meta.id));
        Self::checked_path(&self.data, &path)?;
        if path.exists() {
            return Err(AppError::new("thread identity already exists"));
        }
        fs::create_dir_all(&dir)?;
        crate::util::atomic_write(&path, doc.render().as_bytes())?;
        *self.catalog.lock().unwrap() = None;
        Ok(())
    }
}
