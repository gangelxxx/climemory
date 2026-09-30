use super::*;

pub(super) fn fingerprint_inventory_contents(inventory: &mut SourceInventory) -> Result<()> {
    let started = Instant::now();
    let hashes = source_content_hashes(&inventory.sources)?;
    for (source, content_hash) in inventory.sources.iter_mut().zip(hashes) {
        source.content_hash = Some(content_hash);
    }
    let mut hasher = Sha256::new();
    for source in &inventory.sources {
        hasher.update(source.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(
            source
                .content_hash
                .as_deref()
                .expect("content fingerprints were populated")
                .as_bytes(),
        );
        hasher.update(b"\n");
    }
    inventory.content_epoch = Some(format!("{:x}", hasher.finalize()));
    inventory.content_scan_ms = started.elapsed().as_millis();
    Ok(())
}

#[cfg(feature = "code-index")]
fn source_content_hashes(sources: &[SourceFile]) -> Result<Vec<String>> {
    sources
        .par_iter()
        .map(|source| read_source(source).map(digest))
        .collect()
}

#[cfg(not(feature = "code-index"))]
fn source_content_hashes(sources: &[SourceFile]) -> Result<Vec<String>> {
    sources
        .iter()
        .map(|source| read_source(source).map(digest))
        .collect()
}

fn inventory_epoch(sources: &[SourceFile]) -> String {
    let mut hasher = Sha256::new();
    for source in sources {
        hasher.update(source.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(source.size.to_string().as_bytes());
        hasher.update(b":");
        hasher.update(source.modified_ns.to_string().as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

pub(super) fn discover_inventory(
    project: &Project,
    scope: Option<&Path>,
) -> Result<SourceInventory> {
    discover_inventory_with(project, scope, false, None)
}

/// The grep-flavored inventory: same gitignore-respecting walk/git discovery,
/// but the admission gate also lets UTF-8 text config/doc files in
/// (`code::TEXT_GREP_EXTENSIONS`). Only `code grep` uses this flavor — the
/// semantic index (`code index`/`status`/`find`/`context`) keeps the
/// source-only gate because it resolves a grammar per admitted file.
pub(super) fn discover_inventory_grep(
    project: &Project,
    paths: Option<&[PathBuf]>,
) -> Result<SourceInventory> {
    discover_inventory_with(project, None, true, paths)
}

fn discover_inventory_with(
    project: &Project,
    scope: Option<&Path>,
    include_text: bool,
    grep_paths: Option<&[PathBuf]>,
) -> Result<SourceInventory> {
    #[cfg(test)]
    DISCOVERY_RUNS.with(|runs| runs.set(runs.get() + 1));
    #[cfg(test)]
    notify_discovery(&project.root);
    let started = Instant::now();
    let discovered_at_ms = unix_time_ms();
    let root = fs::canonicalize(&project.root)?;
    let requested = scope.map_or_else(
        || root.clone(),
        |scope| {
            if scope.is_absolute() {
                scope.to_path_buf()
            } else {
                root.join(scope)
            }
        },
    );
    let requested = fs::canonicalize(&requested).map_err(|error| {
        AppError::new(format!(
            "source-code index path {}: {error}",
            requested.display()
        ))
    })?;
    if !requested.starts_with(&root) {
        return Err(AppError::new(
            "source-code index path must stay inside the project root",
        ));
    }
    let complete = requested == root;
    let forced_backend = benchmark_discovery_backend()?;
    let (paths, discovery_backend) = match forced_backend {
        Some(DiscoveryBackend::Git) if complete => (
            git_candidate_paths(&root).ok_or_else(|| {
                AppError::new("forced Git source discovery is unavailable for this repository")
            })?,
            DiscoveryBackend::Git,
        ),
        Some(DiscoveryBackend::Git) => {
            return Err(AppError::new(
                "forced Git source discovery requires a complete project scope",
            ));
        }
        Some(DiscoveryBackend::Walk) => (
            walk_candidate_paths(&root, &requested, grep_paths)?,
            DiscoveryBackend::Walk,
        ),
        None if complete && grep_paths.is_none() && git_discovery_is_worthwhile(&root) => {
            match git_candidate_paths(&root) {
                Some(paths) => (paths, DiscoveryBackend::Git),
                None => (
                    walk_candidate_paths(&root, &requested, grep_paths)?,
                    DiscoveryBackend::Walk,
                ),
            }
        }
        None => (
            walk_candidate_paths(&root, &requested, grep_paths)?,
            DiscoveryBackend::Walk,
        ),
    };
    let mut files = collect_source_files(&root, paths, include_text)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let scan_epoch = inventory_epoch(&files);
    Ok(SourceInventory {
        sources: files,
        complete: complete && grep_paths.is_none(),
        scan_epoch,
        content_epoch: None,
        discovery_ms: started.elapsed().as_millis(),
        content_scan_ms: 0,
        discovery_backend,
        discovered_at_ms,
    })
}

fn benchmark_discovery_backend() -> Result<Option<DiscoveryBackend>> {
    let Some(raw) = std::env::var_os(BENCH_DISCOVERY_BACKEND_ENV) else {
        return Ok(None);
    };
    match raw.to_string_lossy().as_ref() {
        "auto" => Ok(None),
        "git" => Ok(Some(DiscoveryBackend::Git)),
        "walk" => Ok(Some(DiscoveryBackend::Walk)),
        other => Err(AppError::invalid_value(
            BENCH_DISCOVERY_BACKEND_ENV,
            other,
            &["auto", "git", "walk"],
            "unset the benchmark-only discovery override",
        )),
    }
}

#[cfg(test)]
fn notify_discovery(root: &Path) {
    let Ok(mut hooks) = DISCOVERY_HOOKS.lock() else {
        return;
    };
    if let Some(index) = hooks
        .iter()
        .position(|(expected_root, _)| expected_root == root)
    {
        let (_, sender) = hooks.swap_remove(index);
        let _ = sender.send(());
    }
}

#[cfg(test)]
pub(super) fn pause_before_inventory_validation(root: &Path) {
    let hook = PRE_PUBLISH_HOOKS.lock().ok().and_then(|mut hooks| {
        hooks
            .iter()
            .position(|hook| hook.root == root)
            .map(|index| hooks.swap_remove(index))
    });
    if let Some(hook) = hook {
        let _ = hook.reached.send(());
        let _ = hook.resume.recv_timeout(Duration::from_secs(5));
    }
}

#[cfg(test)]
pub(super) fn pause_before_context_query(root: &Path) {
    let hook = CONTEXT_QUERY_HOOKS.lock().ok().and_then(|mut hooks| {
        hooks
            .iter()
            .position(|hook| hook.root == root)
            .map(|index| hooks.swap_remove(index))
    });
    if let Some(hook) = hook {
        let _ = hook.reached.send(());
        let _ = hook.resume.recv_timeout(Duration::from_secs(5));
    }
}

pub(super) fn git_discovery_is_worthwhile(root: &Path) -> bool {
    fs::metadata(root.join(".git/index"))
        .is_ok_and(|metadata| metadata.len() >= GIT_DISCOVERY_MIN_INDEX_BYTES)
}

pub(super) fn walk_candidate_paths(
    root: &Path,
    requested: &Path,
    scopes: Option<&[PathBuf]>,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let filter_root = root.to_path_buf();
    let scopes = scopes.map(<[PathBuf]>::to_vec);
    let mut builder = WalkBuilder::new(requested);
    builder
        .standard_filters(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            let Some(file_type) = entry.file_type() else {
                return true;
            };
            let relative = entry
                .path()
                .strip_prefix(&filter_root)
                .unwrap_or(entry.path());
            if file_type.is_dir()
                && (relative == Path::new("memory") || skip_directory(entry.path()))
            {
                return false;
            }
            // Start at the project root so parent ignore rules and hidden /
            // symlink handling remain identical. Do not descend into sibling
            // trees or admit files outside the selected paths.
            scopes.as_ref().is_none_or(|scopes| {
                scopes.iter().any(|scope| {
                    relative.starts_with(scope)
                        || (file_type.is_dir() && scope.starts_with(relative))
                })
            })
        });
    for entry in builder.build() {
        let entry = entry
            .map_err(|error| AppError::new(format!("source-code discovery failed: {error}")))?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        paths.push(entry.into_path());
    }
    Ok(paths)
}

pub(super) fn git_candidate_paths(root: &Path) -> Option<Vec<PathBuf>> {
    if !root.join(".git").exists() || root.join(".gitmodules").exists() {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut relative_paths = Vec::new();
    for encoded in output.stdout.split(|byte| *byte == 0) {
        if encoded.is_empty() {
            continue;
        }
        if git_path_is_ignore_control(encoded) {
            return None;
        }
        let relative = std::str::from_utf8(encoded).ok()?.to_string();
        let relative_path = Path::new(&relative);
        if relative_path.is_absolute()
            || relative_path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return None;
        }
        relative_paths.push(relative);
    }
    if git_has_ignored_ignore_control(root)? {
        return None;
    }
    let ignored = git_ignored_paths(root, &relative_paths)?;
    let mut paths = Vec::new();
    for relative in relative_paths {
        if !ignored.contains(&relative) && !git_relative_path_is_excluded(Path::new(&relative)) {
            paths.push(root.join(relative));
        }
    }
    Some(paths)
}

fn git_has_ignored_ignore_control(root: &Path) -> Option<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
            ".ignore",
            ":(glob)**/.ignore",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .any(git_path_is_ignore_control),
    )
}

fn git_path_is_ignore_control(encoded: &[u8]) -> bool {
    encoded
        .rsplit(|byte| *byte == b'/')
        .next()
        .is_some_and(|name| name == b".ignore")
}

pub(super) fn git_ignored_paths(root: &Path, paths: &[String]) -> Option<BTreeSet<String>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut input = Vec::with_capacity(paths.iter().map(|path| path.len().saturating_add(1)).sum());
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take()?;
    let writer = std::thread::Builder::new()
        .name("climemory-git-ignore-input".to_string())
        .spawn(move || stdin.write_all(&input))
        .ok()?;
    let output = child.wait_with_output().ok()?;
    writer.join().ok()?.ok()?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return None;
    }
    let mut ignored = BTreeSet::new();
    for encoded in output.stdout.split(|byte| *byte == 0) {
        if !encoded.is_empty() {
            ignored.insert(std::str::from_utf8(encoded).ok()?.to_string());
        }
    }
    Some(ignored)
}

