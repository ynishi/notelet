//! [`NoteService`]: the use cases. Each method validates its inputs with
//! the domain types and then makes exactly one repository call. There is no
//! other logic here on purpose; if a method grows a second step, that step
//! probably belongs in the domain or the adapter.

use std::sync::Arc;

use crate::domain::{
    normalize_append, validate_prefix, DumpEntry, Filter, Note, NoteMeta, NotePath, Page, PageOut,
    Revision, SearchHit,
};
use crate::error::CoreError;
use crate::port::NoteRepo;

/// The application service a transport talks to.
#[derive(Clone)]
pub struct NoteService {
    repo: Arc<dyn NoteRepo>,
}

/// A filter as it arrives on the wire, before typing.
#[derive(Debug, Clone)]
pub struct FilterInput {
    pub field: String,
    pub op: String,
    pub value: String,
}

impl NoteService {
    /// Wrap a repository.
    pub fn new(repo: Arc<dyn NoteRepo>) -> Self {
        Self { repo }
    }

    /// Read one note.
    pub async fn get(&self, path: &str) -> Result<Note, CoreError> {
        let path = NotePath::parse(path)?;
        self.repo
            .get(&path)
            .await?
            .ok_or_else(|| CoreError::NotFound(path.into_string()))
    }

    /// Create (`expected_version = None`) or replace (`Some(v)`) a note.
    /// See [`NoteRepo::put`] for the locking rules.
    pub async fn put(
        &self,
        path: &str,
        body: &str,
        expected_version: Option<i64>,
        by: &str,
    ) -> Result<Note, CoreError> {
        let path = NotePath::parse(path)?;
        self.repo.put(&path, body, expected_version, by).await
    }

    /// Append a line (newline-normalised) to a note, creating it if needed.
    pub async fn append(&self, path: &str, text: &str, by: &str) -> Result<Note, CoreError> {
        let path = NotePath::parse(path)?;
        let text = normalize_append(text);
        self.repo.append(&path, &text, by).await
    }

    /// List note metadata under a prefix with optional filters.
    pub async fn list(
        &self,
        prefix: Option<&str>,
        filters: &[FilterInput],
        limit: Option<u32>,
        cursor: Option<String>,
    ) -> Result<PageOut<NoteMeta>, CoreError> {
        let prefix = prefix.unwrap_or("");
        validate_prefix(prefix)?;
        let filters = filters
            .iter()
            .map(|f| Filter::parse(&f.field, &f.op, &f.value))
            .collect::<Result<Vec<_>, _>>()?;
        let page = Page::new(limit, cursor);
        self.repo.list(prefix, &filters, &page).await
    }

    /// Case-insensitive substring search over bodies under a prefix.
    pub async fn search(
        &self,
        text: &str,
        prefix: Option<&str>,
        limit: Option<u32>,
        cursor: Option<String>,
    ) -> Result<PageOut<SearchHit>, CoreError> {
        if text.trim().is_empty() {
            return Err(CoreError::InvalidFilter("search text is empty".into()));
        }
        let prefix = prefix.unwrap_or("");
        validate_prefix(prefix)?;
        let page = Page::new(limit, cursor);
        self.repo.search(text, prefix, &page).await
    }

    /// Replaced bodies of a note, newest first (at most 50).
    pub async fn history(
        &self,
        path: &str,
        limit: Option<u32>,
    ) -> Result<Vec<Revision>, CoreError> {
        let path = NotePath::parse(path)?;
        let limit = limit.unwrap_or(20).clamp(1, 50);
        self.repo.history(&path, limit).await
    }

