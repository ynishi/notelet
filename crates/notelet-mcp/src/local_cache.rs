//! [`LocalCache`]: a read cache in front of [`RemoteNoteRepo`](crate::remote::RemoteNoteRepo), for a
//! client that reads notes on the hot path. Every read of the remote costs one
//! round trip (about 1-2 s). The cache is a [`NoteRepo`] adapter like the
//! others: the model, the service and the tools do not change. It exists only
//! inside `notelet-mcp stdio` connected to a remote (`NOTELET_REMOTE_URL`), the
//! local client that talks to the hosted notelet over HTTP; `serve` never uses
//! it. It is opt-in (`NOTELET_LOCAL_CACHE=true`, see [`crate::config`]) because
//! it writes note bodies to disk.
//!
//! ```text
//! client (MCP over stdio)
//!   └ NoteService
//!       └ LocalCache
//!           read  (get / get_view / list / search / dump) -> the cache's own notes (mirror)
//!           write (put / append / delete) -> remote -> on success, update mirror
//!           history -> remote (not cached)
//!           poller  -> list(updated_at >= hw - 60s) every 30 s; full path relist every 10th poll
//!           snapshot <-> <cache dir>/notes.json  (written only by the lock holder)
//! ```
//!
//! # Reads
//!
//! The mirror is a map of notes owned by the cache (`store`), which places
//! and removes notes exactly as the remote has them. The in-memory adapter of
//! `notelet-core` cannot be used for that, because its `put` assigns the
//! version and the time itself and the port has no way to place a note as
//! given; so the map answers `list`, `search` and `dump` with the same
//! ordering, filters, paging and snippets as that adapter (a test runs both on
//! the same notes). Once the
//! mirror is **filled** (loaded from the snapshot at start, or after the first
//! complete pass over the remote) `get`, `get_view`, `list`, `search` and
//! `dump` are answered from it. While it is empty they pass through to the
//! remote. `history` and `history_meta` always pass through: a copy has no
//! revisions.
//!
//! # Writes
//!
//! A write goes to the remote. Only when the remote accepts it is the mirror
//! updated from the answer, so a client reads its own writes at once:
//!
//! * `put` places the note [`RemoteNoteRepo`] returns: the sent body with the
//!   remote's version and `updated_at`, and an empty `updated_by` (the remote
//!   does not report it; the next poll that lists the note fills it in, so for
//!   up to one poll interval, 30 s, `get` shows the actor blank and a `list`
//!   filtered on `updated_by` misses the note). A `put` that changed nothing
//!   (`changed: false`) while the mirror is behind it does not place that
//!   synthesised note: the path is read from the remote instead.
//! * `append` places the note `RemoteNoteRepo` reads back after the append.
//! * `delete` removes the path from the mirror.
//!
//! A write never overwrites a mirror entry with a lower or equal version, so a
//! poll that raced ahead of the write's answer wins. An unreachable remote
//! fails the write with [`CoreError::Storage`], exactly as without the cache.
//! Writes are never queued: an unsynced local write would be state, not cache.
//! A `NotFound` from the remote on `get`, `put`, `append` or `delete` drops
//! the path from the mirror at once. A `VersionConflict` from `put` or
//! `delete` means the mirror was behind, so the path is read from the remote
//! and the mirror brought up to it (a client that read the old version and
//! writes again would otherwise conflict until the next poll). That refresh
//! places what it read only if the mirror still holds what it held before the
//! read, so a delete or write made here in the meantime is not undone.
//!
//! # Polling
//!
//! A background task ([`LocalCache::spawn`]) syncs every 30 s
//! ([`LocalCache::sync_once`]). It lists `updated_at >= hw - 60 s`,
//! where `hw` is the highest `updated_at` a listing of the remote has
//! returned; the local clock is never used, so clock skew between this machine
//! and the remote does not matter. The pages are walked to the end. A note is
//! taken only when its version is above the mirror's (versions never repeat
//! within a path, so applying one twice is harmless), and its body is fetched
//! with `get`, a few at a time.
//!
//! The 60 s overlap exists because `pg.rs` stamps `clock_timestamp()` at
//! statement time and the transaction commits later: a note can become visible
//! with an `updated_at` older than one a poll has already seen, so `hw` alone
//! would skip it. `hw` moves only when a whole pass succeeded, so a pass that
//! fails halfway is repeated from the same point.
//!
//! One extra rule rides on the listing: a note whose version equals the
//! mirror's but whose `updated_by` the mirror lacks (it came from a `put`) gets
//! the listing's `updated_by` and `updated_at`, with no fetch.
//!
//! # Deletes
//!
//! The delta listing cannot show an absent note. So at start and on every 10th
//! poll (about 5 min) the pass lists all paths instead of a window, takes
//! whatever is newer as above, and then drops the mirror entries the listing
//! lacks. An entry put into the mirror after that pass began (by a write of
//! this process, or by a fetch) is kept: the listing may predate it. A delete
//! this process made while a fetch of the same path was in flight is also
//! remembered for the rest of the pass, so the fetch cannot bring the note
//! back. A `NotFound` met by a read or write drops the path at once.
//!
//! # Connecting
//!
//! With the cache on, the process starts without waiting for the remote: the
//! background task ([`LocalCache::spawn`], which takes the function that
//! connects) fills the remote slot. While the slot is empty, reads from a
//! filled mirror succeed. A call that needs the remote (a pass-through read,
//! `history`, a write, a poll) does not fail at once: it connects itself,
//! sharing the attempt with the task and with other callers (one attempt at a
//! time; a caller that waited for a failed attempt gets its failure as
//! [`CoreError::Storage`]), and only that attempt, bounded by the connect
//! timeout, can make it fail. So a cold start without a snapshot answers its
//! first read once the connect is done, and "reads go to the remote until the
//! first fill" holds. With no connect function at all the error is "the remote
//! notelet is not connected".
//!
//! A failing call does not empty the slot: [`RemoteNoteRepo`](crate::remote::RemoteNoteRepo)
//! re-opens an expired session by itself, and one timeout or reset fails only
//! the call that met it, as it does without the cache. The slot is replaced
//! only when the client is shown to be dead (the [`Connected`] probe, which for
//! `RemoteNoteRepo` is a closed transport). The back-off is the task's own: when its connect
//! or poll fails it waits 30 s, doubling to 5 min ([`Timing`]), before the
//! next try, and a success resets it. It never delays a caller. Each state
//! change is logged at `warn` or `info`.
//!
//! # Disk
//!
//! See [`disk`]. The directory is `$XDG_CACHE_HOME/notelet/<key>/` (else
//! `$HOME/.cache`, on macOS `~/Library/Caches`), `<key>` being the first 16 hex
//! digits of the sha256 of the remote URL, so the host name never appears in a
//! path; [`crate::config::cache_dir`] resolves it. Only the process holding
//! the exclusive lock on `<dir>/lock` writes `notes.json` (the `hw` and the
//! notes), at most every 30 s (the task persists once per poll) and only after
//! a change, and only once the mirror is filled, so a half-filled mirror never
//! becomes a snapshot that the next start would trust. Every process reads the
//! snapshot once at start; a process that does not hold the lock then keeps its
//! mirror in memory, current through its own poll. Writes of the snapshot do
//! not overlap (see [`LocalCache::persist`]). [`LocalCache::close`]
//! writes a pending change, but the binary reaches it only when `stdio` ends
//! at stdin EOF: a signal ends the process without it. Then up to one persist
//! interval (30 s) of changes is not on disk; nothing is lost, since writes are already on the remote and the next
//! start's relist repairs the snapshot.
//!
//! # Known limitations
//!
//! * While connected, another client's write shows up after at most one poll
//!   (30 s) plus a round trip. While the remote is unreachable, reads show the
//!   last synced state, and the cache logs that it is disconnected.
//! * A delete by another client shows up after at most about 5 min, or at once
//!   when a read or write meets `NotFound`.
//! * Tool answers do not say how old the data is, so tool shapes stay
//!   unchanged.
//! * After a `put`, `updated_by` is blank in the cache until the next poll
//!   lists the note (up to 30 s): `get` shows it empty and a `list` filtered
//!   on `updated_by` does not match the note yet. `append` is not affected (it
//!   reads the note back).
//!
//! # Shared state
//!
//! Request handlers and the one poll task share the mirror, the remote slot
//! and a little bookkeeping (`hw`, the pass sets), each behind its own lock
//! that is never held across a remote call. `apply` serialises the
//! check-then-place of a mirror update, `sync` serialises passes; the
//! file system is touched only on `spawn_blocking`, and timers live only in the
//! spawned task, so the rest can be driven step by step from a test.

