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
//!
//! # What `tools/list` declares
//!
//! Every tool carries annotations, because the MCP defaults (not read-only,
//! destructive, not idempotent, open world) would announce even `note_get`
//! as a destructive tool that reaches the outside world. All tools set
//! `openWorldHint: false` (the store is closed). `note_list`, `note_search`,
//! `note_history`, `note_dump` and `note_get` are `readOnlyHint: true`;
//! `note_put`, `note_restore` and `note_delete` are `destructiveHint: true`
//! and `idempotentHint: true`; `note_append` is `destructiveHint: false` and
//! `idempotentHint: false`. In local mode `note_get` can write `to_file`, so
//! [`NoteletMcp::router_for`] replaces its annotations there by the
//! destructive, idempotent set.
//!
//! The input schemas carry what the tools accept: the fixed-value fields
//! (`note_put.on_conflict`, `note_restore.source`, the `field` and `op` of a
//! `note_list` filter) are enums, so the schema has `enum` and a value outside
//! it fails at parameter parsing, and each `limit` has `minimum` / `maximum`
//! (`1..=1000`; `note_dump` `1..=100`; `note_history` `1..=50`). A `limit`
//! outside the range that still reaches the service is refused with
//! `kind: "Validation"`; it is never clamped. A value outside an enum
//! (`on_conflict`, `source`, a filter `field` or `op`) never reaches a tool:
//! rmcp answers it as an invalid-params protocol error (-32602, "failed to
//! deserialize parameters: ..."), not as a tool result with `kind: "Validation"`.
//!
//! # Why a result is one JSON text block
//!
//! A tool answers a single text content block holding the JSON, with no
//! `outputSchema` and no `structuredContent`. `outputSchema` is optional in
//! the MCP specification (2026-07-28); every client reads text content,
//! several ignore `structuredContent`, and others drop the text when
//! `structuredContent` is present, so adding it would make some clients see
//! less than they see now. What a structured result means is also still being
//! revised upstream. Revisit when the specification settles on a client
//! returning one or the other (at that point declare `outputSchema` from the
//! result types and return both forms).
//!
//! # Local mode
//!
//! Local mode adds a file projection (see [`crate::files`] for the file
//! root, the writes and the conflict file): `note_get` takes `to_file`,
//! `note_put` takes `from_file` and `on_conflict`, and the tool
//! `note_restore` exists. It needs a file root, which is `NOTELET_FILE_ROOT`
//! and nothing else (no working-directory default), and it needs the caller
//! to be the operator's own process:
//!
//! * `stdio` is local when `NOTELET_FILE_ROOT` is set.
//! * `serve` is local only when the bind address is loopback, the root is
//!   set and at least one token is configured ([`local_root`]). A tokenless
//!   loopback port can be reached by any local process or browser page, so
//!   it never gets file access. When `serve` is not local it logs one line
//!   at startup giving the reason.
//!
//! The root should be outside any repository checkout, since note bodies are
//! private. In any other mode the file parameters and `note_restore` are
//! absent.
//!
//! The split is by **router**, not by parameter type. The parameter structs
//! hold every field, and [`NoteletMcp::router_for`] builds the router a
//! server instance lists and dispatches from: the local one is the full
//! `#[tool_router]` router with the local usage notes appended to two
//! descriptions; the public one drops `note_restore`, removes
//! [`LOCAL_ONLY_PARAMS`] from every input schema, and makes `body` of
//! `note_put` a required string (it is optional in the local schema, where
//! `from_file` can stand in for it). A client of a non-local server therefore
//! never sees a file parameter, and if it sends one anyway the call fails
//! with `kind: "Validation"` instead of the field being silently ignored.
//! `just gen-docs` renders the tool table from both routers; the "local
//! only" column is their difference.
//!
//! # Files and conflicts
//!
//! The file logic is in [`crate::files`]; this module routes and maps to
//! JSON. `note_get` with `to_file` writes the body to the file (overwriting
//! it, except a file holding an unresolved conflict) and answers the note
//! without `body`, plus its `sha256` and the `file`; it is refused with
//! `tail_lines` or `max_bytes`, since a partial body written to a file would
//! later replace the whole note. `note_put` takes exactly one of `body` and
//! `from_file`; a file that is not UTF-8 or has a notelet header line is
//! refused. The locking is unchanged: without `expected_version` a put
//! only creates. With `on_conflict: "mark"` (only with `from_file`, only in
//! local mode; the default is `"fail"`) a `Conflict` also rewrites the file
//! into ours / base / theirs, and `note_restore` turns it back into one of
//! them. The base revision is found by version in `note_history` (metadata
//! first, then the bodies up to the match).
//!
//! The errors added: `kind: "Validation"` for a refused input (also a name
//! outside the file root) and `kind: "File"` for a failed read or write, both
//! with `message`. A `Conflict` on a marking put carries, besides
//! `current_version` and `expected_version`, `marked` (`true` when the file
//! was rewritten, `false` when it was not, the message says why, e.g. the
//! file changed during the put), `file`, and `base_version` when the file
//! has a base section. `note_restore` answers `{ file, source,
//! expected_version, section_version }`.