    /// Paths and bodies under a prefix, for backup or export.
    pub async fn dump(
        &self,
        prefix: Option<&str>,
        limit: Option<u32>,
        cursor: Option<String>,
    ) -> Result<PageOut<DumpEntry>, CoreError> {
        let prefix = prefix.unwrap_or("");
        validate_prefix(prefix)?;
        let page = Page::new(limit, cursor);
        self.repo.dump(prefix, &page).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryNoteRepo;

    fn svc() -> NoteService {
        NoteService::new(Arc::new(MemoryNoteRepo::default()))
    }

    fn f(field: &str, op: &str, value: &str) -> FilterInput {
        FilterInput {
            field: field.into(),
            op: op.into(),
            value: value.into(),
        }
    }

    #[tokio::test]
    async fn put_create_then_replace_with_version() {
        let s = svc();
        let n = s.put("projects/x/a.md", "one", None, "t").await.unwrap();
        assert_eq!(n.version, 1);

        // creating again without a version is a conflict carrying the current one
        let e = s
            .put("projects/x/a.md", "two", None, "t")
            .await
            .unwrap_err();
        assert!(
            matches!(e, CoreError::VersionConflict { current: 1, .. }),
            "{e}"
        );

        // wrong version is a conflict
        let e = s
            .put("projects/x/a.md", "two", Some(7), "t")
            .await
            .unwrap_err();
        assert!(matches!(e, CoreError::VersionConflict { current: 1, .. }));

        // right version replaces and bumps
        let n = s.put("projects/x/a.md", "two", Some(1), "t").await.unwrap();
        assert_eq!(n.version, 2);
        assert_eq!(s.get("projects/x/a.md").await.unwrap().body, "two");

        // the replaced body is in history
        let h = s.history("projects/x/a.md", None).await.unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].version, 1);
        assert_eq!(h[0].body, "one");

        // replacing a missing note is not found
        let e = s
            .put("projects/x/none.md", "x", Some(1), "t")
            .await
            .unwrap_err();
        assert!(matches!(e, CoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn append_creates_and_adds_lines_without_version() {
        let s = svc();
        s.append("projects/x/journal.md", "first", "t")
            .await
            .unwrap();
        s.append("projects/x/journal.md", "second\n", "t")
            .await
            .unwrap();
        let n = s.get("projects/x/journal.md").await.unwrap();
        assert_eq!(n.body, "first\nsecond\n");
        assert_eq!(n.version, 2);
        assert!(s
            .history("projects/x/journal.md", None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn list_filters_prefix_and_pages() {
        let s = svc();
        for p in [
            "projects/a/j.md",
            "projects/b/j.md",
            "tasks/t/1.md",
            "tasks/t/2.md",
        ] {
            s.put(
                p,
                "body",
                None,
                if p.starts_with("tasks") { "bot" } else { "me" },
            )
            .await
            .unwrap();
        }
        let all = s.list(None, &[], None, None).await.unwrap();
        assert_eq!(all.items.len(), 4);
        assert_eq!(all.next_cursor, None);

        let t = s.list(Some("tasks/"), &[], None, None).await.unwrap();
        assert_eq!(t.items.len(), 2);

        let by = s
            .list(None, &[f("updated_by", "eq", "bot")], None, None)
            .await
            .unwrap();
        assert_eq!(by.items.len(), 2);

        let like = s
            .list(None, &[f("path", "like", "%/j.md")], None, None)
            .await
            .unwrap();
        assert_eq!(like.items.len(), 2);

        let page1 = s.list(None, &[], Some(3), None).await.unwrap();
        assert_eq!(page1.items.len(), 3);
        let cursor = page1.next_cursor.clone().expect("more rows");
        let page2 = s.list(None, &[], Some(3), Some(cursor)).await.unwrap();
        assert_eq!(page2.items.len(), 1);
        assert_eq!(page2.next_cursor, None);

        assert!(s.list(Some("a b"), &[], None, None).await.is_err());
        assert!(s
            .list(None, &[f("body", "eq", "x")], None, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn search_is_case_insensitive_substring_with_snippet() {
        let s = svc();
        s.put("n/1.md", "The quick brown Fox jumps", None, "t")
            .await
            .unwrap();
        s.put("n/2.md", "nothing here", None, "t").await.unwrap();
        let r = s.search("fox", None, None, None).await.unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].path.as_str(), "n/1.md");
        assert!(r.items[0].snippet.contains("Fox"));
        // LIKE metacharacters in the query are literal
        assert!(s
            .search("%", None, None, None)
            .await
            .unwrap()
            .items
            .is_empty());
        assert!(s.search("  ", None, None, None).await.is_err());
    }

    #[tokio::test]
    async fn dump_returns_bodies_under_prefix() {
        let s = svc();
        s.put("lists/a.md", "A", None, "t").await.unwrap();
        s.put("projects/x/j.md", "J", None, "t").await.unwrap();
        let d = s.dump(Some("lists/"), None, None).await.unwrap();
        assert_eq!(d.items.len(), 1);
        assert_eq!(d.items[0].body, "A");
    }
}
