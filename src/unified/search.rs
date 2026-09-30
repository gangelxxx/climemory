//! Local inverted-index search selects request roots before any thread agent runs.
use super::index::Index;

pub(super) trait SearchBackend {
    /// Must change whenever retrieval semantics or external index configuration changes.
    fn cache_key(&self) -> &str;
    fn search(&self, index: &Index, query: &str) -> Vec<String>;
}

pub(super) struct Indexed;
impl SearchBackend for Indexed {
    fn cache_key(&self) -> &str {
        "indexed-request-roots-v9"
    }
    fn search(&self, index: &Index, query: &str) -> Vec<String> {
        index.search(query)
    }
}
