//! [`NoteletMcp`]: the MCP server. The tools (listed in the generated
//! `notelet://guides/tools`), the guide resources and two transports.
//! Every tool is one [`NoteService`] call plus serialisation.
//! Writes (`note_put`, `note_append`, `note_delete`) answer a
//! [`WriteOutcome`] (path, version, updated_at, size in bytes, sha256,
//! changed) so the caller can verify the round trip without re-reading.
//! Domain errors come back as a failed tool result carrying
//! `{ "kind": ..., "message": ... }` (and `current_version` /
//! `expected_version` on a conflict) so an agent can act on them without
//! parsing prose. An `Internal` failure is logged at `error` with its cause
//! chain and a domain error at `debug`.
//!
//! The `updated_by` of a write is the actor of the credential that
//! authenticated the request ([`crate::auth`]): [`crate::auth::require_bearer`]
//! leaves it in the HTTP request, rmcp hands the request parts to the tool
//! through its extensions, and `note_put` / `note_append` / `note_delete`
//! read it per call (and record it as the `actor` field of their span). A
//! call with no such actor, i.e. over stdio or on a loopback `serve`
//! without tokens, writes as the configured fallback (`NOTELET_ACTOR`).

use std::sync::Arc;

use notelet_core::service::{FilterInput, History};
use notelet_core::{CoreError, NoteService, WriteOutcome};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Extensions, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler};
use serde::Deserialize;

use crate::auth::{Actor, TokenTable};
use crate::config::Config;
use crate::resources;

/// The server: a service handle and the fallback actor written to
/// `updated_by` when the request carries no authenticated actor.
#[derive(Clone)]
pub struct NoteletMcp {
    svc: Arc<NoteService>,
    actor: String,
}

// ---------------------------------------------------------------------------
// tool inputs
// ---------------------------------------------------------------------------

/// `note_get` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetParams {
    /// Note path, e.g. `projects/x/journal.md`.
    pub path: String,
    /// Return only the last N lines (at least 1). A single trailing newline is not counted.
    #[schemars(range(min = 1))]
    pub tail_lines: Option<u32>,
    /// Return at most this many bytes of the end (at least 1), starting on a line when possible.
    #[schemars(range(min = 1))]
    pub max_bytes: Option<u32>,
}

/// `note_put` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PutParams {
    /// Note path.
    pub path: String,
    /// The whole new body (Markdown).
    pub body: String,
    /// The version you last read. Omit to create a note that must not exist yet.
    pub expected_version: Option<i64>,
}

/// `note_delete` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteParams {
    /// Note path.
    pub path: String,
    /// The version you last read. Omit to delete whatever is there.
    pub expected_version: Option<i64>,
}

/// `note_append` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AppendParams {
    /// Note path; created if missing.
    pub path: String,
    /// Text to add at the end. A trailing newline is ensured.
    pub text: String,
}

/// One predicate of `note_list`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FilterParam {
    /// `path` | `updated_at` | `updated_by`
    pub field: String,
    /// `eq` | `ne` | `like` | `ilike` | `gt` | `lt` | `gte` | `lte`
    pub op: String,
    /// Compared as text; as RFC 3339 for `updated_at`; as a LIKE pattern for `like` / `ilike`.
    pub value: String,
}

/// `note_list` input.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ListParams {
    /// Only paths starting with this (e.g. `projects/x/`). Empty = all.
    pub prefix: Option<String>,
    /// Predicates, ANDed.
    pub filters: Option<Vec<FilterParam>>,
    /// Page size, 1..=1000 (default 100).
    pub limit: Option<u32>,
    /// `next_cursor` from the previous page (exclusive: the page starts
    /// strictly after that path).
    pub cursor: Option<String>,
}

/// `note_search` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Case-insensitive substring to find in bodies. `%` and `_` are literal.
    pub text: String,
    /// Only paths starting with this.
    pub prefix: Option<String>,
    /// Page size, 1..=1000 (default 100).
    pub limit: Option<u32>,
    /// `next_cursor` from the previous page (exclusive: the page starts
    /// strictly after that path).
    pub cursor: Option<String>,
}

/// `note_history` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    /// Note path.
    pub path: String,
    /// How many replaced or deleted bodies, newest first, 1..=50 (default 20).
    pub limit: Option<u32>,
    /// Include each revision's body (default true). `false` returns only
    /// path, version, updated_at, updated_by and size (bytes, UTF-8).
    pub include_body: Option<bool>,
}

/// `note_dump` input.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DumpParams {
    /// Only paths starting with this. Empty = everything.
    pub prefix: Option<String>,
    /// Page size, 1..=100 (default 20): every item carries a body.
    pub limit: Option<u32>,
    /// `next_cursor` from the previous page (exclusive: the page starts
    /// strictly after that path).
    pub cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// result helpers
