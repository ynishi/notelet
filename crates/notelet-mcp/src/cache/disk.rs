//! The snapshot half of the remote cache: one directory, one lock, one file.
//!
//! ```text
//! <dir>/lock        exclusive `File::try_lock`; whoever holds it may write
//! <dir>/notes.json  { "hw": <updated_at or null>, "notes": [ <Note>, ... ] }
//! ```
//!
//! The directory is created 0700 and its files 0600 (on unix; elsewhere the
//! platform default), because the snapshot holds note bodies. Modes are set
//! explicitly, not only at creation, so a directory or file that already
//! existed with wider ones (an older run, another tool) is tightened. The caller
//! resolves the directory ([`crate::config::cache_dir`]); this module does
//! no environment lookup.
//!
//! [`DiskCache::open`] takes the lock without waiting. The process that gets
//! it is the **writer**: it alone replaces `notes.json`, through a temporary
//! file in the same directory and a rename ([`crate::files::write_atomic`],
//! the pattern `files.rs` uses), so a reader never sees half a file. A
//! process that does not get it is a reader: it may [`DiskCache::read`] the
//! snapshot, once, and then keeps its mirror in memory only. The lock lives as
//! long as the [`DiskCache`] (the OS drops it with the file, so a crashed
//! writer never leaves one behind) and is the only coordination between
//! processes; nothing waits on it.
//!
//! All calls block. The cache runs them on `spawn_blocking`.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;

use notelet_core::Note;
use serde::{Deserialize, Serialize};

/// What `notes.json` holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// The highest `updated_at` a listing of the remote had returned when the
    /// snapshot was taken (`None` before the first listing). Polling resumes
    /// from it.
    pub hw: Option<chrono::DateTime<chrono::Utc>>,
    pub notes: Vec<Note>,
}

/// An open cache directory. Dropping it releases the lock.
#[derive(Debug)]
pub struct DiskCache {
    dir: PathBuf,
    writer: bool,
    /// Held for its lock.
    _lock: File,
}

const SNAPSHOT: &str = "notes.json";

impl DiskCache {
    /// Create `dir` (0700) and try to take the lock without waiting. An
    /// `Err` is a directory that cannot be made or a lock the platform
    /// refuses to give; a lock held by another process is not an error, it
    /// makes this a reader (see [`DiskCache::is_writer`]).
    pub fn open(dir: PathBuf) -> io::Result<Self> {
        create_private_dir(&dir)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(dir.join("lock"))?;
        // `mode` above applies only when the file is created
        private_file(&dir.join("lock"))?;
        let writer = match lock.try_lock() {
            Ok(()) => true,
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        };
        Ok(Self {
            dir,
            writer,
            _lock: lock,
        })
    }

    /// Whether this process holds the lock and so may [`write`](Self::write).
    pub fn is_writer(&self) -> bool {
        self.writer
    }

    /// The snapshot, or `None` when there is none yet. An unreadable or
    /// malformed file is an `Err`; the caller starts empty.
    pub fn read(&self) -> io::Result<Option<Snapshot>> {
        match std::fs::read(self.dir.join(SNAPSHOT)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Replace the snapshot. Only the lock holder may; for a reader this is
    /// an error and nothing is written.
    pub fn write(&self, snapshot: &Snapshot) -> io::Result<()> {
        if !self.writer {
            return Err(io::Error::other(
                "this process does not hold the cache lock",
            ));
        }
        let bytes = serde_json::to_vec(snapshot).map_err(io::Error::other)?;
        let target = self.dir.join(SNAPSHOT);
        // `write_atomic` gives the new file the mode of the one it replaces,
        // so a file that was ever wider is tightened before (so the copy is
        // never wide) and after
        match private_file(&target) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        crate::files::write_atomic(&target, &bytes)?;
        private_file(&target)
    }
}

/// Mode 0600 on unix, whatever the file had; elsewhere nothing.
fn private_file(path: &std::path::Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// `dir` and any missing parents, 0700 on unix; an existing `dir` is brought
/// to 0700 too, so a directory made by something else cannot leave bodies
/// readable.
fn create_private_dir(dir: &std::path::Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A scratch directory for the tests of this module and of the cache.
#[cfg(test)]
pub(super) mod testing {
    use std::path::PathBuf;

    /// A fresh path under the system temp dir (not yet created), removed
    /// with everything under it on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "notelet-cache-test-{}-{tag}-{n}",
                std::process::id()
            )))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TempDir;
    use super::*;
    use notelet_core::NotePath;

    fn snapshot() -> Snapshot {
        Snapshot {
            hw: Some(chrono::Utc::now()),
            notes: vec![Note {
                path: NotePath::parse("n/a.md").unwrap(),
                body: "body".into(),
                version: 3,
                updated_at: chrono::Utc::now(),
                updated_by: "t".into(),
            }],
        }
    }

    #[test]
    fn the_holder_writes_and_a_reader_only_reads() {
        let tmp = TempDir::new("lock");
        let dir = tmp.0.join("a").join("b");
        let first = DiskCache::open(dir.clone()).unwrap();
        assert!(first.is_writer());
        assert_eq!(first.read().unwrap().map(|s| s.notes.len()), None);
        let snap = snapshot();
        first.write(&snap).unwrap();

        let second = DiskCache::open(dir.clone()).unwrap();
        assert!(!second.is_writer(), "the lock is exclusive");
        let read = second.read().unwrap().unwrap();
        assert_eq!(read.hw, snap.hw);
        assert_eq!(read.notes, snap.notes);
        assert!(second.write(&snap).is_err());

        // releasing the lock lets the next open take it
        drop(first);
        drop(second);
        assert!(DiskCache::open(dir).unwrap().is_writer());
    }

    #[cfg(unix)]
    #[test]
    fn directory_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("mode");
        // a directory that already exists with loose permissions is tightened
        std::fs::create_dir_all(&tmp.0).unwrap();
        std::fs::set_permissions(&tmp.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cache = DiskCache::open(tmp.0.clone()).unwrap();
        cache.write(&snapshot()).unwrap();
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(tmp.0.clone()), 0o700);
        assert_eq!(mode(tmp.0.join("lock")), 0o600);
        assert_eq!(mode(tmp.0.join("notes.json")), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn files_that_already_existed_wide_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("wide");
        std::fs::create_dir_all(&tmp.0).unwrap();
        for name in ["lock", "notes.json"] {
            std::fs::write(tmp.0.join(name), b"{}").unwrap();
            let loose = std::fs::Permissions::from_mode(0o644);
            std::fs::set_permissions(tmp.0.join(name), loose).unwrap();
        }
        let cache = DiskCache::open(tmp.0.clone()).unwrap();
        let mode = |name: &str| {
            std::fs::metadata(tmp.0.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("lock"), 0o600);
        cache.write(&snapshot()).unwrap();
        assert_eq!(mode("notes.json"), 0o600);
    }

    #[test]
    fn a_malformed_snapshot_is_an_error_not_a_panic() {
        let tmp = TempDir::new("bad");
        let cache = DiskCache::open(tmp.0.clone()).unwrap();
        std::fs::write(tmp.0.join("notes.json"), b"{ not json").unwrap();
        let err = cache.read().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
