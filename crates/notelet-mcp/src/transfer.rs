//! Operator bulk transfer: `import <dir>` and `export <dir>`. These are not
//! MCP tools; they run in the binary against [`NoteService`] directly, so
//! moving a file tree in or out never goes through an agent's context.
//!
//! * **import** walks `dir` recursively. Every regular file becomes the note
//!   `<prefix>/<path relative to dir>` (`/`-separated; no prefix means the
//!   relative path alone). An existing note is replaced with its current
//!   version as `expected_version`, so an identical body is a no-op
//!   (`unchanged`). Hidden entries (name starting with `.`), symlinks,
//!   non-UTF-8 files, files that are an unresolved conflict file (they parse
//!   as one, see [`crate::files::Marked`]; a note that merely quotes a header
//!   line is imported) and paths that do not validate are skipped (the reason is logged at `warn`: path, reason). A directory or
//!   entry that cannot be read counts as `failed` (logged the same way) and the walk goes on. `dry_run` reads but
//!   never writes.
//! * **export** creates `dir` if it is missing (failing to do so ends the
//!   command with an error), pages through `dump` and writes each note to `<dir>/<path>`
//!   through [`FileRoot`]: parent directories are created, each file is
//!   written atomically (a temporary file in the same directory, then a
//!   rename), a new file is `0600` on unix, and a name that resolves outside
//!   `dir` (e.g. through a symlink inside it) is refused, so every file lands
//!   inside `dir`. Directories it creates follow the umask. A note with a hidden segment (starting with `.`) is
//!   skipped, as `import` would skip it. Note paths are case-sensitive but
//!   many filesystems are not: a path that differs from an earlier one only
//!   in case is not written and counts as `failed` (the warning names both
//!   paths). A note that cannot be written (e.g. both `a` and `a/b.md`
//!   exist, so `a` would have to be a file and a directory) counts as
//!   `failed` and the export goes on. Every skip or failure is logged at
//!   `warn` with the path and the reason.

use std::path::{Path, PathBuf};

use notelet_core::{CoreError, NotePath, NoteService};

use crate::files::{FileRoot, Marked};

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
        if Marked::parse(&body).is_ok() {
            tracing::warn!(path = %file.display(), reason = "unresolved conflict", "skip");
            summary.skipped += 1;
            continue;
        }
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
    pub skipped: usize,
    pub failed: usize,
}