// ---------------------------------------------------------------------------

fn ok_json<S: serde::Serialize>(value: &S) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

fn write_json(r: Result<WriteOutcome, CoreError>) -> Result<CallToolResult, McpError> {
    match r {
        Ok(w) => ok_json(&w),
        Err(e) => Ok(core_error(e)),
    }
}

/// A domain error as a failed tool result with a machine-readable body. A
/// conflict also carries `current_version` and `expected_version` (`null`
/// when the caller passed none, i.e. tried to create).
fn core_error(err: CoreError) -> CallToolResult {
    let kind = match &err {
        CoreError::NotFound(_) => "NotFound",
        CoreError::InvalidPath(_) | CoreError::InvalidFilter(_) | CoreError::InvalidTail(_) => {
            "Validation"
        }
        CoreError::VersionConflict { .. } => "Conflict",
        CoreError::Contended { .. } | CoreError::Storage(_) => "Internal",
        _ => "Internal",
    };
    if kind == "Internal" {
        tracing::error!(kind, error = %err.chain(), "tool failed");
    } else {
        tracing::debug!(kind, error = %err, "tool rejected");
    }
    let mut body = serde_json::json!({ "kind": kind, "message": err.chain().to_string() });
    if let (
        CoreError::VersionConflict {
            expected, current, ..
        },
        Some(obj),
    ) = (&err, body.as_object_mut())
    {
        obj.insert("current_version".into(), serde_json::Value::from(*current));
        obj.insert("expected_version".into(), serde_json::json!(expected));
    }
    CallToolResult::error(vec![ContentBlock::text(body.to_string())])
}

