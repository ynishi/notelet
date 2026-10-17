//! # notelet-core
//!
//! The domain, ports and storage adapters of **notelet**: a deliberately thin,
//! hosted store for Markdown notes that coding agents read and write over MCP.
//!
//! ## Why it exists
//!
//! Agents accumulate working state: journals, design drafts, private lists.
//! Keeping that state as files inside a repository checkout leaks (an agent
//! quotes a path or a paragraph into a public file) and scatters (every host,
//! worktree and archive ends up with a different copy). Keeping it in a
//! feature-rich knowledge tool costs more to maintain than the notes are
//! worth. notelet is the minimum in between: a key-value store of Markdown
//! bodies addressed by a path, with one lock, one history table, and a filter
//! that is a thin wrapper over SQL.
//!
//! ## The model
//!
//! ```text
//! Note        path -> body (Markdown), version, updated_at, updated_by
//! Tail        a read may return only the end of a body: tail_lines (>= 1) selects the last
//!             lines, max_bytes (>= 1) caps the bytes; the view carries the whole body's
//!             size and truncated, and a truncated body must never be written back with `put`
//! Revision    the body a `put` replaced or a `delete` removed, keyed by (path, version)
//! Filter      field (path | updated_at | updated_by) × op (eq ne like ilike gt lt gte lte) × value
//! Page        limit (1..=1000; dump 1..=100) + exclusive keyset cursor (the last path of the previous page)
//! ```
//!
//! A tail read ([`domain::Tail`], [`domain::NoteView`]) returns the end of the
//! body, cut on a line boundary where it can (a single line longer than
//! `max_bytes` is cut on a char boundary instead; if `max_bytes` is smaller
//! than the UTF-8 width of the last character the result is empty and
//! `truncated`). `size` is always the whole body in bytes, the number
//! `note_list` reports, and `truncated` says the body
//! is partial. The cut is the pure function [`domain::tail_start`], shared by
//! both adapters.
//!
//! Paths are slash-separated segments of `[A-Za-z0-9._-]`; the tree is only
//! a naming convention (`projects/<repo>/journal.md`, `tasks/<task>/...`,
//! `lists/<name>.md`), there is no directory object.
//!
//! ## Locking
//!
//! `put` is optimistic: the caller passes the version it last read; the store
//! replaces the body only if that is still the current version, otherwise it
//! answers [`CoreError::VersionConflict`] with the current version. There is
//! no lease and no explicit lock. A `put` of a body identical to the current
//! one changes nothing (`changed: false`), so retries are idempotent.
//! `append` needs no version at all: it adds a line to the end and cannot
//! clobber anyone. `delete` with a version removes the note only if that is
//! still the current version; without one it removes whatever is there
//! (whereas a `put` without a version means "create"). It keeps the removed
//! body as the last revision. Every write answers a
//! [`domain::WriteOutcome`] (version, byte size, SHA-256, changed); for
//! `delete` these describe the note it removed.
//!
//! ## Errors and logging
//!
//! The one error type is [`CoreError`]. A variant with a cause (`Storage`)
//! keeps it as `source()` and never repeats it in `Display`; `chain()`
//! renders the error and its causes on one line, for a message or a log
//! field. `Contended` is the append race; `VersionConflict` is the
//! optimistic lock; `InvalidPath` / `InvalidFilter` / `InvalidTail` are
//! validation; `NotFound` is a missing note; `Storage` is everything the
//! adapter could not do.
//!
//! The core never logs. It returns errors; the transport decides what to
//! record and at which level. A chain may name a path (a constraint message
//! does), never a body. A validation error quotes the rejected input (a
//! path, an unknown filter field or op, an unparsable `updated_at` value);
//! the MCP tools log these at `debug`, and `import` logs a rejected file
//! path at `warn`. The enum is `#[non_exhaustive]`.
//!
//! ## Layout (lightweight DDD / Clean)
//!
//! | module | role |
//! |---|---|
//! | [`domain`] | value types and their validation; no I/O |
//! | [`port`] | the [`port::NoteRepo`] trait the service depends on |
//! | [`service`] | [`service::NoteService`]: validation, then one repo call |
//! | [`pg`] | the Postgres adapter (sqlx); the only module that knows SQL |
//! | [`memory`] | an in-memory adapter for tests and dry runs |
//!
//! Everything a transport (the MCP binary) needs is [`service::NoteService`];
//! it never touches the repository directly.
//!
//! ## What is deliberately absent
//!
//! Tree structure, node types, properties, projections, snapshots, diffs,
//! tags, a query language, full-text indexing, multiple tables, users.
//! If a need for one of these appears, the answer is usually a path
//! convention or a second small tool, not a feature here.
//!
//! The local file projection in `notelet-mcp` (`to_file`, `from_file` and the
//! conflict file) is an I/O variant of get/put, not a model object, in the
//! same way as `/ui`: the core knows nothing of it.

pub mod domain;
pub mod error;
pub mod memory;
pub mod pg;
pub mod port;
pub mod service;

pub use domain::{
    DumpEntry, Field, Filter, Note, NoteMeta, NotePath, NoteView, Op, Page, PageOut, Revision,
    RevisionMeta, SearchHit, Tail, WriteOutcome,
};
pub use error::CoreError;
pub use memory::MemoryNoteRepo;
pub use pg::PgNoteRepo;
pub use port::{NoteRepo, PutResult};
pub use service::{History, NoteService};
