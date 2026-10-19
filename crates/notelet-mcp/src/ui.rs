//! A read-only browser view of the store, served by `serve` next to `/mcp`.
//!
//! It is a projection for a human reading notes, not part of the model: it
//! calls the same [`NoteService`] methods the tools do (`get`, `list`,
//! `search`), has no route that writes, and adds nothing to the store.
//!
//! | route (GET) | shows |
//! |---|---|
//! | `/ui/` | the top-level prefixes (`projects/`, `tasks/`, `lists/` and any other that exists), top-level notes, a search form |
//! | `/ui/tree/{prefix}` | one page (100) of `list(prefix)`: path, size in bytes, updated_at; the prefixes directly below; `?cursor=` for the next page |
//! | `/ui/note/{path}` | the note's body rendered from Markdown (see [`crate::render`]), with version, updated_at and size; the whole-note reference `notelet:<path>@v<version>`, and per heading an `id`, a `¶` link and the section reference (below) |
//! | `/ui/search?q=&prefix=` | up to three groups, in order. **Prefix match**: note paths whose part after `prefix` starts with `q` (ASCII case-insensitive), each cut to its minimal match (the path up to the end of the segment holding the end of the match, with the trailing `/` when that is not the whole path), so `alc/x.md` and `alc/y.md` give one row `alc/`. **Path match**: paths whose part after `prefix` contains `q` but does not start with it (matching is on the part of the path after `prefix`, which is added back to each row), cut the same way at the first occurrence (`projects/alc/journal.md` gives `projects/alc/`). A cut ending in `/` links to `/ui/tree/`, a whole path to `/ui/note/`; rows are deduplicated and sorted. Both come from a bounded scan of `list(prefix)` that shares [`SCAN_PAGES`] with the prefix scan (the truncation notice is shown when it stops early), and only on the first page: a `cursor` page shows only the third group. **Notes**: `search(q, prefix)` hits (path or body) with snippets and the `next` link. An empty group is not shown; `No match.` appears only when all three are empty. A leading `/` of `prefix` is stripped before any use. An empty `q` with a non-empty `prefix` answers `303 See Other` to `/ui/tree/<prefix>` (an invalid prefix gets the error page); both empty shows only the form |
//!
//! Access: when tokens are configured, every `/ui` request needs HTTP Basic
//! with any listed token as the password (any user name; the same table as
//! `/mcp`, see [`crate::auth`]); otherwise `401` with
//! `WWW-Authenticate: Basic realm="notelet", charset="UTF-8"`. The view is
//! read-only, so which token was used does not matter here. Without tokens
//! (loopback binds only, see [`crate::config`]) there is no
//! credential, so the view accepts only a loopback `Host` (see
//! [`loopback_host`]) and answers `403` to anything else, the same guard
//! against DNS rebinding that `/mcp` gets from rmcp's `allowed_hosts`.
//!
//! References: so a reader can tell an agent which part of which version they
//! mean, the note page shows `notelet:<path>@v<version>` at the top and, next
//! to each heading, `notelet:<path>@v<version>#L<first>-L<last> "<heading
//! line>"` as a `<code class="ref">` with `user-select: all` (one click
//! selects it). Lines are 1-based lines of the body as `note_get` returns it;
//! a section runs from its heading to the line before the next heading of the
//! same or a higher level (see [`crate::render::note_markdown`]). An agent
//! resolves one with `note_get`; when the version has moved on it uses
//! `note_history` for a version a `note_put` or `note_delete` replaced (a
//! version moved on only by `note_append` is not in history, but appends only
//! add lines at the end, so the range holds as given and the last section may
//! now run longer; see `notelet://guides/onboarding`). It is plain text: no script,
//! and the policy below is unchanged.
//!
//! Pages are plain HTML with inline CSS: no script, no external asset, a
//! `Content-Security-Policy` that forbids both, and `Cache-Control:
//! no-store` so note bodies are not kept in a browser or proxy cache. The prefixes shown on `/ui/`
//! and above a listing come from a bounded scan of `list` (at most
//! [`SCAN_PAGES`] pages of 1000 paths); past that the page says the list may
//! be incomplete.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use notelet_core::domain::validate_prefix;
use notelet_core::{CoreError, NoteService};
use serde::Deserialize;

use crate::auth::TokenTable;
use crate::render::{escape, note_markdown, note_ref, query_value};

/// Prefixes always offered on `/ui/`, whether or not anything is under them
/// yet (the path convention from `notelet_core`).
const CONVENTIONAL: [&str; 3] = ["projects/", "tasks/", "lists/"];

/// Upper bound on `list` pages read to find the prefixes below a prefix.
pub const SCAN_PAGES: usize = 5;

/// Rows on one listing page.
const PAGE: u32 = 100;

#[derive(Clone)]
struct Ui {
    svc: Arc<NoteService>,
}

