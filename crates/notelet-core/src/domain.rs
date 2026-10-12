//! Value types. Validation lives here so that every adapter and transport
//! sees the same rules; nothing in this module performs I/O.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// A validated note path: slash-separated segments of `[A-Za-z0-9._-]`,
/// no empty segment, no `.` or `..`, at most [`NotePath::MAX_LEN`] bytes.
///
/// The path is the whole identity of a note. The tree it suggests
/// (`projects/<repo>/journal.md`) is a convention enforced by nothing but
/// the prefix listing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NotePath(String);

impl NotePath {
    /// Upper bound on the byte length of a path.
    pub const MAX_LEN: usize = 512;

    /// Validate `raw` as a path.
    pub fn parse(raw: &str) -> Result<Self, CoreError> {
        if raw.is_empty() {
            return Err(CoreError::InvalidPath("empty".into()));
        }
        if raw.len() > Self::MAX_LEN {
            return Err(CoreError::InvalidPath(format!(
                "longer than {} bytes",
                Self::MAX_LEN
            )));
        }
        for seg in raw.split('/') {
            check_segment(seg, raw)?;
        }
        Ok(Self(raw.to_string()))
    }

    /// The path as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the underlying string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for NotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn check_segment(seg: &str, whole: &str) -> Result<(), CoreError> {
    if seg.is_empty() {
        return Err(CoreError::InvalidPath(format!(
            "empty segment in {whole:?}"
        )));
    }
    if seg == "." || seg == ".." {
        return Err(CoreError::InvalidPath(format!(
            "segment {seg:?} not allowed in {whole:?}"
        )));
    }
    if let Some(c) = seg
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(CoreError::InvalidPath(format!(
            "character {c:?} not allowed in {whole:?}"
        )));
    }
    Ok(())
}

/// Validate a listing prefix. A prefix is looser than a path: it may end in
/// the middle of a segment (`projects/x`) or with a slash, and may be
/// empty (everything). Only the character set is checked.
pub fn validate_prefix(raw: &str) -> Result<(), CoreError> {
    if raw.len() > NotePath::MAX_LEN {
        return Err(CoreError::InvalidPath(format!(
            "prefix longer than {} bytes",
            NotePath::MAX_LEN
        )));
    }
    if let Some(c) = raw
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
    {
        return Err(CoreError::InvalidPath(format!(
            "character {c:?} not allowed in prefix {raw:?}"
        )));
    }
    if raw.contains("/../") || raw.starts_with("../") || raw.ends_with("/..") || raw == ".." {
        return Err(CoreError::InvalidPath(format!(
            "'..' not allowed in prefix {raw:?}"
        )));
    }
    Ok(())
}

/// A note as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub path: NotePath,
    pub body: String,
    pub version: i64,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

/// A note without its body, for listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteMeta {
    pub path: NotePath,
    pub version: i64,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
    /// Body length in bytes (UTF-8).
    pub size: i64,
}

/// What a write (`put`, `append`, `delete`) reports back: enough for the
/// caller to verify the round trip without reading the body again. For
/// `delete` it describes the removed note (its last version, size and
/// SHA-256), which is also what `history` now holds as the newest revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteOutcome {
    pub path: NotePath,
    pub version: i64,
    pub updated_at: DateTime<Utc>,
    /// Body length in bytes (UTF-8) after the write.
    pub size: i64,
    /// Hex SHA-256 of the body after the write.
    pub sha256: String,
    /// `false` when a `put` carried a body identical to the current one and
    /// therefore changed nothing (no version bump, no revision).
    pub changed: bool,
}

impl WriteOutcome {
    /// Describe `note` as the result of a write.
    pub fn of(note: &Note, changed: bool) -> Self {
        Self {
            path: note.path.clone(),
            version: note.version,
            updated_at: note.updated_at,
            size: note.body.len() as i64,
            sha256: sha256_hex(&note.body),
            changed,
        }
    }
}

/// A revision without its body, for `history` when bodies are not wanted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionMeta {
    pub path: NotePath,
    pub version: i64,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
    /// Body length in bytes (UTF-8).
    pub size: i64,
}

/// Hex SHA-256 of `body`.
pub fn sha256_hex(body: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(body.as_bytes()))
}

/// A body that a `put` replaced or a `delete` removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub path: NotePath,
    pub version: i64,
    pub body: String,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

/// A search result: where the text was found and a window around the hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: NotePath,
    pub snippet: String,
    pub updated_at: DateTime<Utc>,
}

/// One entry of a dump: path and full body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpEntry {
    pub path: NotePath,
    pub body: String,
}

/// The columns a filter may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Path,
    UpdatedAt,
    UpdatedBy,
}

