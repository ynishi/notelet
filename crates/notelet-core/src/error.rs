//! The one error type of the core. Transports map its variants onto their
//! own vocabulary (the MCP binary turns `VersionConflict` into a 409-shaped
//! tool error, `NotFound` into 404, the rest into validation or internal).
//!
//! A variant with a cause (`Storage`) keeps it as `source()`. `Display`
//! describes one layer only and never repeats the cause; [`CoreError::chain`]
//! renders the error and every cause below it on one line, for a message or
//! a log field. The enum is `#[non_exhaustive]`, so a transport matching on
//! it keeps a `_` arm.

use thiserror::Error;

/// Every failure the core can report.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// The path (or prefix) is not slash-separated `[A-Za-z0-9._-]` segments.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// No note at this path.
    #[error("not found: {0}")]
    NotFound(String),

    /// `put` or `delete` was given an `expected_version` that is not the
    /// current one (or `put` none, while the note already exists, also when
    /// another create won the race). `current` is what the
    /// caller should read before retrying.
    #[error("version conflict on {path}: expected {}, current {current}", .expected.map_or_else(|| "none".to_string(), |v| v.to_string()))]
    VersionConflict {
        path: String,
        expected: Option<i64>,
        current: i64,
    },

    /// A filter named an unknown field or operator, or its value does not
    /// parse for the field (e.g. a non-RFC 3339 `updated_at`).
    #[error("invalid filter: {0}")]
    InvalidFilter(String),

    /// A tail read was asked for zero lines or zero bytes (`tail_lines` and
    /// `max_bytes` must each be at least 1 when given).
    #[error("invalid tail: {0}")]
    InvalidTail(String),

    /// A create (`append` or a `put` without a version) lost the race to
    /// another create twice in a row; retry.
    #[error("create of {path}: note created concurrently twice")]
    Contended { path: String },

    /// The storage adapter failed. The cause is the adapter's own error,
    /// reachable through `source()`; `Display` names only this layer.
    #[error("storage")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl CoreError {
    /// This error and every cause below it, joined with `": "`, on one
    /// line: `storage: error returned from database: ...`. For a
    /// variant without a source it is the same text as `Display`.
    pub fn chain(&self) -> impl std::fmt::Display + '_ {
        Chain(self)
    }
}

struct Chain<'a>(&'a dyn std::error::Error);

impl std::fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut prev = self.0.to_string();
        f.write_str(&prev)?;
        // Some errors (sqlx's) write their source's text in their own
        // Display and also expose it as `source()`: skip a cause whose
        // text the previous level already ends with.
        for cause in std::iter::successors(self.0.source(), |e| e.source()) {
            let text = cause.to_string();
            if !prev.ends_with(&text) {
                write!(f, ": {text}")?;
            }
            prev = text;
        }
        Ok(())
    }
}

impl From<sqlx::Error> for CoreError {
    fn from(e: sqlx::Error) -> Self {
        CoreError::Storage(Box::new(e))
    }
}

impl From<sqlx::migrate::MigrateError> for CoreError {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        CoreError::Storage(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Error)]
    #[error("outer")]
    struct Outer(#[source] Inner);

    #[derive(Debug, Error)]
    #[error("inner")]
    struct Inner(#[source] std::io::Error);

    #[test]
    fn storage_displays_its_layer_and_chains_the_cause() {
        let e = CoreError::Storage(Box::new(std::io::Error::other("disk gone")));
        assert_eq!(e.to_string(), "storage");
        assert_eq!(e.chain().to_string(), "storage: disk gone");
    }

    #[test]
    fn chain_joins_every_level() {
        let e = CoreError::Storage(Box::new(Outer(Inner(std::io::Error::other("root")))));
        assert_eq!(e.chain().to_string(), "storage: outer: inner: root");
    }

    #[derive(Debug, Error)]
    #[error("wrap: {0}")]
    struct Wrap(#[source] std::io::Error);

    #[test]
    fn chain_skips_a_cause_the_display_already_contains() {
        let e = CoreError::Storage(Box::new(Wrap(std::io::Error::other("inner"))));
        assert_eq!(e.chain().to_string(), "storage: wrap: inner");
    }

    #[test]
    fn chain_without_source_is_display() {
        let e = CoreError::NotFound("x".into());
        assert_eq!(e.chain().to_string(), "not found: x");
    }

    #[test]
    fn contended_display() {
        let e = CoreError::Contended { path: "a/b".into() };
        assert_eq!(
            e.to_string(),
            "create of a/b: note created concurrently twice"
        );
    }
}
