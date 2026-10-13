//! [`NoteService`]: the use cases. Each method validates its inputs with
//! the domain types and then makes exactly one repository call. There is no
//! other logic here on purpose; if a method grows a second step, that step
//! probably belongs in the domain or the adapter.

use std::sync::Arc;

use crate::domain::{
    normalize_append, validate_prefix, DumpEntry, Filter, Note, NoteMeta, NotePath, NoteView, Page,
    PageOut, Revision, RevisionMeta, SearchHit, Tail, WriteOutcome,
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

/// `history` with or without bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum History {
    Full(Vec<Revision>),
    Meta(Vec<RevisionMeta>),
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

    /// Read one note, optionally only its end: `tail_lines` selects the
    /// last lines and `max_bytes` caps what is returned (see [`Tail`]; a
    /// zero for either is [`CoreError::InvalidTail`]). The view carries the
    /// whole body's `size` and `truncated`; a truncated body must not be
    /// written back with `put`.
    pub async fn read(
        &self,
        path: &str,
        tail_lines: Option<u32>,
        max_bytes: Option<u32>,
    ) -> Result<NoteView, CoreError> {
        let path = NotePath::parse(path)?;
        let tail = Tail::new(tail_lines, max_bytes)?;
        self.repo
            .get_view(&path, &tail)
            .await?
            .ok_or_else(|| CoreError::NotFound(path.into_string()))
    }

    /// Create (`expected_version = None`) or replace (`Some(v)`) a note.
    /// See [`NoteRepo::put`] for the locking and idempotency rules.
    pub async fn put(
        &self,
        path: &str,
        body: &str,
        expected_version: Option<i64>,
        by: &str,
    ) -> Result<WriteOutcome, CoreError> {
        let path = NotePath::parse(path)?;
        let r = self.repo.put(&path, body, expected_version, by).await?;
        Ok(WriteOutcome::of(&r.note, r.changed))
    }

    /// Append a line (newline-normalised) to a note, creating it if needed.
    pub async fn append(
        &self,
        path: &str,
        text: &str,
        by: &str,
    ) -> Result<WriteOutcome, CoreError> {
        let path = NotePath::parse(path)?;
        let text = normalize_append(text);
        let note = self.repo.append(&path, &text, by).await?;
        Ok(WriteOutcome::of(&note, true))
    }

    /// Delete a note (its body stays readable through `history`).
    pub async fn delete(
        &self,
        path: &str,
        expected_version: Option<i64>,
        by: &str,
    ) -> Result<WriteOutcome, CoreError> {
        let path = NotePath::parse(path)?;
        let note = self.repo.delete(&path, expected_version, by).await?;
        Ok(WriteOutcome::of(&note, true))
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

    /// Replaced (or deleted) bodies of a note, newest first (at most 50),
    /// with or without the bodies themselves.
    pub async fn history(
        &self,
        path: &str,
        limit: Option<u32>,
        include_body: bool,
    ) -> Result<History, CoreError> {
        let path = NotePath::parse(path)?;
        let limit = limit.unwrap_or(20).clamp(1, 50);
        Ok(if include_body {
            History::Full(self.repo.history(&path, limit).await?)
        } else {
            History::Meta(self.repo.history_meta(&path, limit).await?)
        })
    }

    /// Paths and bodies under a prefix, for backup or export. Pages are
    /// smaller than for listings because every item carries a body.
    pub async fn dump(
        &self,
        prefix: Option<&str>,
        limit: Option<u32>,
        cursor: Option<String>,
    ) -> Result<PageOut<DumpEntry>, CoreError> {
        let prefix = prefix.unwrap_or("");
        validate_prefix(prefix)?;
        let page = Page::for_bodies(limit, cursor);
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
    async fn read_whole_tail_and_validation() {
        let s = svc();
        s.put("n/j.md", "l1\nl2\nl3\n", None, "t").await.unwrap();

        let whole = s.read("n/j.md", None, None).await.unwrap();
        assert_eq!(whole.body, "l1\nl2\nl3\n");
        assert!(!whole.truncated);
        assert_eq!(whole.size, 9);

        let t = s.read("n/j.md", Some(1), None).await.unwrap();
        assert_eq!(t.body, "l3\n");
        assert!(t.truncated);
        assert_eq!(t.size, 9);
        assert_eq!(t.version, 1);

        let c = s.read("n/j.md", Some(3), Some(4)).await.unwrap();
        assert_eq!(c.body, "l3\n");

        for (l, m) in [(Some(0), None), (None, Some(0))] {
            assert!(matches!(
                s.read("n/j.md", l, m).await.unwrap_err(),
                CoreError::InvalidTail(_)
            ));
        }
        assert!(matches!(
            s.read("n/none.md", Some(1), None).await.unwrap_err(),
            CoreError::NotFound(_)
        ));
        assert!(matches!(
            s.read("../x", None, None).await.unwrap_err(),
            CoreError::InvalidPath(_)
        ));
    }

    #[tokio::test]
    async fn put_create_then_replace_with_version() {
        let s = svc();
        let w = s.put("projects/x/a.md", "one", None, "t").await.unwrap();
        assert_eq!(w.version, 1);
        assert!(w.changed);
        assert_eq!(w.size, 3);
        assert_eq!(w.sha256.len(), 64);

        // creating again without a version is a conflict carrying the current one
        let e = s
            .put("projects/x/a.md", "two", None, "t")
            .await
            .unwrap_err();
        assert!(
            matches!(e, CoreError::VersionConflict { current: 1, .. }),
            "{e}"
        );
        assert!(e.to_string().contains("expected none, current 1"), "{e}");

        // wrong version is a conflict
        let e = s
            .put("projects/x/a.md", "two", Some(7), "t")
            .await
            .unwrap_err();
        assert!(matches!(e, CoreError::VersionConflict { current: 1, .. }));
        assert!(e.to_string().contains("expected 7, current 1"), "{e}");

        // right version replaces and bumps
        let w = s.put("projects/x/a.md", "two", Some(1), "t").await.unwrap();
        assert_eq!(w.version, 2);
        assert!(w.changed);
        assert_eq!(s.get("projects/x/a.md").await.unwrap().body, "two");

        // the replaced body is in history
        let h = s.history("projects/x/a.md", None, true).await.unwrap();
        let History::Full(h) = h else { panic!() };
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
    async fn put_same_body_is_a_noop() {
        let s = svc();
        s.put("n/a.md", "same", None, "t").await.unwrap();
        let w = s.put("n/a.md", "same", Some(1), "t").await.unwrap();
        assert!(!w.changed);
        assert_eq!(w.version, 1);
        let History::Meta(h) = s.history("n/a.md", None, false).await.unwrap() else {
            panic!()
        };
        assert!(h.is_empty());
    }

    #[tokio::test]
    async fn delete_keeps_body_in_history() {
        let s = svc();
        s.put("n/d.md", "bye", None, "t").await.unwrap();
        let e = s.delete("n/d.md", Some(9), "t").await.unwrap_err();
        assert!(matches!(e, CoreError::VersionConflict { current: 1, .. }));
        let w = s.delete("n/d.md", Some(1), "t").await.unwrap();
        assert_eq!(w.version, 1);
        assert!(matches!(
            s.get("n/d.md").await.unwrap_err(),
            CoreError::NotFound(_)
        ));
        let History::Full(h) = s.history("n/d.md", None, true).await.unwrap() else {
            panic!()
        };
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].body, "bye");
        assert!(matches!(
            s.delete("n/d.md", None, "t").await.unwrap_err(),
            CoreError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn recreate_after_delete_continues_the_version() {
        let s = svc();
        s.put("n/r.md", "a", None, "t").await.unwrap();
        s.put("n/r.md", "b", Some(1), "t").await.unwrap();
        s.delete("n/r.md", None, "t").await.unwrap();
        let w = s.put("n/r.md", "c", None, "t").await.unwrap();
        assert_eq!(w.version, 3);
        s.delete("n/r.md", Some(3), "t").await.unwrap();
        let w = s.append("n/r.md", "d", "t").await.unwrap();
        assert_eq!(w.version, 4);
        let History::Meta(h) = s.history("n/r.md", None, false).await.unwrap() else {
            panic!()
        };
        let versions: Vec<i64> = h.iter().map(|r| r.version).collect();
        assert_eq!(versions, vec![3, 2, 1]);
    }

    #[tokio::test]
    async fn append_creates_and_adds_lines_without_version() {
        let s = svc();
        s.append("projects/x/journal.md", "first", "t")
            .await
            .unwrap();
        let w = s
            .append("projects/x/journal.md", "second\n", "t")
            .await
            .unwrap();
        assert_eq!(w.version, 2);
        let n = s.get("projects/x/journal.md").await.unwrap();
        assert_eq!(n.body, "first\nsecond\n");
        let History::Meta(h) = s
            .history("projects/x/journal.md", None, false)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(h.is_empty());
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
        assert_eq!(all.items[0].size, 4);

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
    async fn dump_returns_bodies_under_prefix_in_small_pages() {
        let s = svc();
        s.put("lists/a.md", "A", None, "t").await.unwrap();
        s.put("projects/x/j.md", "J", None, "t").await.unwrap();
        let d = s.dump(Some("lists/"), None, None).await.unwrap();
        assert_eq!(d.items.len(), 1);
        assert_eq!(d.items[0].body, "A");
        let d = s.dump(None, Some(1), None).await.unwrap();
        assert_eq!(d.items.len(), 1);
        assert!(d.next_cursor.is_some());
    }
}