impl Field {
    /// Parse the wire name (`path` / `updated_at` / `updated_by`).
    pub fn parse(s: &str) -> Result<Self, CoreError> {
        match s {
            "path" => Ok(Self::Path),
            "updated_at" => Ok(Self::UpdatedAt),
            "updated_by" => Ok(Self::UpdatedBy),
            other => Err(CoreError::InvalidFilter(format!(
                "unknown field {other:?} (path | updated_at | updated_by)"
            ))),
        }
    }

    /// The SQL column name. Identifiers come only from this allowlist;
    /// user input never reaches the SQL text.
    pub fn column(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::UpdatedAt => "updated_at",
            Self::UpdatedBy => "updated_by",
        }
    }
}

/// The comparison operators a filter may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Eq,
    Ne,
    Like,
    ILike,
    Gt,
    Lt,
    Gte,
    Lte,
}

impl Op {
    /// Parse the wire name (`eq` / `ne` / `like` / `ilike` / `gt` / `lt` / `gte` / `lte`).
    pub fn parse(s: &str) -> Result<Self, CoreError> {
        match s {
            "eq" => Ok(Self::Eq),
            "ne" => Ok(Self::Ne),
            "like" => Ok(Self::Like),
            "ilike" => Ok(Self::ILike),
            "gt" => Ok(Self::Gt),
            "lt" => Ok(Self::Lt),
            "gte" => Ok(Self::Gte),
            "lte" => Ok(Self::Lte),
            other => Err(CoreError::InvalidFilter(format!(
                "unknown op {other:?} (eq | ne | like | ilike | gt | lt | gte | lte)"
            ))),
        }
    }

    /// The SQL operator. Same allowlist discipline as [`Field::column`].
    pub fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Like => "LIKE",
            Self::ILike => "ILIKE",
            Self::Gt => ">",
            Self::Lt => "<",
            Self::Gte => ">=",
            Self::Lte => "<=",
        }
    }

    /// Whether the value is a LIKE pattern (the caller controls `%` and `_`).
    pub fn is_pattern(self) -> bool {
        matches!(self, Self::Like | Self::ILike)
    }
}

/// The typed value of a filter, decided by the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterValue {
    Text(String),
    Time(DateTime<Utc>),
}

/// One predicate of a listing: `field op value`. Predicates are ANDed.
///
/// Values are always bound as SQL parameters; for `like` / `ilike` the
/// value is used as the pattern verbatim (so `%` and `_` are the caller's
/// wildcards), for every other op it is compared as text, or as a timestamp
/// when the field is `updated_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub field: Field,
    pub op: Op,
    pub value: FilterValue,
}

impl Filter {
    /// Build a filter from wire strings, parsing the value for the field.
    pub fn parse(field: &str, op: &str, value: &str) -> Result<Self, CoreError> {
        let field = Field::parse(field)?;
        let op = Op::parse(op)?;
        let value = match field {
            Field::UpdatedAt => {
                if op.is_pattern() {
                    return Err(CoreError::InvalidFilter(
                        "like / ilike do not apply to updated_at".into(),
                    ));
                }
                let t = DateTime::parse_from_rfc3339(value).map_err(|e| {
                    CoreError::InvalidFilter(format!(
                        "updated_at value {value:?} is not RFC 3339: {e}"
                    ))
                })?;
                FilterValue::Time(t.with_timezone(&Utc))
            }
            Field::Path | Field::UpdatedBy => FilterValue::Text(value.to_string()),
        };
        Ok(Self { field, op, value })
    }
}

/// Paging for listings: a limit and a keyset cursor.
///
/// Results are ordered by path; the cursor is the last path of the previous
/// page, and the next page starts strictly after it. There is no offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub limit: u32,
    pub cursor: Option<String>,
}

impl Page {
    /// Default page size when the caller gives none.
    pub const DEFAULT_LIMIT: u32 = 100;
    /// Largest page a caller may ask for.
    pub const MAX_LIMIT: u32 = 1000;

    /// Default and largest page for operations that return whole bodies
    /// (`dump`): smaller, because each item is heavy.
    pub const DEFAULT_BODY_LIMIT: u32 = 20;
    /// See [`Page::DEFAULT_BODY_LIMIT`].
    pub const MAX_BODY_LIMIT: u32 = 100;

    /// Clamp `limit` into `1..=MAX_LIMIT` (default 100) and keep the cursor.
    pub fn new(limit: Option<u32>, cursor: Option<String>) -> Self {
        Self::capped(limit, cursor, Self::DEFAULT_LIMIT, Self::MAX_LIMIT)
    }

