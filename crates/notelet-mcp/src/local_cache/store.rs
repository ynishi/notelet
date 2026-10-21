//! The notes [`LocalCache`](super::LocalCache) holds: a map by path, with the
//! reads of the port answered from it.
//!
//! Notes are placed exactly as the remote has them (version, `updated_at` and
//! `updated_by` included), which the in-memory adapter of `notelet-core`
//! cannot do through the port: its `put` assigns the version and the time
//! itself. So the cache keeps its own map and answers `list`, `search` and
//! `dump` here, with the same ordering (by path), keyset paging
//! ([`PageOut::from_overfetch`]), `LIKE` semantics
//! ([`notelet_core::memory::sql_like`]), case-folded substring search and
//! 200-character snippet as that adapter. The filter, search and snippet
//! rules are private to `notelet-core`, so they are written out again below;
//! the test `answers_match_the_memory_adapter` runs both on the same notes and
//! fails if they drift apart.

use std::collections::BTreeMap;

use notelet_core::domain::{DumpEntry, Field, Filter, FilterValue, Op};
use notelet_core::memory::sql_like;
use notelet_core::{Note, NoteMeta, NotePath, Page, PageOut, SearchHit};

/// Notes by path; iteration is ordered by path.
#[derive(Default)]
pub(super) struct Store {
    notes: BTreeMap<String, Note>,
}

impl Store {
    pub(super) fn get(&self, path: &NotePath) -> Option<Note> {
        self.notes.get(path.as_str()).cloned()
    }

    /// Place `note`, replacing any note at its path. Whether it is newer than
    /// what is held is the caller's decision.
    pub(super) fn insert(&mut self, note: Note) {
        self.notes.insert(note.path.as_str().to_string(), note);
    }

    /// Remove the note at `path`; whether there was one.
    pub(super) fn remove(&mut self, path: &NotePath) -> bool {
        self.notes.remove(path.as_str()).is_some()
    }

    /// Every path held, in order.
    pub(super) fn paths(&self) -> Vec<NotePath> {
        self.notes.values().map(|n| n.path.clone()).collect()
    }

    /// Every note held, in order.
    pub(super) fn all(&self) -> Vec<Note> {
        self.notes.values().cloned().collect()
    }

    /// Notes under `prefix` strictly after the page's cursor.
    fn after<'a>(&'a self, prefix: &'a str, page: &'a Page) -> impl Iterator<Item = &'a Note> + 'a {
        self.notes
            .values()
            .filter(move |n| n.path.as_str().starts_with(prefix))
            .filter(move |n| page.cursor.as_deref().is_none_or(|c| n.path.as_str() > c))
    }

    pub(super) fn list(&self, prefix: &str, filters: &[Filter], page: &Page) -> PageOut<NoteMeta> {
        let rows: Vec<NoteMeta> = self
            .after(prefix, page)
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
        PageOut::from_overfetch(rows, page.limit, |m| m.path.as_str())
    }

    pub(super) fn search(&self, text: &str, prefix: &str, page: &Page) -> PageOut<SearchHit> {
        let needle = lowercase_chars(text);
        let rows: Vec<SearchHit> = self
            .after(prefix, page)
            .filter_map(|n| {
                let at = find_lowercase(&n.body, &needle);
                if at.is_none() && find_lowercase(n.path.as_str(), &needle).is_none() {
                    return None;
                }
                let start = at.map_or(0, |at| {
                    n.body[..at]
                        .char_indices()
                        .rev()
                        .nth(59)
                        .map_or(0, |(i, _)| i)
                });
                let snippet: String = n.body[start..].chars().take(200).collect();
                Some(SearchHit {
                    path: n.path.clone(),
                    snippet,
                    updated_at: n.updated_at,
                })
            })
            .take(page.limit as usize + 1)
            .collect();
        PageOut::from_overfetch(rows, page.limit, |h| h.path.as_str())
    }

    pub(super) fn dump(&self, prefix: &str, page: &Page) -> PageOut<DumpEntry> {
        let rows: Vec<DumpEntry> = self
            .after(prefix, page)
            .take(page.limit as usize + 1)
            .map(|n| DumpEntry {
                path: n.path.clone(),
                body: n.body.clone(),
            })
            .collect();
        PageOut::from_overfetch(rows, page.limit, |d| d.path.as_str())
    }
}

/// Whether `note` satisfies `f`.
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

