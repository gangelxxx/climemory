use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PrepareWork {
    pub(super) read: Duration,
    pub(super) hash: Duration,
    pub(super) parse: Duration,
    pub(super) chunk: Duration,
    pub(super) read_files: usize,
    pub(super) read_bytes: usize,
}

impl PrepareWork {
    fn merge(&mut self, other: Self) {
        self.read += other.read;
        self.hash += other.hash;
        self.parse += other.parse;
        self.chunk += other.chunk;
        self.read_files = self.read_files.saturating_add(other.read_files);
        self.read_bytes = self.read_bytes.saturating_add(other.read_bytes);
    }
}

struct PreparedOutcome {
    source: PreparedSource,
    work: PrepareWork,
}

#[derive(Default)]
struct PreparedGroup {
    sources: Vec<PreparedSource>,
    work: PrepareWork,
}

pub(super) struct PreparedBatchCursor<'a> {
    sources: &'a [SourceFile],
    stored: &'a BTreeMap<String, StoredFile>,
    next: usize,
    pending: VecDeque<PreparedSource>,
    pub(super) work: PrepareWork,
    pub(super) prepared_peak_bytes: usize,
    pub(super) lookahead_peak_bytes: usize,
}

impl<'a> PreparedBatchCursor<'a> {
    pub(super) fn new(sources: &'a [SourceFile], stored: &'a BTreeMap<String, StoredFile>) -> Self {
        Self {
            sources,
            stored,
            next: 0,
            pending: VecDeque::new(),
            work: PrepareWork::default(),
            prepared_peak_bytes: 0,
            lookahead_peak_bytes: 0,
        }
    }

    pub(super) fn next_batch(&mut self) -> Result<Option<Vec<PreparedSource>>> {
        if self.pending.is_empty() && self.next >= self.sources.len() {
            return Ok(None);
        }
        let mut batch = Vec::new();
        let mut resident_bytes = 0usize;
        loop {
            if self.pending.is_empty() {
                self.fill_lookahead()?;
            }
            let Some(prepared) = self.pending.pop_front() else {
                break;
            };
            let prepared_bytes = prepared.resident_bytes();
            if prepared_batch_would_overflow(
                batch.len(),
                resident_bytes,
                prepared_bytes,
                PREPARE_BATCH_MAX_BYTES,
            ) {
                self.pending.push_front(prepared);
                break;
            }
            resident_bytes = resident_bytes.saturating_add(prepared_bytes);
            batch.push(prepared);
            if resident_bytes >= PREPARE_BATCH_MAX_BYTES || batch.len() >= PREPARE_BATCH_MAX_FILES {
                break;
            }
        }
        self.prepared_peak_bytes = self.prepared_peak_bytes.max(resident_bytes);
        Ok(Some(batch))
    }

    fn fill_lookahead(&mut self) -> Result<()> {
        if self.next >= self.sources.len() {
            return Ok(());
        }
        let start = self.next;
        let mut source_bytes = 0u64;
        while self.next < self.sources.len() && self.next - start < PREPARE_LOOKAHEAD_MAX_FILES {
            let next_bytes = self.sources[self.next].size.max(0) as u64;
            if self.next > start
                && source_bytes.saturating_add(next_bytes) > PREPARE_LOOKAHEAD_MAX_SOURCE_BYTES
            {
                break;
            }
            source_bytes = source_bytes.saturating_add(next_bytes);
            self.next += 1;
        }
        let prepared = prepare_sources(&self.sources[start..self.next], self.stored)?;
        let lookahead_bytes = prepared
            .sources
            .iter()
            .map(PreparedSource::resident_bytes)
            .fold(0usize, usize::saturating_add);
        self.lookahead_peak_bytes = self.lookahead_peak_bytes.max(lookahead_bytes);
        self.work.merge(prepared.work);
        self.pending.extend(prepared.sources);
        Ok(())
    }
}

#[cfg(feature = "code-index")]
fn prepare_sources(
    sources: &[SourceFile],
    stored: &BTreeMap<String, StoredFile>,
) -> Result<PreparedGroup> {
    let outcomes = sources
        .par_iter()
        .map(|source| prepare_source(source, stored.get(&source.path)))
        .collect::<Vec<_>>();
    collect_prepared(outcomes)
}

#[cfg(not(feature = "code-index"))]
fn prepare_sources(
    sources: &[SourceFile],
    stored: &BTreeMap<String, StoredFile>,
) -> Result<PreparedGroup> {
    let outcomes = sources
        .iter()
        .map(|source| prepare_source(source, stored.get(&source.path)))
        .collect::<Vec<_>>();
    collect_prepared(outcomes)
}

fn collect_prepared(outcomes: Vec<Result<PreparedOutcome>>) -> Result<PreparedGroup> {
    let mut group = PreparedGroup::default();
    group.sources.reserve(outcomes.len());
    for outcome in outcomes {
        let outcome = outcome?;
        group.work.merge(outcome.work);
        group.sources.push(outcome.source);
    }
    Ok(group)
}

pub(super) fn prepared_batch_would_overflow(
    current_files: usize,
    current_bytes: usize,
    next_bytes: usize,
    max_bytes: usize,
) -> bool {
    current_files > 0 && current_bytes.saturating_add(next_bytes) > max_bytes
}