fn filters(input: Option<Vec<FilterParam>>) -> Vec<FilterInput> {
    input
        .unwrap_or_default()
        .into_iter()
        .map(|f| FilterInput {
            field: f.field,
            op: f.op,
            value: f.value,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// tools
// ---------------------------------------------------------------------------

#[tool_router]
impl NoteletMcp {
    /// Build the server over a service.
    pub fn new(svc: Arc<NoteService>, actor: String) -> Self {
        Self { svc, actor }
    }

    /// The actor of this call: the one [`crate::auth::require_bearer`] put in
    /// the HTTP request (rmcp keeps the request parts in the extensions), or
    /// the fallback when there is none. Recorded on the current span.
    fn actor_of(&self, ext: &Extensions) -> String {
        let actor = ext
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Actor>())
            .map_or(self.actor.as_str(), Actor::as_str);
        tracing::Span::current().record("actor", actor);
        actor.to_string()
    }

    #[tool(
        description = "Read one note: path, body (Markdown), version, updated_at, updated_by, size, truncated. Optional tail_lines (>= 1) returns only the last N lines and max_bytes (>= 1) caps the bytes returned, cut on a line start where possible; use them to read the end of a long journal. size is the whole body in bytes (as note_list reports it) and truncated=true means body is only the end of the note. Keep the version if you intend to note_put, but NEVER note_put a body returned with truncated=true: it would replace the whole note with the fragment (to add to a journal use note_append)."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_get", path = %p.path))]
    async fn note_get(
        &self,
        Parameters(p): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.svc.read(&p.path, p.tail_lines, p.max_bytes).await {
            Ok(n) => ok_json(&n),
            Err(e) => Ok(core_error(e)),
        }
    }

    #[tool(
        description = "Create or replace a note's whole body with optimistic locking. Omit expected_version to create (fails with kind=Conflict if it exists); pass the version you read to replace (fails with kind=Conflict and current_version if someone wrote in between — re-read, merge, retry). The replaced body is kept in history. A body identical to the current one changes nothing (changed=false). Returns path, version, updated_at, size (bytes, UTF-8), sha256, changed."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_put", path = %p.path, actor = tracing::field::Empty))]
    async fn note_put(
        &self,
        Parameters(p): Parameters<PutParams>,
        ext: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.actor_of(&ext);
        write_json(
            self.svc
                .put(&p.path, &p.body, p.expected_version, &actor)
                .await,
        )
    }

    #[tool(
        description = "Append text to the end of a note (created if missing), on its own line. No version needed: an append cannot overwrite anyone. Use this for journals; it is also the way to upload a large body in chunks: the first append creates the note. Returns path, version, updated_at, size (bytes, UTF-8), sha256, changed."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_append", path = %p.path, actor = tracing::field::Empty))]
    async fn note_append(
        &self,
        Parameters(p): Parameters<AppendParams>,
        ext: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.actor_of(&ext);
        write_json(self.svc.append(&p.path, &p.text, &actor).await)
    }

    #[tool(
        description = "Remove a note. Its body stays readable through note_history as the last revision. Pass expected_version to refuse if someone wrote in between; omit it to delete whatever is there. Answers the removed note's version, size and sha256."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_delete", path = %p.path, actor = tracing::field::Empty))]
    async fn note_delete(
        &self,
        Parameters(p): Parameters<DeleteParams>,
        ext: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.actor_of(&ext);
        write_json(self.svc.delete(&p.path, p.expected_version, &actor).await)
    }

    #[tool(
        description = "List notes (path, version, updated_at, updated_by, size in bytes (UTF-8); no body) under a prefix, filtered by ANDed predicates {field, op, value}. Fields: path, updated_at, updated_by. Ops: eq ne like ilike gt lt gte lte. Ordered by path; pass next_cursor as cursor to page (exclusive: the page starts strictly after that path). Default 100, max 1000. See notelet://guides/filters."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_list", prefix = ?p.prefix))]
    async fn note_list(
        &self,
        Parameters(p): Parameters<ListParams>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .svc
            .list(p.prefix.as_deref(), &filters(p.filters), p.limit, p.cursor)
            .await
        {
            Ok(page) => ok_json(&page),
            Err(e) => Ok(core_error(e)),
        }
    }

    #[tool(
        description = "Find notes whose body contains the text (case-insensitive substring, no wildcards) under a prefix. Returns path, a snippet around the first hit, updated_at. Ordered by path; paged by next_cursor (exclusive: the page starts strictly after that path)."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_search", prefix = ?p.prefix))]
    async fn note_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .svc
            .search(&p.text, p.prefix.as_deref(), p.limit, p.cursor)
            .await
        {
            Ok(page) => ok_json(&page),
            Err(e) => Ok(core_error(e)),
        }
    }

    #[tool(
        description = "Bodies that note_put replaced or note_delete removed for this path, newest first (default 20, max 50). Appends do not create history. include_body=false returns only path, version, updated_at, updated_by, size (bytes, UTF-8)."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_history", path = %p.path))]
    async fn note_history(
        &self,
        Parameters(p): Parameters<HistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .svc
            .history(&p.path, p.limit, p.include_body.unwrap_or(true))
            .await
        {
            Ok(History::Full(revs)) => ok_json(&revs),
            Ok(History::Meta(revs)) => ok_json(&revs),
            Err(e) => Ok(core_error(e)),
        }
    }

    #[tool(
        description = "Paths and full bodies under a prefix, ordered by path and paged: default 20, max 100 (bodies are heavy); pass next_cursor as cursor (exclusive). For backups, exports, or leaving notelet: the result is a file tree."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_dump", prefix = ?p.prefix))]
    async fn note_dump(
        &self,
        Parameters(p): Parameters<DumpParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.svc.dump(p.prefix.as_deref(), p.limit, p.cursor).await {
            Ok(page) => ok_json(&page),
            Err(e) => Ok(core_error(e)),
        }
    }
}

#[tool_handler]
impl ServerHandler for NoteletMcp {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        );
        info.server_info.name = "notelet".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.instructions = Some(
            "notelet is a thin store of Markdown notes addressed by path \
             (projects/<repo>/journal.md, tasks/<task>/<file>.md, lists/<name>.md). \
             Read notelet://guides/onboarding first; notelet://guides/tools lists every tool."
                .into(),
        );
        info
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(resources::list_all())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        resources::read(&request.uri)
            .map(Into::into)
            .ok_or_else(|| {
                McpError::resource_not_found(format!("unknown resource uri: {}", request.uri), None)
            })
    }
}

// ---------------------------------------------------------------------------
// transports
// ---------------------------------------------------------------------------