fn git_relative_path_is_excluded(relative: &Path) -> bool {
    let mut components = relative.components().peekable();
    let mut ordinal = 0usize;
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return true;
        };
        let name = name.to_string_lossy();
        if name.starts_with('.') || (ordinal == 0 && name == "memory") {
            return true;
        }
        if components.peek().is_some() && skip_directory(Path::new(name.as_ref())) {
            return true;
        }
        ordinal += 1;
    }
    false
}

#[cfg(feature = "code-index")]
fn collect_source_files(
    root: &Path,
    paths: Vec<PathBuf>,
    include_text: bool,
) -> Result<Vec<SourceFile>> {
    if include_text {
        // Grep shares a bounded pool between metadata and content scanning.
        // Keep small inventories serial; index operations retain their own
        // existing global pool and freshness behavior.
        if paths.len() >= 64 {
            if let Some(pool) = grep::worker_pool() {
                return pool.install(|| {
                    paths
                        .into_par_iter()
                        .map(|path| source_file_admitting(root, &path, include_text))
                        .collect::<Result<Vec<_>>>()
                        .map(|files| files.into_iter().flatten().collect())
                });
            }
        }
        return paths
            .into_iter()
            .filter_map(|path| source_file_admitting(root, &path, include_text).transpose())
            .collect();
    }
    // Exact-path grep commonly leaves zero or one candidate. Starting the
    // global worker pool costs more than inspecting that single file.
    match paths.as_slice() {
        [] => return Ok(Vec::new()),
        [path] => {
            return Ok(source_file_admitting(root, path, include_text)?
                .into_iter()
                .collect());
        }
        _ => {}
    }
    paths
        .into_par_iter()
        .try_fold(Vec::new, |mut files, path| {
            if let Some(source) = source_file_admitting(root, &path, include_text)? {
                files.push(source);
            }
            Ok(files)
        })
        .try_reduce(Vec::new, |mut left, mut right| {
            left.append(&mut right);
            Ok(left)
        })
}

