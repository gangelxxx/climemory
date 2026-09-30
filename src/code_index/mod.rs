use crate::code;
#[cfg(test)]
use crate::output::output_stats;
use crate::project::Project;
use crate::util::{
    atomic_write, digest, open_regular_file_no_follow, opened_file_matches_path, AppError, Result,
};
use ignore::WalkBuilder;
#[cfg(feature = "code-index")]
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as ShaDigest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
mod chunks;
mod discover;
mod find;
mod grep;
mod indexing;
mod prepare;
mod presentation;
mod query;
mod report;
mod schema;
#[cfg(test)]
mod search;
mod session;
mod status;
mod storage;
#[cfg(test)]
mod tests;
mod types;

use chunks::*;
use discover::*;
use indexing::*;
use prepare::*;
use schema::*;
#[cfg(test)]
use search::*;
use session::*;
use status::*;
use storage::*;
use types::*;

pub use find::{find, FindContinuationSpec};
pub use grep::{grep, GrepContinuationSpec, GrepModes, GrepScope, GrepSelector};
#[cfg(test)]
pub use indexing::index;
pub use presentation::{SearchPresentation, SearchSort};
#[cfg(test)]
pub use query::context;
pub use report::{CodeIndexStats, CodeIndexTimings, CodeStatus, FreshnessMode};
#[cfg(test)]
pub use status::{status, symbol_anchor_extent};
const CHUNK_LINES: usize = 120;
const CHUNK_OVERLAP: usize = 0;
const PREPARE_BATCH_MAX_FILES: usize = 128;
const PREPARE_BATCH_MAX_BYTES: usize = 32 * 1024 * 1024;
const PREPARE_LOOKAHEAD_MAX_FILES: usize = 64;
const PREPARE_LOOKAHEAD_MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const GIT_DISCOVERY_MIN_INDEX_BYTES: u64 = 4 * 1024 * 1024;
const DELETE_BATCH_FILES: usize = 128;
const BULK_FTS_MIN_CHANGES: usize = 256;
const BULK_FTS_MIN_CHANGE_PERCENT: usize = 20;
const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
const SESSION_FRESHNESS_MS: u64 = 30_000;
const SESSION_LEASE_FILE: &str = "code-index-scan.json";
const BENCH_DISCOVERY_BACKEND_ENV: &str = "CLIMEMORY_BENCH_CODE_DISCOVERY_BACKEND";
const META_COUNT_FILES: &str = "code_count_files";
const META_COUNT_SYMBOLS: &str = "code_count_symbols";
const META_COUNT_EDGES: &str = "code_count_edges";
const META_COUNT_CHUNKS: &str = "code_count_chunks";
const META_DERIVED_CORPUS_EPOCH: &str = "derived_corpus_epoch";
#[cfg(test)]
const EXACT_DEFINITION_BASE_SCORE: i64 = 700;
#[cfg(test)]
const SUFFIX_DEFINITION_BASE_SCORE: i64 = 600;
#[cfg(test)]
const CALLER_BASE_SCORE: i64 = 500;
#[cfg(test)]
const CALLEE_BASE_SCORE: i64 = 350;
#[cfg(test)]
const PRODUCTION_PREFIX: usize = 4;

#[cfg(test)]
thread_local! {
    static DISCOVERY_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(test)]
    static CONNECTION_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(test)]
    static COUNT_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(test)]
    static CORPUS_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(test)]
    static DELETE_BATCH_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(test)]
    static BULK_FTS_STARTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
static DISCOVERY_HOOKS: std::sync::Mutex<Vec<(PathBuf, std::sync::mpsc::Sender<()>)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
struct PrePublishHook {
    root: PathBuf,
    reached: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static PRE_PUBLISH_HOOKS: std::sync::Mutex<Vec<PrePublishHook>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
struct ContextQueryHook {
    root: PathBuf,
    reached: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static CONTEXT_QUERY_HOOKS: std::sync::Mutex<Vec<ContextQueryHook>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
use report::{CodeContext, CodeEvidence};
