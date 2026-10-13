//! Operator bulk transfer: `import <dir>` and `export <dir>`. These are not
//! MCP tools; they run in the binary against [`NoteService`] directly, so
//! moving a file tree in or out never goes through an agent's context.
//!
//! * **import** walks `dir` recursively. Every regular file becomes the note
//!   `<prefix>/<path relative to dir>` (`/`-separated; no prefix means the
//!   relative path alone). An existing note is replaced with its current
//!   version as `expected_version`, so an identical body is a no-op
//!   (`unchanged`). Hidden entries (name starting with `.`), symlinks,
//!   non-UTF-8 files and paths that do not validate are skipped (the reason is logged at `warn`: path, reason). A directory or
//!   entry that cannot be read counts as `failed` (logged the same way) and the walk goes on. `dry_run` reads but
//!   never writes.
//! * **export** pages through `dump` and writes each note to `<dir>/<path>`,
//!   creating parent directories. Paths are validated note paths (no `..`,
//!   no empty segment), so every file lands inside `dir`. A note that cannot
//!   be written (e.g. both `a` and `a/b.md` exist, so `a` would have to be
//!   a file and a directory) counts as `failed` and the export goes on.

use std::path::{Path, PathBuf};

use notelet_core::{CoreError, NotePath, NoteService};

/// What `import` did (or, with `dry_run`, would do).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: usize,
}

impl std::fmt::Display for ImportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "created {} / updated {} / unchanged {} / skipped {} / failed {}",
            self.created, self.updated, self.unchanged, self.skipped, self.failed
        )
    }
}

enum Action {
    Created,
    Updated,
    Unchanged,
}

/// Import the file tree under `dir` (see the module doc).
pub async fn import(
    svc: &NoteService,
    dir: &Path,
    prefix: Option<&str>,
    dry_run: bool,
    by: &str,
) -> anyhow::Result<ImportSummary> {
    let prefix = prefix
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty());
    let mut files = Vec::new();
    let mut summary = ImportSummary::default();
    walk(dir, dir, &mut files, &mut summary);
    files.sort();
    for (rel, file) in files {
        let raw = match prefix {
            Some(p) => format!("{p}/{rel}"),
            None => rel,
        };
        if let Err(e) = NotePath::parse(&raw) {
            tracing::warn!(path = %file.display(), reason = %e, "skip");
            summary.skipped += 1;
            continue;
        }
        let body = match std::fs::read(&file).map(String::from_utf8) {
            Ok(Ok(b)) => b,
            Ok(Err(_)) => {
                tracing::warn!(path = %file.display(), reason = "not UTF-8", "skip");
                summary.skipped += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(path = %file.display(), reason = %e, "fail");
                summary.failed += 1;
                continue;
            }
        };
        match import_one(svc, &raw, &body, dry_run, by).await {
            Ok(Action::Created) => summary.created += 1,
            Ok(Action::Updated) => summary.updated += 1,
            Ok(Action::Unchanged) => summary.unchanged += 1,
            Err(e) => {
                tracing::warn!(path = %raw, reason = %e.chain(), "fail");
                summary.failed += 1;
            }
        }
    }
    Ok(summary)
}

async fn import_one(
    svc: &NoteService,
    path: &str,
    body: &str,
    dry_run: bool,
    by: &str,
) -> Result<Action, CoreError> {
    match svc.get(path).await {
        Ok(cur) if dry_run => Ok(if cur.body == body {
            Action::Unchanged
        } else {
            Action::Updated
        }),
        Ok(cur) => {
            let w = svc.put(path, body, Some(cur.version), by).await?;
            Ok(if w.changed {
                Action::Updated
            } else {
                Action::Unchanged
            })
        }
        Err(CoreError::NotFound(_)) if dry_run => Ok(Action::Created),
        Err(CoreError::NotFound(_)) => {
            svc.put(path, body, None, by).await?;
            Ok(Action::Created)
        }
        Err(e) => Err(e),
    }
}

/// Collect `(relative path with '/', absolute file)` for every regular,
/// non-hidden file under `dir`. Symlinks and hidden entries count as
/// skipped; an unreadable directory or entry counts as failed.
fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>, summary: &mut ImportSummary) {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) => {
            tracing::warn!(path = %dir.display(), reason = %e, "fail");
            summary.failed += 1;
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!(path = %dir.display(), reason = %e, "fail");
                summary.failed += 1;
                continue;
            }
        };
        let path = entry.path();
        let name = entry.file_name();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                tracing::warn!(path = %path.display(), reason = %e, "fail");
                summary.failed += 1;
                continue;
            }
        };
        if name.to_string_lossy().starts_with('.') {
            tracing::warn!(path = %path.display(), reason = "hidden", "skip");
            summary.skipped += 1;
        } else if ft.is_symlink() {
            tracing::warn!(path = %path.display(), reason = "symlink", "skip");
            summary.skipped += 1;
        } else if ft.is_dir() {
            walk(root, &path, out, summary);
        } else if ft.is_file() {
            // `path` came from walking `root`, so it always has that prefix.
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let parts: Option<Vec<&str>> =
                rel.components().map(|c| c.as_os_str().to_str()).collect();
            match parts {
                Some(parts) => out.push((parts.join("/"), path.clone())),
                None => {
                    tracing::warn!(path = %path.display(), reason = "file name is not UTF-8", "skip");
                    summary.skipped += 1;
                }
            }
        } else {
            tracing::warn!(path = %path.display(), reason = "not a regular file", "skip");
            summary.skipped += 1;
        }
    }
}

