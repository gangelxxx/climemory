use super::*;

#[derive(Clone, Debug, Serialize)]
pub struct CodeIndexStats {
    pub state: &'static str,
    pub action: &'static str,
    pub complete: bool,
    pub scanned: usize,
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    pub chunks: usize,
    pub corpus_epoch: String,
    pub scan_epoch: String,
    pub timings: CodeIndexTimings,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CodeIndexTimings {
    pub discovery_ms: u128,
    pub discovery_backend: String,
    pub prepare_ms: u128,
    pub read_work_ms: u128,
    pub hash_work_ms: u128,
    pub parse_work_ms: u128,
    pub chunk_work_ms: u128,
    pub read_files: usize,
    pub read_bytes: usize,
    pub prepared_peak_bytes: usize,
    pub lookahead_peak_bytes: usize,
    pub storage_ms: u128,
    pub fts_ms: u128,
    pub edge_ms: u128,
    pub validation_ms: u128,
    pub publish_ms: u128,
    pub finalize_ms: u128,
    pub total_ms: u128,
}

#[derive(Clone, Debug, Serialize)]
pub struct CodeStatus {
    pub state: &'static str,
    pub fresh: bool,
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    pub chunks: usize,
    pub corpus_epoch: Option<String>,
    pub stored_scan_epoch: Option<String>,
    pub current_scan_epoch: String,
}

#[derive(Clone, Debug, Serialize)]
#[cfg(test)]
pub struct CodeEvidence {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub is_test: bool,
    pub snippet: String,
    #[serde(skip)]
    pub(super) score: i64,
}

#[derive(Clone, Debug)]
#[cfg(test)]
pub struct CodeContext {
    pub status: CodeStatus,
    pub action: &'static str,
    pub evidence: Vec<CodeEvidence>,
    pub omitted: usize,
    pub estimated_tokens: usize,
    pub freshness_mode: &'static str,
    pub scan_reused: bool,
    pub scan_age_ms: Option<u64>,
    pub discovery_ms: u128,
    pub content_scan_ms: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FreshnessMode {
    Strict,
    Session,
}

impl FreshnessMode {
    pub fn parse(raw: Option<&str>, option: &str) -> Result<Self> {
        match raw.unwrap_or("strict") {
            "strict" => Ok(Self::Strict),
            "session" => Ok(Self::Session),
            other => {
                let recovery = if option == "--code-freshness" {
                    "cm thread context \"<task>\" --code auto --code-freshness session"
                } else {
                    "cm code context \"<task>\" --freshness session"
                };
                Err(AppError::invalid_value(
                    option,
                    other,
                    &["strict", "session"],
                    recovery,
                ))
            }
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Session => "session",
        }
    }
}

#[cfg(test)]
impl CodeContext {
    pub fn records(&self, query: &str) -> Vec<Value> {
        let mut records = vec![json!({
            "record": "code_context_summary",
            "query": query,
            "state": self.status.state,
            "fresh": self.status.fresh,
            "action": self.action,
            "files": self.status.files,
            "symbols": self.status.symbols,
            "edges": self.status.edges,
            "chunks": self.status.chunks,
            "corpus_epoch": self.status.corpus_epoch,
            "scan_epoch": self.status.current_scan_epoch,
            "evidence": self.evidence.len(),
            "omitted": self.omitted,
            "complete": self.omitted == 0,
            "estimated_tokens": self.estimated_tokens,
            "freshness_mode": self.freshness_mode,
            "scan_reused": self.scan_reused,
            "scan_age_ms": self.scan_age_ms,
            "discovery_ms": self.discovery_ms,
            "content_scan_ms": self.content_scan_ms,
            "thread_authority_unchanged": true,
        })];
        records.extend(self.evidence.iter().map(|item| {
            json!({
                "record": "code_evidence",
                "path": item.path,
                "start_line": item.start_line,
                "end_line": item.end_line,
                "embed": format!(
                    "![[code:{}#L{}-L{}]]",
                    item.path, item.start_line, item.end_line
                ),
                "reason": item.reason,
                "symbol": item.symbol,
                "kind": item.kind,
                "is_test": item.is_test,
                "snippet": item.snippet,
            })
        }));
        records
    }
}