use std::sync::Arc;

use notelet_core::service::{FilterInput, History};
use notelet_core::{CoreError, NoteService, WriteOutcome};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Extensions, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ServerCapabilities, ServerConfig,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler};
use serde::Deserialize;

use crate::auth::{Actor, TokenTable};
use crate::config::Config;
use crate::files::{self, FileError, FileRoot};
use crate::resources;

/// The parameters that exist only in local mode. They are removed from the
/// tool schemas of every other mode.
pub(crate) const LOCAL_ONLY_PARAMS: &[&str] = &["to_file", "from_file", "on_conflict"];

/// The tool that exists only in local mode.
pub(crate) const LOCAL_ONLY_TOOL: &str = "note_restore";

/// Text appended to a tool's description in local mode only.
const LOCAL_NOTES: &[(&str, &str)] = &[
    (
        "note_get",
        "Local mode: to_file (a path relative to the file root, parent directories are created) writes the body to that file (overwriting it, unless it holds an unresolved conflict) and leaves body out of the answer, which still has version, size and sha256 (of the file); use it for a body too large to pass through your context. to_file is refused together with tail_lines or max_bytes, because a partial body in a file would later replace the whole note.",
    ),
    (
        "note_put",
        "Local mode: give exactly one of body and from_file (a path relative to the file root; UTF-8; refused while it still has notelet's conflict header lines). Locking is the same: without expected_version a put only creates. on_conflict (with from_file only) is \"fail\" (default) or \"mark\": on a Conflict, mark rewrites the file as ours (your file), base (the version you read, when expected_version was given and note_history still has it) and theirs (the current body), in git's marker form with line counts, and the error says marked=true; resolve the file, then note_restore it to a side or edit it by hand, and put again with expected_version=current_version (restoring ours and putting overwrites theirs, as git checkout --ours does). Unlike git, whole bodies are compared (no hunks), nothing is merged for you, and nothing is stored beyond the marker lines in the file.",
    ),
];

/// The server: a service handle and the fallback actor written to
/// `updated_by` when the request carries no authenticated actor. `files` is
/// `Some` in local mode (see the module doc) and holds the file root.
#[derive(Clone)]
pub struct NoteletMcp {
    svc: Arc<NoteService>,
    actor: String,
    files: Option<Arc<FileRoot>>,
    routes: ToolRouter<Self>,
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
    /// Local mode only. Write the body to this file (relative to the file
    /// root) instead of returning it.
    pub to_file: Option<String>,
}

/// `note_put` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PutParams {
    /// Note path.
    pub path: String,
    /// The whole new body (Markdown).
    pub body: Option<String>,
    /// Local mode only. Read the whole new body from this file (relative to
    /// the file root) instead of `body`.
    pub from_file: Option<String>,
    /// The version you last read. Omit to create a note that must not exist yet.
    pub expected_version: Option<i64>,
    /// Local mode only, with `from_file`: `"fail"` (default) or `"mark"`.
    pub on_conflict: Option<OnConflict>,
}

/// What `note_put` does with a file on a `Conflict` (`on_conflict`). The
/// variants carry `//` comments, not doc comments: a doc comment makes
/// schemars emit `oneOf`/`const` instead of the plain `enum` clients expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OnConflict {
    // Only report the conflict (the default).
    Fail,
    // Also rewrite the file into ours / base / theirs.
    Mark,
}

/// The section `note_restore` writes back (`source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RestoreSource {
    // Your file, as it was before marking.
    Ours,
    // The note's body at the time of the conflict.
    Theirs,
    // The version you had read.
    Base,
}

impl RestoreSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ours => "ours",
            Self::Theirs => "theirs",
            Self::Base => "base",
        }
    }
}

/// `note_restore` input (local mode only).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RestoreParams {
    /// A conflict file written by `note_put` with `on_conflict: "mark"`,
    /// relative to the file root.
    pub file: String,
    /// Which section to restore: `"ours"`, `"theirs"` or `"base"`.
    pub source: RestoreSource,
}

/// `note_delete` input.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteParams {
    /// Note path.
    pub path: String,
    /// The version you last read. Required: the delete is refused if someone
    /// wrote in between. (Declared optional only so that a call without it is
    /// answered as `Validation` instead of a schema error.)
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
    pub field: FilterField,
    /// `eq` | `ne` | `like` | `ilike` | `gt` | `lt` | `gte` | `lte`
    pub op: FilterOp,
    /// Compared as text; as RFC 3339 for `updated_at`; as a LIKE pattern for `like` / `ilike`.
    pub value: String,
}

/// The fields `note_list` can filter on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilterField {
    Path,
    UpdatedAt,
    UpdatedBy,
}

impl FilterField {
    fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::UpdatedAt => "updated_at",
            Self::UpdatedBy => "updated_by",
        }
    }
}

