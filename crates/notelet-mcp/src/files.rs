//! Local file projection: the file root and the conflict file.
//!
//! In **local mode** (see [`crate::server`]) `note_get` can write a note's
//! body to a file (`to_file`), `note_put` can read the new body from one
//! (`from_file`), and a put that loses the optimistic lock can leave the
//! three bodies in that file for the caller to resolve (`on_conflict:
//! "mark"`, then `note_restore`). The point is that a large body never passes
//! through the model's context: the model edits a file and names it.
//! This module is the part that touches the disk and parses the marker file;
//! the tools are in [`crate::server`].
//!
//! # The file root
//!
//! Files are named relative to a root, which is `NOTELET_FILE_ROOT` and
//! nothing else: there is no working-directory default, so local mode exists
//! only when the operator names a directory (see [`crate::config`]). Note
//! bodies are private, so the root should be outside any repository
//! checkout. A name is refused when it is empty, contains a NUL byte, ends
//! with a separator, is absolute, contains `..`, or resolves outside the
//! root. The last check canonicalises the name (the deepest part that exists,
//! so a file that is not there yet and its missing parent directories are
//! fine) and requires the result to lie under the canonical root; that is
//! what stops a symlink inside the root that points out of it. After the
//! parent directories of a written file are created, the name is resolved
//! again and must give the same path.
//!
//! # Writing
//!
//! Every write to a user file (`to_file`, a marked file, a restored file)
//! goes through one helper. It writes `.notelet-tmp-<pid>-<n>` in the
//! same directory (a fixed-length name, so it fits the file-name limit
//! whatever the target is called; `n` from a process-wide counter, so concurrent writes to
//! one file never share a temporary; created exclusively, so a link planted
//! at that name is not followed), with mode 0600 on unix so private text is
//! never readable by others while it is written. It calls fsync on it, then
//! copies the permissions of the file it replaces (a new file keeps 0600, on
//! purpose: note bodies are private), renames it over the resolved path and
//! fsyncs the directory (a failure there is logged, not returned). On any
//! failure, or a panic, the temporary file is removed and the original is
//! untouched; a process that is killed can leave a `.notelet-tmp-…` file
//! behind. The resolved path is canonical, so a symlink at the final path
//! has already been followed: the write replaces the link's target, which
//! must be inside the root, and the link survives.
//! Files are created 0600; directories created along the way (by
//! `write_text`, so also by `export`) follow the umask.
//! `to_file` overwrites an existing file, except one that parses as a
//! conflict file written by `note_put` (see below): that is an unresolved
//! conflict, and it must be restored or deleted first. Nothing is ever
//! deleted otherwise.
//!
//! The risk that remains is a race: another process with write access to the
//! root can swap a directory for a link between the check and the use. The
//! root is meant to be a directory only the operator writes to (a read does
//! not use `O_NOFOLLOW`, since that needs a platform constant and no extra
//! dependency is allowed).
//!
//! # The conflict file
//!
//! Git's vocabulary, with three differences: the unit is the **whole body**
//! (there are no hunks), notelet does **no automatic merge** (the caller
//! chooses a side or merges by hand), and **no state is stored** beyond the
//! marker lines in the file itself.
//!
//! ```text
//! <<<<<<< ours (local, 12 lines)
//! <the whole local file>
//! ||||||| base (v7, 3 lines)
//! <the body of v7, read through history>
//! =======
//! <the current remote body>
//! >>>>>>> theirs (v9, 4 lines)
//! ```
//!
//! * `ours` is the file as the caller wrote it, `theirs` the note's current
//!   body and its version, `base` the body of the version the caller had
//!   read (`expected_version`). The base section is present only when
//!   `expected_version` was given and that revision could still be read
//!   (`note_history` keeps the last 50; versions are never reused, so a
//!   version names one body even if the note was deleted and created again).
//! * **Line counts.** Each header states how many lines its section has
//!   (`N lines`, always plural), and the parser reads exactly that many. A
//!   body may therefore contain any line, `=======` and git markers
//!   included, and marking never refuses a body. A line is the text up to and
//!   including `\n`, so `\r\n` and `\n` endings both count as one line and a
//!   lone `\r` is not a line break. The `=======` separator is written for editors; the
//!   parser only checks it. The count of `theirs` is in the closing line
//!   (it is the last line of the file); the parser checks that the lines
//!   between `=======` and it are exactly that many.
//! * **Newline encoding.** A section is followed by a marker line, so a body
//!   that does not end with `\n` gets one added. To give back the exact
//!   original bytes, the header records it: `, no-eol` after the count, e.g.
//!   `<<<<<<< ours (local, 2 lines, no-eol)` (strip the added `\n`; and a
//!   `\r` before it too, so a section with this flag restores exactly even if
//!   an editor converted the file to CRLF), or `, no-eol-cr` when the body itself ends with `\r` (strip
//!   only the added `\n`). An empty body is written as no lines and needs no
//!   flag. The counts include the added newline.
//! * If an editor converts the whole file to CRLF, the counts still hold and
//!   the restore works, but only sections that carry `no-eol` come back
//!   exactly; the others come back with the CRLF line endings the editor
//!   wrote, since the original bytes are gone.
//! * A restore validates the counts and the headers; any mismatch is a
//!   malformed-file error and the file is left unchanged.
//! * `note_put` refuses a `from_file` with a **notelet header line**: a line
//!   starting with `<<<<<<< ours (local`, `||||||| base (v` or
//!   `>>>>>>> theirs (v` (see [`is_notelet_header`]), because an unresolved
//!   conflict file must never become a note. A bare `=======` (a Markdown
//!   setext underline) or a quoted git conflict passes, unless it copies
//!   those headers exactly. A restored body that happens to contain such a
//!   line is likewise refused by the next put.
//! * `note_restore` rewrites the file to one section's body, byte for byte
//!   as it was before marking. It answers `expected_version`, the `theirs`
//!   version (what the next put needs), and `section_version`, the version
//!   the restored body belongs to: theirs or base, and null for ours.
//!   Restoring ours and putting overwrites theirs, as `git checkout --ours`
//!   does.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// Why a file operation was refused or failed.
#[derive(Debug, PartialEq, Eq)]
pub enum FileError {
    /// The request breaks a rule (a bad name, not UTF-8, conflict markers,
    /// a malformed marker file). Reported as `kind: "Validation"`.
    Refused(String),
    /// The disk said no. Reported as `kind: "File"`.
    Io(String),
}