#[cfg(not(feature = "code-index"))]
fn collect_source_files(
    root: &Path,
    paths: Vec<PathBuf>,
    include_text: bool,
) -> Result<Vec<SourceFile>> {
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        if let Some(source) = source_file_admitting(root, &path, include_text)? {
            files.push(source);
        }
    }
    Ok(files)
}

// Only the discovery tests call the source-only flavor directly; production
// callers go through collect_source_files with an explicit admission gate.
#[cfg(test)]
pub(super) fn source_file(root: &Path, path: &Path) -> Result<Option<SourceFile>> {
    source_file_admitting(root, path, false)
}

pub(super) fn source_file_admitting(
    root: &Path,
    path: &Path,
    include_text: bool,
) -> Result<Option<SourceFile>> {
    if !(code::is_source_file(path) || include_text && code::is_text_grep_file(path)) {
        return Ok(None);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES
    {
        return Ok(None);
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| AppError::new("source path escaped project root"))?
        .to_string_lossy()
        .replace('\\', "/");
    // Grep-admitted text config/doc files have no grammar: they carry the
    // sentinel "text" language, which only the grep scan path ever reads
    // (the indexer's parser dispatch never sees them — it keeps the
    // source-only gate above).
    let language = code::lang_for_path(path).unwrap_or("text");
    Ok(Some(SourceFile {
        absolute: path.to_path_buf(),
        path: relative,
        language: language.to_string(),
        size: metadata.len().min(i64::MAX as u64) as i64,
        modified_ns: modified_ns(&metadata),
        content_hash: None,
    }))
}

#[cfg(test)]
pub(super) fn discover_sources(
    project: &Project,
    scope: Option<&Path>,
) -> Result<(Vec<SourceFile>, bool)> {
    let inventory = discover_inventory(project, scope)?;
    Ok((inventory.sources, inventory.complete))
}

fn skip_directory(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    name.starts_with('.')
        || matches!(
            name,
            "target"
                | "node_modules"
                | "vendor"
                | "dist"
                | "build"
                | "out"
                | "coverage"
                | ".venv"
                | "venv"
                | "__pycache__"
        )
}

pub(super) fn modified_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