/// Lowercase one char at a time with `char::to_lowercase`, so the result
/// depends on no neighbouring char (unlike `str::to_lowercase`, which treats
/// a final sigma specially).
fn lowercase_chars(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// Byte offset in `body` of the first char whose lowercase form starts a
/// match of `needle` (already lowercased). Lowercasing can change the UTF-8
/// length of a char, so an offset found in the lowercased text is mapped
/// back to the original char it came from, never used on `body` directly.
fn find_lowercase(body: &str, needle: &str) -> Option<usize> {
    let mut lower = String::with_capacity(body.len());
    let mut origin: Vec<usize> = Vec::with_capacity(body.len());
    for (i, c) in body.char_indices() {
        let before = lower.len();
        lower.extend(c.to_lowercase());
        origin.resize(origin.len() + (lower.len() - before), i);
    }
    let at = lower.find(needle)?;
    Some(origin.get(at).copied().unwrap_or(body.len()))
}

#[cfg(test)]
mod tests {
    use notelet_core::domain::Filter;
    use notelet_core::{MemoryNoteRepo, NoteRepo};

    use super::*;

    fn p(path: &str) -> NotePath {
        NotePath::parse(path).unwrap()
    }

    fn page(limit: u32, cursor: Option<&str>) -> Page {
        Page::new(Some(limit), cursor.map(String::from)).unwrap()
    }

    #[test]
    fn get_insert_remove_and_order_by_path() {
        let mut s = Store::default();
        assert!(s.get(&p("a/1.md")).is_none());
        for (path, version) in [("b/1.md", 1), ("a/2.md", 4), ("a/1.md", 2)] {
            s.insert(Note {
                path: p(path),
                body: path.into(),
                version,
                updated_at: chrono::Utc::now(),
                updated_by: "t".into(),
            });
        }
        let order: Vec<_> = s.paths().iter().map(|x| x.as_str().to_string()).collect();
        assert_eq!(order, ["a/1.md", "a/2.md", "b/1.md"]);
        assert_eq!(s.all().len(), 3);
        // a second insert replaces, whatever the version
        let mut again = s.get(&p("a/2.md")).unwrap();
        again.version = 1;
        s.insert(again);
        assert_eq!(s.get(&p("a/2.md")).unwrap().version, 1);
        assert!(s.remove(&p("a/2.md")));
        assert!(!s.remove(&p("a/2.md")));
        assert_eq!(s.paths().len(), 2);
    }

    /// The same notes in the memory adapter of `notelet-core` and in the
    /// store give the same pages for every read, so the written-out rules
    /// cannot drift from the adapter's unnoticed.
    #[tokio::test]
    async fn answers_match_the_memory_adapter() {
        let mem = MemoryNoteRepo::default();
        let bodies = [
            ("a/one.md", "Alpha beta gamma", "alice"),
            ("a/two.md", "alpha\nsecond line with BETA", "bob"),
            ("a/sub/three.md", "nothing here", "alice"),
            ("b/one.md", "ΑΣ final sigma, İstanbul", "carol"),
            ("b/two.md", "100% done_ok", "bob"),
            ("c.md", &"x".repeat(300), "alice"),
            (
                "d/long.md",
                &format!("{}needle{}", "é".repeat(90), "z".repeat(300)),
                "dave",
            ),
        ];
        for (path, body, by) in bodies {
            mem.put(&p(path), body, None, by).await.unwrap();
        }
        // a second version, so versions differ
        mem.put(&p("a/one.md"), "Alpha beta gamma 2", Some(1), "bob")
            .await
            .unwrap();
        let mut store = Store::default();
        let all = Page::new(Some(Page::MAX_LIMIT), None).unwrap();
        let dumped = mem
            .dump(
                "",
                &Page::for_bodies(Some(Page::MAX_BODY_LIMIT), None).unwrap(),
            )
            .await
            .unwrap();
        for entry in dumped.items {
            store.insert(mem.get(&entry.path).await.unwrap().unwrap());
        }
        assert_eq!(store.paths().len(), bodies.len());

        let times: Vec<_> = mem
            .list("", &[], &all)
            .await
            .unwrap()
            .items
            .iter()
            .map(|m| m.updated_at)
            .collect();
        let filters: Vec<Vec<Filter>> = vec![
            vec![],
            vec![Filter::parse("updated_by", "eq", "alice").unwrap()],
            vec![Filter::parse("updated_by", "ne", "alice").unwrap()],
            vec![Filter::parse("path", "like", "a/%").unwrap()],
            vec![Filter::parse("path", "ilike", "B/_ne.MD").unwrap()],
            vec![Filter::parse("path", "gt", "b/").unwrap()],
            vec![Filter::parse("path", "lte", "b/one.md").unwrap()],
            vec![
                Filter::parse("updated_by", "eq", "bob").unwrap(),
                Filter::parse("path", "like", "a/%").unwrap(),
            ],
            vec![Filter::parse("updated_at", "gte", &times[2].to_rfc3339()).unwrap()],
            vec![Filter::parse("updated_at", "lt", &times[2].to_rfc3339()).unwrap()],
            vec![Filter::parse("updated_at", "eq", &times[3].to_rfc3339()).unwrap()],
        ];
        for prefix in ["", "a/", "a/s", "b", "zz"] {
            for limit in [1, 2, 3, 1000] {
                let mut cursor: Option<String> = None;
                loop {
                    let pg = page(limit, cursor.as_deref());
                    for f in &filters {
                        assert_eq!(
                            store.list(prefix, f, &pg),
                            mem.list(prefix, f, &pg).await.unwrap(),
                            "list {prefix:?} {f:?} {limit} {cursor:?}"
                        );
                    }
                    for text in [
                        "alpha", "BETA", "needle", "istanbul", "σ", "ς", "i̇", "%", "_", "done_ok",
                        "x", "", "one", "nope",
                    ] {
                        assert_eq!(
                            store.search(text, prefix, &pg),
                            mem.search(text, prefix, &pg).await.unwrap(),
                            "search {text:?} {prefix:?} {limit} {cursor:?}"
                        );
                    }
                    let dp =
                        Page::for_bodies(Some(limit.min(Page::MAX_BODY_LIMIT)), cursor.clone())
                            .unwrap();
                    assert_eq!(
                        store.dump(prefix, &dp),
                        mem.dump(prefix, &dp).await.unwrap(),
                        "dump {prefix:?} {limit} {cursor:?}"
                    );
                    cursor = store.list(prefix, &[], &pg).next_cursor;
                    if cursor.is_none() {
                        break;
                    }
                }
            }
        }
    }
}
