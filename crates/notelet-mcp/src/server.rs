//! [`NoteletMcp`]: the MCP server. Eight tools, three guide resources, two
//! transports. Every tool is one [`NoteService`] call plus serialisation.
//! Writes (`note_put`, `note_append`, `note_delete`) answer a
//! [`WriteOutcome`] (path, version, updated_at, size in bytes, sha256,
//! changed) so the caller can verify the round trip without re-reading.
//! Domain errors come back as a failed tool result carrying
//! `{ "kind": ..., "message": ... }` (and `current_version` /
//! `expected_version` on a conflict) so an agent can act on them without
//! parsing prose.

use std::sync::Arc;

use notelet_core::service::{FilterInput, History};
use notelet_core::{CoreError, NoteService, WriteOutcome};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler};
use serde::Deserialize;

use crate::config::Config;
use crate::resources;

/// The server: a service handle and the actor name written to `updated_by`.
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
        CoreError::Storage(_) => "Internal",
    };
    let mut body = serde_json::json!({ "kind": kind, "message": err.to_string() });
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

    #[tool(
        description = "Read one note: path, body (Markdown), version, updated_at, updated_by, size, truncated. Optional tail_lines (>= 1) returns only the last N lines and max_bytes (>= 1) caps the bytes returned, cut on a line start where possible; use them to read the end of a long journal. size is the whole body in bytes (as note_list reports it) and truncated=true means body is only the end of the note. Keep the version if you intend to note_put, but NEVER note_put a body returned with truncated=true: it would replace the whole note with the fragment (to add to a journal use note_append)."
    )]
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
    async fn note_put(
        &self,
        Parameters(p): Parameters<PutParams>,
    ) -> Result<CallToolResult, McpError> {
        write_json(
            self.svc
                .put(&p.path, &p.body, p.expected_version, &self.actor)
                .await,
        )
    }

    #[tool(
        description = "Append text to the end of a note (created if missing), on its own line. No version needed: an append cannot overwrite anyone. Use this for journals; it is also the way to upload a large body in chunks: the first append creates the note. Returns path, version, updated_at, size (bytes, UTF-8), sha256, changed."
    )]
    async fn note_append(
        &self,
        Parameters(p): Parameters<AppendParams>,
    ) -> Result<CallToolResult, McpError> {
        write_json(self.svc.append(&p.path, &p.text, &self.actor).await)
    }

    #[tool(
        description = "Remove a note. Its body stays readable through note_history as the last revision. Pass expected_version to refuse if someone wrote in between; omit it to delete whatever is there. Answers the removed note's version, size and sha256."
    )]
    async fn note_delete(
        &self,
        Parameters(p): Parameters<DeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        write_json(
            self.svc
                .delete(&p.path, p.expected_version, &self.actor)
                .await,
        )
    }

    #[tool(
        description = "List notes (path, version, updated_at, updated_by, size in bytes (UTF-8); no body) under a prefix, filtered by ANDed predicates {field, op, value}. Fields: path, updated_at, updated_by. Ops: eq ne like ilike gt lt gte lte. Ordered by path; pass next_cursor as cursor to page (exclusive: the page starts strictly after that path). Default 100, max 1000. See notelet://guides/filters."
    )]
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
             note_append for journals, note_get + note_put (with expected_version) \
             for documents, note_delete to remove, note_list / note_search to find, \
             note_history to recover, note_dump to export. Read notelet://guides/onboarding first."
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

/// Serve over streamable HTTP at `/mcp`, with `/healthz` open, bearer
/// required on `/mcp` when a token is configured. The read-only browser view
/// ([`crate::ui`]) is merged in under `/ui`, behind HTTP Basic with the same
/// token.
pub async fn run_http(svc: Arc<NoteService>, cfg: Config) -> anyhow::Result<()> {
    let router = build_router(svc, &cfg);
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(
        bind = %cfg.bind,
        auth = if cfg.token.is_some() { "bearer" } else { "none (loopback)" },
        "serving notelet MCP at /mcp, read-only view at /ui/"
    );
    axum::serve(listener, router).await?;
    Ok(())
}