/// Serve over stdio for one local client.
pub async fn run_stdio(svc: Arc<NoteService>, actor: String) -> anyhow::Result<()> {
    use rmcp::{transport::stdio, ServiceExt};
    let server = NoteletMcp::new(svc, actor);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Serve over streamable HTTP at `/mcp`, with `/healthz` open, a listed
/// bearer token required on `/mcp` when any token is configured. The
/// read-only browser view ([`crate::ui`]) is merged in under `/ui`, behind
/// HTTP Basic with the same tokens.
pub async fn run_http(svc: Arc<NoteService>, cfg: Config) -> anyhow::Result<()> {
    let router = build_router(svc, &cfg);
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(
        bind = %cfg.bind,
        auth = if cfg.tokens.is_empty() { "none (loopback)" } else { "bearer" },
        tokens = cfg.tokens.len(),
        "serving notelet MCP at /mcp, read-only view at /ui/"
    );
    axum::serve(listener, router).await?;
    Ok(())
}

/// The full HTTP router `run_http` serves: `/healthz` (open), `/mcp`
/// (bearer when tokens are set, which also selects the write actor) and
/// `/ui` (Basic when tokens are set, loopback `Host` only otherwise). Split
/// out so tests can drive it without binding a socket.
pub(crate) fn build_router(svc: Arc<NoteService>, cfg: &Config) -> axum::Router {
    let actor = cfg.actor.clone();
    let ui = crate::ui::router(svc.clone(), cfg.tokens.clone());
    let mut http_config = StreamableHttpServerConfig::default();
    http_config.json_response = true;
    if !cfg.bind.ip().is_loopback() {
        // bearer replaces the loopback-only Host check for hosted binds
        http_config.allowed_hosts.clear();
    }
    let mcp = StreamableHttpService::new(
        move || Ok(NoteletMcp::new(svc.clone(), actor.clone())),
        Arc::new(LocalSessionManager::default()),
        http_config,
    );

    let mut mcp_router = axum::Router::new().nest_service("/mcp", mcp);
    if !cfg.tokens.is_empty() {
        let tokens: Arc<TokenTable> = Arc::new(cfg.tokens.clone());
        mcp_router = mcp_router.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let tokens = Arc::clone(&tokens);
                async move { crate::auth::require_bearer(tokens, req, next).await }
            },
        ));
    }
    axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(mcp_router)
        .merge(ui)
}

#[cfg(test)]
mod tests {
    use super::*;
    use notelet_core::MemoryNoteRepo;

    fn server() -> NoteletMcp {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        NoteletMcp::new(svc, "test".into())
    }

    #[test]
    fn storage_error_is_internal_and_carries_the_cause() {
        let r = core_error(CoreError::Storage(Box::new(std::io::Error::other(
            "db down",
        ))));
        let body: serde_json::Value = serde_json::from_str(&text_of(&r)).unwrap();
        assert_eq!(body["kind"], "Internal");
        assert!(body["message"].as_str().unwrap().contains("db down"));
    }

    fn text_of(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("")
    }

