//! The storage port. [`NoteRepo`] is the whole contract between the service
//! and an adapter. There are three: the Postgres adapter ([`crate::pg`]) and
//! the in-memory adapter ([`crate::memory`]) in this crate, and the remote
//! adapter in `notelet-mcp`, which forwards each method to the MCP tool of
//! another notelet and so lets a process serve with no database. The MCP
//! binary holds the chosen one as `Arc<dyn NoteRepo>`. An adapter that is not
//! the store itself may not be able to honour every argument: the remote
//! one cannot send `by` (the remote records the actor of the token it
//! presents), which is why `by` is the caller's request, not a guarantee.
//!
//! The port carries the locking semantics ([`NoteRepo::put`],
//! [`NoteRepo::delete`]) because the check-and-write must be atomic inside
//! the adapter (a transaction in Postgres, a mutex in memory); the service
//! cannot compose it from a `get` and a `set` without a race.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::{
    DumpEntry, Filter, Note, NoteMeta, NotePath, NoteView, Page, PageOut, Revision, RevisionMeta,
    SearchHit, Tail,
};
use crate::error::CoreError;

/// What `put` reports: the note as stored, and whether anything changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutResult {
    pub note: Note,
    /// `false` when the body was identical to the current one: no version
    /// bump, no revision. Makes re-runs and retries idempotent.
    pub changed: bool,
}

/// A store of notes keyed by path.
#[async_trait]
pub trait NoteRepo: Send + Sync {
    /// The note at `path`, if any.
    async fn get(&self, path: &NotePath) -> Result<Option<Note>, CoreError>;

    /// The note at `path` as a read returns it: [`NoteView`], whose body is
    /// cut by `tail` (see [`Tail`]; [`Tail::whole`] for all of it). The same
    /// fetch as [`NoteRepo::get`]; the cut happens in
    /// [`NoteView::of`], so adapters cannot disagree about it.
    async fn get_view(&self, path: &NotePath, tail: &Tail) -> Result<Option<NoteView>, CoreError>;

    /// Create or replace the body with optimistic locking.
    ///
    /// * `expected = None`: create. Fails with [`CoreError::VersionConflict`]
    ///   (carrying the current version) if the note already exists.
    /// * `expected = Some(v)`: replace only if the current version is `v`;
    ///   otherwise [`CoreError::VersionConflict`]. [`CoreError::NotFound`] if
    ///   there is no note.
    ///
    /// On replace, if the new body equals the current one nothing is written
    /// (`changed = false`). Otherwise the previous body is recorded as a
    /// [`Revision`] under the old version and the version becomes `old + 1`.
    /// A create starts at 1, or one past the highest revision still kept
    /// for the path (a note re-created after [`NoteRepo::delete`]), so
    /// versions never repeat within a path.
    async fn put(
        &self,
        path: &NotePath,
        body: &str,
        expected: Option<i64>,
        by: &str,
    ) -> Result<PutResult, CoreError>;

    /// Append `text` (already newline-normalised by the service) to the end
    /// of the body, creating the note if needed. No version check, no
    /// revision: an append cannot lose anyone's content. Successive appends
    /// are also the way to upload a large body in chunks.
    async fn append(&self, path: &NotePath, text: &str, by: &str) -> Result<Note, CoreError>;

    /// Remove the note, keeping its current body as the last revision so
    /// `history` can still read it. `expected = Some(v)` must match the
    /// current version (else [`CoreError::VersionConflict`]); `None` deletes
    /// whatever is there (unlike `put`, where `None` means "create").
    /// [`CoreError::NotFound`] if there is no note. Returns the note as it
    /// was. `by` is not recorded: the kept revision carries the last
    /// writer's `updated_by`, and nothing names who deleted it.
    async fn delete(
        &self,
        path: &NotePath,
        expected: Option<i64>,
        by: &str,
    ) -> Result<Note, CoreError>;

    /// Notes whose path starts with `prefix` (empty = all) and satisfy every
    /// filter, ordered by path, paged.
    async fn list(
        &self,
        prefix: &str,
        filters: &[Filter],
        page: &Page,
    ) -> Result<PageOut<NoteMeta>, CoreError>;

    /// Notes whose body contains `text` (case-insensitive substring), under
    /// `prefix`, ordered by path, paged. The snippet is a window around the
    /// first occurrence.
    async fn search(
        &self,
        text: &str,
        prefix: &str,
        page: &Page,
    ) -> Result<PageOut<SearchHit>, CoreError>;

    /// Replaced (or deleted) bodies of `path`, newest first, at most `limit`.
    async fn history(&self, path: &NotePath, limit: u32) -> Result<Vec<Revision>, CoreError>;

    /// The same revisions as [`NoteRepo::history`] without their bodies
    /// (size in bytes instead), so the bodies are never read.
    async fn history_meta(
        &self,
        path: &NotePath,
        limit: u32,
    ) -> Result<Vec<RevisionMeta>, CoreError>;

    /// Paths and full bodies under `prefix`, ordered by path, paged. For
    /// backups and for leaving: a dump is a file tree.
    async fn dump(&self, prefix: &str, page: &Page) -> Result<PageOut<DumpEntry>, CoreError>;
}

#[async_trait]
impl<T: NoteRepo + ?Sized> NoteRepo for Arc<T> {
    async fn get(&self, path: &NotePath) -> Result<Option<Note>, CoreError> {
        (**self).get(path).await
    }
    async fn get_view(&self, path: &NotePath, tail: &Tail) -> Result<Option<NoteView>, CoreError> {
        (**self).get_view(path, tail).await
    }
    async fn put(
        &self,
        path: &NotePath,
        body: &str,
        expected: Option<i64>,
        by: &str,
    ) -> Result<PutResult, CoreError> {
        (**self).put(path, body, expected, by).await
    }
    async fn append(&self, path: &NotePath, text: &str, by: &str) -> Result<Note, CoreError> {
        (**self).append(path, text, by).await
    }
    async fn delete(
        &self,
        path: &NotePath,
        expected: Option<i64>,
        by: &str,
    ) -> Result<Note, CoreError> {
        (**self).delete(path, expected, by).await
    }
    async fn list(
        &self,
        prefix: &str,
        filters: &[Filter],
        page: &Page,
    ) -> Result<PageOut<NoteMeta>, CoreError> {
        (**self).list(prefix, filters, page).await
    }
    async fn search(
        &self,
        text: &str,
        prefix: &str,
        page: &Page,
    ) -> Result<PageOut<SearchHit>, CoreError> {
        (**self).search(text, prefix, page).await
    }
    async fn history(&self, path: &NotePath, limit: u32) -> Result<Vec<Revision>, CoreError> {
        (**self).history(path, limit).await
    }
    async fn history_meta(
        &self,
        path: &NotePath,
        limit: u32,
    ) -> Result<Vec<RevisionMeta>, CoreError> {
        (**self).history_meta(path, limit).await
    }
    async fn dump(&self, prefix: &str, page: &Page) -> Result<PageOut<DumpEntry>, CoreError> {
        (**self).dump(prefix, page).await
    }
}
