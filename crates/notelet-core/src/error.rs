//! The one error type of the core. Transports map its variants onto their
//! own vocabulary (the MCP binary turns `VersionConflict` into a 409-shaped
//! tool error, `NotFound` into 404, the rest into validation or internal).

use thiserror::Error;

/// Every failure the core can report.
#[derive(Debug, Error)]
pub enum CoreError {
    /// The path (or prefix) is not slash-separated `[A-Za-z0-9._-]` segments.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// No note at this path.
    #[error("not found: {0}")]
    NotFound(String),

    /// `put` was given an `expected_version` that is not the current one
    /// (or none, while the note already exists). `current` is what the
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

    /// The storage adapter failed. The message is the adapter's own.
    #[error("storage: {0}")]
    Storage(String),
}

impl From<sqlx::Error> for CoreError {
    fn from(e: sqlx::Error) -> Self {
        CoreError::Storage(e.to_string())
    }
}

impl From<sqlx::migrate::MigrateError> for CoreError {
    fn from(e: sqlx::migrate::MigrateError) -> Self {
        CoreError::Storage(format!("migrate: {e}"))
    }
}