impl FileError {
    /// The `kind` of the tool error.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Refused(_) => "Validation",
            Self::Io(_) => "File",
        }
    }

    /// The message of the tool error.
    pub fn message(&self) -> &str {
        match self {
            Self::Refused(m) | Self::Io(m) => m,
        }
    }
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

fn refused(msg: impl Into<String>) -> FileError {
    FileError::Refused(msg.into())
}

fn io(what: &str, name: &str, e: &std::io::Error) -> FileError {
    FileError::Io(format!("{what} {name:?}: {}", e.kind()))
}

#[cfg(test)]
thread_local! {
    /// Makes the rename of [`write_atomic`] fail, on this thread.
    pub(crate) static FAIL_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Distinguishes the temporary files of concurrent writes in one process.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Removes the temporary file on drop, unless it was renamed away.
struct TmpFile {
    path: PathBuf,
    armed: bool,
}

impl Drop for TmpFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Replace `path` with `bytes` (see the module doc, "Writing").
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no directory"))?;
    path.file_name()
        .ok_or_else(|| std::io::Error::other("no file name"))?;
    let n = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = dir.join(format!(".notelet-tmp-{}-{n}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(&tmp_path)?;
    let mut tmp = TmpFile {
        path: tmp_path,
        armed: true,
    };
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp.path, meta.permissions())?;
    }
    #[cfg(test)]
    if FAIL_RENAME.with(std::cell::Cell::get) {
        return Err(std::io::Error::other("forced failure"));
    }
    std::fs::rename(&tmp.path, path)?;
    tmp.armed = false;
    #[cfg(unix)]
    if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::warn!(error = %e, "could not fsync the directory of a written file");
    }
    Ok(())
}

/// The directory that file names are relative to.
#[derive(Debug, Clone)]
pub struct FileRoot {
    root: PathBuf,
}

