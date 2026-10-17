//! The Postgres adapter. This is the only module that contains SQL.
//!
//! Two rules keep it safe and small:
//!
//! 1. **Identifiers come from allowlists, values are bound.** A filter's
//!    column and operator are rendered from [`Field::column`] and
//!    [`Op::sql`] (fixed strings); every value, prefix, cursor and limit is
//!    a `$n` parameter. User input is never interpolated into SQL text.
//! 2. **Prefix and search use LIKE with `\` as escape.** The prefix and the
//!    search text are escaped with [`escape_like`] so `%` and `_` in them
//!    are literal; only the `like` / `ilike` filter ops hand the caller's
//!    pattern through verbatim, which is their point.
//!
//! `put`, `append` and `delete` run in a transaction with
//! `SELECT ... FOR UPDATE` so the read of the current row and the write
//! cannot interleave with another writer. (A single-statement upsert would
//! not do for `append`: its "one past the highest revision" subquery reads
//! the statement snapshot, so after waiting on a concurrent `delete` it
//! could insert a version that the delete has just kept as a revision.)
//! When no row exists yet, two first appends can both reach the INSERT; the
//! loser gets a unique violation and retries once, finding the row. A `put`
//! without a version on a new path has the same race; its loser answers
//! [`CoreError::VersionConflict`] with the winner's version (if the winner
//! was deleted again before that read, the create is attempted once more;
//! only a second loss is [`CoreError::Contended`]).
//!
//! Every write stamps `updated_at` with `clock_timestamp()`, never `now()`
//! (the transaction start): a writer that waited on a note's row lock commits
//! after the one before it, so for one note `updated_at` follows `version`.
//! This is per note only; across notes it is not a change feed (a long
//! transaction on one note can commit after a reader saw a later stamp on
//! another).
//!
//! A note created at a path that still has revisions (it was deleted
//! earlier) starts one past the highest of them, so its later revisions
//! never collide with the kept ones on `(path, version)`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

use crate::domain::{
    escape_like, DumpEntry, Filter, FilterValue, Note, NoteMeta, NotePath, NoteView, Page, PageOut,
    Revision, RevisionMeta, SearchHit, Tail,
};
use crate::error::CoreError;
use crate::port::{NoteRepo, PutResult};

/// Notes in Postgres.
#[derive(Clone)]
pub struct PgNoteRepo {
    pool: PgPool,
}

/// Why one `append` attempt failed: `Exists` (another writer created the
/// note between our lock and our insert) is retried, the rest is final.
enum AppendError {
    Exists,
    Sql(sqlx::Error),
    Core(CoreError),
}

impl From<sqlx::Error> for AppendError {
    fn from(e: sqlx::Error) -> Self {
        AppendError::Sql(e)
    }
}

impl AppendError {
    fn into_core(self, path: &NotePath) -> CoreError {
        match self {
            AppendError::Exists => CoreError::Contended {
                path: path.to_string(),
            },
            AppendError::Sql(e) => e.into(),
            AppendError::Core(e) => e,
        }
    }
}

#[derive(sqlx::FromRow)]
struct NoteRow {
    path: String,
    body: String,
    version: i64,
    updated_at: DateTime<Utc>,
    updated_by: String,
}

impl NoteRow {
    fn into_note(self) -> Result<Note, CoreError> {
        Ok(Note {
            path: NotePath::parse(&self.path)?,
            body: self.body,
            version: self.version,
            updated_at: self.updated_at,
            updated_by: self.updated_by,
        })
    }
}

