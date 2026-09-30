use super::*;
use crate::model::ThreadDoc;

pub(crate) fn create_memory(parsed: &Parsed, project: &Project) -> Result<()> {
    use std::io::IsTerminal;
    let slug = parsed.required_value("slug", "cm create Settings --slug settings")?;
    let title = parsed
        .arg(1)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| AppError::new("memory creation requires a title"))?;
    let profile = parsed.value("agent").unwrap_or("agent_medium");
    if !project.config.agent.profiles.contains_key(profile) {
        return Err(AppError::new(format!("unknown agent profile '{profile}'")));
    }
    let parent = parsed
        .value("parent")
        .filter(|p| *p != "none")
        .map(|p| project.resolve_thread(p).map(|d| d.meta.id))
        .transpose()?;
    let memory = match (parsed.value("note"), parsed.value("with-file")) {
        (Some(_), Some(_)) => return Err(AppError::new("use --note or --with-file, not both")),
        (Some(note), None) => note.to_owned(),
        (None, Some(path)) => read_initial_memory(fs::File::open(path)?, path)?,
        (None, None) => {
            if !std::io::stdin().is_terminal() {
                read_initial_memory(std::io::stdin().lock(), "stdin")?
            } else {
                String::new()
            }
        }
    };
    let memory = memory.replace("\r\n", "\n").trim().to_string();
    if memory.chars().count() > COMPACT_MEMORY_LIMIT
        || memory
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(AppError::new(format!(
            "initial memory must fit {COMPACT_MEMORY_LIMIT} characters and contain no control characters"
        )));
    }
    let doc = ThreadDoc::new_memory(
        fresh_id(),
        slug.into(),
        title.into(),
        title.into(),
        "memory".into(),
        Vec::new(),
    )?;
    let dir = directory(project, "agent-runs/thread-dialogues")?;
    fs::create_dir_all(&dir)?;
    let _dialogue_lock =
        FileLock::acquire(&dir.join("execution.lock"), Duration::from_millis(100))?;
    validate_parent_chain(project, &doc.meta.id, parent.as_deref())?;
    let _source_lock = project.source_lock()?;
    if project.load_threads()?.iter().any(|existing| {
        existing.meta.slug == slug
            || existing.meta.id == slug
            || existing.meta.slug == doc.meta.id
            || existing.meta.id == doc.meta.id
    }) {
        return Err(AppError::new(format!(
            "thread identity '{slug}' conflicts with an existing thread"
        )));
    }
    let binding = Binding {
        format: BINDING_FORMAT.into(),
        thread_id: doc.meta.id.clone(),
        agent: Some(profile.into()),
        provider: None,
        model: None,
        parent,
        memory,
        revision: 1,
        last_dialogue: None,
        updated: iso_now(),
        document: None,
    };
    let path = binding_path(project, &doc.meta.id)?;
    project.persist(&doc)?;
    // If binding storage fails, retain the metadata identity for an explicit
    // bind retry; never delete a successfully authored source on an I/O error.
    write_json(&path, &binding).map_err(|e| {
        AppError::with_hint(
            e.msg,
            format!("thread metadata was saved; retry cm bind {slug} --agent {profile}"),
        )
    })?;
    let project = Project::open(&project.root)?;
    let mut record = binding_record(&project, &binding)?;
    record["action"] = json!("memory_created");
    crate::output::write_records(&[record])
}

fn read_initial_memory(reader: impl std::io::Read, source: &str) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    reader.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(AppError::new("initial memory input exceeds 8192 bytes"));
    }
    crate::util::decode_body_bytes(&bytes, source)
}