impl FileRoot {
    /// A root at `root` (canonicalised on every use).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolve `name` to an absolute path under the root (see the module
    /// doc). The file need not exist, nor its parent directories.
    pub fn resolve(&self, name: &str) -> Result<PathBuf, FileError> {
        if name.contains('\0') {
            return Err(refused("a file name must not contain a NUL byte"));
        }
        if name.is_empty() {
            return Err(refused("a file name must not be empty"));
        }
        if name.ends_with('/') || (cfg!(windows) && name.ends_with('\\')) {
            return Err(refused(format!(
                "{name:?}: a file name must not end with a separator"
            )));
        }
        let p = Path::new(name);
        if p.is_absolute() || p.has_root() {
            return Err(refused(format!(
                "{name:?}: absolute paths are refused, name the file relative to the file root"
            )));
        }
        let mut clean = PathBuf::new();
        for c in p.components() {
            match c {
                Component::Normal(s) => clean.push(s),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(refused(format!("{name:?}: `..` is refused")));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(refused(format!("{name:?}: absolute paths are refused")));
                }
            }
        }
        if clean.as_os_str().is_empty() {
            return Err(refused("a file name must not be empty"));
        }
        let root = std::fs::canonicalize(&self.root)
            .map_err(|e| FileError::Io(format!("the file root cannot be opened: {}", e.kind())))?;
        let joined = root.join(&clean);
        // the deepest part that exists (a dangling link counts as existing)
        let mut existing = joined.as_path();
        let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
        while std::fs::symlink_metadata(existing).is_err() {
            match (existing.file_name(), existing.parent()) {
                (Some(n), Some(parent)) => {
                    missing.push(n);
                    existing = parent;
                }
                _ => return Err(refused(format!("{name:?}: cannot be resolved"))),
            }
        }
        let mut real = std::fs::canonicalize(existing).map_err(|_| {
            refused(format!(
                "{name:?}: cannot be resolved (a link that points nowhere?)"
            ))
        })?;
        if !real.starts_with(&root) {
            return Err(refused(format!("{name:?}: resolves outside the file root")));
        }
        real.extend(missing.into_iter().rev());
        Ok(real)
    }

    /// Read `name` as UTF-8 text. It must be an existing regular file.
    pub fn read_text(&self, name: &str) -> Result<String, FileError> {
        let path = self.resolve(name)?;
        let meta = std::fs::metadata(&path).map_err(|e| io("cannot read", name, &e))?;
        if !meta.is_file() {
            return Err(refused(format!("{name:?}: not a regular file")));
        }
        let bytes = std::fs::read(&path).map_err(|e| io("cannot read", name, &e))?;
        String::from_utf8(bytes).map_err(|_| refused(format!("{name:?}: not valid UTF-8")))
    }

    /// Read the body for `note_put` from `name`: UTF-8, and without a notelet
    /// header line.
    pub fn read_body(&self, name: &str) -> Result<String, FileError> {
        let text = self.read_text(name)?;
        if has_notelet_header(&text) {
            return Err(refused(format!(
                "{name:?} still has conflict markers (a line starting with \
                 `<<<<<<< ours (local`, `||||||| base (v` or `>>>>>>> theirs (v`): \
                 resolve it, or note_restore it to one side, then put again"
            )));
        }
        Ok(text)
    }

    /// Write `text` to `name` (see the module doc, "Writing"), creating its
    /// parent directories.
    pub fn write_text(&self, name: &str, text: &str) -> Result<(), FileError> {
        let path = self.resolve(name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| io("cannot create the directory of", name, &e))?;
        }
        if self.resolve(name)? != path {
            return Err(refused(format!("{name:?}: changed while it was written")));
        }
        write_atomic(&path, text.as_bytes()).map_err(|e| io("cannot write", name, &e))
    }

    /// Write a note body to `name` for `to_file`: like [`write_text`], but an
    /// existing file that parses as a conflict file is not overwritten, since
    /// it is an unresolved conflict.
    ///
    /// [`write_text`]: FileRoot::write_text
    pub fn write_body(&self, name: &str, body: &str) -> Result<(), FileError> {
        let path = self.resolve(name)?;
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            if let Ok(bytes) = std::fs::read(&path) {
                if Marked::parse(&String::from_utf8_lossy(&bytes)).is_ok() {
                    return Err(refused(format!(
                        "{name:?} holds an unresolved conflict and is not overwritten: \
                         note_restore it or delete it first"
                    )));
                }
            }
        }
        self.write_text(name, body)
    }
}

const OURS: &str = "<<<<<<< ours (";
const BASE: &str = "||||||| base (";
const MID: &str = "=======";
const THEIRS: &str = ">>>>>>> theirs (";

/// A line notelet itself writes as a section header: it starts with
/// `<<<<<<< ours (local`, `||||||| base (v` or `>>>>>>> theirs (v`.
pub fn is_notelet_header(line: &str) -> bool {
    line.starts_with("<<<<<<< ours (local")
        || line.starts_with("||||||| base (v")
        || line.starts_with(">>>>>>> theirs (v")
}

/// Whether any line of `text` is a notelet header line.
pub fn has_notelet_header(text: &str) -> bool {
    text.split_inclusive('\n').any(is_notelet_header)
}

/// Which section `note_restore` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Ours,
    Theirs,
    Base,
}

impl Source {
    /// `"ours" | "theirs" | "base"`.
    pub fn parse(s: &str) -> Result<Self, FileError> {
        match s {
            "ours" => Ok(Self::Ours),
            "theirs" => Ok(Self::Theirs),
            "base" => Ok(Self::Base),
            other => Err(refused(format!(
                "source must be \"ours\", \"theirs\" or \"base\", not {other:?}"
            ))),
        }
    }
}

