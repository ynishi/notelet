//! [`RemoteNoteRepo`]: a [`NoteRepo`] whose store is another notelet,
//! reached through its MCP tools over streamable HTTP. With it `serve` and
//! `stdio` run with no database: a local process (a laptop's stdio server,
//! say) keeps its own guides, browser view and token table and forwards
//! every read and write to a hosted instance.
//!
//! Each port method is the remote tool of the same meaning, with the
//! parameter and result shapes of [`crate::server`] (`get` / `get_view` ->
//! `note_get`, `put` -> `note_put`, `append` -> `note_append`, `delete` ->
//! `note_delete`, `list` -> `note_list`, `search` -> `note_search`,
//! `history` / `history_meta` -> `note_history` with `include_body`, `dump`
//! -> `note_dump`).
//!
//! # TLS
//!
//! `https://` remotes use rustls with the ring provider, installed as the
//! process default before the first client is built (see [`ensure_tls_provider`]);
//! reqwest is compiled without a provider so that aws-lc is not built.
//!
//! # Session
//!
//! One MCP session is opened at start and shared by every local session
//! (every client of `serve`, or the one of `stdio`). rmcp re-initialises it
//! when the remote reports it expired (HTTP 404) and retries the POST that
//! hit it once; a transport error is returned to the call that met it and
//! to no other. The process does not start if the remote is unreachable or
//! refuses the token: [`RemoteNoteRepo::connect`] performs the handshake.
//!
//! Neither rmcp nor its default HTTP client has a timeout, so there is one
//! here: [`REMOTE_TIMEOUT`] (30 s, not configurable) bounds the connect
//! (transport and handshake together, with `tokio::time::timeout`) and each
//! tool call (rmcp's per-request timeout, `PeerRequestOptions`). `connect`
//! opens the session with an `initialize` request, which makes rmcp's
//! client use its session mode whatever the server is: after a timed-out
//! call it POSTs a cancel notification. The cancel is a courtesy to the
//! remote, so it has its own, shorter bound, [`REMOTE_CANCEL_TIMEOUT`]
//! (2 s, `control_request_timeout`), and cancel POSTs go one at a time. So
//! one stalled call costs about the call timeout plus the cancel timeout
//! (32 s), and N concurrent stalled calls about 30 s + N x 2 s (measured
//! with notelet's own server at 300 ms / 100 ms: 20 stalled calls took
//! about 2.3 s). A timeout is
//! [`CoreError::Storage`] ("request timeout after ..." from rmcp, or
//! "connecting to the remote timed out").
//!
//! The text of a transport error includes the remote URL (it is the
//! operator's own, going to their log and to the local client); the token
//! never appears in it, since it is only sent as a header. Nothing strips
//! the URL.
//!
//! # Errors
//!
//! The remote answers a domain error as a failed tool result whose text is
//! `{ "kind", "message" }` (plus `current_version` / `expected_version` on a
//! conflict). It is mapped back: `Conflict` -> [`CoreError::VersionConflict`]
//! keeping `current_version` and `expected_version`; `NotFound` ->
//! [`CoreError::NotFound`] (naming the path asked for); `Validation` ->
//! [`CoreError::InvalidPath`], [`CoreError::InvalidFilter`] or
//! [`CoreError::InvalidTail`], told apart by the prefix of the message,
//! which is the `Display` of the original error. Everything else (`Internal`,
//! an unreadable answer, a transport or protocol failure, a closed
//! connection) is [`CoreError::Storage`] whose `source()` is the cause
//! ([`RemoteFailure`] for what the remote said, the rmcp `ServiceError` for
//! what the transport did), so `chain()` shows it. A remote `Contended`
//! (the append race) arrives as `Internal`, hence as `Storage`.
//!
//! # Actor
//!
//! The remote records the actor of the **token** this client presents
//! (`NOTELET_REMOTE_TOKEN[_FILE]`); the `by` argument of `put`, `append` and
//! `delete` cannot be sent, because no tool takes one, and is ignored. A
//! remote without tokens records its own fallback actor.
//!
//! # Synthesised values
//!
//! The remote's write answers carry version, `updated_at`, size and hash,
//! not `updated_by` and not the body. So:
//!
//! * `put` returns a [`Note`] whose `body` is the input, whose `version` and
//!   `updated_at` are the remote's, and whose `updated_by` is **empty**: it
//!   is not known here (it is the token's actor when `changed`, the previous
//!   writer when not), and nothing is invented. `changed` is the remote's.
//! * `append` cannot know the body after the append, so it reads the note
//!   back with `note_get` and returns that, `updated_by` included. If a
//!   writer got in between, the note is the newer one and its version
//!   differs from the one the append answered with; it is a consistent
//!   snapshot either way (after a race the version and sha the remote's
//!   write answer reported describe the older write, and are not used). If
//!   the note was deleted in between, the read-back is
//!   [`CoreError::NotFound`]. Every append downloads the whole body to
//!   rebuild the `Note`, so on a long journal it costs a full read.
//! * `delete` reads the note first, then deletes at exactly the version it
//!   read (the caller's `expected` must equal it, else the conflict is
//!   raised here without a call), so the returned body is the one removed.
//!   A write between the two calls is a [`CoreError::VersionConflict`], also
//!   when the caller passed `expected = None` (the conflict then reports
//!   `expected: None`; there is no retry).
//!
//! The extra reads make `append` and `delete` two round trips and not
//! atomic with respect to other writers beyond what is described above.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use notelet_core::domain::{DumpEntry, Filter, FilterValue, WriteOutcome};
use notelet_core::{
    CoreError, Note, NoteMeta, NotePath, NoteRepo, NoteView, Page, PageOut, PutResult, Revision,
    RevisionMeta, SearchHit, Tail,
};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientConfig, ClientRequest,
    ServerResult,
};
use rmcp::service::{PeerRequestOptions, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

/// What the remote reported for a failed tool call that has no
/// [`CoreError`] of its own (`Internal`, or an answer that did not parse).
/// Kept as the `source()` of [`CoreError::Storage`].
#[derive(Debug)]
pub struct RemoteFailure {
    pub kind: String,
    pub message: String,
}

impl fmt::Display for RemoteFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "remote {}: {}", self.kind, self.message)
    }
}