fn prepare_source(source: &SourceFile, existing: Option<&StoredFile>) -> Result<PreparedOutcome> {
    if let Some(existing) = existing {
        let metadata_matches =
            existing.size == source.size && existing.modified_ns == source.modified_ns;
        let fingerprint_matches = source
            .content_hash
            .as_ref()
            .is_none_or(|content_hash| content_hash == &existing.content_hash);
        if metadata_matches && fingerprint_matches {
            return Ok(PreparedOutcome {
                source: PreparedSource::Unchanged,
                work: PrepareWork::default(),
            });
        }
        if source.content_hash.as_ref() == Some(&existing.content_hash) {
            return Ok(PreparedOutcome {
                source: PreparedSource::Metadata(source.clone()),
                work: PrepareWork::default(),
            });
        }
    }
    let mut work = PrepareWork::default();
    let started = Instant::now();
    let bytes = read_source(source)?;
    work.read = started.elapsed();
    work.read_files = 1;
    work.read_bytes = bytes.len();
    let started = Instant::now();
    let content_hash = digest(&bytes);
    work.hash = started.elapsed();
    if source
        .content_hash
        .as_ref()
        .is_some_and(|expected| expected != &content_hash)
    {
        return Err(unstable_source_error(&source.absolute));
    }
    if existing.is_some_and(|existing| existing.content_hash == content_hash) {
        return Ok(PreparedOutcome {
            source: PreparedSource::Metadata(source.clone()),
            work,
        });
    }
    let text = String::from_utf8_lossy(&bytes);
    let started = Instant::now();
    let parsed = code::parse(&source.path, &source.language, &text)?;
    work.parse = started.elapsed();
    let started = Instant::now();
    let chunks = chunks_for(&source.path, &text, &parsed.defs);
    work.chunk = started.elapsed();
    Ok(PreparedOutcome {
        source: PreparedSource::Changed {
            source: source.clone(),
            content_hash,
            parsed,
            chunks,
        },
        work,
    })
}

pub(super) fn read_source(source: &SourceFile) -> Result<Vec<u8>> {
    let mut file = open_regular_file_no_follow(&source.absolute)?;
    if !source_metadata_matches(&file.metadata()?, source) {
        return Err(unstable_source_error(&source.absolute));
    }
    let mut bytes = Vec::with_capacity(source.size.max(0) as usize);
    file.read_to_end(&mut bytes)?;
    if !source_metadata_matches(&file.metadata()?, source)
        || !opened_file_matches_path(&file, &source.absolute)
    {
        return Err(unstable_source_error(&source.absolute));
    }
    Ok(bytes)
}

fn source_metadata_matches(metadata: &fs::Metadata, source: &SourceFile) -> bool {
    metadata.len().min(i64::MAX as u64) as i64 == source.size
        && modified_ns(metadata) == source.modified_ns
}

fn unstable_source_error(path: &Path) -> AppError {
    AppError::with_hint(
        format!(
            "source file changed while it was being read: {}",
            path.display()
        ),
        "retry source-code indexing after source writes finish",
    )
}

pub(super) fn definition_targets_for_path(
    connection: &Connection,
    path: &str,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut statement = connection
        .prepare("SELECT name,id FROM source_code_symbols WHERE path=?1 ORDER BY name,id")?;
    let rows = statement.query_map(params![path], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut targets = BTreeMap::<String, BTreeSet<String>>::new();
    for row in rows {
        let (name, id) = row?;
        targets.entry(name).or_default().insert(id);
    }
    Ok(targets)
}

pub(super) fn parsed_definition_targets(
    source: &SourceFile,
    parsed: &code::CodeParse,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut targets = BTreeMap::<String, BTreeSet<String>>::new();
    for definition in &parsed.defs {
        targets
            .entry(definition.name.clone())
            .or_default()
            .insert(code::symbol_id(
                &source.path,
                &definition.kind,
                &definition.name,
                definition.line,
            ));
    }
    targets
}

pub(super) fn extend_definition_targets(
    targets: &mut BTreeMap<String, BTreeSet<String>>,
    additions: BTreeMap<String, BTreeSet<String>>,
) {
    for (name, ids) in additions {
        targets.entry(name).or_default().extend(ids);
    }
}

pub(super) fn changed_global_definition_names(
    connection: &Connection,
    old: &BTreeMap<String, BTreeSet<String>>,
    new: &BTreeMap<String, BTreeSet<String>>,
) -> Result<BTreeSet<String>> {
    let candidates = old
        .keys()
        .chain(new.keys())
        .filter(|name| old.get(*name) != new.get(*name))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut statement =
        connection.prepare("SELECT id FROM source_code_symbols WHERE name=?1 ORDER BY id")?;
    let mut changed = BTreeSet::new();
    for name in candidates {
        let rows = statement.query_map(params![name], |row| row.get(0))?;
        let before = rows.collect::<std::result::Result<BTreeSet<String>, _>>()?;
        let mut after = before.clone();
        if let Some(ids) = old.get(&name) {
            for id in ids {
                after.remove(id);
            }
        }
        if let Some(ids) = new.get(&name) {
            after.extend(ids.iter().cloned());
        }
        if resolution_signature(&before) != resolution_signature(&after) {
            changed.insert(name);
        }
    }
    Ok(changed)
}

fn resolution_signature(targets: &BTreeSet<String>) -> (bool, Option<&str>) {
    (
        !targets.is_empty(),
        (targets.len() == 1).then(|| targets.first().expect("single target exists").as_str()),
    )
}