mod disk;
mod store;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use notelet_core::domain::{DumpEntry, Field, Filter, FilterValue, Op};
use notelet_core::{
    CoreError, Note, NoteMeta, NotePath, NoteRepo, NoteView, Page, PageOut, PutResult, Revision,
    RevisionMeta, SearchHit, Tail,
};
use tokio::task::{JoinHandle, JoinSet};

pub use disk::DiskCache;
use disk::Snapshot;
use store::Store;

/// Seconds a delta listing reaches back from `hw`. See the module doc,
/// "Polling".
const OVERLAP_SECS: i64 = 60;

/// Every this many polls the pass lists all paths. See "Deletes".
const POLLS_PER_RELIST: u64 = 10;

/// Bodies fetched at the same time within a pass.
const FETCH_PARALLEL: usize = 8;

/// How often the task polls, and how it backs off when the remote is
/// unreachable. The default is the documented one; tests shorten it.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Pause between two successful polls (30 s).
    pub poll: Duration,
    /// First pause after a failed connect or a failed poll (30 s); it doubles
    /// with each failure in a row.
    pub backoff_min: Duration,
    /// Ceiling of the doubling back-off (5 min).
    pub backoff_max: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(30),
            backoff_min: Duration::from_secs(30),
            backoff_max: Duration::from_secs(300),
        }
    }
}

impl Timing {
    /// The pause after `current`: doubled, up to the ceiling.
    fn next_backoff(&self, current: Duration) -> Duration {
        (current * 2).min(self.backoff_max)
    }
}

/// The cause of a [`CoreError::Storage`] raised while no remote is connected.
#[derive(Debug)]
struct NotConnected;

impl std::fmt::Display for NotConnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the remote notelet is not connected")
    }
}

impl std::error::Error for NotConnected {}

/// The cause of a [`CoreError::Storage`] returned to a caller that waited
/// for a connect attempt that failed (the attempt's own caller got the
/// original error).
#[derive(Debug)]
struct ConnectFailed(String);

impl std::fmt::Display for ConnectFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "connecting to the remote notelet failed: {}", self.0)
    }
}

impl std::error::Error for ConnectFailed {}

fn not_connected() -> CoreError {
    CoreError::Storage(Box::new(NotConnected))
}

fn storage(e: impl std::error::Error + Send + Sync + 'static) -> CoreError {
    CoreError::Storage(Box::new(e))
}

/// Lock a std mutex; a poisoned one is still consistent here (the guarded
/// values are only ever replaced whole), so its content is used.
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// See [`Core::attempts`].
#[derive(Default)]
struct Attempts {
    finished: u64,
    /// The text of the last attempt's error; `None` when it succeeded.
    failure: Option<String>,
}

/// Tells whether a connected remote can no longer be used at all.
type ClosedProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// What a connect function returns: the remote, and optionally a way to see
/// that its client is dead (for [`RemoteNoteRepo`](crate::remote::RemoteNoteRepo),
/// its transport is closed). A client that is merely failing calls is not
/// dead: it is kept, since it re-opens an expired session by itself.
pub struct Connected {
    repo: Arc<dyn NoteRepo>,
    closed: Option<ClosedProbe>,
}

impl Connected {
    /// A remote with no liveness probe: it is kept until replaced.
    pub fn new(repo: Arc<dyn NoteRepo>) -> Self {
        Self { repo, closed: None }
    }

    /// Replace the remote by a new connection once `closed()` is `true`.
    pub fn closed_when(mut self, closed: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.closed = Some(Arc::new(closed));
        self
    }
}

/// The function that opens the remote, as the cache holds it.
type Connector = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<Connected, CoreError>> + Send>> + Send + Sync,
>;

/// Puts the "changed since the last snapshot" mark back when dropped armed.
struct Pending<'a> {
    dirty: &'a AtomicBool,
    armed: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.dirty.store(true, Ordering::Release);
        }
    }
}

/// Who is placing a note: a write of this process, a poll, or a refresh of
/// one path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Write,
    Poll,
    /// A refresh that read the path while the mirror held this version
    /// (`None`: nothing). It places only if the mirror still holds exactly
    /// that, so a change made here in the meantime (a delete, a write) wins.
    Refresh(Option<i64>),
}

/// Bookkeeping of the pass in progress.
#[derive(Default)]
struct Pass {
    /// Paths placed in the mirror since the pass began.
    touched: HashSet<String>,
    /// Paths this process deleted since the pass began, with the version
    /// deleted: a fetch of that version or older must not bring them back.
    forgotten: HashMap<String, i64>,
}

#[derive(Default)]
struct State {
    /// Highest `updated_at` a listing has returned.
    hw: Option<DateTime<Utc>>,
    /// Successful passes so far.
    polls: u64,
    pass: Option<Pass>,
}

struct Core {
    /// The notes, as the remote has them. Never held across an await.
    mirror: Mutex<Store>,
    remote: Mutex<Option<Connected>>,
    /// Opens the remote when the slot is empty; set by [`LocalCache::spawn`].
    connector: Mutex<Option<Connector>>,
    /// Single-flight: held across a connect attempt.
    connecting: tokio::sync::Mutex<()>,
    /// The last connect attempt: how many have finished, and whether the last
    /// one failed (with its text), so that callers that waited for it share
    /// its outcome. Written under `connecting`.
    attempts: Mutex<Attempts>,
    /// Held across a snapshot write, by the blocking thread that does it.
    persist_lock: Arc<tokio::sync::Mutex<()>>,
    /// Reads are answered from the mirror.
    filled: AtomicBool,
    /// The mirror changed since the snapshot was written.
    dirty: AtomicBool,
    state: Mutex<State>,
    /// Held across a check-then-place on the mirror.
    apply: tokio::sync::Mutex<()>,
    /// Held across a pass.
    sync: tokio::sync::Mutex<()>,
    disk: Option<Arc<DiskCache>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

/// A [`NoteRepo`] that answers reads from a local mirror of a remote notelet
/// and forwards writes to it. See the module doc. Cloning shares the cache.
#[derive(Clone)]
pub struct LocalCache {
    core: Arc<Core>,
}

impl LocalCache {
    /// A cache with no remote yet. With `disk`, the snapshot there (if any)
    /// is loaded into the mirror, which then counts as filled; an unreadable
    /// snapshot is logged and ignored.
    pub async fn new(disk: Option<DiskCache>) -> Self {
        let disk = disk.map(Arc::new);
        let core = Core {
            mirror: Mutex::new(Store::default()),
            remote: Mutex::new(None),
            connector: Mutex::new(None),
            connecting: tokio::sync::Mutex::new(()),
            attempts: Mutex::new(Attempts::default()),
            persist_lock: Arc::new(tokio::sync::Mutex::new(())),
            filled: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            state: Mutex::new(State::default()),
            apply: tokio::sync::Mutex::new(()),
            sync: tokio::sync::Mutex::new(()),
            disk,
            task: Mutex::new(None),
        };
        let this = Self {
            core: Arc::new(core),
        };
        this.load_snapshot().await;
        this
    }