impl std::error::Error for RemoteFailure {}

fn storage(e: impl std::error::Error + Send + Sync + 'static) -> CoreError {
    CoreError::Storage(Box::new(e))
}

fn failure(kind: &str, message: impl Into<String>) -> CoreError {
    storage(RemoteFailure {
        kind: kind.into(),
        message: message.into(),
    })
}

/// Make sure a TLS provider is installed before a client is built. reqwest is built without one (`reqwest-tls-no-provider`) and
/// panics when a client is built with none installed, so the provider is
/// installed here, once per process, before any client exists. It is ring,
/// the provider sqlx's rustls already uses, which avoids aws-lc-sys (it
/// needs cmake) on the five release targets. `install_default` fails when
/// some provider is already installed (another call, or other code in the
/// process); that is fine, the existing one is used, so the `Err` is ignored.
/// The `Once` only saves repeating the attempt.
fn ensure_tls_provider() {
    static PROVIDER: std::sync::Once = std::sync::Once::new();
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// How long the connect, and each tool call, may take. See the module docs.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the cancel rmcp posts after a timed-out call may take.
pub const REMOTE_CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

/// A notelet reached over MCP, as a [`NoteRepo`]. See the module docs.
pub struct RemoteNoteRepo {
    client: RunningService<RoleClient, ClientConfig>,
    timeout: Duration,
}

impl RemoteNoteRepo {
    /// Connect to the MCP endpoint `url` (`http(s)://host/mcp`), presenting
    /// `token` as the bearer credential when given. Performs the MCP
    /// handshake, so an unreachable or unauthorised remote fails here.
    pub async fn connect(url: &str, token: Option<&str>) -> Result<Self, CoreError> {
        Self::connect_with_timeouts(
            url,
            token,
            REMOTE_TIMEOUT,
            REMOTE_TIMEOUT,
            REMOTE_CANCEL_TIMEOUT,
        )
        .await
    }

    /// [`RemoteNoteRepo::connect`] with the connect, per-call and cancel
    /// timeouts given separately (the tests use short call and cancel
    /// timeouts and a roomy connect one, so setup is not what they time).
    async fn connect_with_timeouts(
        url: &str,
        token: Option<&str>,
        connect_timeout: Duration,
        timeout: Duration,
        cancel_timeout: Duration,
    ) -> Result<Self, CoreError> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string())
            // the cancel rmcp posts after a timed-out call has its own bound
            .control_request_timeout(cancel_timeout);
        if let Some(token) = token {
            config = config.auth_header(token);
        }
        ensure_tls_provider();
        let transport = StreamableHttpClientTransport::from_config(config);
        let client =
            tokio::time::timeout(connect_timeout, ClientConfig::default().serve(transport))
                .await
                .map_err(|_| failure("Timeout", "connecting to the remote timed out"))?
                .map_err(storage)?;
        Ok(Self { client, timeout })
    }

    /// Call `tool` and decode its JSON answer. `path` names the note for a
    /// `NotFound`; `expected` is the version the caller sent, for a conflict
    /// whose answer does not echo it.
    async fn call<T: DeserializeOwned>(
        &self,
        tool: &'static str,
        args: Value,
        path: Option<&str>,
        expected: Option<i64>,
    ) -> Result<T, CoreError> {
        let Value::Object(args) = args else {
            return Err(failure("Internal", "tool arguments are not an object"));
        };
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new(tool).with_arguments(args),
        ));
        // Not `call_tool`: that also drives multi-round answers
        // (`InputRequiredResult` / `CreateTaskResult`), which notelet's
        // server never sends; any such answer is "unexpected response type".
        let response = self
            .client
            .send_request_with_option(request, PeerRequestOptions::with_timeout(self.timeout))
            .await
            .map_err(storage)?
            .await_response()
            .await
            .map_err(storage)?;
        let ServerResult::CallToolResult(result) = response else {
            return Err(failure(
                "Internal",
                format!("{tool}: unexpected response type"),
            ));
        };
        if result.is_error == Some(true) {
            return Err(decode_error(&result, path, expected));
        }
        let value = decode_value(&result)?;
        serde_json::from_value(value)
            .map_err(|e| failure("Internal", format!("{tool}: unreadable answer: {e}")))
    }
}