/// The sections of a conflict file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marked {
    /// The caller's file, as written.
    pub ours: String,
    /// The version the caller had read and that version's body, when known.
    pub base: Option<(i64, String)>,
    /// The note's current version and body.
    pub theirs: (i64, String),
}

/// How a section's added newline is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eol {
    /// The body ended with `\n` (or is empty).
    Kept,
    /// A `\n` was added.
    Added,
    /// A `\n` was added to a body that ends with `\r`.
    AddedAfterCr,
}

/// `body` ready to sit before a marker line, how its end was handled, and
/// its number of lines.
fn section(body: &str) -> (String, Eol, usize) {
    let (text, eol) = if body.is_empty() || body.ends_with('\n') {
        (body.to_string(), Eol::Kept)
    } else if body.ends_with('\r') {
        (format!("{body}\n"), Eol::AddedAfterCr)
    } else {
        (format!("{body}\n"), Eol::Added)
    };
    let n = text.split_inclusive('\n').count();
    (text, eol, n)
}

/// The inside of a header's parentheses after the first field.
fn tail(n: usize, eol: Eol) -> String {
    let flag = match eol {
        Eol::Kept => "",
        Eol::Added => ", no-eol",
        Eol::AddedAfterCr => ", no-eol-cr",
    };
    format!("{n} lines{flag}")
}

/// A parsed header: its version (`None` for ours), line count and flag.
struct Header {
    version: Option<i64>,
    lines: usize,
    eol: Eol,
}

/// Parse `<prefix><field>, <n> lines[, flag])`. `local` selects the first
/// field: the word `local` for ours, `v<number>` otherwise.
fn header(line: &str, prefix: &str, local: bool) -> Option<Header> {
    let bare = line.trim_end_matches(['\n', '\r']);
    let inner = bare.strip_prefix(prefix)?.strip_suffix(')')?;
    let mut parts = inner.split(", ");
    let first = parts.next()?;
    let version = if local {
        (first == "local").then_some(None)?
    } else {
        Some(Some(first.strip_prefix('v')?.parse().ok()?))?
    };
    let lines = parts.next()?.strip_suffix(" lines")?.parse().ok()?;
    let eol = match parts.next() {
        None => Eol::Kept,
        Some("no-eol") => Eol::Added,
        Some("no-eol-cr") => Eol::AddedAfterCr,
        Some(_) => return None,
    };
    parts.next().is_none().then_some(Header {
        version,
        lines,
        eol,
    })
}

impl Marked {
    /// The file text (see the module doc).
    pub fn render(&self) -> String {
        let (ours, eol, n) = section(&self.ours);
        let mut out = format!("{OURS}local, {})\n{ours}", tail(n, eol));
        if let Some((v, body)) = &self.base {
            let (body, eol, n) = section(body);
            out.push_str(&format!("{BASE}v{v}, {})\n{body}", tail(n, eol)));
        }
        let (theirs, eol, n) = section(&self.theirs.1);
        out.push_str(&format!(
            "{MID}\n{theirs}{THEIRS}v{}, {})\n",
            self.theirs.0,
            tail(n, eol)
        ));
        out
    }

    /// Read a file written by [`Marked::render`]. Anything else, or a file
    /// whose counts do not match, is [`FileError::Refused`].
    pub fn parse(text: &str) -> Result<Self, FileError> {
        let bad = |why: &str| refused(format!("not a conflict file written by note_put: {why}"));
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        // `n` lines from `at`, as one string
        let take = |at: &mut usize, n: usize, which: &str| -> Result<String, FileError> {
            let end = at
                .checked_add(n)
                .filter(|e| *e <= lines.len())
                .ok_or_else(|| bad(&format!("`{which}` is shorter than its header says")))?;
            let s = lines[*at..end].concat();
            *at = end;
            Ok(s)
        };
        let unflag = |s: String, eol: Eol, which: &str| -> Result<String, FileError> {
            let mut s = s;
            match eol {
                Eol::Kept => {}
                Eol::Added => {
                    if !s.ends_with('\n') {
                        return Err(bad(&format!(
                            "`{which}` is flagged no-eol but has no line to strip"
                        )));
                    }
                    s.pop();
                    if s.ends_with('\r') {
                        s.pop();
                    }
                }
                Eol::AddedAfterCr => {
                    if !s.ends_with("\r\n") {
                        return Err(bad(&format!("`{which}` is flagged no-eol-cr but does not end with a carriage return")));
                    }
                    s.pop();
                }
            }
            Ok(s)
        };

        let head = lines.first().ok_or_else(|| bad("it is empty"))?;
        let h = header(head, OURS, true)
            .ok_or_else(|| bad("no `ours (local, N lines)` header on the first line"))?;
        let mut at = 1;
        let ours = unflag(take(&mut at, h.lines, "ours")?, h.eol, "ours")?;

        let mut base = None;
        if let Some(line) = lines.get(at).filter(|l| l.starts_with(BASE)) {
            let h = header(line, BASE, false).ok_or_else(|| bad("a malformed `base` header"))?;
            at += 1;
            let body = unflag(take(&mut at, h.lines, "base")?, h.eol, "base")?;
            base = Some((h.version.unwrap_or_default(), body));
        }

        match lines.get(at) {
            Some(l) if l.trim_end_matches(['\n', '\r']) == MID => at += 1,
            _ => return Err(bad("no `=======` after the sections")),
        }

        let last = lines.len().checked_sub(1).filter(|l| *l >= at);
        let last = last.ok_or_else(|| bad("no `theirs` header closes the file"))?;
        let h = header(lines[last], THEIRS, false)
            .ok_or_else(|| bad("no `theirs (vN, N lines)` header on the last line"))?;
        if last - at != h.lines {
            return Err(bad(
                "the `theirs` section does not have the number of lines its header says",
            ));
        }
        let theirs = unflag(take(&mut at, h.lines, "theirs")?, h.eol, "theirs")?;
        Ok(Self {
            ours,
            base,
            theirs: (h.version.unwrap_or_default(), theirs),
        })
    }