/// The full HTTP router `run_http` serves: `/healthz` (open), `/mcp`
/// (bearer when a token is set) and `/ui` (Basic when a token is set,
/// loopback `Host` only otherwise). Split out so tests can drive it without
/// binding a socket.
pub(crate) fn build_router(svc: Arc<NoteService>, cfg: &Config) -> axum::Router {
    let actor = cfg.actor.clone();
    let ui = crate::ui::router(svc.clone(), cfg.token.clone());
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
    if let Some(token) = cfg.token.clone() {
        let token: Arc<str> = Arc::from(token);
        mcp_router = mcp_router.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let token = Arc::clone(&token);
                async move { crate::auth::require_bearer(token, req, next).await }
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
            .note_put(Parameters(PutParams {
                path: "projects/x/a.md".into(),
                body: "one".into(),
                expected_version: None,
            }))
            .await
            .unwrap();
        assert_ne!(ok.is_error, Some(true));

        let conflict = s
            .note_put(Parameters(PutParams {
                path: "projects/x/a.md".into(),
                body: "two".into(),
                expected_version: Some(9),
            }))
            .await
            .unwrap();
        assert_eq!(conflict.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&text_of(&conflict)).unwrap();
        assert_eq!(body["kind"], "Conflict");
        assert_eq!(body["current_version"], 1);
        assert_eq!(body["expected_version"], 9);

        let create_again = s
            .note_put(put("projects/x/a.md", "two", None))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&text_of(&create_again)).unwrap();
        assert_eq!(body["kind"], "Conflict");
        assert!(body["expected_version"].is_null());
    }

    fn put(path: &str, body: &str, expected_version: Option<i64>) -> Parameters<PutParams> {
        Parameters(PutParams {
            path: path.into(),
            body: body.into(),
            expected_version,
        })
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
        let w = json_of(&s.note_put(put("n/a.md", "héllo", None)).await.unwrap());
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
        s.note_put(put("n/a.md", "same", None)).await.unwrap();
        let w = json_of(&s.note_put(put("n/a.md", "same", Some(1))).await.unwrap());
        assert_eq!(w["changed"], false);
        assert_eq!(w["version"], 1);
    }

    #[tokio::test]
    async fn delete_then_get_is_not_found_and_history_keeps_it() {
        let s = server();
        s.note_put(put("n/d.md", "bye", None)).await.unwrap();
        let w = json_of(
            &s.note_delete(Parameters(DeleteParams {
                path: "n/d.md".into(),
                expected_version: Some(1),
            }))
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
        s.note_put(put("n/j.md", "l1\nl2\nl3\n", None))
            .await
            .unwrap();

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
        s.note_put(put("n/j.md", "x\n", None)).await.unwrap();
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
            s.note_append(Parameters(AppendParams {
                path: "projects/x/journal.md".into(),
                text: line.into(),
            }))
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

    #[test]
    fn tool_router_lists_eight_tools() {
        let names: Vec<String> = NoteletMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for n in [
            "note_get",
            "note_put",
            "note_append",
            "note_delete",
            "note_list",
            "note_search",
            "note_history",
            "note_dump",
        ] {
            assert!(names.contains(&n.to_string()), "{n} missing from {names:?}");
        }
        assert_eq!(names.len(), 8);
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use notelet_core::MemoryNoteRepo;
    use tower::ServiceExt;

    const TOKEN: &str = "secret";
    // base64("anyone:secret")
    const BASIC_GOOD: &str = "Basic YW55b25lOnNlY3JldA==";

    fn app() -> axum::Router {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        let cfg = Config {
            database_url: String::new(),
            token: Some(TOKEN.into()),
            bind: "127.0.0.1:0".parse().unwrap(),
            actor: "test".into(),
        };
        build_router(svc, &cfg)
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
}