/// What `export` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExportSummary {
    pub exported: usize,
    pub failed: usize,
}

impl std::fmt::Display for ExportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exported {} / failed {}", self.exported, self.failed)
    }
}

/// Write every note under `prefix` to `<dir>/<path>` (see the module doc).
/// A note that cannot be written is counted as failed; a failed `dump`
/// (the store itself) still ends the export with an error.
pub async fn export(
    svc: &NoteService,
    dir: &Path,
    prefix: Option<&str>,
) -> anyhow::Result<ExportSummary> {
    let mut cursor = None;
    let mut summary = ExportSummary::default();
    loop {
        let page = svc
            .dump(prefix, Some(notelet_core::Page::MAX_BODY_LIMIT), cursor)
            .await?;
        for entry in &page.items {
            let file = dir.join(entry.path.as_str());
            let written = match file.parent() {
                Some(parent) => std::fs::create_dir_all(parent),
                None => Ok(()),
            }
            .and_then(|()| std::fs::write(&file, &entry.body));
            match written {
                Ok(()) => summary.exported += 1,
                Err(e) => {
                    tracing::warn!(path = %entry.path, reason = %e, "fail");
                    summary.failed += 1;
                }
            }
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => return Ok(summary),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use notelet_core::MemoryNoteRepo;

    use super::*;

    /// A scratch directory under the workspace `target/` (ignored by git).
    fn scratch(name: &str) -> PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/transfer-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn import_then_export_round_trips() {
        let src = scratch("src");
        std::fs::create_dir_all(src.join("x/y")).unwrap();
        std::fs::write(src.join("a.md"), "A").unwrap();
        std::fs::write(src.join("x/y/b.md"), "B").unwrap();
        std::fs::write(src.join(".hidden"), "H").unwrap();
        std::fs::write(src.join("bad name.md"), "S").unwrap();
        std::fs::write(src.join("bin.dat"), [0xff, 0xfe]).unwrap();

        let svc = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        let dry = import(&svc, &src, Some("projects/x/"), true, "t")
            .await
            .unwrap();
        assert_eq!(dry.created, 2);
        assert!(svc
            .list(None, &[], None, None)
            .await
            .unwrap()
            .items
            .is_empty());

        let s = import(&svc, &src, Some("projects/x"), false, "t")
            .await
            .unwrap();
        assert_eq!(
            s,
            ImportSummary {
                created: 2,
                updated: 0,
                unchanged: 0,
                skipped: 3,
                failed: 0
            }
        );
        assert_eq!(svc.get("projects/x/x/y/b.md").await.unwrap().body, "B");

        std::fs::write(src.join("a.md"), "A2").unwrap();
        let s = import(&svc, &src, Some("projects/x"), false, "t")
            .await
            .unwrap();
        assert_eq!((s.created, s.updated, s.unchanged), (0, 1, 1));

        let out = scratch("out");
        let n = export(&svc, &out, Some("projects/")).await.unwrap();
        assert_eq!(
            n,
            ExportSummary {
                exported: 2,
                failed: 0
            }
        );
        assert_eq!(
            std::fs::read_to_string(out.join("projects/x/a.md")).unwrap(),
            "A2"
        );
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[tokio::test]
    async fn export_counts_a_failed_note_and_goes_on() {
        let svc = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        // `a` must be a file and `a/b.md` needs `a` as a directory.
        svc.put("a", "file", None, "t").await.unwrap();
        svc.put("a/b.md", "child", None, "t").await.unwrap();
        svc.put("c.md", "C", None, "t").await.unwrap();

        let out = scratch("clash");
        let s = export(&svc, &out, None).await.unwrap();
        assert_eq!(
            s,
            ExportSummary {
                exported: 2,
                failed: 1
            }
        );
        assert_eq!(std::fs::read_to_string(out.join("a")).unwrap(), "file");
        assert_eq!(std::fs::read_to_string(out.join("c.md")).unwrap(), "C");
        let _ = std::fs::remove_dir_all(&out);
    }
}