    #[tokio::test]
    async fn put_conflict_is_a_machine_readable_error() {
        let s = server();
        let ok = s
            .note_put(
                Parameters(PutParams {
                    path: "projects/x/a.md".into(),
                    body: "one".into(),
                    expected_version: None,
                }),
                Extensions::default(),
            )
            .await
            .unwrap();
        assert_ne!(ok.is_error, Some(true));

        let conflict = s
            .note_put(
                Parameters(PutParams {
                    path: "projects/x/a.md".into(),
                    body: "two".into(),
                    expected_version: Some(9),
                }),
                Extensions::default(),
            )
            .await
            .unwrap();
        assert_eq!(conflict.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&text_of(&conflict)).unwrap();
        assert_eq!(body["kind"], "Conflict");
        assert_eq!(body["current_version"], 1);
        assert_eq!(body["expected_version"], 9);

        let create_again = put_note(&s, "projects/x/a.md", "two", None).await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&text_of(&create_again)).unwrap();
        assert_eq!(body["kind"], "Conflict");
        assert!(body["expected_version"].is_null());
    }

    async fn put_note(
        s: &NoteletMcp,
        path: &str,
        body: &str,
        expected_version: Option<i64>,
    ) -> Result<CallToolResult, McpError> {
        s.note_put(
            Parameters(PutParams {
                path: path.into(),
                body: body.into(),
                expected_version,
            }),
            Extensions::default(),
        )
        .await
    }

    fn json_of(r: &CallToolResult) -> serde_json::Value {
        assert_ne!(r.is_error, Some(true), "{}", text_of(r));
        let c = r.content.first().expect("content");
        match c.as_text() {
            Some(t) => serde_json::from_str(&t.text).unwrap(),
            None => serde_json::to_value(c).unwrap(),
        }
    }

    #[tokio::test]
    async fn put_reports_sha256_and_changed() {
        let s = server();
        let w = json_of(&put_note(&s, "n/a.md", "héllo", None).await.unwrap());
        assert_eq!(w["version"], 1);
        assert_eq!(w["changed"], true);
        assert_eq!(w["size"], 6);
        assert_eq!(
            w["sha256"],
            notelet_core::domain::sha256_hex("héllo").as_str()
        );
    }

    #[tokio::test]
    async fn put_same_body_reports_unchanged() {
        let s = server();
        put_note(&s, "n/a.md", "same", None).await.unwrap();
        let w = json_of(&put_note(&s, "n/a.md", "same", Some(1)).await.unwrap());
        assert_eq!(w["changed"], false);
        assert_eq!(w["version"], 1);
    }

    #[tokio::test]
    async fn delete_then_get_is_not_found_and_history_keeps_it() {
        let s = server();
        put_note(&s, "n/d.md", "bye", None).await.unwrap();
        let w = json_of(
            &s.note_delete(
                Parameters(DeleteParams {
                    path: "n/d.md".into(),
                    expected_version: Some(1),
                }),
                Extensions::default(),
            )
            .await
            .unwrap(),
        );
        assert_eq!(w["version"], 1);
        let r = s
            .note_get(Parameters(GetParams {
                path: "n/d.md".into(),
                tail_lines: None,
                max_bytes: None,
            }))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&text_of(&r)).unwrap();
        assert_eq!(body["kind"], "NotFound");

        let h = json_of(
            &s.note_history(Parameters(HistoryParams {
                path: "n/d.md".into(),
                limit: None,
                include_body: Some(false),
            }))
            .await
            .unwrap(),
        );
        assert_eq!(h[0]["size"], 3);
        assert!(h[0].get("body").is_none());
    }

    fn get(path: &str, tail_lines: Option<u32>, max_bytes: Option<u32>) -> Parameters<GetParams> {
        Parameters(GetParams {
            path: path.into(),
            tail_lines,
            max_bytes,
        })
    }

    #[tokio::test]
    async fn get_tail_through_the_tool() {
        let s = server();
        put_note(&s, "n/j.md", "l1\nl2\nl3\n", None).await.unwrap();

        let whole = json_of(&s.note_get(get("n/j.md", None, None)).await.unwrap());
        assert_eq!(whole["body"], "l1\nl2\nl3\n");
        assert_eq!(whole["size"], 9);
        assert_eq!(whole["truncated"], false);

        let r = s.note_get(get("n/j.md", Some(2), None)).await.unwrap();
        let t = json_of(&r);
        assert_eq!(t["body"], "l2\nl3\n");
        assert_eq!(t["size"], 9);
        assert_eq!(t["truncated"], true);
        assert_eq!(t["version"], 1);

        let c = json_of(&s.note_get(get("n/j.md", Some(3), Some(4))).await.unwrap());
        assert_eq!(c["body"], "l3\n");
        assert_eq!(c["truncated"], true);
    }

    #[tokio::test]
    async fn get_tail_zero_is_validation() {
        let s = server();
        put_note(&s, "n/j.md", "x\n", None).await.unwrap();
        for (l, m) in [(Some(0), None), (None, Some(0))] {
            let r = s.note_get(get("n/j.md", l, m)).await.unwrap();
            assert_eq!(r.is_error, Some(true));
            let body: serde_json::Value = serde_json::from_str(&text_of(&r)).unwrap();
            assert_eq!(body["kind"], "Validation");
        }
    }

    #[tokio::test]
    async fn invalid_path_is_validation() {
        let s = server();
        let r = s
            .note_get(Parameters(GetParams {
                path: "../etc".into(),
                tail_lines: None,
                max_bytes: None,
            }))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&text_of(&r)).unwrap();
        assert_eq!(body["kind"], "Validation");
    }

    #[tokio::test]
    async fn append_then_list_and_search() {
        let s = server();
        for line in ["alpha", "beta"] {
            s.note_append(
                Parameters(AppendParams {
                    path: "projects/x/journal.md".into(),
                    text: line.into(),
                }),
                Extensions::default(),
            )
            .await
            .unwrap();
        }
        let list = s
            .note_list(Parameters(ListParams {
                prefix: Some("projects/".into()),
                ..Default::default()
            }))
            .await
            .unwrap();
        assert_ne!(list.is_error, Some(true));
        let found = s
            .note_search(Parameters(SearchParams {
                text: "BETA".into(),
                prefix: None,
                limit: None,
                cursor: None,
            }))
            .await
            .unwrap();
        assert!(text_of(&found).contains("projects/x/journal.md"));
    }

    /// Header of the generated guide; free of any count on purpose.
    const TOOLS_GUIDE_HEAD: &str = "# Tools\n\nEvery tool the server offers, generated from the tool router. Run `just gen-docs` after changing a tool.\n\n";

    /// The first sentence of a tool description, as one table cell. Cuts at
    /// the first ". " outside backticks that is followed by an uppercase
    /// letter, or at the final ".". Newlines become spaces and `|` is escaped.
    fn first_sentence(desc: &str) -> String {
        let flat = desc.split_whitespace().collect::<Vec<_>>().join(" ");
        let chars: Vec<(usize, char)> = flat.char_indices().collect();
        let mut in_code = false;
        let mut end = flat.len();
        for (i, &(pos, c)) in chars.iter().enumerate() {
            if c == '`' {
                in_code = !in_code;
            } else if c == '.' && !in_code {
                match (chars.get(i + 1), chars.get(i + 2)) {
                    (None, _) => {
                        end = pos + 1;
                        break;
                    }
                    (Some((_, ' ')), Some((_, n))) if n.is_uppercase() => {
                        end = pos + 1;
                        break;
                    }
                    _ => {}
                }
            }
        }
        flat[..end].trim().replace('|', "\\|")
    }

    #[test]
    fn first_sentence_cuts_safely() {
        assert_eq!(first_sentence("Read it. Then more."), "Read it.");
        assert_eq!(
            first_sentence("Use e.g. a path. Then more."),
            "Use e.g. a path."
        );
        assert_eq!(first_sentence("Run `a. B` now. Next."), "Run `a. B` now.");
        assert_eq!(first_sentence("Line one.\nLine two."), "Line one.");
        assert_eq!(first_sentence("Line one.\r\nLine two."), "Line one.");
        assert_eq!(first_sentence("A.\n\nB."), "A.");
        assert_eq!(first_sentence("A.  B."), "A.");
        assert_eq!(first_sentence("No end"), "No end");
        assert_eq!(first_sentence("a | b. C"), "a \\| b.");
    }

    /// Markdown table of the tool router: one row per tool, sorted by name.
    /// The "local only" column is empty until a tool has such parameters.
    fn render_tool_table() -> String {
        let mut tools = NoteletMcp::tool_router().list_all();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let mut out = String::from("| tool | does | local only |\n|---|---|---|\n");
        for t in tools {
            let desc = t.description.as_deref().unwrap_or("");
            let cell = first_sentence(desc);
            assert_eq!(
                cell.matches('`').count() % 2,
                0,
                "unbalanced backticks in the first sentence of {}",
                t.name
            );
            out.push_str(&format!("| `{}` | {} | |\n", t.name, cell));
        }
        out
    }

    /// Replace each file with its new content: write every `<file>.tmp`
    /// first, then rename each onto its file. On any failure the `.tmp`
    /// files are removed and the error returned.
    fn replace_files(files: &[(&std::path::Path, &str)]) -> std::io::Result<()> {
        let tmp_of = |path: &std::path::Path| {
            let mut name = path.file_name().expect("file name").to_os_string();
            name.push(".tmp");
            path.with_file_name(name)
        };
        let result = (|| {
            for (path, content) in files {
                std::fs::write(tmp_of(path), content)?;
            }
            for (path, _) in files {
                std::fs::rename(tmp_of(path), path)?;
            }
            Ok(())
        })();
        if result.is_err() {
            for (path, _) in files {
                let _ = std::fs::remove_file(tmp_of(path));
            }
        }
        result
    }

    /// Golden test: the guide file and the README block between the
    /// markers equal the rendered table. `NOTELET_UPDATE_DOCS=1` rewrites
    /// them; set it only through `just gen-docs`, never in a normal run.
    #[test]
    fn tool_docs_match_router() {
        const BEGIN: &str = "<!-- tools:begin -->";
        const END: &str = "<!-- tools:end -->";
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let guide_path = root.join("docs/guides/tools.md");
        let readme_path = root.join("../../README.md");
        let table = render_tool_table();
        let guide = format!("{TOOLS_GUIDE_HEAD}{table}");
        let readme = std::fs::read_to_string(&readme_path)
            .expect("read README.md")
            .replace("\r\n", "\n");
        assert_eq!(
            readme.matches(BEGIN).count(),
            1,
            "README.md needs exactly one begin marker"
        );
        assert_eq!(
            readme.matches(END).count(),
            1,
            "README.md needs exactly one end marker"
        );
        let b = readme.find(BEGIN).unwrap() + BEGIN.len();
        let e = readme.find(END).unwrap();
        assert!(b <= e, "README.md markers are out of order");
        let new_readme = format!("{}\n{}{}", &readme[..b], table, &readme[e..]);

        if std::env::var("NOTELET_UPDATE_DOCS").as_deref() == Ok("1") {
            replace_files(&[(&guide_path, &guide), (&readme_path, &new_readme)])
                .expect("rewrite the generated docs");
            eprintln!(
                "rewrote {} and {}",
                guide_path.display(),
                readme_path.display()
            );
            return;
        }
        let on_disk = std::fs::read_to_string(&guide_path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert_eq!(
            on_disk, guide,
            "docs/guides/tools.md is out of date; run `just gen-docs`"
        );
        assert_eq!(
            readme, new_readme,
            "the tool table in README.md is out of date; run `just gen-docs`"
        );
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use crate::auth::digest;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use notelet_core::MemoryNoteRepo;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    const TOKEN: &str = "secret";
    // base64("anyone:secret")
    const BASIC_GOOD: &str = "Basic YW55b25lOnNlY3JldA==";

    fn config(entries: &[(&str, &str)]) -> Config {
        let mut tokens = TokenTable::default();
        for (token, actor) in entries {
            assert!(tokens.insert(digest(token), Actor::new(actor)));
        }
        Config {
            tokens,
            bind: "127.0.0.1:0".parse().unwrap(),
            actor: "fallback".into(),
        }
    }

    fn app_with(entries: &[(&str, &str)]) -> (axum::Router, Arc<NoteService>) {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        (build_router(svc.clone(), &config(entries)), svc)
    }

    fn app() -> axum::Router {
        app_with(&[(TOKEN, "test")]).0
    }

    async fn status(method: &str, uri: &str, auth: Option<&str>) -> StatusCode {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost");
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        app()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn healthz_needs_no_credentials() {
        assert_eq!(status("GET", "/healthz", None).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn basic_does_not_satisfy_the_mcp_bearer_check() {
        assert_eq!(
            status("POST", "/mcp", Some(BASIC_GOOD)).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(status("POST", "/mcp", None).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bearer_does_not_satisfy_the_ui_basic_check() {
        let bearer = format!("Bearer {TOKEN}");
        assert_eq!(
            status("GET", "/ui/", Some(&bearer)).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status("GET", "/ui/", Some(BASIC_GOOD)).await,
            StatusCode::OK
        );
    }

    /// One JSON-RPC POST to `/mcp` with `token`; answers the status, the
    /// session id header if any, and the JSON body (`null` when empty).
    async fn rpc(
        app: &axum::Router,
        token: &str,
        session: Option<&str>,
        body: Value,
    ) -> (StatusCode, Option<String>, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        if let Some(sid) = session {
            req = req.header("mcp-session-id", sid);
            req = req.header("mcp-protocol-version", "2025-11-25");
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let sid = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, sid, value)
    }

    /// Open an MCP session with `token` (initialize + initialized).
    async fn open_session(app: &axum::Router, token: &str) -> String {
        let (st, sid, init) = rpc(
            app,
            token,
            None,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{init}");
        let sid = sid.expect("session id");
        let (st, _, _) = rpc(
            app,
            token,
            Some(&sid),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED);
        sid
    }

    async fn call_tool(
        app: &axum::Router,
        token: &str,
        sid: &str,
        id: u64,
        name: &str,
        args: Value,
    ) -> Value {
        let (st, _, v) = rpc(
            app,
            token,
            Some(sid),
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                   "params": {"name": name, "arguments": args}}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_ne!(v["result"]["isError"], true, "{v}");
        v
    }

    async fn updated_by(svc: &NoteService, path: &str) -> String {
        svc.read(path, None, None).await.unwrap().updated_by
    }

    #[tokio::test]
    async fn updated_by_is_the_actor_of_the_token_on_each_write() {
        let (app, svc) = app_with(&[("token-a", "alice"), ("token-b", "bob")]);
        let a = open_session(&app, "token-a").await;
        let b = open_session(&app, "token-b").await;

        call_tool(
            &app,
            "token-a",
            &a,
            2,
            "note_put",
            json!({"path": "n/put.md", "body": "x"}),
        )
        .await;
        call_tool(
            &app,
            "token-b",
            &b,
            2,
            "note_put",
            json!({"path": "n/put-b.md", "body": "x"}),
        )
        .await;
        assert_eq!(updated_by(&svc, "n/put.md").await, "alice");
        assert_eq!(updated_by(&svc, "n/put-b.md").await, "bob");

        // the actor follows the credential of each request, not the session:
        // bob appends to alice's note over alice's session id.
        call_tool(
            &app,
            "token-b",
            &a,
            3,
            "note_append",
            json!({"path": "n/put.md", "text": "y"}),
        )
        .await;
        assert_eq!(updated_by(&svc, "n/put.md").await, "bob");
        call_tool(
            &app,
            "token-a",
            &a,
            4,
            "note_append",
            json!({"path": "n/app.md", "text": "y"}),
        )
        .await;
        assert_eq!(updated_by(&svc, "n/app.md").await, "alice");

        // core does not record who deleted: the kept revision carries the
        // last writer (alice); delete's actor is only on the span (see below)
        call_tool(
            &app,
            "token-b",
            &b,
            3,
            "note_delete",
            json!({"path": "n/app.md"}),
        )
        .await;
        let hist = svc.history("n/app.md", None, false).await.unwrap();
        let notelet_core::service::History::Meta(revs) = hist else {
            panic!("meta history expected")
        };
        assert_eq!(revs[0].updated_by, "alice");
    }

    thread_local! {
        /// Actors recorded on spans by the current thread. Per thread, so
        /// concurrently running tests never see each other's spans.
        static ACTORS: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Records the `actor` field of every span the current thread records.
    struct ActorSpy;

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ActorSpy {
        fn on_record(
            &self,
            _id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct V;
            impl tracing::field::Visit for V {
                fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                    if f.name() == "actor" {
                        ACTORS.with(|a| a.borrow_mut().push(v.to_string()));
                    }
                }
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            values.record(&mut V);
        }
    }

    /// Installs the spy as the process-wide subscriber, once.
    ///
    /// A scoped `set_default` subscriber is not enough here. Tracing caches
    /// each callsite's interest the first time any thread reaches it. While
    /// only one dispatcher is registered (tracing-core `Rebuilder::JustOne`),
    /// that cache is built from the reaching thread's default only. Another
    /// test that reaches `note_delete` first with no subscriber caches
    /// "never", so this thread's span is disabled and `actor` is never
    /// recorded. A global subscriber is every thread's default, so no thread
    /// can cache "never". A later scoped subscriber elsewhere makes tracing
    /// combine interests across all dispatchers ("sometimes"), and this
    /// thread still uses the global one, so adding one does not reopen the
    /// race.
    ///
    /// `ACTORS` is thread-local and recording happens on the calling thread,
    /// so the call under test must run on the test thread (a current_thread
    /// `#[tokio::test]`, not inside `tokio::spawn`).
    ///
    /// Do not install a `Dispatch::none()` or `from_static` scoped default in
    /// this binary: it raises the scoped count without registering and would
    /// bring back the `JustOne` path.
    fn install_actor_spy() {
        use tracing_subscriber::layer::SubscriberExt;
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(ActorSpy))
                .expect("no other test installs a global subscriber");
        });
    }

    #[tokio::test]
    async fn delete_actor_is_recorded_on_the_span() {
        install_actor_spy();
        ACTORS.with(|a| a.borrow_mut().clear());
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        svc.put("n/d.md", "x", None, "alice").await.unwrap();
        let s = NoteletMcp::new(svc, "fallback".into());
        let mut ext = Extensions::default();
        ext.insert(
            Request::builder()
                .extension(Actor::new("bob"))
                .body(())
                .unwrap()
                .into_parts()
                .0,
        );
        s.note_delete(
            Parameters(DeleteParams {
                path: "n/d.md".into(),
                expected_version: None,
            }),
            ext,
        )
        .await
        .unwrap();
        assert_eq!(ACTORS.with(|a| a.borrow().clone()), vec!["bob".to_string()]);
    }

    #[tokio::test]
    async fn unknown_token_is_401_and_writes_nothing() {
        let (app, svc) = app_with(&[("token-a", "alice")]);
        let sid = open_session(&app, "token-a").await;
        let (st, _, _) = rpc(
            &app,
            "token-evil",
            Some(&sid),
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                   "params": {"name": "note_put",
                              "arguments": {"path": "n/x.md", "body": "x"}}}),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert!(svc.read("n/x.md", None, None).await.is_err());
    }

    #[tokio::test]
    async fn legacy_single_token_writes_as_the_fallback_actor() {
        // what `from_env` builds for NOTELET_TOKEN alone (see config tests)
        let (app, svc) = app_with(&[("legacy", "fallback")]);
        let sid = open_session(&app, "legacy").await;
        call_tool(
            &app,
            "legacy",
            &sid,
            2,
            "note_put",
            json!({"path": "n/l.md", "body": "x"}),
        )
        .await;
        assert_eq!(updated_by(&svc, "n/l.md").await, "fallback");
    }

    #[tokio::test]
    async fn without_http_parts_the_fallback_actor_is_used() {
        // stdio, and a tokenless loopback `serve`: nothing in the extensions
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        let s = NoteletMcp::new(svc.clone(), "fallback".into());
        s.note_put(
            Parameters(PutParams {
                path: "n/s.md".into(),
                body: "x".into(),
                expected_version: None,
            }),
            Extensions::default(),
        )
        .await
        .unwrap();
        assert_eq!(updated_by(&svc, "n/s.md").await, "fallback");

        // parts present but no actor (the middleware did not run)
        let mut ext = Extensions::default();
        ext.insert(Request::builder().body(()).unwrap().into_parts().0);
        s.note_append(
            Parameters(AppendParams {
                path: "n/s.md".into(),
                text: "y".into(),
            }),
            ext,
        )
        .await
        .unwrap();
        assert_eq!(updated_by(&svc, "n/s.md").await, "fallback");
    }
}