/// The JSON a successful result carries: its first text block parsed, else
/// the structured content, else the first block as a value.
fn decode_value(r: &CallToolResult) -> Result<Value, CoreError> {
    if let Some(text) = r.content.first().and_then(|c| c.as_text()) {
        return serde_json::from_str(&text.text)
            .map_err(|e| failure("Internal", format!("answer is not JSON: {e}")));
    }
    if let Some(v) = &r.structured_content {
        return Ok(v.clone());
    }
    r.content
        .first()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| failure("Internal", format!("unreadable answer: {e}")))?
        .ok_or_else(|| failure("Internal", "empty answer"))
}

/// Map a failed tool result back onto [`CoreError`] (see the module docs).
fn decode_error(r: &CallToolResult, path: Option<&str>, expected: Option<i64>) -> CoreError {
    let text: String = r
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect();
    let Ok(body) = serde_json::from_str::<Value>(&text) else {
        return failure("Unknown", text);
    };
    let kind = body["kind"].as_str().unwrap_or("Unknown");
    let message = body["message"].as_str().unwrap_or("").to_string();
    match kind {
        "NotFound" => CoreError::NotFound(path.map_or_else(
            || {
                message
                    .strip_prefix("not found: ")
                    .unwrap_or(&message)
                    .to_string()
            },
            str::to_string,
        )),
        "Conflict" => match body["current_version"].as_i64() {
            Some(current) => CoreError::VersionConflict {
                path: path.unwrap_or_default().to_string(),
                expected: body.get("expected_version").map_or(expected, Value::as_i64),
                current,
            },
            None => failure(kind, message),
        },
        "Validation" => {
            let strip = |prefix: &str| message.strip_prefix(prefix).map(str::to_string);
            if let Some(m) = strip("invalid filter: ") {
                CoreError::InvalidFilter(m)
            } else if let Some(m) = strip("invalid tail: ") {
                CoreError::InvalidTail(m)
            } else {
                CoreError::InvalidPath(strip("invalid path: ").unwrap_or(message))
            }
        }
        other => failure(other, message),
    }
}