/// The comparison operators of a `note_list` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FilterOp {
    Eq,
    Ne,
    Like,
    Ilike,
    Gt,
    Lt,
    Gte,
    Lte,
}

impl FilterOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Like => "like",
            Self::Ilike => "ilike",
            Self::Gt => "gt",
            Self::Lt => "lt",
            Self::Gte => "gte",
            Self::Lte => "lte",
        }
    }
}

/// `note_list` input.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ListParams {
    /// Only paths starting with this (e.g. `projects/x/`). Empty = all.
    pub prefix: Option<String>,
    /// Predicates, ANDed.
    pub filters: Option<Vec<FilterParam>>,
    /// Page size, 1..=1000 (default 100); outside the range is refused.
    #[schemars(range(min = 1, max = 1000))]
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
    /// Page size, 1..=1000 (default 100); outside the range is refused.
    #[schemars(range(min = 1, max = 1000))]
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
    /// How many replaced or deleted bodies, newest first, 1..=50 (default 20); outside the range is refused.
    #[schemars(range(min = 1, max = 50))]
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
    /// Page size, 1..=100 (default 20): every item carries a body; outside the range is refused.
    #[schemars(range(min = 1, max = 100))]
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

/// A failed tool result with a machine-readable body: `kind`, `message` and
/// whatever `extra` adds.
fn error_result(
    kind: &str,
    message: String,
    extra: serde_json::Map<String, serde_json::Value>,
) -> CallToolResult {
    let mut body = serde_json::json!({ "kind": kind, "message": message });
    if let Some(obj) = body.as_object_mut() {
        obj.extend(extra);
    }
    CallToolResult::error(vec![ContentBlock::text(body.to_string())])
}

/// A refused or failed file operation as a failed tool result.
fn file_error(err: &FileError) -> CallToolResult {
    tracing::debug!(kind = err.kind(), error = %err, "tool rejected");
    error_result(err.kind(), err.message().to_string(), Default::default())
}

/// A request that breaks a rule of the tool itself.
fn refuse(message: impl Into<String>) -> CallToolResult {
    file_error(&FileError::Refused(message.into()))
}

/// The `current_version` / `expected_version` fields of a conflict.
fn conflict_fields(
    expected: Option<i64>,
    current: i64,
) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert("current_version".into(), serde_json::Value::from(current));
    m.insert("expected_version".into(), serde_json::json!(expected));
    m
}

/// A domain error as a failed tool result with a machine-readable body. A
/// conflict also carries `current_version` and `expected_version` (`null`
/// when the caller passed none, i.e. tried to create).
fn core_error(err: CoreError) -> CallToolResult {
    let kind = match &err {
        CoreError::NotFound(_) => "NotFound",
        CoreError::InvalidPath(_)
        | CoreError::InvalidFilter(_)
        | CoreError::InvalidTail(_)
        | CoreError::InvalidLimit(_) => "Validation",
        CoreError::VersionConflict { .. } => "Conflict",
        CoreError::Contended { .. } | CoreError::Storage(_) => "Internal",
        _ => "Internal",
    };
    if kind == "Internal" {
        tracing::error!(kind, error = %err.chain(), "tool failed");
    } else {
        tracing::debug!(kind, error = %err, "tool rejected");
    }
    let extra = match &err {
        CoreError::VersionConflict {
            expected, current, ..
        } => conflict_fields(*expected, *current),
        _ => Default::default(),
    };
    error_result(kind, err.chain().to_string(), extra)
}

fn filters(input: Option<Vec<FilterParam>>) -> Vec<FilterInput> {
    input
        .unwrap_or_default()
        .into_iter()
        .map(|f| FilterInput {
            field: f.field.as_str().into(),
            op: f.op.as_str().into(),
            value: f.value,
        })
        .collect()
}

/// Apply `f` to the `properties` of a tool's input schema.
fn edit_properties(
    tool: &mut rmcp::model::Tool,
    f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) {
    let schema = Arc::make_mut(&mut tool.input_schema);
    if let Some(props) = schema
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        f(props);
    }
}

/// The refusal for a local-only parameter (or the tool) outside local mode.
fn not_local(what: &str) -> CallToolResult {
    refuse(format!(
        "{what} is available only in local mode: NOTELET_FILE_ROOT must be set to an existing \
         directory and, for serve, the bind address must be loopback and a token configured"
    ))
}

// ---------------------------------------------------------------------------
// tools
// ---------------------------------------------------------------------------

#[tool_router]
impl NoteletMcp {
    /// Build the server over a service, not in local mode: no file
    /// parameters, no `note_restore`.
    pub fn new(svc: Arc<NoteService>, actor: String) -> Self {
        Self {
            svc,
            actor,
            files: None,
            routes: Self::router_for(false),
        }
    }

    /// Build the server in local mode, with file names relative to `root`.
    pub fn local(svc: Arc<NoteService>, actor: String, root: FileRoot) -> Self {
        Self {
            svc,
            actor,
            files: Some(Arc::new(root)),
            routes: Self::router_for(true),
        }
    }