    /// The body of `source` and the version it belongs to: theirs has its
    /// version, base its version, and ours none.
    pub fn pick(&self, source: Source) -> (&str, Option<i64>) {
        match source {
            Source::Ours => (&self.ours, None),
            Source::Theirs => (&self.theirs.1, Some(self.theirs.0)),
            Source::Base => match &self.base {
                Some((v, b)) => (b, Some(*v)),
                None => ("", None),
            },
        }
    }
}

/// Rewrite `file` as the conflict file of `ours`, `base` and `theirs`, and
/// answer the base version (when there is a base). `ours` is the text the
/// put read from the file: if the file differs now it was edited during the
/// put, and nothing is written. The `Err` is the reason it was not marked.
pub fn mark(
    root: &FileRoot,
    file: &str,
    ours: &str,
    base: Option<(i64, String)>,
    theirs: (i64, String),
) -> Result<Option<i64>, String> {
    match root.read_text(file) {
        Ok(now) if now == ours => {}
        Ok(_) => return Err("the file changed during the put".into()),
        Err(e) => return Err(format!("the file could not be read again ({e})")),
    }
    let base_version = base.as_ref().map(|(v, _)| *v);
    let marked = Marked {
        ours: ours.to_string(),
        base,
        theirs,
    };
    root.write_text(file, &marked.render())
        .map_err(|e| format!("the file could not be rewritten ({e})"))?;
    Ok(base_version)
}

/// What `note_restore` answers besides the file and the source.
#[derive(Debug, PartialEq, Eq)]
pub struct Restored {
    /// The `theirs` version: what the next put passes as `expected_version`.
    pub expected_version: i64,
    /// The version the restored body belongs to (theirs or base); `None`
    /// for ours.
    pub section_version: Option<i64>,
}

/// Rewrite the conflict file `file` to the body of `source`. A malformed
/// file, or a missing base section, is an error and the file is unchanged.
pub fn restore(root: &FileRoot, file: &str, source: &str) -> Result<Restored, FileError> {
    let source = Source::parse(source)?;
    let marked = Marked::parse(&root.read_text(file)?)?;
    if source == Source::Base && marked.base.is_none() {
        return Err(refused(format!(
            "{file:?} has no base section (the put had no expected_version, or its revision could not be read)"
        )));
    }
    let (body, section_version) = marked.pick(source);
    root.write_text(file, body)?;
    Ok(Restored {
        expected_version: marked.theirs.0,
        section_version,
    })
}

