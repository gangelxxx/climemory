use super::*;

#[derive(Clone, Debug)]
pub(super) struct SourceFile {
    pub(super) absolute: PathBuf,
    pub(super) path: String,
    pub(super) language: String,
    pub(super) size: i64,
    pub(super) modified_ns: i64,
    pub(super) content_hash: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct SourceInventory {
    pub(super) sources: Vec<SourceFile>,
    pub(super) complete: bool,
    pub(super) scan_epoch: String,
    pub(super) content_epoch: Option<String>,
    pub(super) discovery_ms: u128,
    pub(super) content_scan_ms: u128,
    pub(super) discovery_backend: DiscoveryBackend,
    pub(super) discovered_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DiscoveryBackend {
    Git,
    Walk,
}

impl DiscoveryBackend {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Walk => "walk",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ScanLease {
    pub(super) scan_epoch: String,
    pub(super) scanned_at_ms: u64,
}

#[derive(Clone, Debug)]
pub(super) struct StoredFile {
    pub(super) content_hash: String,
    pub(super) size: i64,
    pub(super) modified_ns: i64,
}

pub(super) enum PreparedSource {
    Unchanged,
    Metadata(SourceFile),
    Changed {
        source: SourceFile,
        content_hash: String,
        parsed: code::CodeParse,
        chunks: Vec<Chunk>,
    },
}

impl PreparedSource {
    pub(super) fn resident_bytes(&self) -> usize {
        let base = std::mem::size_of::<Self>();
        match self {
            Self::Unchanged => base,
            Self::Metadata(source) => base.saturating_add(source_resident_bytes(source)),
            Self::Changed {
                source,
                content_hash,
                parsed,
                chunks,
            } => {
                let definitions = parsed.defs.iter().fold(
                    std::mem::size_of::<Vec<code::Def>>(),
                    |total, definition| {
                        total
                            .saturating_add(std::mem::size_of::<code::Def>())
                            .saturating_add(definition.name.len())
                            .saturating_add(definition.kind.len())
                            .saturating_add(definition.signature.len())
                    },
                );
                let references = parsed.refs.iter().fold(
                    std::mem::size_of::<Vec<code::Ref>>(),
                    |total, reference| {
                        total
                            .saturating_add(std::mem::size_of::<code::Ref>())
                            .saturating_add(reference.name.len())
                            .saturating_add(reference.kind.len())
                    },
                );
                let chunks =
                    chunks
                        .iter()
                        .fold(std::mem::size_of::<Vec<Chunk>>(), |total, chunk| {
                            total
                                .saturating_add(std::mem::size_of::<Chunk>())
                                .saturating_add(chunk.key.len())
                                .saturating_add(chunk.symbols.len())
                                .saturating_add(chunk.body.len())
                        });
                base.saturating_add(source_resident_bytes(source))
                    .saturating_add(content_hash.len())
                    .saturating_add(definitions)
                    .saturating_add(references)
                    .saturating_add(chunks)
            }
        }
    }
}

fn source_resident_bytes(source: &SourceFile) -> usize {
    std::mem::size_of::<SourceFile>()
        .saturating_add(source.absolute.as_os_str().to_string_lossy().len())
        .saturating_add(source.path.len())
        .saturating_add(source.language.len())
        .saturating_add(source.content_hash.as_ref().map_or(0, String::len))
}
