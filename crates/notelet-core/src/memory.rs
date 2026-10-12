//! An in-memory [`NoteRepo`] for tests and dry runs. It mirrors the Postgres
//! adapter's semantics (ordering by path, keyset paging, LIKE patterns,
//! revision on replace and delete, no-op on an identical body) so the service tests exercise the same contract the
//! binary will meet in production.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::Mutex;

use crate::domain::{
    DumpEntry, Field, Filter, FilterValue, Note, NoteMeta, NotePath, Op, Page, PageOut, Revision,
    RevisionMeta, SearchHit,
};
use crate::error::CoreError;
use crate::port::{NoteRepo, PutResult};

/// Notes in a `BTreeMap` (so iteration is already ordered by path).
#[derive(Default)]
pub struct MemoryNoteRepo {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    notes: BTreeMap<String, Note>,
    revisions: Vec<Revision>,
}

impl Inner {
    /// The version a newly created note at `path` starts at: one past the
    /// highest revision kept for it (1 when there is none), so a note
    /// re-created after `delete` never reuses a revision's version.
    fn first_version(&self, path: &NotePath) -> i64 {
        self.revisions
            .iter()
            .filter(|r| r.path == *path)
            .map(|r| r.version)
            .max()
            .unwrap_or(0)
            + 1
    }
}

/// The current note, kept as the revision of its own version.
fn revision_of(n: Note) -> Revision {
    Revision {
        path: n.path,
        version: n.version,
        body: n.body,
        updated_at: n.updated_at,
        updated_by: n.updated_by,
    }
}

/// SQL `LIKE` semantics: `%` any run, `_` one char, `\` escapes.
pub fn sql_like(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = if case_insensitive {
        (
            pattern.to_lowercase().chars().collect(),
            text.to_lowercase().chars().collect(),
        )
    } else {
        (pattern.chars().collect(), text.chars().collect())
    };
    like_match(&p, &t)
}

fn like_match(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('%') => (0..=t.len()).any(|i| like_match(&p[1..], &t[i..])),
        Some('_') => !t.is_empty() && like_match(&p[1..], &t[1..]),
        Some('\\') => match (p.get(1), t.first()) {
            (Some(e), Some(c)) if e == c => like_match(&p[2..], &t[1..]),
            _ => false,
        },
        Some(c) => t.first() == Some(c) && like_match(&p[1..], &t[1..]),
    }
}

fn passes(note: &Note, f: &Filter) -> bool {
    match (&f.value, f.field) {
        (FilterValue::Time(v), Field::UpdatedAt) => match f.op {
            Op::Eq => note.updated_at == *v,
            Op::Ne => note.updated_at != *v,
            Op::Gt => note.updated_at > *v,
            Op::Lt => note.updated_at < *v,
            Op::Gte => note.updated_at >= *v,
            Op::Lte => note.updated_at <= *v,
            Op::Like | Op::ILike => false,
        },
        (FilterValue::Text(v), field) => {
            let s = match field {
                Field::Path => note.path.as_str(),
                Field::UpdatedBy => note.updated_by.as_str(),
                Field::UpdatedAt => return false,
            };
            match f.op {
                Op::Eq => s == v,
                Op::Ne => s != v,
                Op::Like => sql_like(v, s, false),
                Op::ILike => sql_like(v, s, true),
                Op::Gt => s > v.as_str(),
                Op::Lt => s < v.as_str(),
                Op::Gte => s >= v.as_str(),
                Op::Lte => s <= v.as_str(),
            }
        }
        _ => false,
    }
}

fn after_cursor<'a>(
    notes: &'a BTreeMap<String, Note>,
    prefix: &'a str,
    cursor: Option<&'a str>,
) -> impl Iterator<Item = &'a Note> + 'a {
    notes
        .values()
        .filter(move |n| n.path.as_str().starts_with(prefix))
        .filter(move |n| cursor.is_none_or(|c| n.path.as_str() > c))
}

#[async_trait]
impl NoteRepo for MemoryNoteRepo {
    async fn get(&self, path: &NotePath) -> Result<Option<Note>, CoreError> {
        Ok(self.inner.lock().await.notes.get(path.as_str()).cloned())
    }

    async fn put(
        &self,
        path: &NotePath,
        body: &str,
        expected: Option<i64>,
        by: &str,
    ) -> Result<PutResult, CoreError> {
        let mut g = self.inner.lock().await;
        let current = g.notes.get(path.as_str()).cloned();
        let next = match (expected, current) {
            (None, Some(cur)) => {
                return Err(CoreError::VersionConflict {
                    path: path.to_string(),
                    expected: None,
                    current: cur.version,
                })
            }
            (Some(_), None) => return Err(CoreError::NotFound(path.to_string())),
            (Some(e), Some(cur)) if cur.version != e => {
                return Err(CoreError::VersionConflict {
                    path: path.to_string(),
                    expected: Some(e),
                    current: cur.version,
                })
            }
            (Some(_), Some(cur)) if cur.body == body => {
                return Ok(PutResult {
                    note: cur,
                    changed: false,
                })
            }
            (None, None) => Note {
                path: path.clone(),
                body: body.to_string(),
                version: g.first_version(path),
                updated_at: Utc::now(),
                updated_by: by.to_string(),
            },
            (Some(_), Some(cur)) => {
                let version = cur.version + 1;
                g.revisions.push(revision_of(cur));
                Note {
                    path: path.clone(),
                    body: body.to_string(),
                    version,
                    updated_at: Utc::now(),
                    updated_by: by.to_string(),
                }
            }
        };
        g.notes.insert(path.as_str().to_string(), next.clone());
        Ok(PutResult {
            note: next,
            changed: true,
        })
    }