    /// The tool router of one mode (see the module doc).
    pub(crate) fn router_for(local: bool) -> ToolRouter<Self> {
        let mut router = Self::tool_router();
        if local {
            for (name, note) in LOCAL_NOTES {
                if let Some(route) = router.map.get_mut(*name) {
                    let d = route.attr.description.take().unwrap_or_default();
                    route.attr.description = Some(format!("{d} {note}").into());
                }
            }
            // `to_file` makes `note_get` a writer (of a local file).
            if let Some(route) = router.map.get_mut("note_get") {
                route.attr.annotations = Some(
                    ToolAnnotations::new()
                        .read_only(false)
                        .destructive(true)
                        .idempotent(true)
                        .open_world(false),
                );
            }
            if let Some(route) = router.map.get_mut("note_put") {
                edit_properties(&mut route.attr, |props| {
                    if let Some(serde_json::Value::String(d)) =
                        props.get_mut("body").and_then(|b| b.get_mut("description"))
                    {
                        d.push_str(" Give exactly one of body and from_file.");
                    }
                });
            }
        } else {
            router.remove_route(LOCAL_ONLY_TOOL);
            for route in router.map.values_mut() {
                edit_properties(&mut route.attr, |props| {
                    for name in LOCAL_ONLY_PARAMS {
                        props.remove(*name);
                    }
                });
                // the enum of a removed parameter must not stay behind in $defs
                let schema = Arc::make_mut(&mut route.attr.input_schema);
                if let Some(defs) = schema
                    .get_mut("$defs")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    defs.remove("OnConflict");
                    if defs.is_empty() {
                        schema.remove("$defs");
                    }
                }
            }
            if let Some(route) = router.map.get_mut("note_put") {
                let schema = Arc::make_mut(&mut route.attr.input_schema);
                if let Some(req) = schema
                    .get_mut("required")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    req.push("body".into());
                }
                edit_properties(&mut route.attr, |props| {
                    if let Some(body) = props.get_mut("body").and_then(|b| b.as_object_mut()) {
                        body.insert("type".into(), "string".into());
                    }
                });
            }
        }
        router
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
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Read one note: path, body (Markdown), version, updated_at, updated_by, size, truncated. Optional tail_lines (>= 1) returns only the last N lines and max_bytes (>= 1) caps the bytes returned, cut on a line start where possible; use them to read the end of a long journal. size is the whole body in bytes (as note_list reports it) and truncated=true means body is only the end of the note. Keep the version if you intend to note_put, but NEVER note_put a body returned with truncated=true: it would replace the whole note with the fragment (to add to a journal use note_append)."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_get", path = %p.path))]
    async fn note_get(
        &self,
        Parameters(p): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(file) = p.to_file.as_deref() else {
            return match self.svc.read(&p.path, p.tail_lines, p.max_bytes).await {
                Ok(n) => ok_json(&n),
                Err(e) => Ok(core_error(e)),
            };
        };
        let Some(root) = self.files.as_deref() else {
            return Ok(not_local("to_file"));
        };
        if p.tail_lines.is_some() || p.max_bytes.is_some() {
            return Ok(refuse(
                "to_file cannot be combined with tail_lines or max_bytes: a partial body \
                 written to a file would later replace the whole note through note_put",
            ));
        }
        if let Err(e) = root.resolve(file) {
            return Ok(file_error(&e));
        }
        let note = match self.svc.get(&p.path).await {
            Ok(n) => n,
            Err(e) => return Ok(core_error(e)),
        };
        if let Err(e) = root.write_body(file, &note.body) {
            return Ok(file_error(&e));
        }
        let out = serde_json::json!({
            "path": note.path,
            "version": note.version,
            "updated_at": note.updated_at,
            "updated_by": note.updated_by,
            "size": note.body.len(),
            "truncated": false,
            "sha256": notelet_core::domain::sha256_hex(&note.body),
            "file": file,
        });
        ok_json(&out)
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Create or replace a note's whole body with optimistic locking. Omit expected_version to create (fails with kind=Conflict if it exists); pass the version you read to replace (fails with kind=Conflict and current_version if someone wrote in between — re-read, merge, retry). The replaced body is kept in history. A body identical to the current one changes nothing (changed=false). Returns path, version, updated_at, size (bytes, UTF-8), sha256, changed."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_put", path = %p.path, actor = tracing::field::Empty))]
    async fn note_put(
        &self,
        Parameters(p): Parameters<PutParams>,
        ext: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.actor_of(&ext);
        let mark = p.on_conflict == Some(OnConflict::Mark);
        let root = self.files.as_deref();
        if root.is_none() {
            for (given, name) in [
                (p.from_file.is_some(), "from_file"),
                (p.on_conflict.is_some(), "on_conflict"),
            ] {
                if given {
                    return Ok(not_local(name));
                }
            }
        }
        if p.on_conflict.is_some() && p.from_file.is_none() {
            return Ok(refuse("on_conflict needs from_file"));
        }
        let body = match (p.body.as_deref(), p.from_file.as_deref(), root) {
            (Some(_), Some(_), _) => {
                return Ok(refuse("give exactly one of body and from_file, not both"));
            }
            (None, None, None) => return Ok(refuse("body is required")),
            (None, None, Some(_)) => {
                return Ok(refuse(
                    "give exactly one of body and from_file, got neither",
                ));
            }
            (Some(b), None, _) => b.to_string(),
            (None, Some(f), Some(root)) => match root.read_body(f) {
                Ok(text) => text,
                Err(e) => return Ok(file_error(&e)),
            },
            (None, Some(_), None) => unreachable!("refused above"),
        };
        match self
            .svc
            .put(&p.path, &body, p.expected_version, &actor)
            .await
        {
            Err(CoreError::VersionConflict {
                expected, current, ..
            }) if mark => {
                let file = p.from_file.as_deref().unwrap_or_default();
                let root = root.expect("mark implies from_file implies local");
                Ok(self
                    .mark_conflict(root, file, &p.path, &body, expected, current)
                    .await)
            }
            other => write_json(other),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Rewrite a conflict file (written by note_put with on_conflict=\"mark\") to one section's body, exactly as it was before marking; local mode only. Pass source as \"ours\" (your file), \"theirs\" (the note's body at the time of the conflict) or \"base\" (the version you had read). Answers file, source, expected_version (the theirs version: pass it to the next note_put) and section_version (the version the restored body belongs to: theirs or base; null for ours). Restoring ours and putting overwrites theirs, as git checkout --ours does; restoring theirs and editing keeps their work. A file that is not a well-formed conflict file, whose line counts do not match, or that has no such section, is an error and stays unchanged. Unlike git there is no merge and no stored state: the markers in the file are all there is."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_restore", file = %p.file))]
    async fn note_restore(
        &self,
        Parameters(p): Parameters<RestoreParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(root) = self.files.as_deref() else {
            return Ok(not_local(LOCAL_ONLY_TOOL));
        };
        match files::restore(root, &p.file, p.source.as_str()) {
            Ok(r) => ok_json(&serde_json::json!({
                "file": p.file,
                "source": p.source.as_str(),
                "expected_version": r.expected_version,
                "section_version": r.section_version,
            })),
            Err(e) => Ok(file_error(&e)),
        }
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        ),
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
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Remove a note. Its body stays readable through note_history as the last revision. Requires expected_version (the version you read; refused with Conflict if someone wrote in between). Answers the removed note's version, size and sha256."
    )]
    #[tracing::instrument(level = "debug", skip_all, fields(tool = "note_delete", path = %p.path, actor = tracing::field::Empty))]
    async fn note_delete(
        &self,
        Parameters(p): Parameters<DeleteParams>,
        ext: Extensions,
    ) -> Result<CallToolResult, McpError> {
        let actor = self.actor_of(&ext);
        let Some(expected) = p.expected_version else {
            return Ok(refuse(
                "note_delete needs expected_version: the version you last read (note_get)",
            ));
        };
        write_json(self.svc.delete(&p.path, expected, &actor).await)
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "List notes (path, version, updated_at, updated_by, size in bytes (UTF-8); no body) under a prefix, filtered by ANDed predicates {field, op, value}. Fields: path, updated_at, updated_by. Ops: eq ne like ilike gt lt gte lte. Ordered by path; pass next_cursor as cursor to page (exclusive: the page starts strictly after that path). Default 100, max 1000; a limit outside 1..=1000 is refused (kind=Validation), not clamped. See notelet://guides/filters."
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
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Find notes whose body contains the text (case-insensitive substring, no wildcards) under a prefix. Returns path, a snippet around the first hit, updated_at. Ordered by path; paged by next_cursor (exclusive: the page starts strictly after that path). Default limit 100, max 1000; a limit outside the range is refused (kind=Validation)."
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
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Bodies that note_put replaced or note_delete removed for this path, newest first (default 20, max 50; a limit outside 1..=50 is refused with kind=Validation). Appends do not create history. include_body=false returns only path, version, updated_at, updated_by, size (bytes, UTF-8)."
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
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Paths and full bodies under a prefix, ordered by path and paged: default 20, max 100 (bodies are heavy; a limit outside 1..=100 is refused with kind=Validation); pass next_cursor as cursor (exclusive). For backups, exports, or leaving notelet: the result is a file tree."
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

    /// A put with `on_conflict: "mark"` lost the lock: read the current
    /// note (and the base revision, when `expected` names one that history
    /// still has), rewrite `file` as the conflict file, and answer the
    /// `Conflict` with `marked`, `file` and `base_version`. When marking is
    /// not possible the answer is the plain conflict with `marked: false`
    /// and the reason in the message.
    async fn mark_conflict(
        &self,
        root: &FileRoot,
        file: &str,
        path: &str,
        ours: &str,
        expected: Option<i64>,
        current: i64,
    ) -> CallToolResult {
        let conflict = CoreError::VersionConflict {
            path: path.to_string(),
            expected,
            current,
        };
        let unmarked = |current: i64, why: &str| {
            let mut extra = conflict_fields(expected, current);
            extra.insert("marked".into(), false.into());
            extra.insert("file".into(), file.into());
            tracing::debug!(why, "conflict not marked");
            error_result(
                "Conflict",
                format!("{}; not marked: {why}", conflict.chain()),
                extra,
            )
        };
        let theirs = match self.svc.get(path).await {
            Ok(n) => n,
            Err(e) => return unmarked(current, &format!("the note could not be read back ({e})")),
        };
        let base = match expected {
            Some(v) => self.base_of(path, v).await,
            None => None,
        };
        let theirs_version = theirs.version;
        match files::mark(root, file, ours, base, (theirs_version, theirs.body)) {
            Err(why) => unmarked(theirs_version, &why),
            Ok(base_version) => {
                let mut extra = conflict_fields(expected, theirs_version);
                extra.insert("marked".into(), true.into());
                extra.insert("file".into(), file.into());
                if let Some(v) = base_version {
                    extra.insert("base_version".into(), v.into());
                }
                error_result(
                    "Conflict",
                    format!(
                        "{}; {file:?} now holds ours, {}theirs (v{theirs_version}): resolve it (edit it, or note_restore it to a side; restoring ours overwrites theirs) and put again with expected_version={theirs_version}",
                        conflict.chain(),
                        if base_version.is_some() { "base, " } else { "" },
                    ),
                    extra,
                )
            }
        }
    }

    /// The body of revision `version` of `path`, when history still has it.
    /// The metadata list finds the version (and only the version: a note
    /// deleted and created again continues its numbering, so a version names
    /// one body); then the bodies are read up to that revision.
    async fn base_of(&self, path: &str, version: i64) -> Option<(i64, String)> {
        let History::Meta(metas) = self.svc.history(path, Some(50), false).await.ok()? else {
            return None;
        };
        let at = metas.iter().position(|r| r.version == version)?;
        let History::Full(revs) = self
            .svc
            .history(path, Some(u32::try_from(at + 1).ok()?), true)
            .await
            .ok()?
        else {
            return None;
        };
        revs.into_iter()
            .find(|r| r.version == version)
            .map(|r| (version, r.body))
    }
}

#[tool_handler(router = self.routes)]
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

/// Serve over stdio for one local client. Local mode (files under `root`)
/// is on when a root is given.
pub async fn run_stdio(
    svc: Arc<NoteService>,
    actor: String,
    root: Option<FileRoot>,
) -> anyhow::Result<()> {
    use rmcp::{transport::stdio, ServiceExt};
    let server = match root {
        Some(root) => NoteletMcp::local(svc, actor, root),
        None => {
            tracing::info!("file projection off: NOTELET_FILE_ROOT is not set");
            NoteletMcp::new(svc, actor)
        }
    };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// The file root of a `serve` in local mode, or why there is none: local
/// mode needs a loopback bind, `NOTELET_FILE_ROOT`, and at least one token.
pub(crate) fn local_root(cfg: &Config) -> Result<FileRoot, &'static str> {
    if !cfg.bind.ip().is_loopback() {
        return Err("the bind address is not loopback");
    }
    let Some(root) = &cfg.file_root else {
        return Err("NOTELET_FILE_ROOT is not set");
    };
    if cfg.tokens.is_empty() {
        return Err("no token is configured");
    }
    Ok(FileRoot::new(root.clone()))
}

/// Serve over streamable HTTP at `/mcp`, with `/healthz` open, a listed
/// bearer token required on `/mcp` when any token is configured. The
/// read-only browser view ([`crate::ui`]) is merged in under `/ui`, behind
/// HTTP Basic with the same tokens. Local mode is decided by [`local_root`];
/// when it is off the reason is logged once.
pub async fn run_http(svc: Arc<NoteService>, cfg: Config) -> anyhow::Result<()> {
    match local_root(&cfg) {
        Ok(_) => tracing::info!("file projection on"),
        Err(why) => tracing::info!("file projection off: {why}"),
    }
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
    let root = local_root(cfg).ok();
    let ui = crate::ui::router(svc.clone(), cfg.tokens.clone());
    let mut http_config = StreamableHttpServerConfig::default();
    http_config.json_response = true;
    if !cfg.bind.ip().is_loopback() {
        // bearer replaces the loopback-only Host check for hosted binds
        http_config.allowed_hosts.clear();
    }
    let mcp = StreamableHttpService::new(
        move || {
            Ok(match &root {
                Some(root) => NoteletMcp::local(svc.clone(), actor.clone(), root.clone()),
                None => NoteletMcp::new(svc.clone(), actor.clone()),
            })
        },
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

    fn tool(local: bool, name: &str) -> rmcp::model::Tool {
        NoteletMcp::router_for(local)
            .list_all()
            .into_iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("no tool {name}"))
    }

    /// `(read_only, destructive, idempotent, open_world)` of a tool.
    fn hints(local: bool, name: &str) -> [Option<bool>; 4] {
        let a = tool(local, name).annotations.expect("annotations");
        [
            a.read_only_hint,
            a.destructive_hint,
            a.idempotent_hint,
            a.open_world_hint,
        ]
    }

    #[test]
    fn tools_list_carries_annotations() {
        let (t, f) = (Some(true), Some(false));
        for local in [false, true] {
            for name in ["note_list", "note_search", "note_history", "note_dump"] {
                assert_eq!(hints(local, name), [t, None, None, f], "{name}");
            }
            for name in ["note_put", "note_delete"] {
                assert_eq!(hints(local, name), [f, t, t, f], "{name}");
            }
            assert_eq!(hints(local, "note_append"), [f, f, f, f]);
        }
        assert_eq!(hints(true, "note_restore"), [f, t, t, f]);
        // note_get reads on the public router and writes `to_file` on the local one
        assert_eq!(hints(false, "note_get"), [t, None, None, f]);
        assert_eq!(hints(true, "note_get"), [f, t, t, f]);
    }

    /// The schema of `$defs.<def>` or of the property itself.
    fn defs(t: &rmcp::model::Tool) -> serde_json::Value {
        t.input_schema
            .get("$defs")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    #[test]
    fn tools_list_schemas_carry_enums_and_ranges() {
        let list = tool(false, "note_list");
        let d = defs(&list);
        assert_eq!(
            d["FilterField"]["enum"],
            serde_json::json!(["path", "updated_at", "updated_by"])
        );
        assert_eq!(
            d["FilterOp"]["enum"],
            serde_json::json!(["eq", "ne", "like", "ilike", "gt", "lt", "gte", "lte"])
        );
        assert_eq!(
            d["FilterParam"]["properties"]["field"]["$ref"],
            "#/$defs/FilterField"
        );
        let local_put = tool(true, "note_put");
        assert_eq!(
            defs(&local_put)["OnConflict"]["enum"],
            serde_json::json!(["fail", "mark"])
        );
        assert_eq!(
            defs(&tool(true, "note_restore"))["RestoreSource"]["enum"],
            serde_json::json!(["ours", "theirs", "base"])
        );
        // the public router has no on_conflict, so no enum for it either
        assert!(defs(&tool(false, "note_put")).get("OnConflict").is_none());

        for (name, max) in [
            ("note_list", 1000),
            ("note_search", 1000),
            ("note_history", 50),
            ("note_dump", 100),
        ] {
            let limit = &tool(false, name).input_schema["properties"]["limit"];
            assert_eq!(limit["minimum"], 1, "{name}");
            assert_eq!(limit["maximum"], max, "{name}");
        }
    }

    /// The schema enums and the core parsers name the same fields and ops.
    /// The exhaustive matches on the core types stop compiling when the core
    /// gains a variant, so the schema cannot silently fall behind.
    #[test]
    fn filter_enums_round_trip_through_the_core_parsers() {
        use notelet_core::{Field, Op};
        for v in [
            FilterField::Path,
            FilterField::UpdatedAt,
            FilterField::UpdatedBy,
        ] {
            let core = Field::parse(v.as_str()).unwrap();
            let back = match core {
                Field::Path => FilterField::Path,
                Field::UpdatedAt => FilterField::UpdatedAt,
                Field::UpdatedBy => FilterField::UpdatedBy,
            };
            assert_eq!(back, v);
            assert_eq!(core.column(), v.as_str());
        }
        for v in [
            FilterOp::Eq,
            FilterOp::Ne,
            FilterOp::Like,
            FilterOp::Ilike,
            FilterOp::Gt,
            FilterOp::Lt,
            FilterOp::Gte,
            FilterOp::Lte,
        ] {
            let back = match Op::parse(v.as_str()).unwrap() {
                Op::Eq => FilterOp::Eq,
                Op::Ne => FilterOp::Ne,
                Op::Like => FilterOp::Like,
                Op::ILike => FilterOp::Ilike,
                Op::Gt => FilterOp::Gt,
                Op::Lt => FilterOp::Lt,
                Op::Gte => FilterOp::Gte,
                Op::Lte => FilterOp::Lte,
            };
            assert_eq!(back, v);
        }
    }

    #[tokio::test]
    async fn out_of_range_limit_is_a_validation_error() {
        let s = server();
        let kind = |r: CallToolResult| {
            assert_eq!(r.is_error, Some(true));
            let v: serde_json::Value = serde_json::from_str(&text_of(&r)).unwrap();
            v["kind"].as_str().unwrap().to_string()
        };
        for limit in [0, 1001] {
            let r = s
                .note_list(Parameters(ListParams {
                    limit: Some(limit),
                    ..Default::default()
                }))
                .await
                .unwrap();
            assert_eq!(kind(r), "Validation", "list {limit}");
            let r = s
                .note_search(Parameters(SearchParams {
                    text: "x".into(),
                    prefix: None,
                    limit: Some(limit),
                    cursor: None,
                }))
                .await
                .unwrap();
            assert_eq!(kind(r), "Validation", "search {limit}");
        }
        for limit in [0, 101] {
            let r = s
                .note_dump(Parameters(DumpParams {
                    limit: Some(limit),
                    ..Default::default()
                }))
                .await
                .unwrap();
            assert_eq!(kind(r), "Validation", "dump {limit}");
        }
        for limit in [0, 51] {
            let r = s
                .note_history(Parameters(HistoryParams {
                    path: "n/a.md".into(),
                    limit: Some(limit),
                    include_body: None,
                }))
                .await
                .unwrap();
            assert_eq!(kind(r), "Validation", "history {limit}");
        }
        // the bounds themselves are accepted
        let r = s
            .note_list(Parameters(ListParams {
                limit: Some(1000),
                ..Default::default()
            }))
            .await
            .unwrap();
        assert_ne!(r.is_error, Some(true));
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
                    body: Some("one".into()),
                    from_file: None,
                    expected_version: None,
                    on_conflict: None,
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
                    body: Some("two".into()),
                    from_file: None,
                    expected_version: Some(9),
                    on_conflict: None,
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
                body: Some(body.into()),
                from_file: None,
                expected_version,
                on_conflict: None,
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
    async fn delete_without_expected_version_is_validation() {
        let s = server();
        put_note(&s, "n/d.md", "keep", None).await.unwrap();
        let r = s
            .note_delete(
                Parameters(DeleteParams {
                    path: "n/d.md".into(),
                    expected_version: None,
                }),
                Extensions::default(),
            )
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&text_of(&r)).unwrap();
        assert_eq!(body["kind"], "Validation");
        assert!(s.svc.get("n/d.md").await.is_ok());
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
                to_file: None,
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
            to_file: None,
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
                to_file: None,
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
    const TOOLS_GUIDE_HEAD: &str = "# Tools\n\nEvery tool the server offers, generated from the tool router. Run `just gen-docs` after changing a tool. The last four columns are the tool annotations in `tools/list`: `-` means the hint is not set because it only applies to a tool that is not read-only.\n\n";

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

    /// The names of the input properties of a tool.
    fn property_names(t: &rmcp::model::Tool) -> Vec<String> {
        t.input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// A hint as a table cell: `yes`, `no`, or `-` when the tool does not set
    /// it (it has no meaning for a read-only tool).
    fn hint(h: Option<bool>) -> &'static str {
        match h {
            Some(true) => "yes",
            Some(false) => "no",
            None => "-",
        }
    }

    /// The hint cell of a tool: the public router's value, followed by the
    /// local router's when that differs (`note_get` writes `to_file` there).
    fn hint_cell(
        public: Option<&rmcp::model::Tool>,
        local: &rmcp::model::Tool,
        pick: fn(&rmcp::model::ToolAnnotations) -> Option<bool>,
    ) -> String {
        let of = |t: &rmcp::model::Tool| t.annotations.as_ref().and_then(pick);
        let l = of(local);
        match public.map(of) {
            Some(p) if p != l => format!("{} (local: {})", hint(p), hint(l)),
            _ => hint(l).to_string(),
        }
    }

    /// Markdown table of the tool routers: one row per tool, sorted by name.
    /// The "local only" column is the difference between the local router and
    /// the public one: a tool the public router lacks is "the tool", otherwise
    /// the parameters it lacks. The hint columns are the annotations; where
    /// the local router differs from the public one the cell says so.
    fn render_tool_table() -> String {
        let mut tools = NoteletMcp::router_for(true).list_all();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let public = NoteletMcp::router_for(false);
        let mut out = String::from(
            "| tool | does | local only | read-only | destructive | idempotent | open world |\n|---|---|---|---|---|---|---|\n",
        );
        for t in tools {
            let desc = t.description.as_deref().unwrap_or("");
            let cell = first_sentence(desc);
            assert_eq!(
                cell.matches('`').count() % 2,
                0,
                "unbalanced backticks in the first sentence of {}",
                t.name
            );
            let local_only = match public.get(&t.name) {
                None => "the tool".to_string(),
                Some(p) => {
                    let public_props = property_names(p);
                    let mut only: Vec<String> = property_names(&t)
                        .into_iter()
                        .filter(|n| !public_props.contains(n))
                        .collect();
                    only.sort();
                    only.iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            };
            let sep = if local_only.is_empty() { "" } else { " " };
            let p = public.get(&t.name);
            let hints = [
                hint_cell(p, &t, |a| a.read_only_hint),
                hint_cell(p, &t, |a| a.destructive_hint),
                hint_cell(p, &t, |a| a.idempotent_hint),
                hint_cell(p, &t, |a| a.open_world_hint),
            ]
            .join(" | ");
            out.push_str(&format!(
                "| `{}` | {} |{sep}{local_only} | {hints} |\n",
                t.name, cell
            ));
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
            file_root: None,
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
            json!({"path": "n/app.md", "expected_version": 1}),
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
                expected_version: Some(1),
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
                body: Some("x".into()),
                from_file: None,
                expected_version: None,
                on_conflict: None,
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

#[cfg(test)]
mod file_tests;