/// `Some(NotFound)` -> `None`, any other error stays.
fn absent<T>(r: Result<T, CoreError>) -> Result<Option<T>, CoreError> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(CoreError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `delete` sends the version it read, not the caller's. When the caller
/// passed none, a conflict must say so (`expected: None`), not name our
/// version.
fn report_expected(e: CoreError, caller_expected: Option<i64>) -> CoreError {
    match e {
        CoreError::VersionConflict { path, current, .. } if caller_expected.is_none() => {
            CoreError::VersionConflict {
                path,
                expected: None,
                current,
            }
        }
        e => e,
    }
}

fn note_of(v: NoteView) -> Note {
    Note {
        path: v.path,
        body: v.body,
        version: v.version,
        updated_at: v.updated_at,
        updated_by: v.updated_by,
    }
}

/// A filter back to its wire strings.
fn filter_json(f: &Filter) -> Value {
    let op = serde_json::to_value(f.op).unwrap_or_default();
    json!({
        "field": f.field.column(),
        "op": op,
        "value": match &f.value {
            FilterValue::Text(s) => s.clone(),
            FilterValue::Time(t) => t.to_rfc3339(),
        },
    })
}

#[async_trait]
impl NoteRepo for RemoteNoteRepo {
    async fn get(&self, path: &NotePath) -> Result<Option<Note>, CoreError> {
        let r: Result<NoteView, _> = self
            .call(
                "note_get",
                json!({ "path": path.as_str() }),
                Some(path.as_str()),
                None,
            )
            .await;
        Ok(absent(r)?.map(note_of))
    }

    async fn get_view(&self, path: &NotePath, tail: &Tail) -> Result<Option<NoteView>, CoreError> {
        let r = self
            .call(
                "note_get",
                json!({
                    "path": path.as_str(),
                    "tail_lines": tail.lines(),
                    "max_bytes": tail.max_bytes(),
                }),
                Some(path.as_str()),
                None,
            )
            .await;
        absent(r)
    }

    async fn put(
        &self,
        path: &NotePath,
        body: &str,
        expected: Option<i64>,
        _by: &str,
    ) -> Result<PutResult, CoreError> {
        let w: WriteOutcome = self
            .call(
                "note_put",
                json!({ "path": path.as_str(), "body": body, "expected_version": expected }),
                Some(path.as_str()),
                expected,
            )
            .await?;
        Ok(PutResult {
            note: Note {
                path: path.clone(),
                body: body.to_string(),
                version: w.version,
                updated_at: w.updated_at,
                updated_by: String::new(),
            },
            changed: w.changed,
        })
    }

    async fn append(&self, path: &NotePath, text: &str, _by: &str) -> Result<Note, CoreError> {
        let _written: WriteOutcome = self
            .call(
                "note_append",
                json!({ "path": path.as_str(), "text": text }),
                Some(path.as_str()),
                None,
            )
            .await?;
        let view: NoteView = self
            .call(
                "note_get",
                json!({ "path": path.as_str() }),
                Some(path.as_str()),
                None,
            )
            .await?;
        Ok(note_of(view))
    }

    async fn delete(
        &self,
        path: &NotePath,
        expected: Option<i64>,
        _by: &str,
    ) -> Result<Note, CoreError> {
        let note = self
            .get(path)
            .await?
            .ok_or_else(|| CoreError::NotFound(path.as_str().to_string()))?;
        if let Some(e) = expected {
            if e != note.version {
                return Err(CoreError::VersionConflict {
                    path: path.as_str().to_string(),
                    expected,
                    current: note.version,
                });
            }
        }
        let _removed: WriteOutcome = self
            .call(
                "note_delete",
                json!({ "path": path.as_str(), "expected_version": note.version }),
                Some(path.as_str()),
                Some(note.version),
            )
            .await
            .map_err(|e| report_expected(e, expected))?;
        Ok(note)
    }

    async fn list(
        &self,
        prefix: &str,
        filters: &[Filter],
        page: &Page,
    ) -> Result<PageOut<NoteMeta>, CoreError> {
        self.call(
            "note_list",
            json!({
                "prefix": prefix,
                "filters": filters.iter().map(filter_json).collect::<Vec<_>>(),
                "limit": page.limit,
                "cursor": page.cursor,
            }),
            None,
            None,
        )
        .await
    }

    async fn search(
        &self,
        text: &str,
        prefix: &str,
        page: &Page,
    ) -> Result<PageOut<SearchHit>, CoreError> {
        self.call(
            "note_search",
            json!({
                "text": text,
                "prefix": prefix,
                "limit": page.limit,
                "cursor": page.cursor,
            }),
            None,
            None,
        )
        .await
    }

    async fn history(&self, path: &NotePath, limit: u32) -> Result<Vec<Revision>, CoreError> {
        self.call(
            "note_history",
            json!({ "path": path.as_str(), "limit": limit, "include_body": true }),
            Some(path.as_str()),
            None,
        )
        .await
    }

    async fn history_meta(
        &self,
        path: &NotePath,
        limit: u32,
    ) -> Result<Vec<RevisionMeta>, CoreError> {
        self.call(
            "note_history",
            json!({ "path": path.as_str(), "limit": limit, "include_body": false }),
            Some(path.as_str()),
            None,
        )
        .await
    }

    async fn dump(&self, prefix: &str, page: &Page) -> Result<PageOut<DumpEntry>, CoreError> {
        self.call(
            "note_dump",
            json!({ "prefix": prefix, "limit": page.limit, "cursor": page.cursor }),
            None,
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use notelet_core::{MemoryNoteRepo, NoteService};

    use super::*;
    use crate::auth::{digest, Actor, TokenTable};
    use crate::config::Config;

    const TOKEN: &str = "throwaway-remote-token";

    /// A loopback `serve` router on an ephemeral port over a memory store,
    /// with one token that writes as `alice`. Returns the store (to look at
    /// what the server recorded) and a repo pointed at it.
    async fn fixture() -> (Arc<MemoryNoteRepo>, RemoteNoteRepo) {
        let mem = Arc::new(MemoryNoteRepo::default());
        let svc = Arc::new(NoteService::new(mem.clone()));
        let mut tokens = TokenTable::default();
        assert!(tokens.insert(digest(TOKEN), Actor::new("alice")));
        let cfg = Config {
            tokens,
            bind: "127.0.0.1:0".parse().unwrap(),
            actor: "fallback".into(),
            file_root: None,
        };
        let router = crate::server::build_router(svc, &cfg);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let repo = RemoteNoteRepo::connect(&format!("http://{addr}/mcp"), Some(TOKEN))
            .await
            .unwrap();
        (mem, repo)
    }

    fn p(s: &str) -> NotePath {
        NotePath::parse(s).unwrap()
    }

    #[tokio::test]
    async fn put_then_get_round_trips_and_the_token_actor_is_recorded() {
        let (mem, repo) = fixture().await;
        let r = repo
            .put(&p("n/a.md"), "héllo", None, "ignored")
            .await
            .unwrap();
        assert!(r.changed);
        assert_eq!(r.note.version, 1);
        assert_eq!(r.note.body, "héllo");
        assert_eq!(r.note.updated_by, "", "synthesised: not known here");

        let got = repo.get(&p("n/a.md")).await.unwrap().unwrap();
        assert_eq!(got.body, "héllo");
        assert_eq!(got.version, 1);
        assert_eq!(got.updated_at, r.note.updated_at);
        // the server recorded the actor of the token, not `by`
        assert_eq!(got.updated_by, "alice");
        let stored = mem.get(&p("n/a.md")).await.unwrap().unwrap();
        assert_eq!(stored.updated_by, "alice");

        let same = repo.put(&p("n/a.md"), "héllo", Some(1), "x").await.unwrap();
        assert!(!same.changed);

        assert!(repo.get(&p("n/none.md")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn append_records_the_actor_and_returns_the_read_back_note() {
        let (mem, repo) = fixture().await;
        repo.append(&p("n/j.md"), "one\n", "ignored").await.unwrap();
        let n = repo.append(&p("n/j.md"), "two\n", "ignored").await.unwrap();
        assert_eq!(n.body, "one\ntwo\n");
        assert_eq!(n.updated_by, "alice");
        let stored = mem.get(&p("n/j.md")).await.unwrap().unwrap();
        assert_eq!(stored.version, n.version);
        assert_eq!(stored.updated_by, "alice");
    }

    #[tokio::test]
    async fn version_mismatch_is_a_conflict_with_the_current_version() {
        let (_mem, repo) = fixture().await;
        repo.put(&p("n/c.md"), "one", None, "x").await.unwrap();
        repo.put(&p("n/c.md"), "two", Some(1), "x").await.unwrap();

        match repo.put(&p("n/c.md"), "three", Some(1), "x").await {
            Err(CoreError::VersionConflict {
                path,
                expected,
                current,
            }) => {
                assert_eq!(path, "n/c.md");
                assert_eq!(expected, Some(1));
                assert_eq!(current, 2);
            }
            other => panic!("expected conflict, got {other:?}"),
        }
        // creating what exists: expected None
        match repo.put(&p("n/c.md"), "again", None, "x").await {
            Err(CoreError::VersionConflict {
                expected, current, ..
            }) => assert_eq!((expected, current), (None, 2)),
            other => panic!("expected conflict, got {other:?}"),
        }
        // replacing what is missing
        assert!(matches!(
            repo.put(&p("n/missing.md"), "x", Some(1), "x").await,
            Err(CoreError::NotFound(path)) if path == "n/missing.md"
        ));
    }

    #[tokio::test]
    async fn delete_returns_the_body_and_the_note_is_gone() {
        let (mem, repo) = fixture().await;
        repo.put(&p("n/d.md"), "bye", None, "x").await.unwrap();

        // a stale expected is refused without deleting
        assert!(matches!(
            repo.delete(&p("n/d.md"), Some(7), "x").await,
            Err(CoreError::VersionConflict {
                expected: Some(7),
                current: 1,
                ..
            })
        ));
        assert!(repo.get(&p("n/d.md")).await.unwrap().is_some());

        let gone = repo.delete(&p("n/d.md"), Some(1), "x").await.unwrap();
        assert_eq!(gone.body, "bye");
        assert_eq!(gone.version, 1);
        assert!(repo.get(&p("n/d.md")).await.unwrap().is_none());
        assert!(mem.get(&p("n/d.md")).await.unwrap().is_none());
        // the body is kept as the last revision
        let h = repo.history(&p("n/d.md"), 10).await.unwrap();
        assert_eq!(h[0].body, "bye");
        let hm = repo.history_meta(&p("n/d.md"), 10).await.unwrap();
        assert_eq!(hm[0].size, 3);

        assert!(matches!(
            repo.delete(&p("n/d.md"), None, "x").await,
            Err(CoreError::NotFound(_))
        ));
        // no expected: deletes whatever is there
        repo.put(&p("n/e.md"), "e", None, "x").await.unwrap();
        assert_eq!(
            repo.delete(&p("n/e.md"), None, "x").await.unwrap().body,
            "e"
        );
    }

    #[tokio::test]
    async fn list_pages_with_a_cursor_and_filters() {
        let (_mem, repo) = fixture().await;
        for name in ["a", "b", "c", "d", "e"] {
            repo.put(&p(&format!("l/{name}.md")), name, None, "x")
                .await
                .unwrap();
        }
        repo.put(&p("other/z.md"), "z", None, "x").await.unwrap();

        let first = repo
            .list("l/", &[], &Page::new(Some(2), None))
            .await
            .unwrap();
        let paths: Vec<_> = first.items.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["l/a.md", "l/b.md"]);
        assert_eq!(first.next_cursor.as_deref(), Some("l/b.md"));

        let second = repo
            .list("l/", &[], &Page::new(Some(2), first.next_cursor))
            .await
            .unwrap();
        let paths: Vec<_> = second.items.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["l/c.md", "l/d.md"]);

        let last = repo
            .list("l/", &[], &Page::new(Some(2), second.next_cursor))
            .await
            .unwrap();
        assert_eq!(last.items.len(), 1);
        assert!(last.next_cursor.is_none());

        let by = Filter::parse("updated_by", "eq", "alice").unwrap();
        let all = repo.list("", &[by], &Page::new(None, None)).await.unwrap();
        assert_eq!(all.items.len(), 6);
        let since = Filter::parse("updated_at", "gt", "2999-01-01T00:00:00Z").unwrap();
        let none = repo
            .list("", &[since], &Page::new(None, None))
            .await
            .unwrap();
        assert!(none.items.is_empty());
    }

    #[tokio::test]
    async fn not_found_and_validation_map_to_their_variants() {
        let (_mem, repo) = fixture().await;
        assert!(matches!(
            repo.history(&p("n/none.md"), 5).await.map(|h| h.len()),
            Ok(0)
        ));
        // a tail read is passed through
        let last_line = Tail::new(Some(1), None).unwrap();
        repo.put(&p("n/t.md"), "l1\nl2\n", None, "x").await.unwrap();
        let v = repo
            .get_view(&p("n/t.md"), &last_line)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.body, "l2\n");
        assert!(v.truncated);
        assert_eq!(v.size, 6);
        assert!(repo
            .get_view(&p("n/none.md"), &Tail::whole())
            .await
            .unwrap()
            .is_none());
        // a prefix that is not a path
        assert!(matches!(
            repo.list("../x", &[], &Page::new(None, None)).await,
            Err(CoreError::InvalidPath(_))
        ));
    }

    #[tokio::test]
    async fn search_and_dump_pass_through() {
        let (_mem, repo) = fixture().await;
        repo.put(&p("s/a.md"), "The Needle here", None, "x")
            .await
            .unwrap();
        repo.put(&p("s/b.md"), "hay", None, "x").await.unwrap();
        let hits = repo
            .search("needle", "s/", &Page::new(None, None))
            .await
            .unwrap();
        assert_eq!(hits.items.len(), 1);
        assert_eq!(hits.items[0].path.as_str(), "s/a.md");
        assert!(hits.items[0].snippet.contains("Needle"));

        let d = repo
            .dump("s/", &Page::for_bodies(Some(1), None))
            .await
            .unwrap();
        assert_eq!(d.items[0].body, "The Needle here");
        let d2 = repo
            .dump("s/", &Page::for_bodies(Some(1), d.next_cursor))
            .await
            .unwrap();
        assert_eq!(d2.items[0].body, "hay");
    }

    #[tokio::test]
    async fn a_wrong_token_fails_at_connect_and_is_a_storage_error() {
        let mem = Arc::new(MemoryNoteRepo::default());
        let svc = Arc::new(NoteService::new(mem));
        let mut tokens = TokenTable::default();
        tokens.insert(digest(TOKEN), Actor::new("alice"));
        let cfg = Config {
            tokens,
            bind: "127.0.0.1:0".parse().unwrap(),
            actor: "fallback".into(),
            file_root: None,
        };
        let router = crate::server::build_router(svc, &cfg);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let err = RemoteNoteRepo::connect(&format!("http://{addr}/mcp"), Some("wrong"))
            .await
            .err()
            .expect("must fail");
        assert!(matches!(err, CoreError::Storage(_)), "{err:?}");
        assert!(!err.chain().to_string().contains("wrong"));
    }

    #[tokio::test]
    async fn an_https_connect_fails_fast_without_a_crypto_provider_panic() {
        // Building the reqwest client for an https URL sets up rustls and
        // would panic if no process-wide crypto provider were installed.
        // Nothing listens on port 1, so the connect is refused at once.
        let started = std::time::Instant::now();
        let err = RemoteNoteRepo::connect("https://127.0.0.1:1/mcp", Some("throwaway"))
            .await
            .err()
            .expect("nothing listens there");
        assert!(matches!(err, CoreError::Storage(_)), "{err:?}");
        assert!(
            started.elapsed() < REMOTE_TIMEOUT / 3,
            "{:?}",
            started.elapsed()
        );
        assert!(!err.chain().to_string().contains("throwaway"));
    }

    #[tokio::test]
    async fn connecting_to_a_listener_that_never_answers_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let held = tokio::spawn(async move {
            let mut conns = Vec::new();
            while let Ok(c) = listener.accept().await {
                conns.push(c); // accepted, never answered
            }
        });
        let started = std::time::Instant::now();
        let err = RemoteNoteRepo::connect_with_timeouts(
            &format!("http://{addr}/mcp"),
            None,
            Duration::from_millis(300),
            Duration::from_millis(300),
            Duration::from_millis(100),
        )
        .await
        .err()
        .expect("must time out");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(err, CoreError::Storage(_)), "{err:?}");
        assert!(
            err.chain().to_string().contains("timed out"),
            "{}",
            err.chain()
        );
        held.abort();
    }

    #[tokio::test]
    async fn a_call_the_remote_never_answers_times_out() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stall = Arc::new(AtomicBool::new(false));
        let mem = Arc::new(MemoryNoteRepo::default());
        let svc = Arc::new(NoteService::new(mem));
        let cfg = Config {
            tokens: TokenTable::default(),
            bind: "127.0.0.1:0".parse().unwrap(),
            actor: "fallback".into(),
            file_root: None,
        };
        let st = stall.clone();
        let router = crate::server::build_router(svc, &cfg).layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let st = st.clone();
                async move {
                    if st.load(Ordering::SeqCst) {
                        return std::future::pending::<axum::response::Response>().await;
                    }
                    next.run(req).await
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        // setup gets a roomy connect timeout; only the calls use the short one
        let repo = Arc::new(
            RemoteNoteRepo::connect_with_timeouts(
                &format!("http://{addr}/mcp"),
                None,
                Duration::from_secs(5),
                Duration::from_millis(300),
                Duration::from_millis(100),
            )
            .await
            .unwrap(),
        );
        repo.put(&p("n/s.md"), "x", None, "x").await.unwrap();

        stall.store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let err = repo.get(&p("n/s.md")).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(err, CoreError::Storage(_)), "{err:?}");
        assert!(
            err.chain().to_string().contains("timeout"),
            "{}",
            err.chain()
        );

        // more calls than rmcp's 16 concurrent POSTs, all stalled: each fails.
        // Predicted: call timeout + 20 x cancel timeout = 2.3 s; allow 3x.
        let stalled_at = std::time::Instant::now();
        let handles: Vec<_> = (0..20)
            .map(|_| {
                let repo = repo.clone();
                tokio::spawn(async move { repo.get(&p("n/s.md")).await })
            })
            .collect();
        for h in handles {
            let r = h.await.unwrap();
            assert!(matches!(r, Err(CoreError::Storage(_))), "{r:?}");
        }
        assert!(
            stalled_at.elapsed() < Duration::from_secs(7),
            "{:?}",
            stalled_at.elapsed()
        );

        // the shared session still works once the remote answers again
        stall.store(false, Ordering::SeqCst);
        let got = repo.get(&p("n/s.md")).await.unwrap().unwrap();
        assert_eq!(got.body, "x");
        let w = repo.put(&p("n/s.md"), "y", Some(1), "x").await.unwrap();
        assert_eq!(w.note.version, 2);
    }

    fn failed(text: &str) -> CallToolResult {
        CallToolResult::error(vec![rmcp::model::ContentBlock::text(text)])
    }

    #[test]
    fn error_results_map_back_and_validation_prefixes_are_pinned() {
        // the prefixes come from CoreError's Display on the remote side
        for (err, prefix) in [
            (CoreError::InvalidPath("x".into()), "invalid path: "),
            (CoreError::InvalidFilter("x".into()), "invalid filter: "),
            (CoreError::InvalidTail("x".into()), "invalid tail: "),
        ] {
            assert!(err.to_string().starts_with(prefix), "{err}");
        }
        let v = |m: &str| failed(&json!({"kind": "Validation", "message": m}).to_string());
        assert!(matches!(
            decode_error(&v("invalid path: bad"), None, None),
            CoreError::InvalidPath(m) if m == "bad"
        ));
        assert!(matches!(
            decode_error(&v("invalid filter: bad"), None, None),
            CoreError::InvalidFilter(m) if m == "bad"
        ));
        assert!(matches!(
            decode_error(&v("invalid tail: bad"), None, None),
            CoreError::InvalidTail(m) if m == "bad"
        ));

        // conflict: current_version kept; expected from the answer, even null
        let c = failed(
            &json!({"kind": "Conflict", "message": "m", "current_version": 4, "expected_version": null})
                .to_string(),
        );
        assert!(matches!(
            decode_error(&c, Some("a/b"), Some(9)),
            CoreError::VersionConflict { path, expected: None, current: 4 } if path == "a/b"
        ));
        // expected_version absent: the one the caller sent
        let c =
            failed(&json!({"kind": "Conflict", "message": "m", "current_version": 4}).to_string());
        assert!(matches!(
            decode_error(&c, Some("a/b"), Some(9)),
            CoreError::VersionConflict {
                expected: Some(9),
                current: 4,
                ..
            }
        ));
        // conflict without current_version: not a conflict we can act on
        let c = failed(&json!({"kind": "Conflict", "message": "m"}).to_string());
        assert!(matches!(
            decode_error(&c, Some("a/b"), None),
            CoreError::Storage(_)
        ));

        // not found: the path asked for, else the message without its prefix
        let n = failed(&json!({"kind": "NotFound", "message": "not found: a/b"}).to_string());
        assert!(
            matches!(decode_error(&n, Some("x/y"), None), CoreError::NotFound(p) if p == "x/y")
        );
        assert!(matches!(decode_error(&n, None, None), CoreError::NotFound(p) if p == "a/b"));

        // anything else, and text that is not JSON, keep their text as the cause
        let i = failed(&json!({"kind": "Internal", "message": "db down"}).to_string());
        let e = decode_error(&i, None, None);
        assert!(matches!(e, CoreError::Storage(_)));
        assert!(e.chain().to_string().contains("db down"));
        let e = decode_error(&failed("plain words"), None, None);
        assert!(matches!(e, CoreError::Storage(_)));
        assert!(e.chain().to_string().contains("plain words"));
    }

    #[test]
    fn delete_conflict_reports_the_callers_expected() {
        let c = || CoreError::VersionConflict {
            path: "n/r.md".into(),
            expected: Some(1),
            current: 2,
        };
        assert!(matches!(
            report_expected(c(), None),
            CoreError::VersionConflict {
                expected: None,
                current: 2,
                ..
            }
        ));
        assert!(matches!(
            report_expected(c(), Some(1)),
            CoreError::VersionConflict {
                expected: Some(1),
                current: 2,
                ..
            }
        ));
        assert!(matches!(
            report_expected(CoreError::NotFound("x".into()), None),
            CoreError::NotFound(_)
        ));
    }
}