    async fn delete(
        &self,
        path: &NotePath,
        expected: Option<i64>,
        _by: &str,
    ) -> Result<Note, CoreError> {
        let mut g = self.inner.lock().await;
        let Some(cur) = g.notes.get(path.as_str()).cloned() else {
            return Err(CoreError::NotFound(path.to_string()));
        };
        if let Some(e) = expected {
            if cur.version != e {
                return Err(CoreError::VersionConflict {
                    path: path.to_string(),
                    expected: Some(e),
                    current: cur.version,
                });
            }
        }
        g.revisions.push(revision_of(cur.clone()));
        g.notes.remove(path.as_str());
        Ok(cur)
    }

    async fn append(&self, path: &NotePath, text: &str, by: &str) -> Result<Note, CoreError> {
        let mut g = self.inner.lock().await;
        let start = g.first_version(path) - 1;
        let entry = g
            .notes
            .entry(path.as_str().to_string())
            .or_insert_with(|| Note {
                path: path.clone(),
                body: String::new(),
                version: start,
                updated_at: Utc::now(),
                updated_by: by.to_string(),
            });
        if !entry.body.is_empty() && !entry.body.ends_with('\n') {
            entry.body.push('\n');
        }
        entry.body.push_str(text);
        entry.version += 1;
        entry.updated_at = Utc::now();
        entry.updated_by = by.to_string();
        Ok(entry.clone())
    }

    async fn list(
        &self,
        prefix: &str,
        filters: &[Filter],
        page: &Page,
    ) -> Result<PageOut<NoteMeta>, CoreError> {
        let g = self.inner.lock().await;
        let rows: Vec<NoteMeta> = after_cursor(&g.notes, prefix, page.cursor.as_deref())
            .filter(|n| filters.iter().all(|f| passes(n, f)))
            .take(page.limit as usize + 1)
            .map(|n| NoteMeta {
                path: n.path.clone(),
                version: n.version,
                updated_at: n.updated_at,
                updated_by: n.updated_by.clone(),
                size: n.body.len() as i64,
            })
            .collect();
        Ok(PageOut::from_overfetch(rows, page.limit, |m| {
            m.path.as_str()
        }))
    }

    async fn search(
        &self,
        text: &str,
        prefix: &str,
        page: &Page,
    ) -> Result<PageOut<SearchHit>, CoreError> {
        let needle = text.to_lowercase();
        let g = self.inner.lock().await;
        let rows: Vec<SearchHit> = after_cursor(&g.notes, prefix, page.cursor.as_deref())
            .filter_map(|n| {
                let lower = n.body.to_lowercase();
                let at = lower.find(&needle)?;
                let start = n.body[..at]
                    .char_indices()
                    .rev()
                    .nth(59)
                    .map_or(0, |(i, _)| i);
                let snippet: String = n.body[start..].chars().take(200).collect();
                Some(SearchHit {
                    path: n.path.clone(),
                    snippet,
                    updated_at: n.updated_at,
                })
            })
            .take(page.limit as usize + 1)
            .collect();
        Ok(PageOut::from_overfetch(rows, page.limit, |h| {
            h.path.as_str()
        }))
    }

    async fn history(&self, path: &NotePath, limit: u32) -> Result<Vec<Revision>, CoreError> {
        let g = self.inner.lock().await;
        let mut rows: Vec<Revision> = g
            .revisions
            .iter()
            .filter(|r| r.path == *path)
            .cloned()
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.version));
        rows.truncate(limit as usize);
        Ok(rows)
    }

    async fn history_meta(
        &self,
        path: &NotePath,
        limit: u32,
    ) -> Result<Vec<RevisionMeta>, CoreError> {
        let g = self.inner.lock().await;
        let mut rows: Vec<RevisionMeta> = g
            .revisions
            .iter()
            .filter(|r| r.path == *path)
            .map(|r| RevisionMeta {
                path: r.path.clone(),
                version: r.version,
                updated_at: r.updated_at,
                updated_by: r.updated_by.clone(),
                size: r.body.len() as i64,
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.version));
        rows.truncate(limit as usize);
        Ok(rows)
    }

    async fn dump(&self, prefix: &str, page: &Page) -> Result<PageOut<DumpEntry>, CoreError> {
        let g = self.inner.lock().await;
        let rows: Vec<DumpEntry> = after_cursor(&g.notes, prefix, page.cursor.as_deref())
            .take(page.limit as usize + 1)
            .map(|n| DumpEntry {
                path: n.path.clone(),
                body: n.body.clone(),
            })
            .collect();
        Ok(PageOut::from_overfetch(rows, page.limit, |d| {
            d.path.as_str()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::sql_like;

    #[test]
    fn like_semantics() {
        assert!(sql_like("%/j.md", "projects/a/j.md", false));
        assert!(sql_like("projects/_/j.md", "projects/a/j.md", false));
        assert!(!sql_like("projects/_/j.md", "projects/ab/j.md", false));
        assert!(sql_like("50\\%", "50%", false));
        assert!(!sql_like("50\\%", "500", false));
        assert!(sql_like("PROJ%", "projects/x", true));
        assert!(!sql_like("PROJ%", "projects/x", false));
    }
}
