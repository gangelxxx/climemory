use super::*;
use std::sync::RwLock;

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(20);
const LOCK_MARKER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const LEGACY_INCOMPLETE_MARKER_FRESH_FOR: Duration = Duration::from_secs(120);
const LOCK_MARKER_READ_CAP: u64 = 512;
const LOCK_MARKER_TAG: &str = "cm-lock/1";

/// Documented bound for a foreground command waiting behind a live
/// same-project health-worker lock: bounded acquisition retries with short
/// backoff and jitter, then an actionable error naming the owner kind, the
/// waited duration, and the retry count. Every other owner kind keeps the
/// plain bounded wait (`wait_for_lock`).
const HEALTH_LOCK_RETRY_BUDGET: Duration = Duration::from_secs(10);
const HEALTH_LOCK_MAX_RETRIES: u32 = 40;
const HEALTH_LOCK_BACKOFF_INITIAL: Duration = Duration::from_millis(25);
const HEALTH_LOCK_BACKOFF_MAX: Duration = Duration::from_millis(250);

/// Who holds a project lock. Stamped into every marker this process creates
/// and used by contenders to tell cm's own health worker apart from
/// unrelated writers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockOwnerKind {
    Foreground,
    HealthWorker,
    InternalWorker,
    /// A `cm agent run` host holding a per-task host lock (Proposal
    /// hosted-agent-workflow-mvp).
    AgentHost,
}

impl LockOwnerKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::HealthWorker => "health-worker",
            Self::InternalWorker => "internal-worker",
            Self::AgentHost => "agent-host",
        }
    }

    pub(super) fn parse(text: &str) -> Option<Self> {
        match text {
            "foreground" => Some(Self::Foreground),
            "health-worker" => Some(Self::HealthWorker),
            "internal-worker" => Some(Self::InternalWorker),
            "agent-host" => Some(Self::AgentHost),
            _ => None,
        }
    }
}

/// Identity this process stamps into lock markers. The kind defaults to
/// foreground (only the `__thread_worker` entry point re-tags itself); the
/// project key stays unset until a `Project` binds one, and such markers read
/// as non-matching owners to contenders, which keeps the normal wait policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessLockOwner {
    pub kind: LockOwnerKind,
    pub project: Option<String>,
    pub build: String,
}

static PROCESS_LOCK_OWNER: RwLock<ProcessLockOwner> = RwLock::new(ProcessLockOwner {
    kind: LockOwnerKind::Foreground,
    project: None,
    build: String::new(),
});

pub(crate) fn process_lock_owner() -> ProcessLockOwner {
    let mut owner = PROCESS_LOCK_OWNER
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    owner.build = crate::build_info::BINARY_VERSION.to_string();
    owner
}

pub struct FileLock {
    path: PathBuf,
    marker: Option<File>,
    heartbeat: Option<MarkerHeartbeat>,
    _guard: File,
}