impl std::fmt::Display for ExportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exported {} / skipped {} / failed {}",
            self.exported, self.skipped, self.failed
        )
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
    std::fs::create_dir_all(dir)?;
    let root = FileRoot::new(dir.to_path_buf());
    // case-folded path -> the path that was written under it
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    loop {
        let page = svc
            .dump(prefix, Some(notelet_core::Page::MAX_BODY_LIMIT), cursor)
            .await?;
        for entry in &page.items {
            let path = entry.path.as_str();
            if path.split('/').any(|seg| seg.starts_with('.')) {
                tracing::warn!(path = %path, reason = "hidden", "skip");
                summary.skipped += 1;
                continue;
            }
            let folded = path.to_lowercase();
            if let Some(first) = seen.get(&folded) {
                tracing::warn!(
                    path = %path,
                    reason = %format!("differs from {first} only in case"),
                    "fail"
                );
                summary.failed += 1;
                continue;
            }
            match root.write_text(path, &entry.body) {
                Ok(()) => {
                    seen.insert(folded, path.to_owned());
                    summary.exported += 1;
                }
                Err(e) => {
                    tracing::warn!(path = %path, reason = %e, "fail");
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
                skipped: 0,
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
                skipped: 0,
                failed: 1
            }
        );
        assert_eq!(std::fs::read_to_string(out.join("a")).unwrap(), "file");
        assert_eq!(std::fs::read_to_string(out.join("c.md")).unwrap(), "C");
        let _ = std::fs::remove_dir_all(&out);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exported_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let svc = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        svc.put("m.md", "M", None, "t").await.unwrap();
        let out = scratch("mode");
        export(&svc, &out, None).await.unwrap();
        let mode = std::fs::metadata(out.join("m.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn export_does_not_follow_a_symlink_out_of_the_directory() {
        let svc = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        svc.put("esc/n.md", "N", None, "t").await.unwrap();
        svc.put("ok.md", "O", None, "t").await.unwrap();
        let outside = scratch("outside");
        let out = scratch("link");
        std::os::unix::fs::symlink(&outside, out.join("esc")).unwrap();
        let s = export(&svc, &out, None).await.unwrap();
        assert_eq!((s.exported, s.failed), (1, 1));
        assert!(!outside.join("n.md").exists());
        let _ = std::fs::remove_dir_all(&out);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn paths_differing_only_in_case_write_one_and_fail_one() {
        let svc = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        svc.put("A.md", "upper", None, "t").await.unwrap();
        svc.put("a.md", "lower", None, "t").await.unwrap();
        let out = scratch("case");
        let s = export(&svc, &out, None).await.unwrap();
        assert_eq!((s.exported, s.skipped, s.failed), (1, 0, 1));
        let _ = std::fs::remove_dir_all(&out);
    }

    #[tokio::test]
    async fn hidden_segments_are_skipped_on_export_and_import() {
        let svc = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        svc.put(".h.md", "H", None, "t").await.unwrap();
        svc.put("d/.e/f.md", "F", None, "t").await.unwrap();
        svc.put("v.md", "V", None, "t").await.unwrap();
        let out = scratch("hidden");
        let s = export(&svc, &out, None).await.unwrap();
        assert_eq!((s.exported, s.skipped, s.failed), (1, 2, 0));
        assert!(!out.join(".h.md").exists());
        assert!(!out.join("d").exists());

        let src = scratch("hidden-src");
        std::fs::create_dir_all(src.join(".e")).unwrap();
        std::fs::write(src.join(".e/f.md"), "F").unwrap();
        std::fs::write(src.join(".h.md"), "H").unwrap();
        let svc2 = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        let i = import(&svc2, &src, None, false, "t").await.unwrap();
        assert_eq!((i.created, i.skipped), (0, 2));
        let _ = std::fs::remove_dir_all(&out);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[tokio::test]
    async fn a_long_note_name_exports() {
        let svc = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        let name = format!("{}.md", "n".repeat(250));
        svc.put(&name, "long", None, "t").await.unwrap();
        let out = scratch("long");
        let s = export(&svc, &out, None).await.unwrap();
        assert_eq!((s.exported, s.failed), (1, 0));
        assert_eq!(std::fs::read_to_string(out.join(&name)).unwrap(), "long");
        let _ = std::fs::remove_dir_all(&out);
    }

    #[tokio::test]
    async fn import_skips_a_conflict_file_but_not_a_quoted_header() {
        let src = scratch("conflict");
        let marked = crate::files::Marked {
            ours: "mine\n".into(),
            base: Some((1, "base\n".into())),
            theirs: (2, "theirs\n".into()),
        }
        .render();
        std::fs::write(src.join("c.md"), &marked).unwrap();
        std::fs::write(src.join("q.md"), "quote:\n<<<<<<< ours (local)\nend\n").unwrap();
        let svc = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        let s = import(&svc, &src, None, false, "t").await.unwrap();
        assert_eq!((s.created, s.skipped, s.failed), (1, 1, 0));
        assert!(svc.get("q.md").await.is_ok());
        assert!(svc.get("c.md").await.is_err());
        let _ = std::fs::remove_dir_all(&src);
    }

    #[tokio::test]
    async fn export_creates_a_missing_directory() {
        let svc = NoteService::new(Arc::new(MemoryNoteRepo::default()));
        svc.put("m/n.md", "N", None, "t").await.unwrap();
        let out = scratch("missing").join("not/yet");
        let s = export(&svc, &out, None).await.unwrap();
        assert_eq!((s.exported, s.failed), (1, 0));
        assert_eq!(std::fs::read_to_string(out.join("m/n.md")).unwrap(), "N");
        let _ = std::fs::remove_dir_all(out.parent().unwrap().parent().unwrap());
    }
}
