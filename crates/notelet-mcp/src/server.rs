//! [`NoteletMcp`]: the MCP server. Seven tools, three guide resources, two
//! transports. Every tool is one [`NoteService`] call plus serialisation;
//! domain errors come back as a failed tool result carrying
//! `{ "kind": ..., "message": ... }` (and `current_version` on a conflict)
//! so an agent can act on them without parsing prose.

use std::sync::Arc;

use notelet_core::service::FilterInput;
use notelet_core::{CoreError, NoteService};
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

/// `note_get` / `note_history` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PathParams {
    /// Note path, e.g. `projects/x/journal.md`.
    pub path: String,
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
    /// `next_cursor` from the previous page.
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
    /// `next_cursor` from the previous page.
    pub cursor: Option<String>,
}

/// `note_history` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    /// Note path.
    pub path: String,
    /// How many replaced bodies, newest first, 1..=50 (default 20).
    pub limit: Option<u32>,
}

/// `note_dump` input.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DumpParams {
    /// Only paths starting with this. Empty = everything.
    pub prefix: Option<String>,
    /// Page size, 1..=1000 (default 100).
    pub limit: Option<u32>,
    /// `next_cursor` from the previous page.
    pub cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// result helpers
// ---------------------------------------------------------------------------

fn ok_json<S: serde::Serialize>(value: &S) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

/// A domain error as a failed tool result with a machine-readable body.
fn core_error(err: CoreError) -> CallToolResult {
    let (kind, extra) = match &err {
        CoreError::NotFound(_) => ("NotFound", None),
        CoreError::InvalidPath(_) | CoreError::InvalidFilter(_) => ("Validation", None),
        CoreError::VersionConflict { current, .. } => ("Conflict", Some(*current)),
        CoreError::Storage(_) => ("Internal", None),
    };
    let mut body = serde_json::json!({ "kind": kind, "message": err.to_string() });
    if let (Some(cur), Some(obj)) = (extra, body.as_object_mut()) {
        obj.insert("current_version".into(), serde_json::Value::from(cur));
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
        description = "Read one note: path, body (Markdown), version, updated_at, updated_by. Keep the version if you intend to note_put."
    )]
    async fn note_get(
        &self,
        Parameters(p): Parameters<PathParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.svc.get(&p.path).await {
            Ok(n) => ok_json(&n),
            Err(e) => Ok(core_error(e)),
        }
    }

    #[tool(
        description = "Create or replace a note's whole body with optimistic locking. Omit expected_version to create (fails with kind=Conflict if it exists); pass the version you read to replace (fails with kind=Conflict and current_version if someone wrote in between — re-read, merge, retry). The replaced body is kept in history."
    )]
    async fn note_put(
        &self,
        Parameters(p): Parameters<PutParams>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .svc
            .put(&p.path, &p.body, p.expected_version, &self.actor)
            .await
        {
            Ok(n) => ok_json(
                &serde_json::json!({ "path": n.path, "version": n.version, "updated_at": n.updated_at }),
            ),
            Err(e) => Ok(core_error(e)),
        }
    }

    #[tool(
        description = "Append text to the end of a note (created if missing), on its own line. No version needed: an append cannot overwrite anyone. Use this for journals."
    )]
    async fn note_append(
        &self,
        Parameters(p): Parameters<AppendParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.svc.append(&p.path, &p.text, &self.actor).await {
            Ok(n) => ok_json(
                &serde_json::json!({ "path": n.path, "version": n.version, "updated_at": n.updated_at }),
            ),
            Err(e) => Ok(core_error(e)),
        }
    }

    #[tool(
        description = "List notes (path, version, updated_at, updated_by, size; no body) under a prefix, filtered by ANDed predicates {field, op, value}. Fields: path, updated_at, updated_by. Ops: eq ne like ilike gt lt gte lte. Ordered by path; pass next_cursor to page. See notelet://guides/filters."
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
        description = "Find notes whose body contains the text (case-insensitive substring, no wildcards) under a prefix. Returns path, a snippet around the first hit, updated_at. Ordered by path; paged by next_cursor."
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
        description = "Bodies that note_put replaced for this path, newest first (default 20, max 50). Appends do not create history."
    )]
    async fn note_history(
        &self,
        Parameters(p): Parameters<HistoryParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.svc.history(&p.path, p.limit).await {
            Ok(revs) => ok_json(&revs),
            Err(e) => Ok(core_error(e)),
        }
    }

    #[tool(
        description = "Paths and full bodies under a prefix, ordered by path and paged. For backups, exports, or leaving notelet: the result is a file tree."
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
             for documents, note_list / note_search to find, note_history to recover, \
             note_dump to export. Read notelet://guides/onboarding first."
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
/// required on `/mcp` when a token is configured.
pub async fn run_http(svc: Arc<NoteService>, cfg: Config) -> anyhow::Result<()> {
    let actor = cfg.actor.clone();
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
    let router = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .merge(mcp_router);

    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(
        bind = %cfg.bind,
        auth = if cfg.token.is_some() { "bearer" } else { "none (loopback)" },
        "serving notelet MCP at /mcp"
    );
    axum::serve(listener, router).await?;
    Ok(())
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
    }

    #[tokio::test]
    async fn invalid_path_is_validation() {
        let s = server();
        let r = s
            .note_get(Parameters(PathParams {
                path: "../etc".into(),
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
    fn tool_router_lists_seven_tools() {
        let names: Vec<String> = NoteletMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for n in [
            "note_get",
            "note_put",
            "note_append",
            "note_list",
            "note_search",
            "note_history",
            "note_dump",
        ] {
            assert!(names.contains(&n.to_string()), "{n} missing from {names:?}");
        }
        assert_eq!(names.len(), 7);
    }
}
