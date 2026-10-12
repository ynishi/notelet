//! The storage port. [`NoteRepo`] is the whole contract between the service
//! and an adapter; the Postgres adapter and the in-memory adapter both
//! implement it, and the MCP binary holds it as `Arc<dyn NoteRepo>`.
//!
//! The port carries the locking semantics ([`NoteRepo::put`]) because the
//! check-and-write must be atomic inside the adapter (a transaction in
//! Postgres, a mutex in memory); the service cannot compose it from a `get`
//! and a `set` without a race.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::{
    DumpEntry, Filter, Note, NoteMeta, NotePath, Page, PageOut, Revision, SearchHit,
};
use crate::error::CoreError;

/// A store of notes keyed by path.
#[async_trait]
pub trait NoteRepo: Send + Sync {
    /// The note at `path`, if any.
    async fn get(&self, path: &NotePath) -> Result<Option<Note>, CoreError>;

    /// Create or replace the body with optimistic locking.
    ///
    /// * `expected = None`: create. Fails with [`CoreError::VersionConflict`]
    ///   (carrying the current version) if the note already exists.
    /// * `expected = Some(v)`: replace only if the current version is `v`;
    ///   otherwise [`CoreError::VersionConflict`]. [`CoreError::NotFound`] if
    ///   there is no note.
    ///
    /// On replace, the previous body is recorded as a [`Revision`] under the
    /// old version. The new version is `old + 1` (or 1 on create).
    async fn put(
        &self,
        path: &NotePath,
        body: &str,
        expected: Option<i64>,
        by: &str,
    ) -> Result<Note, CoreError>;

    /// Append `text` (already newline-normalised by the service) to the end
    /// of the body, creating the note if needed. No version check, no
    /// revision: an append cannot lose anyone's content.
    async fn append(&self, path: &NotePath, text: &str, by: &str) -> Result<Note, CoreError>;

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

    /// Replaced bodies of `path`, newest first, at most `limit`.
    async fn history(&self, path: &NotePath, limit: u32) -> Result<Vec<Revision>, CoreError>;

    /// Paths and full bodies under `prefix`, ordered by path, paged. For
    /// backups and for leaving: a dump is a file tree.
    async fn dump(&self, prefix: &str, page: &Page) -> Result<PageOut<DumpEntry>, CoreError>;
}

#[async_trait]
impl<T: NoteRepo + ?Sized> NoteRepo for Arc<T> {
    async fn get(&self, path: &NotePath) -> Result<Option<Note>, CoreError> {
        (**self).get(path).await
    }
    async fn put(
        &self,
        path: &NotePath,
        body: &str,
        expected: Option<i64>,
        by: &str,
    ) -> Result<Note, CoreError> {
        (**self).put(path, body, expected, by).await
    }
    async fn append(&self, path: &NotePath, text: &str, by: &str) -> Result<Note, CoreError> {
        (**self).append(path, text, by).await
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
    async fn dump(&self, prefix: &str, page: &Page) -> Result<PageOut<DumpEntry>, CoreError> {
        (**self).dump(prefix, page).await
    }
}