    /// Clamp `limit` into `1..=MAX_BODY_LIMIT` (default 20); for `dump`.
    pub fn for_bodies(limit: Option<u32>, cursor: Option<String>) -> Self {
        Self::capped(
            limit,
            cursor,
            Self::DEFAULT_BODY_LIMIT,
            Self::MAX_BODY_LIMIT,
        )
    }

    fn capped(limit: Option<u32>, cursor: Option<String>, default: u32, max: u32) -> Self {
        let limit = limit.unwrap_or(default).clamp(1, max);
        Self { limit, cursor }
    }
}

/// A page of results. `next_cursor` is present when more rows follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageOut<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

impl<T> PageOut<T> {
    /// Build a page from `limit + 1` fetched rows: keep `limit`, and if a
    /// row was left over, the cursor is the path of the last kept row.
    pub fn from_overfetch(mut rows: Vec<T>, limit: u32, path_of: impl Fn(&T) -> &str) -> Self {
        let limit = limit as usize;
        if rows.len() > limit {
            rows.truncate(limit);
            let next = rows.last().map(|r| path_of(r).to_string());
            Self {
                items: rows,
                next_cursor: next,
            }
        } else {
            Self {
                items: rows,
                next_cursor: None,
            }
        }
    }
}

/// Escape `%`, `_` and `\` in `s` so it matches literally inside a LIKE
/// pattern that uses `\` as the escape character.
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Make sure appended text ends with exactly one newline, so successive
/// appends land on their own lines.
pub fn normalize_append(text: &str) -> String {
    let trimmed = text.trim_end_matches('\n');
    format!("{trimmed}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_accepts_tree_like_names() {
        for ok in [
            "projects/x/journal.md",
            "tasks/t/01-notes.md",
            "lists/x.md",
            "a",
            "a.b-c_d",
        ] {
            assert!(NotePath::parse(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn path_rejects_escapes_and_junk() {
        for bad in [
            "", "/a", "a/", "a//b", "a/../b", ".", "..", "a b", "a/é", "a\\b",
        ] {
            assert!(NotePath::parse(bad).is_err(), "{bad:?}");
        }
        let long = "a".repeat(NotePath::MAX_LEN + 1);
        assert!(NotePath::parse(&long).is_err());
    }

    #[test]
    fn prefix_is_looser_than_path() {
        for ok in ["", "projects/", "projects/x", "tasks/x/"] {
            assert!(validate_prefix(ok).is_ok(), "{ok:?}");
        }
        for bad in ["a b", "../x", "x/../y", "a%"] {
            assert!(validate_prefix(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn filter_parses_and_types_values() {
        let f = Filter::parse("path", "like", "projects/%").unwrap();
        assert_eq!(f.field, Field::Path);
        assert_eq!(f.op, Op::Like);
        assert_eq!(f.value, FilterValue::Text("projects/%".into()));

        let f = Filter::parse("updated_at", "gte", "2026-10-12T00:00:00Z").unwrap();
        assert!(matches!(f.value, FilterValue::Time(_)));

        assert!(Filter::parse("updated_at", "like", "x").is_err());
        assert!(Filter::parse("updated_at", "gt", "yesterday").is_err());
        assert!(Filter::parse("body", "eq", "x").is_err());
        assert!(Filter::parse("path", "contains", "x").is_err());
    }

    #[test]
    fn page_clamps_limit() {
        assert_eq!(Page::new(None, None).limit, Page::DEFAULT_LIMIT);
        assert_eq!(Page::new(Some(0), None).limit, 1);
        assert_eq!(Page::new(Some(5000), None).limit, Page::MAX_LIMIT);
    }

    #[test]
    fn overfetch_sets_cursor_only_when_more_rows_exist() {
        let rows: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let p = PageOut::from_overfetch(rows.clone(), 2, |s| s.as_str());
        assert_eq!(p.items, vec!["a", "b"]);
        assert_eq!(p.next_cursor.as_deref(), Some("b"));
        let p = PageOut::from_overfetch(rows, 3, |s| s.as_str());
        assert_eq!(p.next_cursor, None);
    }

    #[test]
    fn body_page_is_smaller() {
        assert_eq!(Page::for_bodies(None, None).limit, Page::DEFAULT_BODY_LIMIT);
        assert_eq!(
            Page::for_bodies(Some(999), None).limit,
            Page::MAX_BODY_LIMIT
        );
    }

    #[test]
    fn sha256_is_hex_of_utf8() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex("abc").len(), 64);
    }

    #[test]
    fn like_escape_and_append_normalisation() {
        assert_eq!(escape_like("50%_x\\"), "50\\%\\_x\\\\");
        assert_eq!(normalize_append("line"), "line\n");
        assert_eq!(normalize_append("line\n\n"), "line\n");
    }
}