/// The `/ui` routes, with Basic auth in front when `tokens` is non-empty and
/// a loopback-`Host` check in front when it is empty. Merge it into the HTTP
/// router; it owns its full paths.
pub fn router(svc: Arc<NoteService>, tokens: TokenTable) -> Router {
    let mut r = Router::new()
        .route("/ui", get(index))
        .route("/ui/", get(index))
        .route("/ui/tree/{*prefix}", get(tree))
        .route("/ui/note/{*path}", get(note))
        .route("/ui/search", get(search))
        .with_state(Ui { svc });
    if !tokens.is_empty() {
        let tokens = Arc::new(tokens);
        r = r.layer(axum::middleware::from_fn(
            move |req: Request, next: Next| {
                let tokens = Arc::clone(&tokens);
                async move { require_basic(tokens, req, next).await }
            },
        ));
    } else {
        r = r.layer(axum::middleware::from_fn(require_loopback_host));
    }
    r
}

/// Without a token: pass only requests whose `Host` (or, when there is no
/// `Host` header as with HTTP/2, the request URI's authority) is loopback.
async fn require_loopback_host(req: Request, next: Next) -> Response {
    let host = match req.headers().get(header::HOST) {
        Some(v) => v.to_str().ok(),
        None => req.uri().authority().map(|a| a.as_str()),
    };
    if host.is_some_and(loopback_host) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "forbidden: host not allowed").into_response()
    }
}

/// `true` when a `Host` value names this machine: `localhost`, `127.0.0.1`
/// or `::1` (bare or as `[::1]`), with or without a numeric `:port`.
/// Matching is case-insensitive, as rmcp's `allowed_hosts` check is (it
/// lowercases before comparing), so `LOCALHOST:8080` is accepted. Anything
/// else, including other `127.0.0.0/8` addresses, a malformed port or
/// trailing junk, is rejected.
pub(crate) fn loopback_host(value: &str) -> bool {
    fn port_ok(p: &str) -> bool {
        !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())
    }
    if let Some(rest) = value.strip_prefix('[') {
        // brackets are only for an IPv6 literal
        let Some((inner, after)) = rest.split_once(']') else {
            return false;
        };
        let port_fine = after.is_empty() || after.strip_prefix(':').is_some_and(port_ok);
        return port_fine && inner == "::1";
    }
    let host = if value == "::1" {
        value
    } else {
        match value.split_once(':') {
            None => value,
            Some((h, p)) if port_ok(p) => h,
            Some(_) => return false,
        }
    };
    ["localhost", "127.0.0.1", "::1"]
        .iter()
        .any(|ok| host.eq_ignore_ascii_case(ok))
}

async fn require_basic(tokens: Arc<TokenTable>, req: Request, next: Next) -> Response {
    if crate::auth::basic_actor(req.headers(), &tokens).is_some() {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static(r#"Basic realm="notelet", charset="UTF-8""#),
            )],
            "authentication required",
        )
            .into_response()
    }
}

// --- handlers ---------------------------------------------------------------

async fn index(State(ui): State<Ui>) -> Response {
    let scan = match children(&ui.svc, "").await {
        Ok(s) => s,
        Err(e) => return error_page(e),
    };
    let mut prefixes: BTreeSet<String> = CONVENTIONAL.iter().map(|p| p.to_string()).collect();
    prefixes.extend(scan.prefixes);
    let mut body = String::from("<h1>notelet</h1>");
    body.push_str(&search_form("", ""));
    body.push_str("<h2>Prefixes</h2><ul>");
    for p in &prefixes {
        body.push_str(&format!(
            r#"<li><a href="/ui/tree/{0}">{0}</a></li>"#,
            escape(p)
        ));
    }
    body.push_str("</ul>");
    if !scan.notes.is_empty() {
        body.push_str("<h2>Top-level notes</h2><ul>");
        for n in &scan.notes {
            body.push_str(&format!(
                r#"<li><a href="/ui/note/{0}">{0}</a></li>"#,
                escape(n)
            ));
        }
        body.push_str("</ul>");
    }
    if scan.truncated {
        body.push_str(TRUNCATED);
    }
    page("notelet", &body)
}

#[derive(Debug, Default, Deserialize)]
struct TreeQuery {
    cursor: Option<String>,
}

async fn tree(
    State(ui): State<Ui>,
    Path(prefix): Path<String>,
    Query(q): Query<TreeQuery>,
) -> Response {
    let cursor = q.cursor.filter(|c| !c.is_empty());
    let listing = match ui.svc.list(Some(&prefix), &[], Some(PAGE), cursor).await {
        Ok(p) => p,
        Err(e) => return error_page(e),
    };
    let scan = match children(&ui.svc, &prefix).await {
        Ok(s) => s,
        Err(e) => return error_page(e),
    };
    let mut body = format!("{}<h1>{}</h1>", crumbs(&prefix), escape(&prefix));
    body.push_str(&search_form("", &prefix));
    if !scan.prefixes.is_empty() {
        body.push_str("<h2>Below</h2><ul>");
        for p in &scan.prefixes {
            body.push_str(&format!(
                r#"<li><a href="/ui/tree/{0}">{0}</a></li>"#,
                escape(p)
            ));
        }
        body.push_str("</ul>");
        if scan.truncated {
            body.push_str(TRUNCATED);
        }
    }
    if listing.items.is_empty() {
        body.push_str("<p>No notes under this prefix.</p>");
    } else {
        body.push_str(
            r#"<table><thead><tr><th>path</th><th class="n">bytes</th><th>updated_at</th></tr></thead><tbody>"#,
        );
        for m in &listing.items {
            body.push_str(&format!(
                r#"<tr><td><a href="/ui/note/{0}">{0}</a></td><td class="n">{1}</td><td>{2}</td></tr>"#,
                escape(m.path.as_str()),
                m.size,
                m.updated_at.format("%Y-%m-%d %H:%M:%S UTC"),
            ));
        }
        body.push_str("</tbody></table>");
    }
    if let Some(next) = &listing.next_cursor {
        body.push_str(&format!(
            r#"<p><a href="/ui/tree/{}?cursor={}">next</a></p>"#,
            escape(&prefix),
            escape(&query_value(next)),
        ));
    }
    page(&prefix, &body)
}