    async fn load_snapshot(&self) {
        let Some(disk) = self.core.disk.clone() else {
            return;
        };
        let writer = disk.is_writer();
        let read = tokio::task::spawn_blocking(move || disk.read()).await;
        match read {
            Ok(Ok(Some(snapshot))) => {
                let mut n = 0usize;
                for note in snapshot.notes {
                    // the file is ours, but a path is checked before it is trusted
                    if NotePath::parse(note.path.as_str()).is_ok() {
                        locked(&self.core.mirror).insert(note);
                        n += 1;
                    }
                }
                locked(&self.core.state).hw = snapshot.hw;
                self.core.filled.store(true, Ordering::Release);
                tracing::info!(notes = n, writer, "local cache: loaded the snapshot");
            }
            Ok(Ok(None)) => tracing::info!(writer, "local cache: no snapshot yet"),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "local cache: ignoring an unreadable snapshot")
            }
            Err(e) => tracing::warn!(error = %e, "local cache: reading the snapshot failed"),
        }
    }

    /// Put a connected remote in the slot (the task and the on-demand connect
    /// do it with [`Connected`]).
    #[cfg(test)]
    fn set_remote(&self, remote: Arc<dyn NoteRepo>) {
        *locked(&self.core.remote) = Some(Connected::new(remote));
    }

    /// Empty the slot.
    #[cfg(test)]
    fn clear_remote(&self) {
        *locked(&self.core.remote) = None;
    }

    /// Whether a live remote is in the slot.
    #[cfg(test)]
    fn is_connected(&self) -> bool {
        self.slot().is_some()
    }

    /// Whether reads are answered from the mirror.
    #[cfg(test)]
    fn is_filled(&self) -> bool {
        self.core.filled.load(Ordering::Acquire)
    }

    /// The remote in the slot, unless its client is dead.
    fn slot(&self) -> Option<Arc<dyn NoteRepo>> {
        let slot = locked(&self.core.remote);
        let held = slot.as_ref()?;
        match &held.closed {
            Some(closed) if closed() => None,
            _ => Some(held.repo.clone()),
        }
    }

    /// The remote to call. A failing call never empties the slot; only a
    /// client that is dead does. With no usable remote and a connect function,
    /// connects now ([`Self::connect_now`]) instead of failing: a caller
    /// never waits for the back-off of the background task, only for its own
    /// attempt, which the connect timeout bounds.
    async fn remote(&self) -> Result<Arc<dyn NoteRepo>, CoreError> {
        match self.slot() {
            Some(repo) => Ok(repo),
            None => self.connect_now().await,
        }
    }

    /// Open the remote, one attempt at a time. A caller that waited for an
    /// attempt that failed gets that failure (as [`CoreError::Storage`])
    /// rather than starting another; one that waited for a success uses it.
    async fn connect_now(&self) -> Result<Arc<dyn NoteRepo>, CoreError> {
        let connector = locked(&self.core.connector)
            .clone()
            .ok_or_else(not_connected)?;
        let seen = locked(&self.core.attempts).finished;
        let _attempt = self.core.connecting.lock().await;
        if let Some(repo) = self.slot() {
            return Ok(repo);
        }
        {
            // an attempt finished while this caller waited: if it failed,
            // share its failure; if it succeeded but the client is gone
            // again, this caller makes a fresh attempt
            let last = locked(&self.core.attempts);
            if last.finished != seen {
                if let Some(failure) = &last.failure {
                    return Err(storage(ConnectFailed(failure.clone())));
                }
            }
        }
        let result = connector().await;
        let mut last = locked(&self.core.attempts);
        last.finished += 1;
        match result {
            Ok(connected) => {
                last.failure = None;
                let repo = connected.repo.clone();
                *locked(&self.core.remote) = Some(connected);
                tracing::info!("local cache: connected to the remote");
                Ok(repo)
            }
            Err(e) => {
                last.failure = Some(e.chain().to_string());
                Err(e)
            }
        }
    }

    fn serves_from_mirror(&self) -> bool {
        self.core.filled.load(Ordering::Acquire)
    }

    // ---- mirror updates -------------------------------------------------

    /// Place `note` unless the mirror already holds this version or a newer
    /// one (or, for a poll, this process deleted it during the pass).
    async fn place(&self, note: Note, source: Source) -> bool {
        let _g = self.core.apply.lock().await;
        let path = note.path.as_str().to_string();
        let held = self.held(&note.path);
        if let Source::Refresh(expected) = source {
            if held.as_ref().map(|n| n.version) != expected {
                return false;
            }
        }
        if source != Source::Write {
            let st = locked(&self.core.state);
            let deleted = st.pass.as_ref().and_then(|p| p.forgotten.get(&path));
            if deleted.is_some_and(|v| *v >= note.version) {
                return false;
            }
        }
        if held.is_some_and(|cur| cur.version >= note.version) {
            return false;
        }
        locked(&self.core.mirror).insert(note);
        let mut st = locked(&self.core.state);
        if let Some(pass) = st.pass.as_mut() {
            pass.touched.insert(path.clone());
            if source == Source::Write {
                pass.forgotten.remove(&path);
            }
        }
        self.core.dirty.store(true, Ordering::Release);
        true
    }

    /// Drop `path`. `deleted` is the version a delete removed, remembered for
    /// the rest of the pass; `None` (a `NotFound`) remembers the version the
    /// mirror held.
    async fn forget(&self, path: &NotePath, deleted: Option<i64>) {
        let _g = self.core.apply.lock().await;
        self.forget_locked(path, deleted).await;
    }

    /// [`Self::forget`] with `apply` already held.
    async fn forget_locked(&self, path: &NotePath, deleted: Option<i64>) {
        let cur = self.held(path);
        let version = deleted.or(cur.as_ref().map(|n| n.version));
        if cur.is_some() {
            locked(&self.core.mirror).remove(path);
            self.core.dirty.store(true, Ordering::Release);
        }
        let mut st = locked(&self.core.state);
        if let Some(pass) = st.pass.as_mut() {
            pass.touched.remove(path.as_str());
            if let Some(v) = version {
                let slot = pass.forgotten.entry(path.as_str().to_string()).or_insert(v);
                *slot = (*slot).max(v);
            }
        }
    }

    /// What a poll does with a listed note: `true` when its body has to be
    /// fetched; otherwise it may only fill in a missing `updated_by`.
    async fn consider(&self, meta: &NoteMeta) -> bool {
        let _g = self.core.apply.lock().await;
        let cur = self.held(&meta.path);
        let Some(cur) = cur else {
            return true;
        };
        if cur.version < meta.version {
            return true;
        }
        if cur.version == meta.version && cur.updated_by.is_empty() && !meta.updated_by.is_empty() {
            let patched = Note {
                updated_at: meta.updated_at,
                updated_by: meta.updated_by.clone(),
                ..cur
            };
            locked(&self.core.mirror).insert(patched);
            if let Some(pass) = locked(&self.core.state).pass.as_mut() {
                pass.touched.insert(meta.path.as_str().to_string());
            }
            self.core.dirty.store(true, Ordering::Release);
        }
        false
    }

    // ---- sync -----------------------------------------------------------

    /// One poll: the delta listing, or the full one when it is every 10th
    /// (and the first). Fails with [`CoreError::Storage`] when no remote is
    /// connected or the remote cannot be reached; the mirror keeps what it
    /// has.
    pub async fn sync_once(&self) -> Result<(), CoreError> {
        let _s = self.core.sync.lock().await;
        let full = locked(&self.core.state)
            .polls
            .is_multiple_of(POLLS_PER_RELIST);
        self.pass(full).await
    }

    /// A full pass now, whatever the poll count.
    #[cfg(test)]
    async fn relist(&self) -> Result<(), CoreError> {
        let _s = self.core.sync.lock().await;
        self.pass(true).await
    }

    async fn pass(&self, full: bool) -> Result<(), CoreError> {
        let remote = self.remote().await?;
        let hw = {
            let mut st = locked(&self.core.state);
            st.pass = Some(Pass::default());
            st.hw
        };
        let scanned = self.scan(&remote, full, hw).await;
        let mut st = locked(&self.core.state);
        st.pass = None;
        let top = scanned?;
        if top != st.hw {
            st.hw = top;
            self.core.dirty.store(true, Ordering::Release);
        }
        st.polls += 1;
        if full {
            self.core.filled.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// List (all, or the window after `hw`), fetch what is newer, and for a
    /// full pass drop what is gone. Returns the new high-water mark.
    async fn scan(
        &self,
        remote: &Arc<dyn NoteRepo>,
        full: bool,
        hw: Option<DateTime<Utc>>,
    ) -> Result<Option<DateTime<Utc>>, CoreError> {
        let filters = match (full, hw) {
            (false, Some(hw)) => vec![Filter {
                field: Field::UpdatedAt,
                op: Op::Gte,
                value: FilterValue::Time(hw - chrono::Duration::seconds(OVERLAP_SECS)),
            }],
            _ => Vec::new(),
        };
        let mut top = hw;
        let mut seen = HashSet::new();
        let mut cursor = None;
        loop {
            let page = Page::new(Some(Page::MAX_LIMIT), cursor)?;
            let out = remote.list("", &filters, &page).await?;
            let mut wanted = Vec::new();
            for meta in &out.items {
                top = top.max(Some(meta.updated_at));
                if full {
                    seen.insert(meta.path.as_str().to_string());
                }
                if self.consider(meta).await {
                    wanted.push(meta.path.clone());
                }
            }
            self.fetch(remote, wanted).await?;
            match out.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        if full {
            self.prune(&seen).await;
        }
        Ok(top)
    }

    /// Fetch the bodies of `paths`, [`FETCH_PARALLEL`] at a time, and place
    /// them. A note that is gone by now is dropped.
    async fn fetch(
        &self,
        remote: &Arc<dyn NoteRepo>,
        paths: Vec<NotePath>,
    ) -> Result<(), CoreError> {
        for chunk in paths.chunks(FETCH_PARALLEL) {
            let mut set = JoinSet::new();
            for path in chunk {
                let remote = remote.clone();
                let path = path.clone();
                set.spawn(async move {
                    let got = remote.get(&path).await;
                    (path, got)
                });
            }
            while let Some(joined) = set.join_next().await {
                let (path, got) = joined.map_err(storage)?;
                match got? {
                    Some(note) => {
                        self.place(note, Source::Poll).await;
                    }
                    None => self.forget(&path, None).await,
                }
            }
        }
        Ok(())
    }

    /// Drop the mirror entries a full listing lacks, except those placed
    /// after the pass began.
    async fn prune(&self, seen: &HashSet<String>) {
        let _g = self.core.apply.lock().await;
        let touched = locked(&self.core.state)
            .pass
            .as_ref()
            .map(|p| p.touched.clone())
            .unwrap_or_default();
        let mut mirror = locked(&self.core.mirror);
        let gone: Vec<NotePath> = mirror
            .paths()
            .into_iter()
            .filter(|p| !seen.contains(p.as_str()) && !touched.contains(p.as_str()))
            .collect();
        for path in &gone {
            mirror.remove(path);
        }
        drop(mirror);
        if !gone.is_empty() {
            tracing::info!(
                dropped = gone.len(),
                "local cache: dropped notes deleted elsewhere"
            );
            self.core.dirty.store(true, Ordering::Release);
        }
    }

    // ---- disk -----------------------------------------------------------

    /// Write the snapshot if this process holds the lock, the mirror is
    /// filled and something changed since the last write. Returns whether a
    /// file was written. A failed write keeps the change pending. Writes do
    /// not overlap: the blocking thread that writes holds `persist_lock` until
    /// it is done, even if the caller was cancelled meanwhile, so a later
    /// call (the one of [`LocalCache::close`]) always lands last.
    pub async fn persist(&self) -> Result<bool, CoreError> {
        let Some(disk) = self.core.disk.clone() else {
            return Ok(false);
        };
        if !disk.is_writer() || !self.serves_from_mirror() {
            return Ok(false);
        }
        let write = self.core.persist_lock.clone().lock_owned().await;
        if !self.core.dirty.swap(false, Ordering::AcqRel) {
            return Ok(false);
        }
        // until the write is handed to its thread, a failure or a cancel
        // puts the change back
        let mut pending = Pending {
            dirty: &self.core.dirty,
            armed: true,
        };
        let snapshot = self.snapshot().await;
        pending.armed = false;
        let core = self.core.clone();
        tokio::task::spawn_blocking(move || {
            let _write = write;
            let written = disk.write(&snapshot);
            if written.is_err() {
                core.dirty.store(true, Ordering::Release);
            }
            written
        })
        .await
        .map_err(storage)?
        .map_err(storage)?;
        Ok(true)
    }

    /// The mirror and `hw` as one value, taken while no update can
    /// interleave.
    async fn snapshot(&self) -> Snapshot {
        let _g = self.core.apply.lock().await;
        let hw = locked(&self.core.state).hw;
        let notes = locked(&self.core.mirror).all();
        Snapshot { hw, notes }
    }

    // ---- the task -------------------------------------------------------

    /// Start the background task with the documented timing. `connect` opens
    /// the remote and must be bounded in time (`RemoteNoteRepo::connect` is,
    /// by its connect timeout). It is called by the task, and by any call that
    /// finds no usable remote, one attempt at a time. Replaces a task started
    /// before.
    pub fn spawn<C, F>(&self, connect: C)
    where
        C: Fn() -> F + Send + Sync + 'static,
        F: Future<Output = Result<Connected, CoreError>> + Send + 'static,
    {
        self.spawn_with(Timing::default(), connect);
    }

    /// [`LocalCache::spawn`] with explicit [`Timing`].
    pub fn spawn_with<C, F>(&self, timing: Timing, connect: C)
    where
        C: Fn() -> F + Send + Sync + 'static,
        F: Future<Output = Result<Connected, CoreError>> + Send + 'static,
    {
        let connector: Connector = Arc::new(move || Box::pin(connect()));
        *locked(&self.core.connector) = Some(connector);
        let this = self.clone();
        let handle = tokio::spawn(async move { this.run(timing).await });
        if let Some(old) = locked(&self.core.task).replace(handle) {
            old.abort();
        }
    }

    /// The task: connect (if no call has), poll, persist, wait. Its own
    /// failures are retried with the back-off; a caller never waits for it.
    async fn run(self, timing: Timing) {
        let mut backoff = timing.backoff_min;
        loop {
            let failure = match self.remote().await {
                Err(e) => Some(e),
                Ok(_) => self.sync_once().await.err(),
            };
            if let Some(e) = failure {
                tracing::warn!(
                    error = %e.chain(),
                    retry_in_secs = backoff.as_secs(),
                    "local cache: the remote is unreachable (disconnected); serving the last synced state"
                );
                tokio::time::sleep(backoff).await;
                backoff = timing.next_backoff(backoff);
                continue;
            }
            backoff = timing.backoff_min;
            if let Err(e) = self.persist().await {
                tracing::warn!(error = %e.chain(), "local cache: writing the snapshot failed");
            }
            tokio::time::sleep(timing.poll).await;
        }
    }

    /// Stop the task, wait for it, and write a pending change. The explicit
    /// end of the cache: dropping it does neither. A snapshot write the task
    /// had in flight finishes first (see [`LocalCache::persist`]), so
    /// the one written here is the last.
    pub async fn close(&self) {
        let task = locked(&self.core.task).take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        if let Err(e) = self.persist().await {
            tracing::warn!(error = %e.chain(), "local cache: writing the snapshot failed");
        }
    }
}

#[async_trait]
impl NoteRepo for LocalCache {
    async fn get(&self, path: &NotePath) -> Result<Option<Note>, CoreError> {
        if self.serves_from_mirror() {
            return Ok(self.held(path));
        }
        let got = self.remote().await?.get(path).await?;
        if got.is_none() {
            self.forget(path, None).await;
        }
        Ok(got)
    }

    async fn get_view(&self, path: &NotePath, tail: &Tail) -> Result<Option<NoteView>, CoreError> {
        if self.serves_from_mirror() {
            return Ok(self.held(path).map(|n| NoteView::of(n, tail)));
        }
        let got = self.remote().await?.get_view(path, tail).await?;
        if got.is_none() {
            self.forget(path, None).await;
        }
        Ok(got)
    }

    async fn put(
        &self,
        path: &NotePath,
        body: &str,
        expected: Option<i64>,
        by: &str,
    ) -> Result<PutResult, CoreError> {
        match self.remote().await?.put(path, body, expected, by).await {
            Ok(done) => {
                let behind = !done.changed && self.mirror_version(path) < Some(done.note.version);
                if behind {
                    // nothing was written, and the answer's note has no
                    // `updated_by`: take the remote's own copy
                    self.refresh(path).await;
                } else {
                    self.place(done.note.clone(), Source::Write).await;
                }
                Ok(done)
            }
            Err(e) => Err(self.after_failure(path, e).await),
        }
    }

    async fn append(&self, path: &NotePath, text: &str, by: &str) -> Result<Note, CoreError> {
        match self.remote().await?.append(path, text, by).await {
            Ok(note) => {
                self.place(note.clone(), Source::Write).await;
                Ok(note)
            }
            Err(e) => Err(self.after_failure(path, e).await),
        }
    }

    async fn delete(&self, path: &NotePath, expected: i64, by: &str) -> Result<Note, CoreError> {
        match self.remote().await?.delete(path, expected, by).await {
            Ok(note) => {
                self.forget(path, Some(note.version)).await;
                Ok(note)
            }
            Err(e) => Err(self.after_failure(path, e).await),
        }
    }

    async fn list(
        &self,
        prefix: &str,
        filters: &[Filter],
        page: &Page,
    ) -> Result<PageOut<NoteMeta>, CoreError> {
        if self.serves_from_mirror() {
            return Ok(locked(&self.core.mirror).list(prefix, filters, page));
        }
        self.remote().await?.list(prefix, filters, page).await
    }

    async fn search(
        &self,
        text: &str,
        prefix: &str,
        page: &Page,
    ) -> Result<PageOut<SearchHit>, CoreError> {
        if self.serves_from_mirror() {
            return Ok(locked(&self.core.mirror).search(text, prefix, page));
        }
        self.remote().await?.search(text, prefix, page).await
    }

    async fn history(&self, path: &NotePath, limit: u32) -> Result<Vec<Revision>, CoreError> {
        self.remote().await?.history(path, limit).await
    }

    async fn history_meta(
        &self,
        path: &NotePath,
        limit: u32,
    ) -> Result<Vec<RevisionMeta>, CoreError> {
        self.remote().await?.history_meta(path, limit).await
    }

    async fn dump(&self, prefix: &str, page: &Page) -> Result<PageOut<DumpEntry>, CoreError> {
        if self.serves_from_mirror() {
            return Ok(locked(&self.core.mirror).dump(prefix, page));
        }
        self.remote().await?.dump(prefix, page).await
    }
}

impl LocalCache {
    /// The version the mirror holds for `path`.
    fn mirror_version(&self, path: &NotePath) -> Option<i64> {
        self.held(path).map(|n| n.version)
    }

    /// The note the mirror holds at `path`.
    fn held(&self, path: &NotePath) -> Option<Note> {
        locked(&self.core.mirror).get(path)
    }

    /// Read `path` from the remote and bring the mirror to it (newer
    /// versions only; a note that is gone is dropped), but only if the mirror
    /// still holds what it held before the read: a delete or write made here
    /// while the read was in flight wins, and the next poll repairs the rest.
    /// Best effort: a failure leaves the mirror as it is.
    async fn refresh(&self, path: &NotePath) {
        let held = self.mirror_version(path);
        let Ok(remote) = self.remote().await else {
            return;
        };
        match remote.get(path).await {
            Ok(Some(note)) => {
                self.place(note, Source::Refresh(held)).await;
            }
            Ok(None) => {
                let _g = self.core.apply.lock().await;
                if self.mirror_version(path) == held {
                    self.forget_locked(path, None).await;
                }
            }
            Err(_) => {}
        }
    }

    /// A failed write. `NotFound` drops the path; `VersionConflict` means the
    /// mirror was behind, so the path is refreshed (a client that reads and
    /// then writes again would otherwise conflict until the next poll). The
    /// error is returned unchanged.
    async fn after_failure(&self, path: &NotePath, e: CoreError) -> CoreError {
        match e {
            CoreError::NotFound(_) => self.forget(path, None).await,
            CoreError::VersionConflict { .. } => self.refresh(path).await,
            _ => {}
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use notelet_core::MemoryNoteRepo;
    use tokio::sync::Notify;

    use super::disk::testing::TempDir;
    use super::*;

    /// Lets a test stop a call of the fake remote half way: the call has its
    /// answer, announces it reached the gate, and waits to be released.
    #[derive(Default)]
    struct Gate {
        reached: Notify,
        release: Notify,
    }

    /// A remote backed by a memory store, that can be taken down, that counts
    /// and records what it is asked, and that can hold a call at a gate.
    #[derive(Default)]
    struct Spy {
        /// "Another client" writes here directly.
        inner: MemoryNoteRepo,
        down: AtomicBool,
        /// The next this many calls fail, then calls succeed again.
        fail_next: AtomicUsize,
        /// Answer a `put` without `updated_by`, as `RemoteNoteRepo` does.
        blank_actor: AtomicBool,
        gets: AtomicUsize,
        lists: Mutex<Vec<Vec<Filter>>>,
        list_gate: Mutex<Option<Arc<Gate>>>,
        get_gate: Mutex<Option<Arc<Gate>>>,
    }

    async fn gated<T>(slot: &Mutex<Option<Arc<Gate>>>, value: T) -> T {
        let gate = locked(slot).take();
        if let Some(gate) = gate {
            gate.reached.notify_one();
            gate.release.notified().await;
        }
        value
    }

    impl Spy {
        fn up(&self) -> Result<(), CoreError> {
            if self.down.load(Ordering::Relaxed) {
                return Err(storage(std::io::Error::other("remote down")));
            }
            let failing = self.fail_next.load(Ordering::Relaxed);
            if failing > 0 {
                self.fail_next.store(failing - 1, Ordering::Relaxed);
                return Err(storage(std::io::Error::other("one call lost")));
            }
            Ok(())
        }

        fn gets(&self) -> usize {
            self.gets.load(Ordering::Relaxed)
        }

        fn gate_list(&self) -> Arc<Gate> {
            let gate = Arc::new(Gate::default());
            *locked(&self.list_gate) = Some(gate.clone());
            gate
        }

        fn gate_get(&self) -> Arc<Gate> {
            let gate = Arc::new(Gate::default());
            *locked(&self.get_gate) = Some(gate.clone());
            gate
        }

        /// What a listing of everything says now.
        async fn metas(&self) -> Vec<NoteMeta> {
            let page = Page::new(None, None).unwrap();
            self.inner.list("", &[], &page).await.unwrap().items
        }
    }

    #[async_trait]
    impl NoteRepo for Spy {
        async fn get(&self, path: &NotePath) -> Result<Option<Note>, CoreError> {
            self.up()?;
            self.gets.fetch_add(1, Ordering::Relaxed);
            let got = self.inner.get(path).await?;
            Ok(gated(&self.get_gate, got).await)
        }
        async fn get_view(
            &self,
            path: &NotePath,
            tail: &Tail,
        ) -> Result<Option<NoteView>, CoreError> {
            self.up()?;
            self.gets.fetch_add(1, Ordering::Relaxed);
            self.inner.get_view(path, tail).await
        }
        async fn put(
            &self,
            path: &NotePath,
            body: &str,
            expected: Option<i64>,
            by: &str,
        ) -> Result<PutResult, CoreError> {
            self.up()?;
            let mut done = self.inner.put(path, body, expected, by).await?;
            if self.blank_actor.load(Ordering::Relaxed) {
                done.note.updated_by.clear();
            }
            Ok(done)
        }
        async fn append(&self, path: &NotePath, text: &str, by: &str) -> Result<Note, CoreError> {
            self.up()?;
            self.inner.append(path, text, by).await
        }
        async fn delete(
            &self,
            path: &NotePath,
            expected: i64,
            by: &str,
        ) -> Result<Note, CoreError> {
            self.up()?;
            self.inner.delete(path, expected, by).await
        }
        async fn list(
            &self,
            prefix: &str,
            filters: &[Filter],
            page: &Page,
        ) -> Result<PageOut<NoteMeta>, CoreError> {
            self.up()?;
            locked(&self.lists).push(filters.to_vec());
            let out = self.inner.list(prefix, filters, page).await?;
            Ok(gated(&self.list_gate, out).await)
        }
        async fn search(
            &self,
            text: &str,
            prefix: &str,
            page: &Page,
        ) -> Result<PageOut<SearchHit>, CoreError> {
            self.up()?;
            self.inner.search(text, prefix, page).await
        }
        async fn history(&self, path: &NotePath, limit: u32) -> Result<Vec<Revision>, CoreError> {
            self.up()?;
            self.inner.history(path, limit).await
        }
        async fn history_meta(
            &self,
            path: &NotePath,
            limit: u32,
        ) -> Result<Vec<RevisionMeta>, CoreError> {
            self.up()?;
            self.inner.history_meta(path, limit).await
        }
        async fn dump(&self, prefix: &str, page: &Page) -> Result<PageOut<DumpEntry>, CoreError> {
            self.up()?;
            self.inner.dump(prefix, page).await
        }
    }

    fn p(path: &str) -> NotePath {
        NotePath::parse(path).unwrap()
    }

    fn page() -> Page {
        Page::new(None, None).unwrap()
    }

    /// A cache with a spy remote connected, mirror empty.
    async fn fixture() -> (Arc<Spy>, LocalCache) {
        let spy = Arc::new(Spy::default());
        let cache = LocalCache::new(None).await;
        cache.set_remote(spy.clone());
        (spy, cache)
    }

    /// "Another client" writes `path`.
    async fn other(spy: &Spy, path: &str, body: &str) -> Note {
        let path = p(path);
        let cur = spy.inner.get(&path).await.unwrap();
        spy.inner
            .put(&path, body, cur.map(|n| n.version), "other")
            .await
            .unwrap()
            .note
    }

    fn is_storage(e: &CoreError) -> bool {
        matches!(e, CoreError::Storage(_))
    }

    #[tokio::test]
    async fn reads_pass_through_until_filled_then_come_from_the_mirror() {
        let (spy, cache) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        // empty mirror: the read goes to the remote
        let got = cache.get(&p("a/1.md")).await.unwrap().unwrap();
        assert_eq!((got.body.as_str(), spy.gets()), ("one", 1));
        assert!(!cache.is_filled());

        cache.sync_once().await.unwrap();
        assert!(cache.is_filled());
        assert_eq!(spy.gets(), 2, "the fill fetched the one body");

        // the remote moves on; the filled mirror does not ask
        other(&spy, "a/1.md", "two").await;
        let got = cache.get(&p("a/1.md")).await.unwrap().unwrap();
        assert_eq!((got.body.as_str(), got.version), ("one", 1));
        let view = cache
            .get_view(&p("a/1.md"), &Tail::whole())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.body, "one");
        assert_eq!(cache.list("", &[], &page()).await.unwrap().items.len(), 1);
        assert_eq!(
            cache.search("one", "", &page()).await.unwrap().items.len(),
            1
        );
        assert_eq!(cache.dump("", &page()).await.unwrap().items[0].body, "one");
        assert_eq!(spy.gets(), 2);

        // the next poll brings the change
        cache.sync_once().await.unwrap();
        let got = cache.get(&p("a/1.md")).await.unwrap().unwrap();
        assert_eq!((got.body.as_str(), got.version), ("two", 2));
    }

    #[tokio::test]
    async fn a_delta_poll_lists_from_hw_minus_the_overlap_and_fetches_only_newer_versions() {
        let (spy, cache) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        other(&spy, "a/2.md", "two").await;
        let hw = spy
            .metas()
            .await
            .iter()
            .map(|m| m.updated_at)
            .max()
            .unwrap();
        cache.sync_once().await.unwrap();
        assert_eq!(spy.gets(), 2);

        other(&spy, "a/3.md", "three").await;
        cache.sync_once().await.unwrap();
        // 1 and 2 are inside the window but not newer: only 3 was fetched
        assert_eq!(spy.gets(), 3);
        let lists = locked(&spy.lists).clone();
        assert!(lists[0].is_empty(), "the first pass lists everything");
        assert_eq!(
            lists[1],
            vec![Filter {
                field: Field::UpdatedAt,
                op: Op::Gte,
                value: FilterValue::Time(hw - chrono::Duration::seconds(60)),
            }],
            "hw is the remote's, minus 60 s"
        );
        assert!(cache.get(&p("a/3.md")).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn every_tenth_poll_lists_all_paths_and_drops_what_is_gone() {
        let (spy, cache) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        let gone = other(&spy, "a/2.md", "two").await;
        cache.sync_once().await.unwrap();
        spy.inner
            .delete(&gone.path, gone.version, "other")
            .await
            .unwrap();
        // polls 1..=9 are deltas: they cannot see a delete
        for _ in 1..10 {
            cache.sync_once().await.unwrap();
            assert!(cache.get(&gone.path).await.unwrap().is_some());
        }
        // the 10th is a full listing
        cache.sync_once().await.unwrap();
        assert!(cache.get(&gone.path).await.unwrap().is_none());
        assert!(cache.get(&p("a/1.md")).await.unwrap().is_some());
        let lists = locked(&spy.lists).clone();
        assert_eq!(lists.len(), 11);
        assert!(lists[10].is_empty() && lists[1..10].iter().all(|f| f.len() == 1));
    }

    #[tokio::test]
    async fn a_relist_keeps_a_note_put_after_it_began() {
        let (spy, cache) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        let gone = other(&spy, "a/2.md", "two").await;
        cache.sync_once().await.unwrap();
        spy.inner
            .delete(&gone.path, gone.version, "other")
            .await
            .unwrap();

        let gate = spy.gate_list();
        let relist = tokio::spawn({
            let cache = cache.clone();
            async move { cache.relist().await }
        });
        // the listing is taken; now this process writes a new note
        gate.reached.notified().await;
        cache
            .put(&p("a/new.md"), "fresh", None, "me")
            .await
            .unwrap();
        gate.release.notify_one();
        relist.await.unwrap().unwrap();

        assert!(cache.get(&p("a/new.md")).await.unwrap().is_some(), "kept");
        assert!(cache.get(&p("a/1.md")).await.unwrap().is_some());
        assert!(cache.get(&gone.path).await.unwrap().is_none(), "dropped");
    }

    #[tokio::test]
    async fn a_fetch_in_flight_cannot_bring_back_a_note_this_process_deleted() {
        let (spy, cache) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        cache.sync_once().await.unwrap();
        other(&spy, "a/1.md", "two").await;

        let gate = spy.gate_get();
        let poll = tokio::spawn({
            let cache = cache.clone();
            async move { cache.sync_once().await }
        });
        gate.reached.notified().await;
        // the fetch holds version 2; this process deletes version 2
        cache.delete(&p("a/1.md"), 2, "me").await.unwrap();
        gate.release.notify_one();
        poll.await.unwrap().unwrap();
        assert!(cache.get(&p("a/1.md")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn writes_go_to_the_remote_and_then_to_the_mirror() {
        let (spy, cache) = fixture().await;
        cache.sync_once().await.unwrap();
        assert!(cache.is_filled());

        let done = cache.put(&p("w/n.md"), "hello", None, "me").await.unwrap();
        assert!(done.changed);
        // read own write at once, with the remote's version
        let got = cache.get(&p("w/n.md")).await.unwrap().unwrap();
        assert_eq!((got.body.as_str(), got.version), ("hello", 1));
        assert_eq!(got, spy.inner.get(&p("w/n.md")).await.unwrap().unwrap());

        let appended = cache.append(&p("w/n.md"), "more", "me").await.unwrap();
        assert_eq!(appended.version, 2);
        let got = cache.get(&p("w/n.md")).await.unwrap().unwrap();
        assert_eq!(got.body, "hello\nmore");

        // history is not cached
        assert_eq!(cache.history(&p("w/n.md"), 10).await.unwrap().len(), 0);
        cache
            .put(&p("w/n.md"), "again", Some(2), "me")
            .await
            .unwrap();
        assert_eq!(cache.history(&p("w/n.md"), 10).await.unwrap().len(), 1);
        assert_eq!(cache.history_meta(&p("w/n.md"), 10).await.unwrap().len(), 1);

        let removed = cache.delete(&p("w/n.md"), 3, "me").await.unwrap();
        assert_eq!(removed.body, "again");
        assert!(cache.get(&p("w/n.md")).await.unwrap().is_none());
        assert_eq!(spy.inner.get(&p("w/n.md")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_refused_write_leaves_the_mirror_and_a_not_found_drops_the_path() {
        let (spy, cache) = fixture().await;
        let n = other(&spy, "w/n.md", "one").await;
        cache.sync_once().await.unwrap();

        // a conflict changes nothing
        let err = cache.put(&n.path, "x", Some(9), "me").await.unwrap_err();
        assert!(matches!(err, CoreError::VersionConflict { .. }), "{err}");
        assert_eq!(cache.get(&n.path).await.unwrap().unwrap().body, "one");

        // the note is deleted elsewhere: the write meets NotFound
        spy.inner.delete(&n.path, 1, "other").await.unwrap();
        let err = cache.put(&n.path, "x", Some(1), "me").await.unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)), "{err}");
        assert!(
            cache.get(&n.path).await.unwrap().is_none(),
            "dropped at once"
        );

        // the same for a delete
        let m = other(&spy, "w/m.md", "one").await;
        cache.sync_once().await.unwrap();
        spy.inner.delete(&m.path, 1, "other").await.unwrap();
        let err = cache.delete(&m.path, 1, "me").await.unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)), "{err}");
        assert!(cache.get(&m.path).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_pass_through_read_that_finds_nothing_drops_the_path() {
        let (spy, cache) = fixture().await;
        let n = other(&spy, "w/n.md", "one").await;
        // a partial mirror (a failed first fill) is not served, but is kept tidy
        cache.place(n.clone(), Source::Poll).await;
        assert!(!cache.is_filled());
        spy.inner.delete(&n.path, 1, "other").await.unwrap();
        assert!(cache.get(&n.path).await.unwrap().is_none());
        assert!(cache.held(&n.path).is_none());
    }

    #[tokio::test]
    async fn a_put_gets_its_actor_from_the_next_listing_without_a_fetch() {
        let (spy, cache) = fixture().await;
        cache.sync_once().await.unwrap();
        spy.blank_actor.store(true, Ordering::Relaxed);
        cache.put(&p("w/n.md"), "hello", None, "me").await.unwrap();
        let got = cache.get(&p("w/n.md")).await.unwrap().unwrap();
        assert_eq!(got.updated_by, "", "the remote did not say");

        let gets = spy.gets();
        cache.sync_once().await.unwrap();
        let got = cache.get(&p("w/n.md")).await.unwrap().unwrap();
        assert_eq!((got.updated_by.as_str(), got.version), ("me", 1));
        assert_eq!(spy.gets(), gets, "patched from the listing, no get");
    }

    #[tokio::test]
    async fn without_a_remote_a_filled_mirror_reads_and_everything_else_fails() {
        let (spy, cache) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        cache.sync_once().await.unwrap();

        cache.clear_remote();
        assert!(!cache.is_connected());
        assert_eq!(cache.get(&p("a/1.md")).await.unwrap().unwrap().body, "one");
        assert_eq!(cache.list("", &[], &page()).await.unwrap().items.len(), 1);
        for err in [
            cache
                .put(&p("a/1.md"), "x", Some(1), "me")
                .await
                .unwrap_err(),
            cache.append(&p("a/1.md"), "x", "me").await.unwrap_err(),
            cache.delete(&p("a/1.md"), 1, "me").await.unwrap_err(),
            cache.history(&p("a/1.md"), 5).await.unwrap_err(),
            cache.sync_once().await.unwrap_err(),
        ] {
            assert!(is_storage(&err), "{err}");
            assert!(err.chain().to_string().contains("not connected"), "{err}");
        }

        // an unfilled cache has nothing to answer from either
        let empty = LocalCache::new(None).await;
        let err = empty.get(&p("a/1.md")).await.unwrap_err();
        assert!(is_storage(&err), "{err}");
        assert!(empty.list("", &[], &page()).await.is_err());
    }

    #[tokio::test]
    async fn a_remote_that_goes_down_fails_writes_and_polls_but_not_cached_reads() {
        let (spy, cache) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        cache.sync_once().await.unwrap();
        spy.down.store(true, Ordering::Relaxed);
        assert!(cache.get(&p("a/1.md")).await.unwrap().is_some());
        assert!(is_storage(&cache.sync_once().await.unwrap_err()));
        assert!(is_storage(
            &cache.put(&p("a/2.md"), "x", None, "me").await.unwrap_err()
        ));
        assert!(cache.get(&p("a/2.md")).await.unwrap().is_none());
        // back up: the failed poll did not move hw or the poll count
        spy.down.store(false, Ordering::Relaxed);
        other(&spy, "a/1.md", "two").await;
        cache.sync_once().await.unwrap();
        assert_eq!(cache.get(&p("a/1.md")).await.unwrap().unwrap().body, "two");
    }

    #[tokio::test]
    async fn the_snapshot_is_written_by_the_lock_holder_and_loaded_by_everyone() {
        let tmp = TempDir::new("snap");
        let (spy, _) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        other(&spy, "a/2.md", "two").await;

        let writer = LocalCache::new(Some(DiskCache::open(tmp.0.clone()).unwrap())).await;
        assert!(!writer.is_filled(), "no snapshot yet");
        writer.set_remote(spy.clone());
        // an unfilled mirror is never written
        writer.put(&p("a/3.md"), "three", None, "me").await.unwrap();
        assert!(!writer.persist().await.unwrap());
        assert!(!tmp.0.join("notes.json").exists());

        writer.sync_once().await.unwrap();
        assert!(writer.persist().await.unwrap());
        assert!(!writer.persist().await.unwrap(), "only after a change");
        writer.put(&p("a/4.md"), "four", None, "me").await.unwrap();
        assert!(writer.persist().await.unwrap());

        // a second process: reader, but it starts from the snapshot
        let reader = LocalCache::new(Some(DiskCache::open(tmp.0.clone()).unwrap())).await;
        assert!(reader.is_filled());
        assert!(!reader.is_connected());
        let notes = reader.list("", &[], &page()).await.unwrap().items;
        assert_eq!(notes.len(), 4);
        let got = reader.get(&p("a/4.md")).await.unwrap().unwrap();
        assert_eq!((got.body.as_str(), got.version), ("four", 1));
        reader.set_remote(spy.clone());
        reader.sync_once().await.unwrap();
        reader.put(&p("a/5.md"), "five", None, "me").await.unwrap();
        assert!(!reader.persist().await.unwrap(), "a reader never writes");

        // the writer's file is intact and has the hw, the reader's change is not in it
        drop((writer, reader));
        let next = LocalCache::new(Some(DiskCache::open(tmp.0.clone()).unwrap())).await;
        assert_eq!(next.list("", &[], &page()).await.unwrap().items.len(), 4);
        assert!(locked(&next.core.state).hw.is_some());
    }

    #[tokio::test]
    async fn a_snapshot_start_still_relists_and_catches_up() {
        let tmp = TempDir::new("catchup");
        let (spy, _) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        let gone = other(&spy, "a/2.md", "two").await;
        {
            let first = LocalCache::new(Some(DiskCache::open(tmp.0.clone()).unwrap())).await;
            first.set_remote(spy.clone());
            first.sync_once().await.unwrap();
            first.close().await;
        }
        spy.inner.delete(&gone.path, 1, "other").await.unwrap();
        other(&spy, "a/1.md", "newer").await;

        let again = LocalCache::new(Some(DiskCache::open(tmp.0.clone()).unwrap())).await;
        assert!(again.is_filled());
        assert_eq!(again.get(&gone.path).await.unwrap().unwrap().body, "two");
        again.set_remote(spy.clone());
        again.sync_once().await.unwrap();
        assert!(again.get(&gone.path).await.unwrap().is_none());
        assert_eq!(
            again.get(&p("a/1.md")).await.unwrap().unwrap().body,
            "newer"
        );
    }

    /// Poll `cond` until it holds, for at most 10 s.
    async fn eventually<F, Fut>(what: &str, mut cond: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        for _ in 0..1000 {
            if cond().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn the_task_connects_with_back_off_fills_polls_and_persists() {
        let tmp = TempDir::new("task");
        let (spy, _) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        let cache = LocalCache::new(Some(DiskCache::open(tmp.0.clone()).unwrap())).await;

        let attempts = Arc::new(AtomicUsize::new(0));
        let timing = Timing {
            poll: Duration::from_millis(10),
            backoff_min: Duration::from_millis(5),
            backoff_max: Duration::from_millis(20),
        };
        cache.spawn_with(timing, {
            let (spy, attempts) = (spy.clone(), attempts.clone());
            move || {
                let (spy, attempts) = (spy.clone(), attempts.clone());
                async move {
                    if attempts.fetch_add(1, Ordering::Relaxed) < 2 {
                        return Err(storage(std::io::Error::other("refused")));
                    }
                    Ok(Connected::new(spy))
                }
            }
        });

        let c = cache.clone();
        eventually("the first fill", || {
            let c = c.clone();
            async move { c.is_filled() }
        })
        .await;
        assert!(attempts.load(Ordering::Relaxed) >= 3, "two refusals first");

        // another client's change arrives by polling
        other(&spy, "a/2.md", "two").await;
        let c = cache.clone();
        eventually("the poll", || {
            let c = c.clone();
            async move { c.get(&p("a/2.md")).await.unwrap().is_some() }
        })
        .await;

        // the remote is down for a while: polls fail and are retried with the
        // back-off, the connection is kept, and the change arrives afterwards
        spy.down.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(cache.is_connected(), "failed polls do not drop the client");
        spy.down.store(false, Ordering::Relaxed);
        other(&spy, "a/3.md", "three").await;
        let c = cache.clone();
        eventually("the recovery", || {
            let c = c.clone();
            async move { c.get(&p("a/3.md")).await.unwrap().is_some() }
        })
        .await;

        cache.close().await;
        assert!(tmp.0.join("notes.json").exists());
    }

    /// Timing that never gets to poll or back off within a test: only what a
    /// caller does itself can make progress.
    fn slow_task() -> Timing {
        Timing {
            poll: Duration::from_secs(3600),
            backoff_min: Duration::from_secs(3600),
            backoff_max: Duration::from_secs(3600),
        }
    }

    #[tokio::test]
    async fn a_cold_start_call_waits_for_the_connect_instead_of_failing() {
        let spy = Arc::new(Spy::default());
        other(&spy, "a/1.md", "one").await;
        let cache = LocalCache::new(None).await;
        let attempts = Arc::new(AtomicUsize::new(0));
        cache.spawn_with(slow_task(), {
            let (spy, attempts) = (spy.clone(), attempts.clone());
            move || {
                let (spy, attempts) = (spy.clone(), attempts.clone());
                async move {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    // the remote becomes ready only after a delay
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok(Connected::new(spy))
                }
            }
        });

        // no snapshot, no mirror: these go to the remote, which is still being
        // connected; all of them share the one attempt (the task's, or theirs)
        let (one, page) = (p("a/1.md"), page());
        let (a, b, c) = tokio::join!(
            cache.get(&one),
            cache.list("", &[], &page),
            cache.history(&one, 5),
        );
        assert_eq!(a.unwrap().unwrap().body, "one");
        assert_eq!(b.unwrap().items.len(), 1);
        assert!(c.unwrap().is_empty());
        assert_eq!(attempts.load(Ordering::Relaxed), 1, "single flight");
        cache.close().await;
    }

    #[tokio::test]
    async fn callers_that_waited_for_a_failed_attempt_share_its_failure() {
        let cache = LocalCache::new(None).await;
        let attempts = Arc::new(AtomicUsize::new(0));
        cache.spawn_with(slow_task(), {
            let attempts = attempts.clone();
            move || {
                let attempts = attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Err(storage(std::io::Error::other("refused")))
                }
            }
        });
        let (one, two) = (p("a/1.md"), p("a/2.md"));
        let (a, b) = tokio::join!(cache.get(&one), cache.get(&two));
        for err in [a.unwrap_err(), b.unwrap_err()] {
            assert!(is_storage(&err), "{err}");
            assert!(err.chain().to_string().contains("refused"), "{err}");
        }
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        cache.close().await;
    }

    #[tokio::test]
    async fn one_lost_call_fails_that_call_only_and_the_next_one_goes_through_at_once() {
        let (spy, cache) = fixture().await;
        cache.sync_once().await.unwrap();
        // no task runs here: only the calls below can meet the injected failure
        spy.fail_next.store(1, Ordering::Relaxed);
        let err = cache
            .put(&p("w/n.md"), "one", None, "me")
            .await
            .unwrap_err();
        assert!(is_storage(&err), "{err}");
        assert!(cache.is_connected(), "the client is kept");
        let started = std::time::Instant::now();
        cache.put(&p("w/n.md"), "one", None, "me").await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));

        // a failed poll keeps the client too
        spy.fail_next.store(1, Ordering::Relaxed);
        assert!(cache.sync_once().await.is_err());
        assert!(cache.is_connected());
        cache.sync_once().await.unwrap();
    }

    #[tokio::test]
    async fn a_client_shown_to_be_dead_is_replaced_on_demand() {
        let spy = Arc::new(Spy::default());
        other(&spy, "a/1.md", "one").await;
        let cache = LocalCache::new(None).await;
        let dead = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        cache.spawn_with(slow_task(), {
            let (spy, dead, attempts) = (spy.clone(), dead.clone(), attempts.clone());
            move || {
                let (spy, dead, attempts) = (spy.clone(), dead.clone(), attempts.clone());
                async move {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    // the new connection is alive again
                    dead.store(false, Ordering::Relaxed);
                    let probe = dead.clone();
                    Ok(Connected::new(spy).closed_when(move || probe.load(Ordering::Relaxed)))
                }
            }
        });
        assert!(cache.get(&p("a/1.md")).await.unwrap().is_some());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert!(cache.get(&p("a/1.md")).await.unwrap().is_some());
        assert_eq!(attempts.load(Ordering::Relaxed), 1, "alive: kept");

        dead.store(true, Ordering::Relaxed);
        assert!(cache.get(&p("a/1.md")).await.unwrap().is_some());
        assert_eq!(attempts.load(Ordering::Relaxed), 2, "dead: connected anew");
        cache.close().await;
    }

    #[tokio::test]
    async fn a_conflict_refreshes_the_stale_entry_so_the_next_write_can_succeed() {
        let (spy, cache) = fixture().await;
        let n = other(&spy, "w/n.md", "one").await;
        cache.sync_once().await.unwrap();
        // another client moves it on; this mirror is behind until the next poll
        other(&spy, "w/n.md", "two").await;
        assert_eq!(cache.get(&n.path).await.unwrap().unwrap().version, 1);

        let err = cache.put(&n.path, "mine", Some(1), "me").await.unwrap_err();
        assert!(matches!(err, CoreError::VersionConflict { .. }), "{err}");
        let got = cache.get(&n.path).await.unwrap().unwrap();
        assert_eq!((got.body.as_str(), got.version), ("two", 2));
        cache.put(&n.path, "mine", Some(2), "me").await.unwrap();

        // the same for a delete, and for a create that finds the note there
        other(&spy, "w/n.md", "three").await;
        let err = cache.delete(&n.path, 3, "me").await.unwrap_err();
        assert!(matches!(err, CoreError::VersionConflict { .. }), "{err}");
        assert_eq!(cache.get(&n.path).await.unwrap().unwrap().body, "three");
        let err = cache.put(&n.path, "x", None, "me").await.unwrap_err();
        assert!(matches!(err, CoreError::VersionConflict { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_refresh_in_flight_cannot_bring_back_a_note_deleted_meanwhile() {
        let (spy, cache) = fixture().await;
        let n = other(&spy, "w/n.md", "one").await;
        cache.sync_once().await.unwrap();
        other(&spy, "w/n.md", "two").await;

        // this write conflicts (the mirror is behind) and starts a refresh,
        // whose read of the remote is held at the gate
        let gate = spy.gate_get();
        let writer = tokio::spawn({
            let (cache, path) = (cache.clone(), n.path.clone());
            async move { cache.put(&path, "mine", Some(1), "me").await }
        });
        gate.reached.notified().await;
        // meanwhile another session deletes the note (no pass is running)
        cache.delete(&n.path, 2, "me").await.unwrap();
        gate.release.notify_one();
        let err = writer.await.unwrap().unwrap_err();
        assert!(matches!(err, CoreError::VersionConflict { .. }), "{err}");
        assert!(cache.get(&n.path).await.unwrap().is_none(), "stays gone");
    }

    #[tokio::test]
    async fn an_unchanged_put_behind_the_mirror_takes_the_remote_copy() {
        let (spy, cache) = fixture().await;
        let n = other(&spy, "w/n.md", "one").await;
        cache.sync_once().await.unwrap();
        other(&spy, "w/n.md", "same").await;
        // the remote answers without `updated_by`, as RemoteNoteRepo does
        spy.blank_actor.store(true, Ordering::Relaxed);
        let done = cache.put(&n.path, "same", Some(2), "me").await.unwrap();
        assert!(!done.changed);
        let got = cache.get(&n.path).await.unwrap().unwrap();
        assert_eq!((got.version, got.updated_by.as_str()), (2, "other"));
    }

    /// How long a read that must not wait for the fill may take in a test.
    const PROMPT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn a_read_during_a_slow_fill_goes_to_the_remote_without_waiting_for_the_fill() {
        let (spy, cache) = fixture().await;
        for i in 0..3 {
            other(&spy, &format!("a/{i}.md"), "body").await;
        }
        // the fill is held inside the fetch of its first body
        let gate = spy.gate_get();
        let fill = tokio::spawn({
            let cache = cache.clone();
            async move { cache.sync_once().await }
        });
        gate.reached.notified().await;
        assert!(!cache.is_filled());

        let gets = spy.gets();
        let read = tokio::time::timeout(PROMPT, async {
            let got = cache.get(&p("a/2.md")).await.unwrap().unwrap();
            let view = cache.get_view(&p("a/1.md"), &Tail::whole()).await;
            let listed = cache.list("", &[], &page()).await.unwrap();
            (got, view.unwrap().unwrap(), listed)
        })
        .await
        .expect("the read waited for the fill");
        assert_eq!(read.0.body, "body");
        assert_eq!(read.2.items.len(), 3);
        assert!(spy.gets() >= gets + 2, "both reads went to the remote");
        assert!(!cache.is_filled(), "the fill is still held");

        gate.release.notify_one();
        fill.await.unwrap().unwrap();
        assert!(cache.is_filled());
    }

    #[tokio::test]
    async fn a_read_while_the_fill_waits_for_a_listing_does_not_wait_for_it_either() {
        let (spy, cache) = fixture().await;
        other(&spy, "a/1.md", "one").await;
        let gate = spy.gate_list();
        let fill = tokio::spawn({
            let cache = cache.clone();
            async move { cache.sync_once().await }
        });
        gate.reached.notified().await;

        let got = tokio::time::timeout(PROMPT, cache.get(&p("a/1.md")))
            .await
            .expect("the read waited for the fill");
        assert_eq!(got.unwrap().unwrap().body, "one");
        assert!(!cache.is_filled());

        gate.release.notify_one();
        fill.await.unwrap().unwrap();
        assert!(cache.is_filled());
    }

    #[tokio::test]
    async fn a_cold_start_read_waits_for_the_connect_and_not_for_the_fill_that_follows() {
        let spy = Arc::new(Spy::default());
        for i in 0..3 {
            other(&spy, &format!("a/{i}.md"), "body").await;
        }
        let gate = spy.gate_get();
        let cache = LocalCache::new(None).await;
        // the task connects (after a delay) and then starts its fill, which
        // the gate holds in its first body fetch
        cache.spawn_with(slow_task(), {
            let spy = spy.clone();
            move || {
                let spy = spy.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(Connected::new(spy))
                }
            }
        });

        let got = tokio::time::timeout(PROMPT, cache.get_view(&p("a/1.md"), &Tail::whole()))
            .await
            .expect("the read waited for more than the connect");
        assert_eq!(got.unwrap().unwrap().body, "body");
        assert!(!cache.is_filled(), "the read did not wait for the fill");
        // the fill does run: it reaches its gate, and finishes when released
        tokio::time::timeout(PROMPT, gate.reached.notified())
            .await
            .expect("the fill never started");
        gate.release.notify_one();
        let c = cache.clone();
        eventually("the fill", || {
            let c = c.clone();
            async move { c.is_filled() }
        })
        .await;
        cache.close().await;
    }

    #[test]
    fn the_back_off_doubles_from_30_s_to_5_min() {
        let t = Timing::default();
        let mut d = t.backoff_min;
        let mut seen = vec![d.as_secs()];
        for _ in 0..5 {
            d = t.next_backoff(d);
            seen.push(d.as_secs());
        }
        assert_eq!(seen, [30, 60, 120, 240, 300, 300]);
        assert_eq!(t.poll, Duration::from_secs(30));
    }
}