impl PgNoteRepo {
    /// Connect a small pool to `database_url`.
    pub async fn connect(database_url: &str) -> Result<Self, CoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    /// Wrap an existing pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply the embedded migrations (`migrations/`). Idempotent.
    pub async fn migrate(&self) -> Result<(), CoreError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    /// The pool, for health checks.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Copy the current row into `revisions` under its own version.
    async fn keep_revision(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        cur: &NoteRow,
    ) -> Result<(), CoreError> {
        sqlx::query(
            "INSERT INTO revisions (path, version, body, updated_at, updated_by) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&cur.path)
        .bind(cur.version)
        .bind(&cur.body)
        .bind(cur.updated_at)
        .bind(&cur.updated_by)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// The error for a `put` that lost the race to create `path`: the
    /// version the winner wrote, read after our transaction is gone. If the
    /// winner has been deleted again in the meantime there is nothing to
    /// name, and [`CoreError::Contended`] comes back for the caller to retry
    /// the create.
    async fn lost_create(&self, path: &NotePath) -> CoreError {
        let current: Result<Option<i64>, sqlx::Error> =
            sqlx::query_scalar("SELECT version FROM notes WHERE path = $1")
                .bind(path.as_str())
                .fetch_optional(&self.pool)
                .await;
        match current {
            Ok(Some(current)) => CoreError::VersionConflict {
                path: path.to_string(),
                expected: None,
                current,
            },
            Ok(None) => CoreError::Contended {
                path: path.to_string(),
            },
            Err(e) => e.into(),
        }
    }

    /// One `put` attempt; [`CoreError::Contended`] means a create lost its
    /// race and the winner is already gone.
    async fn put_once(
        &self,
        path: &NotePath,
        body: &str,
        expected: Option<i64>,
        by: &str,
    ) -> Result<PutResult, CoreError> {
        let mut tx = self.pool.begin().await?;
        let current: Option<NoteRow> = sqlx::query_as(
            "SELECT path, body, version, updated_at, updated_by FROM notes WHERE path = $1 FOR UPDATE",
        )
        .bind(path.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        let row: NoteRow = match (expected, current) {
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
                tx.commit().await?;
                return Ok(PutResult {
                    note: cur.into_note()?,
                    changed: false,
                });
            }
            (None, None) => {
                match sqlx::query_as(
                    "INSERT INTO notes (path, body, version, updated_at, updated_by) \
                     VALUES ($1, $2, (SELECT coalesce(max(version), 0) + 1 FROM revisions WHERE path = $1), clock_timestamp(), $3) \
                     RETURNING path, body, version, updated_at, updated_by",
                )
                .bind(path.as_str())
                .bind(body)
                .bind(by)
                .fetch_one(&mut *tx)
                .await
                {
                    Ok(row) => row,
                    Err(e) if e.as_database_error().is_some_and(|d| d.is_unique_violation()) => {
                        // Another create won the race for this path. Roll
                        // back, read what it wrote, and answer as if the
                        // note had existed when we looked.
                        drop(tx);
                        return Err(self.lost_create(path).await);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            (Some(_), Some(cur)) => {
                Self::keep_revision(&mut tx, &cur).await?;
                sqlx::query_as(
                    "UPDATE notes SET body = $2, version = version + 1, updated_at = clock_timestamp(), updated_by = $3 \
                     WHERE path = $1 RETURNING path, body, version, updated_at, updated_by",
                )
                .bind(path.as_str())
                .bind(body)
                .bind(by)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        Ok(PutResult {
            note: row.into_note()?,
            changed: true,
        })
    }

    /// One `append` attempt in its own transaction: lock the row, then
    /// either create it at one past the highest kept revision, or extend
    /// its body (a newline in between unless the body is empty or already
    /// ends with one) and bump the version.
    async fn append_once(
        &self,
        path: &NotePath,
        text: &str,
        by: &str,
    ) -> Result<Note, AppendError> {
        let mut tx = self.pool.begin().await?;
        let current: Option<NoteRow> = sqlx::query_as(
            "SELECT path, body, version, updated_at, updated_by FROM notes WHERE path = $1 FOR UPDATE",
        )
        .bind(path.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let row: NoteRow = match current {
            None => sqlx::query_as(
                "INSERT INTO notes (path, body, version, updated_at, updated_by) \
                 VALUES ($1, $2, (SELECT coalesce(max(version), 0) + 1 FROM revisions WHERE path = $1), clock_timestamp(), $3) \
                 RETURNING path, body, version, updated_at, updated_by",
            )
            .bind(path.as_str())
            .bind(text)
            .bind(by)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| match e.as_database_error() {
                Some(d) if d.is_unique_violation() => AppendError::Exists,
                _ => AppendError::Sql(e),
            })?,
            Some(cur) => {
                let mut body = cur.body;
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(text);
                sqlx::query_as(
                    "UPDATE notes SET body = $2, version = version + 1, updated_at = clock_timestamp(), updated_by = $3 \
                     WHERE path = $1 RETURNING path, body, version, updated_at, updated_by",
                )
                .bind(path.as_str())
                .bind(&body)
                .bind(by)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        row.into_note().map_err(AppendError::Core)
    }

    fn push_prefix_and_cursor(qb: &mut QueryBuilder<Postgres>, prefix: &str, cursor: Option<&str>) {
        if !prefix.is_empty() {
            qb.push(" AND path LIKE ")
                .push_bind(format!("{}%", escape_like(prefix)))
                .push(" ESCAPE '\\'");
        }
        if let Some(c) = cursor {
            qb.push(" AND path > ").push_bind(c.to_string());
        }
    }
}

#[async_trait]
impl NoteRepo for PgNoteRepo {
    async fn get(&self, path: &NotePath) -> Result<Option<Note>, CoreError> {
        let row: Option<NoteRow> = sqlx::query_as(
            "SELECT path, body, version, updated_at, updated_by FROM notes WHERE path = $1",
        )
        .bind(path.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(NoteRow::into_note).transpose()
    }

    async fn get_view(&self, path: &NotePath, tail: &Tail) -> Result<Option<NoteView>, CoreError> {
        Ok(self.get(path).await?.map(|note| NoteView::of(note, tail)))
    }

    async fn put(
        &self,
        path: &NotePath,
        body: &str,
        expected: Option<i64>,
        by: &str,
    ) -> Result<PutResult, CoreError> {
        // A create that lost its race to a note that was deleted again
        // before we could read it finds the path empty: create once more,
        // as `append` does. Only a second loss is `Contended`.
        match self.put_once(path, body, expected, by).await {
            Err(CoreError::Contended { .. }) => self.put_once(path, body, expected, by).await,
            r => r,
        }
    }

    async fn delete(
        &self,
        path: &NotePath,
        expected: i64,
        _by: &str, // not recorded: the kept revision names the last writer
    ) -> Result<Note, CoreError> {
        let mut tx = self.pool.begin().await?;
        let current: Option<NoteRow> = sqlx::query_as(
            "SELECT path, body, version, updated_at, updated_by FROM notes WHERE path = $1 FOR UPDATE",
        )
        .bind(path.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(cur) = current else {
            return Err(CoreError::NotFound(path.to_string()));
        };
        if cur.version != expected {
            return Err(CoreError::VersionConflict {
                path: path.to_string(),
                expected: Some(expected),
                current: cur.version,
            });
        }
        Self::keep_revision(&mut tx, &cur).await?;
        sqlx::query("DELETE FROM notes WHERE path = $1")
            .bind(path.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        cur.into_note()
    }

    async fn append(&self, path: &NotePath, text: &str, by: &str) -> Result<Note, CoreError> {
        // A first append races another first append on the INSERT; the loser
        // sees a unique violation, and on the retry the row exists and is
        // locked like any other.
        match self.append_once(path, text, by).await {
            Err(AppendError::Exists) => match self.append_once(path, text, by).await {
                Err(AppendError::Exists) => Err(CoreError::Contended {
                    path: path.to_string(),
                }),
                r => r.map_err(|e| e.into_core(path)),
            },
            r => r.map_err(|e| e.into_core(path)),
        }
    }

    async fn list(
        &self,
        prefix: &str,
        filters: &[Filter],
        page: &Page,
    ) -> Result<PageOut<NoteMeta>, CoreError> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT path, version, updated_at, updated_by, octet_length(body)::bigint AS size FROM notes WHERE TRUE",
        );
        Self::push_prefix_and_cursor(&mut qb, prefix, page.cursor.as_deref());
        for f in filters {
            qb.push(" AND ")
                .push(f.field.column())
                .push(" ")
                .push(f.op.sql())
                .push(" ");
            match &f.value {
                FilterValue::Text(v) => {
                    qb.push_bind(v.clone());
                    if f.op.is_pattern() {
                        qb.push(" ESCAPE '\\'");
                    }
                }
                FilterValue::Time(t) => {
                    qb.push_bind(*t);
                }
            }
        }
        qb.push(" ORDER BY path LIMIT ")
            .push_bind(i64::from(page.limit) + 1);
        let rows = qb.build().fetch_all(&self.pool).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(NoteMeta {
                path: NotePath::parse(r.try_get::<String, _>("path")?.as_str())?,
                version: r.try_get("version")?,
                updated_at: r.try_get("updated_at")?,
                updated_by: r.try_get("updated_by")?,
                size: r.try_get("size")?,
            });
        }
        Ok(PageOut::from_overfetch(out, page.limit, |m| {
            m.path.as_str()
        }))
    }

    async fn search(
        &self,
        text: &str,
        prefix: &str,
        page: &Page,
    ) -> Result<PageOut<SearchHit>, CoreError> {
        let pattern = format!("%{}%", escape_like(text));
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT path, updated_at, \
               substr(body, greatest(position(lower(",
        );
        qb.push_bind(text.to_string())
            .push(") in lower(body)) - 60, 1), 200) AS snippet FROM notes WHERE body ILIKE ")
            .push_bind(pattern)
            .push(" ESCAPE '\\'");
        Self::push_prefix_and_cursor(&mut qb, prefix, page.cursor.as_deref());
        qb.push(" ORDER BY path LIMIT ")
            .push_bind(i64::from(page.limit) + 1);
        let rows = qb.build().fetch_all(&self.pool).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(SearchHit {
                path: NotePath::parse(r.try_get::<String, _>("path")?.as_str())?,
                snippet: r.try_get("snippet")?,
                updated_at: r.try_get("updated_at")?,
            });
        }
        Ok(PageOut::from_overfetch(out, page.limit, |h| {
            h.path.as_str()
        }))
    }

    async fn history(&self, path: &NotePath, limit: u32) -> Result<Vec<Revision>, CoreError> {
        let rows: Vec<NoteRow> = sqlx::query_as(
            "SELECT path, body, version, updated_at, updated_by FROM revisions \
             WHERE path = $1 ORDER BY version DESC LIMIT $2",
        )
        .bind(path.as_str())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(Revision {
                    path: NotePath::parse(&r.path)?,
                    version: r.version,
                    body: r.body,
                    updated_at: r.updated_at,
                    updated_by: r.updated_by,
                })
            })
            .collect()
    }

    async fn history_meta(
        &self,
        path: &NotePath,
        limit: u32,
    ) -> Result<Vec<RevisionMeta>, CoreError> {
        let rows = sqlx::query(
            "SELECT path, version, updated_at, updated_by, octet_length(body)::bigint AS size \
             FROM revisions WHERE path = $1 ORDER BY version DESC LIMIT $2",
        )
        .bind(path.as_str())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(RevisionMeta {
                path: NotePath::parse(r.try_get::<String, _>("path")?.as_str())?,
                version: r.try_get("version")?,
                updated_at: r.try_get("updated_at")?,
                updated_by: r.try_get("updated_by")?,
                size: r.try_get("size")?,
            });
        }
        Ok(out)
    }

    async fn dump(&self, prefix: &str, page: &Page) -> Result<PageOut<DumpEntry>, CoreError> {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new("SELECT path, body FROM notes WHERE TRUE");
        Self::push_prefix_and_cursor(&mut qb, prefix, page.cursor.as_deref());
        qb.push(" ORDER BY path LIMIT ")
            .push_bind(i64::from(page.limit) + 1);
        let rows = qb.build().fetch_all(&self.pool).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(DumpEntry {
                path: NotePath::parse(r.try_get::<String, _>("path")?.as_str())?,
                body: r.try_get("body")?,
            });
        }
        Ok(PageOut::from_overfetch(out, page.limit, |d| {
            d.path.as_str()
        }))
    }
}