#[cfg(test)]
/// A scratch directory under the workspace `target/` (ignored by git).
/// Each call gets its own directory (a process-wide counter is part of the
/// name): tests in parallel that ask for the same `name` would otherwise
/// share it, and the `remove_dir_all` below would delete the other's root.
pub(crate) fn scratch(name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/files-tests")
        .join(format!("{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(ours: &str, base: Option<(i64, &str)>, theirs: &str) -> Marked {
        Marked {
            ours: ours.into(),
            base: base.map(|(v, b)| (v, b.into())),
            theirs: (9, theirs.into()),
        }
    }

    #[test]
    fn render_has_the_documented_shape() {
        let m = marked("mine\n", Some((7, "old\nolder\n")), "yours\n");
        assert_eq!(
            m.render(),
            "<<<<<<< ours (local, 1 lines)\nmine\n||||||| base (v7, 2 lines)\nold\nolder\n=======\nyours\n>>>>>>> theirs (v9, 1 lines)\n"
        );
        let two = marked("mine\n", None, "");
        assert_eq!(
            two.render(),
            "<<<<<<< ours (local, 1 lines)\nmine\n=======\n>>>>>>> theirs (v9, 0 lines)\n"
        );
    }

    #[test]
    fn the_newline_flag_is_in_the_header_that_names_the_section() {
        let m = marked("a", Some((7, "b\r")), "c\nd");
        assert_eq!(
            m.render(),
            "<<<<<<< ours (local, 1 lines, no-eol)\na\n||||||| base (v7, 1 lines, no-eol-cr)\nb\r\n=======\nc\nd\n>>>>>>> theirs (v9, 2 lines, no-eol)\n"
        );
    }

    #[test]
    fn round_trips_every_newline_shape() {
        let bodies = [
            "",
            "\n",
            "\n\n",
            "a",
            "a\n",
            "a\nb",
            "a\r\nb\r\n",
            "a\r\nb",
            "a\r",
            "x\n\n",
        ];
        for ours in bodies {
            for base in bodies {
                for theirs in bodies {
                    for with_base in [true, false] {
                        let m = marked(ours, with_base.then_some((7, base)), theirs);
                        let text = m.render();
                        assert_eq!(Marked::parse(&text).unwrap(), m, "{text:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn bodies_may_contain_any_marker_like_line() {
        let nasty = "a\n=======\n<<<<<<< ours (local, 1 lines)\n||||||| base (v1, 1 lines)\n>>>>>>> theirs (v9, 1 lines)\n<<<<<<< x\n>>>>>>> y\nlast";
        let m = marked(nasty, Some((1, nasty)), nasty);
        assert_eq!(Marked::parse(&m.render()).unwrap(), m);
    }

    #[test]
    fn a_crlf_conversion_by_an_editor_still_restores() {
        let m = marked("one", Some((1, "b")), "t");
        let crlf = m.render().replace('\n', "\r\n");
        assert_eq!(Marked::parse(&crlf).unwrap(), m);
    }

    #[test]
    fn only_notelet_headers_are_guarded() {
        for yes in [
            "<<<<<<< ours (local, 1 lines)",
            "<<<<<<< ours (local)\n",
            "||||||| base (v1, 2 lines)",
            ">>>>>>> theirs (v2, 0 lines)",
        ] {
            assert!(is_notelet_header(yes), "{yes:?}");
        }
        for no in [
            "=======",
            "Title\n=======\n",
            "<<<<<<< HEAD",
            "||||||| merged common ancestors",
            ">>>>>>> feature",
            "<<<<<<< ours",
            " <<<<<<< ours (local",
        ] {
            assert!(!has_notelet_header(no), "{no:?}");
        }
        assert!(has_notelet_header("a\n>>>>>>> theirs (v2)\nb"));
    }

    #[test]
    fn parse_refuses_malformed_files_and_wrong_counts() {
        let good = marked("a\n", Some((7, "b\n")), "c\n").render();
        assert!(Marked::parse(&good).is_ok());
        for bad in [
            "",
            "plain text\n",
            "<<<<<<< ours (local, 1 lines)\na\n",
            "<<<<<<< ours (local, 1 lines)\na\n=======\nc\n",
            "<<<<<<< ours (elsewhere, 1 lines)\na\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            "<<<<<<< ours (local, 1 lines)\na\n=======\nc\n>>>>>>> theirs (x9, 1 lines)\n",
            "<<<<<<< ours (local)\na\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            "<<<<<<< ours (local, 1 lines)\na\n||||||| base (v, 1 lines)\nb\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            // counts edited into a mismatch
            "<<<<<<< ours (local, 2 lines)\na\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            "<<<<<<< ours (local, 0 lines)\na\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            "<<<<<<< ours (local, 1 lines)\na\n=======\nc\n>>>>>>> theirs (v9, 2 lines)\n",
            "<<<<<<< ours (local, 1 lines)\na\n=======\nc\n>>>>>>> theirs (v9, 0 lines)\n",
            "<<<<<<< ours (local, 1 lines)\na\n||||||| base (v1, 5 lines)\nb\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            "<<<<<<< ours (local, 99999999999999999999 lines)\na\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            // flags that do not fit the body
            "<<<<<<< ours (local, 0 lines, no-eol)\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            "<<<<<<< ours (local, 1 lines, no-eol-cr)\na\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            "<<<<<<< ours (local, 1 lines, other)\na\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\n",
            // text after the closing header
            "<<<<<<< ours (local, 1 lines)\na\n=======\nc\n>>>>>>> theirs (v9, 1 lines)\ntrailing\n",
        ] {
            assert!(
                matches!(Marked::parse(bad), Err(FileError::Refused(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn pick_gives_the_version_each_section_belongs_to() {
        let m = marked("o", Some((7, "b")), "t");
        assert_eq!(m.pick(Source::Ours), ("o", None));
        assert_eq!(m.pick(Source::Base), ("b", Some(7)));
        assert_eq!(m.pick(Source::Theirs), ("t", Some(9)));
        assert_eq!(marked("o", None, "t").pick(Source::Base), ("", None));
    }

    #[test]
    fn names_are_confined_to_the_root() {
        let dir = scratch("root");
        let outside = scratch("outside");
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.md"), "a").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
            std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("file-link")).unwrap();
            std::os::unix::fs::symlink(outside.join("nothing"), dir.join("dangling")).unwrap();
            std::os::unix::fs::symlink(dir.join("sub"), dir.join("inner-link")).unwrap();
        }
        let root = FileRoot::new(dir.clone());
        assert!(root.resolve("sub/a.md").unwrap().ends_with("sub/a.md"));
        assert!(root.resolve("./sub/a.md").is_ok());
        assert!(
            root.resolve("new/deeper/x.md").is_ok(),
            "missing parents are fine"
        );
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "sub/../../x",
            "sub/../a",
            "sub/",
            "new/",
            outside.to_str().unwrap(),
        ] {
            assert!(
                matches!(root.resolve(bad), Err(FileError::Refused(_))),
                "{bad:?}"
            );
        }
        let nul = root.resolve("a\0b").unwrap_err();
        assert!(nul.message().contains("NUL"), "{nul}");
        #[cfg(unix)]
        {
            for bad in ["link/secret.txt", "link/new.txt", "file-link", "dangling"] {
                assert!(
                    matches!(root.resolve(bad), Err(FileError::Refused(_))),
                    "{bad:?}"
                );
            }
            assert!(
                root.resolve("inner-link/a.md").is_ok(),
                "a link that stays inside is fine"
            );
        }
    }

    #[test]
    fn text_io_creates_parents_and_checks_utf8() {
        let dir = scratch("io");
        let root = FileRoot::new(dir.clone());
        root.write_text("a/b/c.md", "x\n").unwrap();
        assert_eq!(root.read_text("a/b/c.md").unwrap(), "x\n");
        std::fs::write(dir.join("bin"), [0xff, 0xfe]).unwrap();
        assert!(matches!(root.read_text("bin"), Err(FileError::Refused(m)) if m.contains("UTF-8")));
        assert!(
            matches!(root.read_text("a"), Err(FileError::Refused(_))),
            "a directory"
        );
        assert!(matches!(
            root.read_text("missing.md"),
            Err(FileError::Io(_))
        ));
    }

    #[test]
    fn read_body_refuses_headers_but_not_setext_underlines() {
        let dir = scratch("body");
        let root = FileRoot::new(dir.clone());
        std::fs::write(dir.join("ok.md"), "Title\n=======\ntext\n").unwrap();
        assert!(root.read_body("ok.md").is_ok());
        std::fs::write(
            dir.join("git.md"),
            "```\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> b\n```\n",
        )
        .unwrap();
        assert!(root.read_body("git.md").is_ok());
        std::fs::write(dir.join("bad.md"), "a\n>>>>>>> theirs (v2, 1 lines)\n").unwrap();
        let e = root.read_body("bad.md").unwrap_err();
        assert!(e.message().contains("conflict markers"), "{e}");
    }

    #[test]
    fn write_body_does_not_overwrite_an_unresolved_conflict() {
        let dir = scratch("overwrite");
        let root = FileRoot::new(dir.clone());
        root.write_body("w.md", "one\n").unwrap();
        root.write_body("w.md", "two\n").unwrap();
        assert_eq!(root.read_text("w.md").unwrap(), "two\n");
        let conflict = marked("a\n", None, "b\n").render();
        std::fs::write(dir.join("c.md"), &conflict).unwrap();
        let e = root.write_body("c.md", "x\n").unwrap_err();
        assert!(e.message().contains("note_restore"), "{e}");
        assert_eq!(std::fs::read_to_string(dir.join("c.md")).unwrap(), conflict);
    }

    #[test]
    fn a_note_that_quotes_notelet_headers_can_be_written_twice() {
        let dir = scratch("quoting");
        let root = FileRoot::new(dir.clone());
        let body = "intro\n>>>>>>> theirs (v2, 1 lines)\n<<<<<<< ours (local, 1 lines)\nend";
        root.write_body("q.md", body).unwrap();
        root.write_body("q.md", body).unwrap();
        assert_eq!(root.read_text("q.md").unwrap(), body);
    }

    #[test]
    fn concurrent_writes_to_one_file_leave_exactly_one_body() {
        let dir = scratch("concurrent");
        let root = FileRoot::new(dir.clone());
        let a = "a".repeat(200_000);
        let b = "b".repeat(300_000);
        for _ in 0..30 {
            let barrier = std::sync::Barrier::new(2);
            let (ra, rb) = std::thread::scope(|s| {
                let ha = s.spawn(|| {
                    barrier.wait();
                    root.write_text("same.md", &a)
                });
                let hb = s.spawn(|| {
                    barrier.wait();
                    root.write_text("same.md", &b)
                });
                (ha.join().unwrap(), hb.join().unwrap())
            });
            assert_eq!(ra, Ok(()));
            assert_eq!(rb, Ok(()));
            let now = std::fs::read_to_string(dir.join("same.md")).unwrap();
            assert!(now == a || now == b, "a torn or mixed file");
            assert!(leftovers(&dir).is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("private");
        let root = FileRoot::new(dir.clone());
        root.write_text("n.md", "secret").unwrap();
        let mode = std::fs::metadata(dir.join("n.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("notelet-tmp"))
            .collect()
    }

    #[test]
    fn a_name_at_the_length_limit_can_be_written() {
        let dir = scratch("longname");
        let root = FileRoot::new(dir.clone());
        let name = "a".repeat(255);
        root.write_text(&name, "long").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(&name)).unwrap(), "long");
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn a_failed_write_leaves_the_original_and_no_temporary_file() {
        let dir = scratch("atomic");
        let root = FileRoot::new(dir.clone());
        std::fs::write(dir.join("w.md"), "original").unwrap();
        FAIL_RENAME.with(|f| f.set(true));
        let e = root.write_text("w.md", "new").unwrap_err();
        FAIL_RENAME.with(|f| f.set(false));
        assert!(matches!(e, FileError::Io(_)), "{e}");
        assert_eq!(
            std::fs::read_to_string(dir.join("w.md")).unwrap(),
            "original"
        );
        assert!(leftovers(&dir).is_empty());

        // a rename that fails for real: the target is a directory
        std::fs::create_dir(dir.join("d")).unwrap();
        assert!(root.write_text("d", "x").is_err());
        assert!(dir.join("d").is_dir());
        assert!(leftovers(&dir).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perms");
        let root = FileRoot::new(dir.clone());
        std::fs::write(dir.join("w.md"), "a").unwrap();
        std::fs::set_permissions(dir.join("w.md"), std::fs::Permissions::from_mode(0o640)).unwrap();
        root.write_text("w.md", "b").unwrap();
        let mode = std::fs::metadata(dir.join("w.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
        assert_eq!(std::fs::read_to_string(dir.join("w.md")).unwrap(), "b");
    }

    #[test]
    fn mark_does_not_rewrite_a_file_that_changed_during_the_put() {
        let dir = scratch("changed");
        let root = FileRoot::new(dir.clone());
        std::fs::write(dir.join("w.md"), "edited meanwhile\n").unwrap();
        let why = mark(&root, "w.md", "what was put\n", None, (2, "t\n".into())).unwrap_err();
        assert!(why.contains("changed during the put"), "{why}");
        assert_eq!(
            std::fs::read_to_string(dir.join("w.md")).unwrap(),
            "edited meanwhile\n"
        );
        std::fs::write(dir.join("w.md"), "what was put\n").unwrap();
        assert_eq!(
            mark(
                &root,
                "w.md",
                "what was put\n",
                Some((1, "b\n".into())),
                (2, "t\n".into())
            ),
            Ok(Some(1))
        );
        assert!(has_notelet_header(
            &std::fs::read_to_string(dir.join("w.md")).unwrap()
        ));
    }

    #[test]
    fn restore_answers_both_versions_and_leaves_bad_files_alone() {
        let dir = scratch("restore");
        let root = FileRoot::new(dir.clone());
        let text = marked("o\n", Some((7, "b\n")), "t\n").render();
        for (source, body, section) in [
            ("ours", "o\n", None),
            ("theirs", "t\n", Some(9)),
            ("base", "b\n", Some(7)),
        ] {
            std::fs::write(dir.join("w.md"), &text).unwrap();
            let r = restore(&root, "w.md", source).unwrap();
            assert_eq!(r.expected_version, 9);
            assert_eq!(r.section_version, section, "{source}");
            assert_eq!(std::fs::read_to_string(dir.join("w.md")).unwrap(), body);
        }
        // a count edited by hand
        let edited = text.replace("ours (local, 1 lines)", "ours (local, 2 lines)");
        std::fs::write(dir.join("w.md"), &edited).unwrap();
        assert!(matches!(
            restore(&root, "w.md", "ours"),
            Err(FileError::Refused(_))
        ));
        assert_eq!(std::fs::read_to_string(dir.join("w.md")).unwrap(), edited);
        assert!(restore(&root, "w.md", "mine").is_err());
    }
}