struct MarkerHeartbeat {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl MarkerHeartbeat {
    fn start(marker: &File, interval: Duration) -> Result<Self> {
        let marker = marker.try_clone()?;
        let (stop, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("climemory-lock-heartbeat".to_string())
            .spawn(move || loop {
                match receiver.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        let _ = marker
                            .set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()));
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for MarkerHeartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(super) fn lock_guard_path(path: &Path) -> PathBuf {
    let mut guard_path = path.as_os_str().to_os_string();
    guard_path.push(".guard");
    PathBuf::from(guard_path)
}

fn open_lock_guard(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    configure_no_follow(&mut options);
    match options.open(path) {
        Ok(file) => return Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::with_hint(
            format!(
                "project lock guard is not a regular file: {}",
                path.display()
            ),
            "remove the unexpected guard path and retry the command",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    configure_no_follow(&mut options);
    let file = options.open(path)?;
    ensure_regular_guard(&file, path)?;
    Ok(file)
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn configure_no_follow(_options: &mut OpenOptions) {}

fn ensure_regular_guard(file: &File, path: &Path) -> Result<()> {
    if !opened_file_is_regular(file)? {
        return Err(AppError::with_hint(
            format!(
                "project lock guard is not a regular file: {}",
                path.display()
            ),
            "remove the unexpected guard path and retry the command",
        ));
    }
    Ok(())
}

fn opened_file_is_regular(file: &File) -> Result<bool> {
    let metadata = file.metadata()?;
    #[cfg(windows)]
    let is_reparse_point = {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    #[cfg(not(windows))]
    let is_reparse_point = false;
    Ok(metadata.is_file() && !is_reparse_point)
}

pub(crate) fn open_regular_file_no_follow(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options.open(path)?;
    if !opened_file_is_regular(&file)? {
        return Err(AppError::new(format!(
            "source path is not a regular file: {}",
            path.display()
        )));
    }
    Ok(file)
}

pub(crate) fn opened_file_matches_path(file: &File, path: &Path) -> bool {
    let Ok(current) = open_regular_file_no_follow(path) else {
        return false;
    };
    same_file_identity(file, &current)
}

fn remove_lock_marker(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(AppError::with_hint(
            format!("project lock marker is not a file: {}", path.display()),
            "remove the unexpected marker path and retry the command",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Marker layout: a legacy first line (`<pid> <timestamp>`, under the old
/// 128-byte read cap so pre-v1 builds still parse the owner pid) followed by
/// a tagged extension line carrying the owner kind, project key, and build.
pub(super) fn create_lock_marker_as(path: &Path, owner: &ProcessLockOwner) -> Result<Option<File>> {
    let mut marker = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let result = (|| -> Result<()> {
        writeln!(marker, "{} {}", std::process::id(), iso_now())?;
        writeln!(
            marker,
            "{} {} {} {}",
            LOCK_MARKER_TAG,
            owner.kind.as_str(),
            owner.project.as_deref().unwrap_or("-"),
            owner.build
        )?;
        marker.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        remove_owned_lock_marker(marker, path);
        return Err(error);
    }
    Ok(Some(marker))
}

/// Owner metadata parsed from a marker. Pre-v1 markers (and markers with a
/// missing or malformed extension line) are `Legacy`: they keep the normal
/// bounded wait and are never retried as health-worker locks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MarkerOwner {
    Legacy,
    V1 {
        kind: LockOwnerKind,
        project: Option<String>,
        build: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LegacyMarkerState {
    Missing,
    Active(MarkerOwner),
    Stale,
}

pub(super) fn legacy_marker_state(path: &Path) -> Result<LegacyMarkerState> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LegacyMarkerState::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Ok(LegacyMarkerState::Stale);
    }
    if !metadata.is_file() {
        return Err(AppError::with_hint(
            format!("project lock marker is not a file: {}", path.display()),
            "remove the unexpected marker path and retry the command",
        ));
    }

    let marker = match open_lock_marker(path)? {
        Some(marker) => marker,
        None => return Ok(LegacyMarkerState::Missing),
    };
    let metadata = marker.metadata()?;
    let mut bytes = Vec::new();
    marker.take(LOCK_MARKER_READ_CAP).read_to_end(&mut bytes)?;
    let owner = marker_owner(&bytes);
    if let Some(pid) = legacy_marker_owner(&bytes) {
        if pid == std::process::id() {
            return Ok(LegacyMarkerState::Stale);
        }
        return Ok(if process_is_running(pid) {
            LegacyMarkerState::Active(owner)
        } else {
            LegacyMarkerState::Stale
        });
    }

    Ok(if marker_timestamp_is_recent(&metadata) {
        LegacyMarkerState::Active(owner)
    } else {
        LegacyMarkerState::Stale
    })
}

fn marker_timestamp_is_recent(metadata: &fs::Metadata) -> bool {
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age < LEGACY_INCOMPLETE_MARKER_FRESH_FOR,
        Err(skew) => skew.duration() < LEGACY_INCOMPLETE_MARKER_FRESH_FOR,
    }
}

pub(super) fn open_lock_marker(path: &Path) -> Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let marker = match options.open(path) {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !opened_file_is_regular(&marker)? {
        return Err(AppError::with_hint(
            format!("project lock marker is not a file: {}", path.display()),
            "remove the unexpected marker path and retry the command",
        ));
    }
    Ok(Some(marker))
}

fn lock_marker_matches_path(marker: &File, path: &Path) -> bool {
    let Ok(Some(current)) = open_lock_marker(path) else {
        return false;
    };
    same_file_identity(marker, &current)
}

fn remove_owned_lock_marker(marker: File, path: &Path) {
    let owns_marker = lock_marker_matches_path(&marker, path);
    drop(marker);
    if owns_marker {
        let _ = fs::remove_file(path);
    }
}

#[cfg(unix)]
fn same_file_identity(left: &File, right: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(left), Ok(right)) = (left.metadata(), right.metadata()) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &File, right: &File) -> bool {
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    struct FileInformation {
        attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut c_void,
            information: *mut FileInformation,
        ) -> i32;
    }

    fn identity(
        file: &File,
        get_information: unsafe extern "system" fn(*mut c_void, *mut FileInformation) -> i32,
    ) -> Option<(u32, u64)> {
        let mut information = MaybeUninit::<FileInformation>::uninit();
        if unsafe { get_information(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
            return None;
        }
        let information = unsafe { information.assume_init() };
        Some((
            information.volume_serial_number,
            (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low),
        ))
    }

    match (
        identity(left, GetFileInformationByHandle),
        identity(right, GetFileInformationByHandle),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(_left: &File, _right: &File) -> bool {
    false
}

pub(super) fn legacy_marker_owner(bytes: &[u8]) -> Option<u32> {
    let end = bytes.iter().position(|byte| *byte == b'\n')?;
    let pid = std::str::from_utf8(&bytes[..end])
        .ok()?
        .split_whitespace()
        .next()?
        .parse::<u32>()
        .ok()?;
    valid_process_id(pid).then_some(pid)
}

/// Parses the v1 extension line. Anything that is not a well-formed tagged
/// line — pre-v1 markers included — degrades to `MarkerOwner::Legacy`.
pub(super) fn marker_owner(bytes: &[u8]) -> MarkerOwner {
    let text = std::str::from_utf8(bytes).unwrap_or("");
    let Some(extension) = text.lines().nth(1) else {
        return MarkerOwner::Legacy;
    };
    let mut fields = extension.split_whitespace();
    let (Some(tag), Some(kind), Some(project), Some(build)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return MarkerOwner::Legacy;
    };
    if tag != LOCK_MARKER_TAG {
        return MarkerOwner::Legacy;
    }
    let Some(kind) = LockOwnerKind::parse(kind) else {
        return MarkerOwner::Legacy;
    };
    MarkerOwner::V1 {
        kind,
        project: (project != "-").then(|| project.to_string()),
        build: build.to_string(),
    }
}

#[cfg(unix)]
fn valid_process_id(pid: u32) -> bool {
    pid != 0 && libc::pid_t::try_from(pid).is_ok()
}

#[cfg(not(unix))]
fn valid_process_id(pid: u32) -> bool {
    pid != 0
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    use std::ffi::c_void;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 0x102;
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return std::io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER);
    }
    let state = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        CloseHandle(handle);
    }
    match state {
        WAIT_OBJECT_0 => false,
        WAIT_TIMEOUT => true,
        _ => true,
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_running(_pid: u32) -> bool {
    true
}

fn wait_for_lock(started: Instant, wait: Duration, marker_path: &Path) -> Result<()> {
    let remaining = wait.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(lock_timeout_error(marker_path));
    }
    std::thread::sleep(remaining.min(LOCK_POLL_INTERVAL));
    Ok(())
}

/// Feedback item 11: a transient writer-lock conflict used to be an opaque
/// "another writer" — the timeout error now names the marker's owner (the
/// v1 owner kind, or the legacy marker pid) plus the marker age whenever the
/// marker is readable; unreadable markers keep the plain message.
pub(super) fn lock_timeout_error(marker_path: &Path) -> AppError {
    let hint = "retry the same command after the other cm process exits";
    let Ok(LegacyMarkerState::Active(owner)) = legacy_marker_state(marker_path) else {
        return AppError::with_hint("another climemory writer holds the project lock", hint);
    };
    let age = fs::symlink_metadata(marker_path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|age| format!(", marker age {:.0}s", age.as_secs_f64()))
        .unwrap_or_default();
    let owner_text = match &owner {
        MarkerOwner::V1 { kind, project, .. } => match project {
            Some(project) => format!("owner: {} project {}", kind.as_str(), project),
            None => format!("owner: {}", kind.as_str()),
        },
        MarkerOwner::Legacy => {
            let pid = open_lock_marker(marker_path)
                .ok()
                .flatten()
                .and_then(|marker| {
                    let mut bytes = Vec::new();
                    marker
                        .take(LOCK_MARKER_READ_CAP)
                        .read_to_end(&mut bytes)
                        .ok()?;
                    legacy_marker_owner(&bytes)
                });
            match pid {
                Some(pid) => format!("owner: legacy marker pid {pid}"),
                None => "owner: unknown marker".to_string(),
            }
        }
    };
    AppError::with_hint(
        format!("another climemory writer holds the project lock ({owner_text}{age})"),
        hint,
    )
}

/// Bounds for the health-worker retry path; tests shrink every field to keep
/// the fixtures fast and deterministic.
#[derive(Clone, Copy, Debug)]
pub(super) struct HealthLockRetryPolicy {
    pub budget: Duration,
    pub max_retries: u32,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
}

impl Default for HealthLockRetryPolicy {
    fn default() -> Self {
        Self {
            budget: HEALTH_LOCK_RETRY_BUDGET,
            max_retries: HEALTH_LOCK_MAX_RETRIES,
            backoff_initial: HEALTH_LOCK_BACKOFF_INITIAL,
            backoff_max: HEALTH_LOCK_BACKOFF_MAX,
        }
    }
}

/// Only a foreground contender retries, and only behind a live health-worker
/// marker from the same project and build. A health worker never retries
/// behind another health worker, so a startup burst cannot turn into a
/// foreground/worker retry loop.
pub(super) fn is_retriable_health_lock(owner: &MarkerOwner, ours: &ProcessLockOwner) -> bool {
    match owner {
        MarkerOwner::V1 {
            kind: LockOwnerKind::HealthWorker,
            project: Some(project),
            build,
        } => {
            ours.kind == LockOwnerKind::Foreground
                && ours.project.as_deref() == Some(project.as_str())
                && *build == ours.build
        }
        _ => false,
    }
}

struct HealthLockRetry {
    policy: HealthLockRetryPolicy,
    started: Instant,
    retries: u32,
}

impl HealthLockRetry {
    fn new(policy: HealthLockRetryPolicy, wait: Duration) -> Self {
        let mut policy = policy;
        // Never bound a health-worker wait tighter than the normal policy the
        // caller already tolerates (the code-index lock waits 30s normally).
        policy.budget = policy.budget.max(wait);
        Self {
            policy,
            started: Instant::now(),
            retries: 0,
        }
    }

    fn wait(&mut self) -> Result<()> {
        let remaining = self.policy.budget.saturating_sub(self.started.elapsed());
        if self.retries >= self.policy.max_retries || remaining.is_zero() {
            return Err(AppError::with_hint(
                format!(
                    "cm health worker holds the project lock (waited {:.1}s across {} retries)",
                    self.started.elapsed().as_secs_f64(),
                    self.retries
                ),
                "retry the same command; the health worker releases its lock shortly",
            ));
        }
        let shift = self.retries.min(10);
        let base = self
            .policy
            .backoff_initial
            .as_nanos()
            .saturating_mul(1u128 << shift)
            .min(self.policy.backoff_max.as_nanos());
        // Jitter in [base/2, base]: spreads a queued burst of foreground
        // contenders so they do not re-poll in lockstep.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.subsec_nanos())
            .unwrap_or(0);
        let spread = base / 2;
        let sleep = spread + u128::from(nanos) % (spread + 1);
        let sleep = Duration::from_nanos(sleep.min(u128::from(u64::MAX)) as u64);
        std::thread::sleep(sleep.min(remaining));
        self.retries += 1;
        Ok(())
    }
}

// Keep the ordinary deadline independent from a preceding health-worker wait.
struct LockWaitPhase {
    ordinary_started: Option<Instant>,
    recovering_health: bool,
}
impl LockWaitPhase {
    fn new() -> Self {
        Self {
            ordinary_started: Some(Instant::now()),
            recovering_health: false,
        }
    }
    fn observe(&mut self, holder: Option<&MarkerOwner>, ours: &ProcessLockOwner) -> bool {
        let health = holder.map_or(self.recovering_health, |h| {
            is_retriable_health_lock(h, ours)
        });
        self.recovering_health = health;
        if health {
            self.ordinary_started = None;
        }
        health
    }
    fn ordinary(&mut self, wait: Duration, path: &Path) -> Result<()> {
        wait_for_lock(
            *self.ordinary_started.get_or_insert_with(Instant::now),
            wait,
            path,
        )
    }
}

impl FileLock {
    pub fn acquire(path: &Path, wait: Duration) -> Result<Self> {
        Self::acquire_with_heartbeat(path, wait, LOCK_MARKER_HEARTBEAT_INTERVAL)
    }

    pub(super) fn acquire_with_heartbeat(
        path: &Path,
        wait: Duration,
        heartbeat_interval: Duration,
    ) -> Result<Self> {
        Self::acquire_with_policy(
            path,
            wait,
            heartbeat_interval,
            &process_lock_owner(),
            HealthLockRetryPolicy::default(),
        )
    }

    /// Owner-aware acquisition. The retry policy lives entirely inside lock
    /// acquisition — before the caller's mutation commit point — so a retried
    /// command continues its original operation exactly once instead of
    /// replaying a persisted mutation.
    pub(super) fn acquire_with_policy(
        path: &Path,
        wait: Duration,
        heartbeat_interval: Duration,
        owner: &ProcessLockOwner,
        retry_policy: HealthLockRetryPolicy,
    ) -> Result<Self> {
        let guard = open_lock_guard(&lock_guard_path(path))?;
        let mut phase = LockWaitPhase::new();
        let mut health_retry: Option<HealthLockRetry> = None;
        loop {
            match guard.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    // The holder's guard gives no owner metadata; peek at the
                    // marker the holder published right after taking the
                    // guard. During a known health-worker handoff the marker can
                    // disappear before its guard unlocks; retain the bounded
                    // health budget instead of reverting to an expired ordinary wait.
                    let holder = legacy_marker_state(path)
                        .ok()
                        .and_then(|state| match state {
                            LegacyMarkerState::Active(owner) => Some(owner),
                            _ => None,
                        });
                    if phase.observe(holder.as_ref(), owner) {
                        health_retry
                            .get_or_insert_with(|| HealthLockRetry::new(retry_policy, wait))
                            .wait()?;
                    } else {
                        phase.ordinary(wait, path)?;
                    }
                }
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        let mut recovered_stale_marker = false;
        loop {
            match legacy_marker_state(path)? {
                LegacyMarkerState::Active(marker_owner) => {
                    if phase.observe(Some(&marker_owner), owner) {
                        health_retry
                            .get_or_insert_with(|| HealthLockRetry::new(retry_policy, wait))
                            .wait()?;
                    } else {
                        phase.ordinary(wait, path)?;
                    }
                    continue;
                }
                LegacyMarkerState::Stale => {
                    if recovered_stale_marker {
                        phase.ordinary(wait, path)?;
                    }
                    remove_lock_marker(path)?;
                    recovered_stale_marker = true;
                    continue;
                }
                LegacyMarkerState::Missing => {}
            }
            let marker = match create_lock_marker_as(path, owner)? {
                Some(marker) => marker,
                None => {
                    phase.ordinary(wait, path)?;
                    continue;
                }
            };
            let heartbeat = match MarkerHeartbeat::start(&marker, heartbeat_interval) {
                Ok(heartbeat) => heartbeat,
                Err(error) => {
                    remove_owned_lock_marker(marker, path);
                    return Err(error);
                }
            };
            return Ok(Self {
                path: path.to_path_buf(),
                marker: Some(marker),
                heartbeat: Some(heartbeat),
                _guard: guard,
            });
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        drop(self.heartbeat.take());
        if let Some(marker) = self.marker.take() {
            remove_owned_lock_marker(marker, &self.path);
        }
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;
    #[test]
    fn health_handoff_gets_an_independent_ordinary_deadline() {
        let ours = ProcessLockOwner {
            kind: LockOwnerKind::Foreground,
            project: Some("project".into()),
            build: "build".into(),
        };
        let health = MarkerOwner::V1 {
            kind: LockOwnerKind::HealthWorker,
            project: ours.project.clone(),
            build: ours.build.clone(),
        };
        let foreground = MarkerOwner::V1 {
            kind: LockOwnerKind::Foreground,
            project: ours.project.clone(),
            build: ours.build.clone(),
        };
        let mut phase = LockWaitPhase::new();
        phase.ordinary_started = Some(Instant::now() - Duration::from_secs(5));
        assert!(!phase.observe(None, &ours));
        assert!(phase.observe(Some(&health), &ours));
        assert!(phase.ordinary_started.is_none());
        assert!(
            phase.observe(None, &ours),
            "missing marker during known health handoff retains health budget"
        );
        assert!(!phase.observe(Some(&foreground), &ours));
        assert!(
            !phase.observe(None, &ours),
            "later foreground handoff cannot inherit historical health status"
        );
        let temp = tempfile::tempdir().unwrap();
        phase
            .ordinary(Duration::from_secs(1), &temp.path().join("lock"))
            .unwrap();
        assert!(phase.ordinary_started.unwrap().elapsed() < Duration::from_secs(1));
        let mut retry = HealthLockRetry::new(
            HealthLockRetryPolicy {
                budget: Duration::from_secs(1),
                max_retries: 0,
                backoff_initial: Duration::ZERO,
                backoff_max: Duration::ZERO,
            },
            Duration::ZERO,
        );
        assert!(
            retry.wait().is_err(),
            "health handoff never removes the bounded retry policy"
        );
    }
}
