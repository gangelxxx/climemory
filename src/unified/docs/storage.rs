//! Publish prepared sections using the same Markdown and agent binding formats
//! as authored/session threads. Runtime state remains a resumable checkpoint.
use super::*;
use crate::model::ThreadDoc;

const AREA: &str = "document-import";

fn identity(source: &str, node: &str) -> String {
    digest(format!("cm-document-thread-v1:{source}:{node}"))[..32].into()
}

pub(super) fn id(source: &str, node: &str) -> String {
    identity(&digest(source), &digest(node))
}

fn owned(doc: &ThreadDoc) -> bool {
    let [kind, source, node] = doc.meta.tags.as_slice() else {
        return false;
    };
    let expected = identity(source, node);
    doc.meta.area == AREA
        && kind == "document"
        && doc.meta.id == expected
        && doc.meta.slug == format!("document-{expected}")
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(1000)
        .collect()
}

fn thread_path(project: &Project, id: &str) -> Result<PathBuf> {
    let path = project
        .data
        .join("threads")
        .join(&id[..2])
        .join(format!("{id}.md"));
    Project::checked_path(&project.data, &path)?;
    Ok(path)
}

fn files(
    project: &Project,
    source: &str,
    document: &Document,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut result = BTreeMap::new();
    for thread in &document.threads {
        let id = id(source, &thread.id);
        let parent = thread.parent.as_ref().map(|p| self::id(source, p));
        let mut doc = ThreadDoc::new_memory(
            id.clone(),
            format!("document-{id}"),
            one_line(&thread.title),
            one_line(&thread.passport.summary),
            AREA.into(),
            vec!["document".into(), digest(source), digest(&thread.id)],
        )?;
        doc.historical = format!(
            "# {}\n\nSource: {}\nSource SHA-256: {}\n",
            doc.meta.title, source, document.source_hash
        );
        if let Some(parent) = &parent {
            doc.historical += &format!("Parent: thread:{parent}\n");
        }
        doc.historical += &format!("\n{}\n\n## Children\n", thread.passport.summary);
        for child in document
            .threads
            .iter()
            .filter(|t| t.parent.as_ref() == Some(&thread.id))
        {
            doc.historical += &format!(
                "- thread:{} — {}\n",
                self::id(source, &child.id),
                one_line(&child.title)
            );
        }
        doc.historical += "\n## Original fragments\n";
        for fragment in &thread.fragments {
            doc.historical += &format!("\n{source}:{}\n{}\n", fragment.line, fragment.text);
        }
        let rendered = doc.render();
        // Preserve the normal reader's limits, including frontmatter overhead.
        if rendered.len() > 4_000_000 {
            return Err(AppError::new(
                "generated thread exceeds the 4 MB native thread limit",
            ));
        }
        ThreadDoc::parse(&rendered)?;
        result.insert(thread_path(project, &id)?, rendered.into_bytes());
        let (path, bytes) = crate::thread_agents::document_binding(
            project,
            &id,
            parent,
            thread.passport.summary.clone(),
            document.updated.clone(),
            (source.into(), thread.id.clone()),
            document.generation,
        )?;
        result.insert(path, bytes);
    }
    Ok(result)
}

fn bytes(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(meta) if !meta.is_file() || meta.len() > 4_000_000 => {
            Err(AppError::new("invalid generated thread file"))
        }
        Ok(_) => Ok(Some(fs::read(path)?)),
    }
}

pub(super) fn current(project: &Project, source: &str, document: &Document) -> bool {
    !document.updated.is_empty()
        && files(project, source, document).is_ok_and(|files| {
            files.iter().all(|(path, expected)| {
                bytes(path).is_ok_and(|actual| actual.as_ref() == Some(expected))
            })
        })
}

fn locks(project: &Project) -> Result<(FileLock, FileLock)> {
    let path = project
        .data
        .join("agent-runs/thread-dialogues/execution.lock");
    Project::checked_path(&project.data, &path)?;
    fs::create_dir_all(path.parent().unwrap())?;
    let dialogue = FileLock::acquire(&path, Duration::from_millis(100))?;
    Ok((dialogue, project.source_lock()?))
}

fn check_identity(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let matches = if path.extension().is_some_and(|s| s == "md") {
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| ThreadDoc::parse(s).ok())
            .is_some_and(|doc| owned(&doc) && doc.meta.id == id)
    } else {
        serde_json::from_slice::<Value>(bytes).is_ok_and(|value| {
            value["thread_id"] == id
                && value["document"][0]
                    .as_str()
                    .zip(value["document"][1].as_str())
                    .is_some_and(|(source, node)| self::id(source, node) == id)
        })
    };
    if !matches {
        return Err(AppError::new(format!(
            "document thread identity collision: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn publish(project: &Project, source: &str, document: &Document) -> Result<()> {
    let files = files(project, source, document)?;
    let _locks = locks(project)?;
    // Validate every destination before updating either native representation.
    for path in files.keys() {
        if let Some(old) = bytes(path)? {
            check_identity(path, &old)?;
        }
    }
    // Metadata first: a concurrently read binding must always have a thread.
    for markdown in [true, false] {
        for (path, content) in &files {
            if path.extension().is_some_and(|s| s == "md") != markdown {
                continue;
            }
            if bytes(path)?.as_ref() != Some(content) {
                fs::create_dir_all(path.parent().unwrap())?;
                atomic_write(path, content)?;
            }
        }
    }
    Ok(())
}

pub(super) fn prune(
    project: &Project,
    documents: &BTreeMap<String, Document>,
    snapshot: &BTreeMap<String, String>,
) -> Result<()> {
    let keep: BTreeSet<_> = documents
        .iter()
        .filter(|(path, _)| snapshot.contains_key(*path))
        .flat_map(|(source, doc)| doc.threads.iter().map(move |t| id(source, &t.id)))
        .collect();
    let _locks = locks(project)?;
    let project = Project::open(&project.root)?;
    // Bindings retain ownership even if the corresponding Markdown was deleted.
    // Validate all destinations before deleting either half of a native thread.
    let mut obsolete_bindings = Vec::new();
    let mut obsolete_threads = Vec::new();
    for (thread_id, source, node) in crate::thread_agents::document_bindings(&project)? {
        if thread_id != id(&source, &node) || keep.contains(&thread_id) {
            continue;
        }
        let markdown = thread_path(&project, &thread_id)?;
        if let Some(content) = bytes(&markdown)? {
            check_identity(&markdown, &content)?;
        }
        let (binding, _) = crate::thread_agents::document_binding(
            &project,
            &thread_id,
            None,
            String::new(),
            String::new(),
            (source, node),
            0,
        )?;
        obsolete_bindings.push(binding);
    }
    for doc in project
        .load_threads()?
        .iter()
        .filter(|d| owned(d) && !keep.contains(&d.meta.id))
    {
        let path = thread_path(&project, &doc.meta.id)?;
        // Only remove the canonical file, never a user-owned copy elsewhere.
        let Some(content) = bytes(&path)? else {
            continue;
        };
        check_identity(&path, &content)?;
        obsolete_threads.push(path);
    }
    // Remove bindings first so ordinary inventory never sees dangling bindings.
    for path in obsolete_bindings.into_iter().chain(obsolete_threads) {
        fs::remove_file(path)?;
    }
    Ok(())
}