async fn note(State(ui): State<Ui>, Path(path): Path<String>) -> Response {
    let n = match ui.svc.get(&path).await {
        Ok(n) => n,
        Err(e) => return error_page(e),
    };
    let path = n.path.as_str();
    let body = format!(
        r#"{crumbs}<h1>{path}</h1><p class="meta">version {version} · {updated} · {size} bytes · by {by}</p><p class="meta"><code class="ref">{whole}</code></p><article>{html}</article>"#,
        crumbs = crumbs(path),
        path = escape(path),
        version = n.version,
        updated = n.updated_at.format("%Y-%m-%d %H:%M:%S UTC"),
        size = n.body.len(),
        by = escape(&n.updated_by),
        whole = escape(&note_ref(path, n.version)),
        html = note_markdown(&n.body, path, n.version),
    );
    page(path, &body)
}

#[derive(Debug, Default, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    prefix: Option<String>,
    cursor: Option<String>,
}

async fn search(State(ui): State<Ui>, Query(sq): Query<SearchQuery>) -> Response {
    let text = sq.q.unwrap_or_default();
    let prefix = sq
        .prefix
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string();
    let q = text.trim();
    if q.is_empty() {
        if prefix.is_empty() {
            let mut body = String::from(r#"<p><a href="/ui/">notelet</a></p><h1>Search</h1>"#);
            body.push_str(&search_form(&text, &prefix));
            return page("search", &body);
        }
        if let Err(e) = validate_prefix(&prefix) {
            return error_page(e);
        }
        return Redirect::to(&format!("/ui/tree/{prefix}")).into_response();
    }
    let cursor = sq.cursor.filter(|c| !c.is_empty());
    let mut body = String::from(r#"<p><a href="/ui/">notelet</a></p><h1>Search</h1>"#);
    body.push_str(&search_form(q, &prefix));
    let mut shown = false;
    if cursor.is_none() {
        let (starts, contains, truncated) = match path_matches(&ui.svc, &prefix, q).await {
            Ok(m) => m,
            Err(e) => return error_page(e),
        };
        for (title, rows) in [("Prefix match", &starts), ("Path match", &contains)] {
            if rows.is_empty() {
                continue;
            }
            shown = true;
            body.push_str(&format!("<h2>{title}</h2><ul>"));
            for cut in rows {
                let kind = if cut.ends_with('/') { "tree" } else { "note" };
                body.push_str(&format!(
                    r#"<li><a href="/ui/{kind}/{0}">{0}</a></li>"#,
                    escape(cut)
                ));
            }
            body.push_str("</ul>");
        }
        if truncated {
            body.push_str(TRUNCATED);
        }
    }
    let hits = match ui.svc.search(q, Some(&prefix), Some(PAGE), cursor).await {
        Ok(h) => h,
        Err(e) => return error_page(e),
    };
    if !hits.items.is_empty() {
        shown = true;
        body.push_str(r#"<h2>Notes</h2><ul class="hits">"#);
        for h in &hits.items {
            body.push_str(&format!(
                r#"<li><a href="/ui/note/{0}">{0}</a> <span class="meta">{1}</span><pre>{2}</pre></li>"#,
                escape(h.path.as_str()),
                h.updated_at.format("%Y-%m-%d %H:%M:%S UTC"),
                escape(&h.snippet),
            ));
        }
        body.push_str("</ul>");
    }
    if !shown {
        body.push_str("<p>No match.</p>");
    }
    if let Some(next) = &hits.next_cursor {
        body.push_str(&format!(
            r#"<p><a href="/ui/search?q={}&amp;prefix={}&amp;cursor={}">next</a></p>"#,
            escape(&query_value(q)),
            escape(&query_value(&prefix)),
            escape(&query_value(next)),
        ));
    }
    page("search", &body)
}

// --- helpers ----------------------------------------------------------------

const TRUNCATED: &str =
    r#"<p class="meta">Only the first paths were scanned; this list may be incomplete.</p>"#;

/// What sits directly below `prefix`: deeper prefixes (ending in `/`) and
/// notes at this level.
struct Children {
    prefixes: BTreeSet<String>,
    notes: Vec<String>,
    truncated: bool,
}

/// Scan up to [`SCAN_PAGES`] pages of `list(prefix)` and group the paths by
/// their next segment. Grouping is done here rather than by cursor tricks
/// because the store's ordering follows the database collation.
async fn children(svc: &NoteService, prefix: &str) -> Result<Children, CoreError> {
    let mut out = Children {
        prefixes: BTreeSet::new(),
        notes: Vec::new(),
        truncated: false,
    };
    let mut cursor = None;
    for _ in 0..SCAN_PAGES {
        let page = svc.list(Some(prefix), &[], Some(1000), cursor).await?;
        for m in &page.items {
            let rest = &m.path.as_str()[prefix.len()..];
            match rest.find('/') {
                Some(i) => {
                    out.prefixes.insert(format!("{prefix}{}", &rest[..=i]));
                }
                None => out.notes.push(m.path.as_str().to_string()),
            }
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => return Ok(out),
        }
    }
    out.truncated = true;
    Ok(out)
}

/// The minimal match of `q` in `path`: `path` cut at the end of the segment
/// that holds the end of the first (ASCII case-insensitive) occurrence of
/// `q`, keeping the trailing `/` when the cut is not the whole path. `None`
/// when `q` does not occur. Paths are ASCII, so byte offsets agree between
/// the path and its lowercase form.
fn minimal_match(path: &str, q: &str) -> Option<(String, bool)> {
    let lower = path.to_ascii_lowercase();
    let at = lower.find(&q.to_ascii_lowercase())?;
    let last = at + q.len() - 1;
    let end = match path[last..].find('/') {
        Some(i) => last + i + 1,
        None => path.len(),
    };
    Some((path[..end].to_string(), at == 0))
}

/// Scan up to [`SCAN_PAGES`] pages of `list(prefix)` and return the minimal
/// matches of `q` that start the part of a path after `prefix`, those that only occur inside one, and
/// whether the scan stopped early.
async fn path_matches(
    svc: &NoteService,
    prefix: &str,
    q: &str,
) -> Result<(BTreeSet<String>, BTreeSet<String>, bool), CoreError> {
    let mut starts = BTreeSet::new();
    let mut contains = BTreeSet::new();
    let mut cursor = None;
    for _ in 0..SCAN_PAGES {
        let page = svc.list(Some(prefix), &[], Some(1000), cursor).await?;
        for m in &page.items {
            let rest = m
                .path
                .as_str()
                .strip_prefix(prefix)
                .unwrap_or(m.path.as_str());
            let head = &m.path.as_str()[..m.path.as_str().len() - rest.len()];
            match minimal_match(rest, q) {
                Some((cut, true)) => {
                    starts.insert(format!("{head}{cut}"));
                }
                Some((cut, false)) => {
                    contains.insert(format!("{head}{cut}"));
                }
                None => {}
            }
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => return Ok((starts, contains, false)),
        }
    }
    Ok((starts, contains, true))
}

/// Links to `/ui/` and to every `/`-terminated ancestor of `path`. Each link
/// shows only its own segment (`notelet / tasks / x`), the href is the full
/// prefix.
fn crumbs(path: &str) -> String {
    let mut out = String::from(r#"<nav><a href="/ui/">notelet</a>"#);
    let mut end = 0;
    while let Some(i) = path[end..].find('/') {
        let start = end;
        end += i + 1;
        if end == path.len() {
            break;
        }
        let href = escape(&path[..end]);
        let label = escape(&path[start..end - 1]);
        out.push_str(&format!(r#" / <a href="/ui/tree/{href}">{label}</a>"#));
    }
    out.push_str("</nav>");
    out
}

fn search_form(q: &str, prefix: &str) -> String {
    format!(
        r#"<form action="/ui/search" method="get"><input type="search" name="q" value="{}" placeholder="search paths and bodies"> <input type="text" name="prefix" value="{}" placeholder="prefix"> <button type="submit">search</button></form>"#,
        escape(q),
        escape(prefix),
    )
}

fn error_page(e: CoreError) -> Response {
    let (status, msg) = match &e {
        CoreError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        CoreError::InvalidPath(_) | CoreError::InvalidFilter(_) | CoreError::InvalidTail(_) => {
            (StatusCode::BAD_REQUEST, e.to_string())
        }
        _ => {
            tracing::error!(error = %e.chain(), "ui request failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
        }
    };
    let body = format!(
        r#"<p><a href="/ui/">notelet</a></p><h1>{}</h1><p>{}</p>"#,
        status.as_u16(),
        escape(&msg)
    );
    let mut r = page("error", &body);
    *r.status_mut() = status;
    r
}

const CSS: &str = "body{font:15px/1.55 system-ui,sans-serif;max-width:60rem;margin:0 auto;padding:1rem;color:#1d1d1f;background:#fff}\
a{color:#0b57d0}nav{margin-bottom:.5rem}nav,.meta{color:#666;font-size:.9em}\
table{border-collapse:collapse;width:100%}td,th{padding:.25rem .5rem;border-bottom:1px solid #ddd;text-align:left}.n{text-align:right}\
pre{background:#f5f5f5;padding:.5rem;overflow-x:auto;white-space:pre-wrap}code{background:#f5f5f5}.ref{-webkit-user-select:all;user-select:all;font-size:.8rem;font-weight:normal}a.anchor{text-decoration:none;font-size:.8em}\
article{border-top:1px solid #ddd;padding-top:.5rem}ul.hits{list-style:none;padding:0}form{margin:.5rem 0}\
@media (prefers-color-scheme:dark){body{color:#e6e6e6;background:#151515}a{color:#8ab4f8}pre,code{background:#222}td,th,article{border-color:#333}}";

/// Wrap a fragment in a page. `title` is escaped here.
fn page(title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="referrer" content="no-referrer"><title>{} · notelet</title><style>{CSS}</style></head><body>{body}</body></html>"#,
        escape(title)
    );
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
                ),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        html,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use notelet_core::MemoryNoteRepo;
    use tower::ServiceExt;

    // "anyone:secret" and "anyone:wrong"
    const GOOD: &str = "Basic YW55b25lOnNlY3JldA==";
    const BAD: &str = "Basic YW55b25lOndyb25n";

    async fn app(token: Option<&str>) -> Router {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        svc.put("n/a.md", "# Title\n\nsome **bold** needle\n", None, "t")
            .await
            .unwrap();
        svc.put("projects/x/b.md", "other", None, "t")
            .await
            .unwrap();
        let mut tokens = TokenTable::default();
        if let Some(t) = token {
            tokens.insert(crate::auth::digest(t), crate::auth::Actor::new("tester"));
        }
        router(svc, tokens)
    }

    async fn call(app: Router, uri: &str, auth: Option<&str>) -> (StatusCode, String, Response) {
        let mut req = HttpRequest::builder()
            .uri(uri)
            .header(header::HOST, "localhost:8490");
        if let Some(a) = auth {
            req = req.header(header::AUTHORIZATION, a);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let (parts, body) = resp.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        (status, text, Response::from_parts(parts, Body::empty()))
    }

    #[tokio::test]
    async fn note_requires_basic_with_the_token() {
        let (s, _, r) = call(app(Some("secret")).await, "/ui/note/n/a.md", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(r.headers()[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .starts_with(r#"Basic realm="notelet""#));

        let (s, _, _) = call(app(Some("secret")).await, "/ui/note/n/a.md", Some(BAD)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);

        let (s, body, r) = call(app(Some("secret")).await, "/ui/note/n/a.md", Some(GOOD)).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"<h1 id="title">Title "#), "{body}");
        assert!(body.contains("<strong>bold</strong>"), "{body}");
        assert!(body.contains("version 1"), "{body}");
        assert!(r.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
    }

    #[tokio::test]
    async fn tree_lists_paths_and_child_prefixes() {
        let (s, body, _) = call(app(Some("secret")).await, "/ui/tree/n/", Some(GOOD)).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"href="/ui/note/n/a.md""#), "{body}");

        let (_, body, _) = call(app(None).await, "/ui/tree/projects/", None).await;
        assert!(body.contains(r#"href="/ui/tree/projects/x/""#), "{body}");
        assert!(body.contains("projects/x/b.md"), "{body}");
    }

    #[tokio::test]
    async fn search_shows_hits_and_empty_query_shows_form() {
        let (s, body, _) = call(app(Some("secret")).await, "/ui/search?q=NEEDLE", Some(GOOD)).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"href="/ui/note/n/a.md""#), "{body}");
        assert!(body.contains("needle"), "{body}");

        let (s, body, _) = call(app(Some("secret")).await, "/ui/search?q=", Some(GOOD)).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"action="/ui/search""#), "{body}");
        assert!(!body.contains("/ui/note/"), "{body}");
    }

    #[tokio::test]
    async fn index_lists_conventional_and_existing_prefixes() {
        let (s, body, _) = call(app(None).await, "/ui/", None).await;
        assert_eq!(s, StatusCode::OK);
        for p in ["projects/", "tasks/", "lists/", "n/"] {
            assert!(
                body.contains(&format!(r#"href="/ui/tree/{p}""#)),
                "{p}: {body}"
            );
        }
    }

    #[tokio::test]
    async fn open_without_token_and_missing_note_is_404() {
        let (s, _, _) = call(app(None).await, "/ui/note/n/a.md", None).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, _) = call(app(None).await, "/ui/note/n/missing.md", None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_methods_are_not_routes() {
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/ui/note/n/a.md")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .unwrap();
        let resp = app(None).await.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    async fn with_host(app: Router, host: Option<&str>) -> StatusCode {
        let mut req = HttpRequest::builder().uri("/ui/");
        if let Some(h) = host {
            req = req.header(header::HOST, h);
        }
        app.oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[test]
    fn loopback_host_accepts_only_this_machine() {
        for ok in [
            "localhost",
            "localhost:8080",
            "LOCALHOST",
            "LocalHost:1",
            "127.0.0.1",
            "127.0.0.1:8490",
            "[::1]",
            "[::1]:8490",
            "::1",
        ] {
            assert!(loopback_host(ok), "{ok}");
        }
        for bad in [
            "",
            "evil.example",
            "evil.example:8080",
            "localhost.example",
            "example.localhost",
            "127.0.0.2",
            "0.0.0.0",
            "localhost:",
            "localhost:80x",
            "localhost:8080:1",
            "[::1",
            "[::1]x",
            "[::1]:",
            "[::2]:8490",
            "::1:8490",
            "[localhost]",
            "[127.0.0.1]",
        ] {
            assert!(!loopback_host(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn without_token_only_loopback_host_is_served() {
        assert_eq!(
            with_host(app(None).await, Some("localhost:8080")).await,
            StatusCode::OK
        );
        assert_eq!(
            with_host(app(None).await, Some("[::1]:8490")).await,
            StatusCode::OK
        );
        assert_eq!(
            with_host(app(None).await, Some("evil.example")).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            with_host(app(None).await, None).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn with_token_host_is_not_checked() {
        let req = HttpRequest::builder()
            .uri("/ui/")
            .header(header::HOST, "notes.example.com")
            .header(header::AUTHORIZATION, GOOD)
            .body(Body::empty())
            .unwrap();
        let resp = app(Some("secret")).await.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn pages_are_not_cached() {
        for uri in ["/ui/", "/ui/note/n/a.md", "/ui/note/n/missing.md"] {
            let (_, _, r) = call(app(None).await, uri, None).await;
            assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store", "{uri}");
        }
    }

    #[tokio::test]
    async fn raw_html_in_a_note_is_shown_escaped() {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        svc.put(
            "n/x.md",
            "before\n\n<script>alert(1)</script>\n\ninline <script>alert(2)</script> end\n",
            None,
            "t",
        )
        .await
        .unwrap();
        let (s, body, _) = call(router(svc, TokenTable::default()), "/ui/note/n/x.md", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("&lt;script&gt;alert(1)"), "{body}");
        assert!(body.contains("&lt;script&gt;alert(2)"), "{body}");
        assert!(!body.contains("<script"), "{body}");
    }

    #[tokio::test]
    async fn note_page_shows_references_with_the_version() {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        svc.put("n/r.md", "# A\none\n## B\ntwo\n", None, "t")
            .await
            .unwrap();
        svc.put("n/r.md", "# A\none\n## B\ntwo\nthree\n", Some(1), "t")
            .await
            .unwrap();
        let (s, body, r) = call(router(svc, TokenTable::default()), "/ui/note/n/r.md", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(
            body.contains(r#"<code class="ref">notelet:n/r.md@v2</code>"#),
            "{body}"
        );
        assert!(
            body.contains(r###"<code class="ref">notelet:n/r.md@v2#L1-L5 "# A"</code>"###),
            "{body}"
        );
        assert!(
            body.contains(r###"<code class="ref">notelet:n/r.md@v2#L3-L5 "## B"</code>"###),
            "{body}"
        );
        assert!(body.contains("user-select:all"), "{body}");
        assert!(!body.contains("<script"), "{body}");
        assert_eq!(
            r.headers()[header::CONTENT_SECURITY_POLICY],
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
        );
    }

    #[tokio::test]
    async fn heading_with_raw_html_stays_escaped() {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        svc.put("n/h.md", "## <script>alert(1)</script> \"q\"\n", None, "t")
            .await
            .unwrap();
        let (_, body, _) = call(router(svc, TokenTable::default()), "/ui/note/n/h.md", None).await;
        assert!(body.contains("&lt;script&gt;alert(1)"), "{body}");
        assert!(!body.contains("<script"), "{body}");
    }

    #[tokio::test]
    async fn search_snippet_is_escaped() {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        svc.put("n/s.md", "if a <b> needle </b> c\n", None, "t")
            .await
            .unwrap();
        let (s, body, _) = call(
            router(svc, TokenTable::default()),
            "/ui/search?q=needle",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"href="/ui/note/n/s.md""#), "{body}");
        assert!(body.contains("&lt;b&gt;"), "{body}");
        assert!(!body.contains("<b>"), "{body}");
    }

    /// The `cursor=` value of the `next` link, still percent-encoded, with
    /// the HTML escaping undone.
    fn next_href(body: &str) -> Option<String> {
        let end = body.find(r#"">next</a>"#)?;
        let start = body[..end].rfind("href=\"")? + "href=\"".len();
        Some(body[start..end].replace("&amp;", "&"))
    }

    fn note_links(body: &str) -> Vec<String> {
        body.split(r#"<a href="/ui/note/"#)
            .skip(1)
            .filter_map(|s| s.split('"').next().map(str::to_string))
            .collect()
    }

    #[tokio::test]
    async fn tree_next_cursor_returns_the_following_page() {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        let total = PAGE as usize + 5;
        for i in 0..total {
            svc.put(&format!("p/{i:04}.md"), "x", None, "t")
                .await
                .unwrap();
        }
        let app = router(svc, TokenTable::default());

        let (s, body, _) = call(app.clone(), "/ui/tree/p/", None).await;
        assert_eq!(s, StatusCode::OK);
        let first = note_links(&body);
        assert_eq!(first.len(), PAGE as usize, "{body}");
        let next = next_href(&body).expect("first page has a next link");
        assert!(next.starts_with("/ui/tree/p/?cursor="), "{next}");

        let (s, body, _) = call(app, &next, None).await;
        assert_eq!(s, StatusCode::OK);
        let second = note_links(&body);
        assert_eq!(second.len(), total - PAGE as usize, "{body}");
        assert!(next_href(&body).is_none(), "{body}");

        let mut all: Vec<String> = first.into_iter().chain(second).collect();
        let expected: Vec<String> = (0..total).map(|i| format!("p/{i:04}.md")).collect();
        all.sort();
        assert_eq!(all, expected);
    }

    async fn paths_app(paths: &[&str]) -> Router {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        for p in paths {
            svc.put(p, "body", None, "t").await.unwrap();
        }
        router(svc, TokenTable::default())
    }

    #[tokio::test]
    async fn search_prefix_group_is_deduplicated() {
        let app = paths_app(&["alc/x.md", "alc/y.md", "alcove/z.md", "alc.md"]).await;
        let (s, body, _) = call(app, "/ui/search?q=alc", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("<h2>Prefix match</h2>"), "{body}");
        assert_eq!(body.matches(r#"href="/ui/tree/alc/""#).count(), 1, "{body}");
        assert!(body.contains(r#"href="/ui/tree/alcove/""#), "{body}");
        assert!(body.contains(r#"<li><a href="/ui/note/alc.md">"#), "{body}");
        assert!(!body.contains("<h2>Path match</h2>"), "{body}");
    }

    #[tokio::test]
    async fn search_path_group_cuts_to_segment_or_note() {
        let app = paths_app(&["projects/alc/journal.md", "note/some/alc.md"]).await;
        let (_, body, _) = call(app, "/ui/search?q=alc", None).await;
        assert!(body.contains("<h2>Path match</h2>"), "{body}");
        assert!(!body.contains("<h2>Prefix match</h2>"), "{body}");
        assert!(
            body.contains(r#"<li><a href="/ui/tree/projects/alc/">projects/alc/</a></li>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<li><a href="/ui/note/note/some/alc.md">note/some/alc.md</a></li>"#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn search_path_groups_are_case_insensitive() {
        let app = paths_app(&["alc/x.md", "projects/alc/j.md"]).await;
        let (_, body, _) = call(app, "/ui/search?q=Alc", None).await;
        assert!(body.contains(r#"href="/ui/tree/alc/""#), "{body}");
        assert!(body.contains(r#"href="/ui/tree/projects/alc/""#), "{body}");
    }

    #[tokio::test]
    async fn search_path_groups_absent_on_cursor_page() {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        for i in 0..(PAGE as usize + 5) {
            svc.put(&format!("alc/{i:04}.md"), "x", None, "t")
                .await
                .unwrap();
        }
        let app = router(svc, TokenTable::default());
        let (_, body, _) = call(app.clone(), "/ui/search?q=alc", None).await;
        assert!(body.contains("<h2>Prefix match</h2>"), "{body}");
        let next = next_href(&body).expect("first page has a next link");
        assert!(next.contains("cursor="), "{next}");
        let (_, body, _) = call(app, &next, None).await;
        assert!(body.contains("<h2>Notes</h2>"), "{body}");
        assert!(!body.contains("<h2>Prefix match</h2>"), "{body}");
        assert!(!body.contains("<h2>Path match</h2>"), "{body}");
    }

    #[tokio::test]
    async fn search_next_link_carries_the_stripped_prefix() {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        for i in 0..(PAGE as usize + 5) {
            svc.put(&format!("alc/{i:04}.md"), "x", None, "t")
                .await
                .unwrap();
        }
        let app = router(svc, TokenTable::default());
        let (_, body, _) = call(app, "/ui/search?q=%20alc%20&prefix=/alc", None).await;
        let next = next_href(&body).expect("next link");
        assert!(
            next.starts_with("/ui/search?q=alc&prefix=alc&cursor="),
            "{next}"
        );
    }

    #[tokio::test]
    async fn search_prefix_match_is_relative_to_the_user_prefix() {
        let app = paths_app(&["projects/alc/x.md", "projects/b/alc.md"]).await;
        let (_, body, _) = call(app.clone(), "/ui/search?q=alc&prefix=projects/", None).await;
        let (pm, rest) = body
            .split_once("<h2>Path match</h2>")
            .unwrap_or((&body, ""));
        assert!(pm.contains("<h2>Prefix match</h2>"), "{body}");
        assert!(pm.contains(r#"href="/ui/tree/projects/alc/""#), "{body}");
        assert!(
            rest.contains(r#"href="/ui/note/projects/b/alc.md""#),
            "{body}"
        );

        let (_, body, _) = call(app.clone(), "/ui/search?q=x&prefix=projects/", None).await;
        assert!(!body.contains("<h2>Prefix match</h2>"), "{body}");
        assert!(body.contains("<h2>Path match</h2>"), "{body}");
        assert!(
            body.contains(r#"<li><a href="/ui/note/projects/alc/x.md">"#),
            "{body}"
        );

        // A prefix that ends mid-segment: the remainder is `c/x.md`.
        let (_, body, _) = call(app, "/ui/search?q=c&prefix=projects/al", None).await;
        assert!(body.contains("<h2>Prefix match</h2>"), "{body}");
        assert!(body.contains(r#"href="/ui/tree/projects/alc/""#), "{body}");
    }

    #[tokio::test]
    async fn search_path_groups_say_when_the_scan_was_cut_short() {
        let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
        for i in 0..(SCAN_PAGES * 1000 + 1) {
            svc.put(&format!("big/{i:05}.md"), "x", None, "t")
                .await
                .unwrap();
        }
        let app = router(svc, TokenTable::default());
        let started = std::time::Instant::now();
        let (_, body, _) = call(app, "/ui/search?q=big", None).await;
        eprintln!("truncated-scan fixture + request: {:?}", started.elapsed());
        assert!(body.contains(TRUNCATED), "{body}");
        assert!(body.contains(r#"href="/ui/tree/big/""#), "{body}");
    }

    #[tokio::test]
    async fn search_empty_q_with_prefix_redirects_to_the_tree() {
        let app = paths_app(&["projects/x.md"]).await;
        let (s, _, r) = call(app.clone(), "/ui/search?q=%20&prefix=projects/", None).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(r.headers()[header::LOCATION], "/ui/tree/projects/");

        let (s, _, r) = call(app.clone(), "/ui/search?q=&prefix=/projects/", None).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(r.headers()[header::LOCATION], "/ui/tree/projects/");

        let (s, body, _) = call(app, "/ui/search?q=&prefix=a%3Cb", None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(body.contains("a&lt;b"), "{body}");
        assert!(!body.contains("a<b"), "{body}");
    }

    #[tokio::test]
    async fn search_strips_leading_slash_from_prefix() {
        let app = paths_app(&["projects/alc/j.md", "other/alc/j.md"]).await;
        let (s, body, _) = call(app, "/ui/search?q=alc&prefix=/projects", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"href="/ui/tree/projects/alc/""#), "{body}");
        assert!(!body.contains("other/"), "{body}");
        assert!(body.contains(r#"name="prefix" value="projects""#), "{body}");
    }

    #[tokio::test]
    async fn search_no_match_only_when_all_groups_empty() {
        let app = paths_app(&["alc/x.md"]).await;
        let (_, body, _) = call(app.clone(), "/ui/search?q=zzz", None).await;
        assert!(body.contains("<p>No match.</p>"), "{body}");
        assert!(!body.contains("<h2>"), "{body}");
        let (_, body, _) = call(app, "/ui/search?q=alc", None).await;
        assert!(!body.contains("No match."), "{body}");
    }

    #[tokio::test]
    async fn search_path_rows_are_escaped() {
        // Paths are restricted to a safe charset, so the rows carry no
        // markup of their own; what could carry it is the echoed query.
        let app = paths_app(&["a.b/x.md"]).await;
        let (_, body, _) = call(app.clone(), "/ui/search?q=a.b", None).await;
        assert!(
            body.contains(r#"<li><a href="/ui/tree/a.b/">a.b/</a></li>"#),
            "{body}"
        );
        let (_, body, _) = call(app, "/ui/search?q=%3Cscript%3E", None).await;
        assert!(body.contains("&lt;script&gt;"), "{body}");
        assert!(!body.contains("<script"), "{body}");
    }

    #[test]
    fn minimal_match_cuts_at_the_segment_end() {
        assert_eq!(
            minimal_match("alc/x.md", "alc"),
            Some(("alc/".into(), true))
        );
        assert_eq!(
            minimal_match("alc.md", "alc"),
            Some(("alc.md".into(), true))
        );
        assert_eq!(
            minimal_match("a/xALCy/z.md", "alc"),
            Some(("a/xALCy/".into(), false))
        );
        assert_eq!(
            minimal_match("alc/x.md", "alc/"),
            Some(("alc/".into(), true))
        );
        assert_eq!(minimal_match("a.md", "é"), None);
        // a match spanning `/` is cut at the end of the segment holding its end
        assert_eq!(
            minimal_match("alc/xyz/a.md", "c/x"),
            Some(("alc/xyz/".into(), false))
        );
        // q equal to the whole path, longer than it, and `/` alone
        assert_eq!(
            minimal_match("a/b.md", "a/b.md"),
            Some(("a/b.md".into(), true))
        );
        assert_eq!(minimal_match("a.md", "a.md.bak"), None);
        assert_eq!(minimal_match("a/b.md", "/"), Some(("a/".into(), false)));
    }

    #[test]
    fn crumbs_link_every_ancestor() {
        let c = crumbs("projects/x/a.md");
        assert!(
            c.contains(r#"href="/ui/tree/projects/">projects</a>"#),
            "{c}"
        );
        assert!(c.contains(r#"href="/ui/tree/projects/x/">x</a>"#), "{c}");
        assert!(!c.contains("a.md"), "{c}");
        assert!(
            !c.contains(">projects/x<"),
            "label is the segment only: {c}"
        );
    }
}
