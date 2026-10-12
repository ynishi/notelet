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
//! | `/ui/note/{path}` | the note's body rendered from Markdown (see [`crate::render`]), with version, updated_at and size |
//! | `/ui/search?q=&prefix=` | `search(q, prefix)` hits with snippets; an empty `q` shows only the form |
//!
//! Access: when a token is configured, every `/ui` request needs HTTP Basic
//! with that token as the password (any user name); otherwise `401` with
//! `WWW-Authenticate: Basic realm="notelet"`. Without a token (loopback
//! only, see [`crate::config`]) the view is open, like `/mcp`.
//!
//! Pages are plain HTML with inline CSS: no script, no external asset, and
//! a `Content-Security-Policy` that forbids both. The prefixes shown on `/ui/`
//! and above a listing come from a bounded scan of `list` (at most
//! [`SCAN_PAGES`] pages of 1000 paths); past that the page says the list may
//! be incomplete.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use notelet_core::{CoreError, NoteService};
use serde::Deserialize;

use crate::render::{escape, markdown, query_value};

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

/// The `/ui` routes, with Basic auth in front when `token` is set. Merge it
/// into the HTTP router; it owns its full paths.
pub fn router(svc: Arc<NoteService>, token: Option<String>) -> Router {
    let mut r = Router::new()
        .route("/ui", get(index))
        .route("/ui/", get(index))
        .route("/ui/tree/{*prefix}", get(tree))
        .route("/ui/note/{*path}", get(note))
        .route("/ui/search", get(search))
        .with_state(Ui { svc });
    if let Some(token) = token {
        let token: Arc<str> = Arc::from(token);
        r = r.layer(axum::middleware::from_fn(
            move |req: Request, next: Next| {
                let token = Arc::clone(&token);
                async move { require_basic(token, req, next).await }
            },
        ));
    }
    r
}

async fn require_basic(token: Arc<str>, req: Request, next: Next) -> Response {
    if crate::auth::basic_matches(req.headers(), &token) {
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
        r#"{crumbs}<h1>{path}</h1><p class="meta">version {version} · {updated} · {size} bytes · by {by}</p><article>{html}</article>"#,
        crumbs = crumbs(path),
        path = escape(path),
        version = n.version,
        updated = n.updated_at.format("%Y-%m-%d %H:%M:%S UTC"),
        size = n.body.len(),
        by = escape(&n.updated_by),
        html = markdown(&n.body),
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
    let prefix = sq.prefix.unwrap_or_default();
    let mut body = String::from(r#"<p><a href="/ui/">notelet</a></p><h1>Search</h1>"#);
    body.push_str(&search_form(&text, &prefix));
    if text.trim().is_empty() {
        return page("search", &body);
    }
    let cursor = sq.cursor.filter(|c| !c.is_empty());
    let hits = match ui
        .svc
        .search(&text, Some(&prefix), Some(PAGE), cursor)
        .await
    {
        Ok(h) => h,
        Err(e) => return error_page(e),
    };
    if hits.items.is_empty() {
        body.push_str("<p>No match.</p>");
    } else {
        body.push_str(r#"<ul class="hits">"#);
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
    if let Some(next) = &hits.next_cursor {
        body.push_str(&format!(
            r#"<p><a href="/ui/search?q={}&amp;prefix={}&amp;cursor={}">next</a></p>"#,
            escape(&query_value(&text)),
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

/// Links to `/ui/` and to every `/`-terminated ancestor of `path`.
fn crumbs(path: &str) -> String {
    let mut out = String::from(r#"<nav><a href="/ui/">notelet</a>"#);
    let mut end = 0;
    while let Some(i) = path[end..].find('/') {
        end += i + 1;
        if end == path.len() {
            break;
        }
        let p = escape(&path[..end]);
        out.push_str(&format!(r#" / <a href="/ui/tree/{p}">{p}</a>"#));
    }
    out.push_str("</nav>");
    out
}

fn search_form(q: &str, prefix: &str) -> String {
    format!(
        r#"<form action="/ui/search" method="get"><input type="search" name="q" value="{}" placeholder="search note bodies"> <input type="text" name="prefix" value="{}" placeholder="prefix"> <button type="submit">search</button></form>"#,
        escape(q),
        escape(prefix),
    )
}

fn error_page(e: CoreError) -> Response {
    let (status, msg) = match &e {
        CoreError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        CoreError::InvalidPath(_) | CoreError::InvalidFilter(_) => {
            (StatusCode::BAD_REQUEST, e.to_string())
        }
        _ => {
            tracing::error!(error = %e, "ui request failed");
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
pre{background:#f5f5f5;padding:.5rem;overflow-x:auto;white-space:pre-wrap}code{background:#f5f5f5}\
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
        router(svc, token.map(str::to_string))
    }

    async fn call(app: Router, uri: &str, auth: Option<&str>) -> (StatusCode, String, Response) {
        let mut req = HttpRequest::builder().uri(uri);
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
        assert!(body.contains("<h1>Title</h1>"), "{body}");
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
            .body(Body::empty())
            .unwrap();
        let resp = app(None).await.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn crumbs_link_every_ancestor() {
        let c = crumbs("projects/x/a.md");
        assert!(c.contains(r#"href="/ui/tree/projects/""#), "{c}");
        assert!(c.contains(r#"href="/ui/tree/projects/x/""#), "{c}");
        assert!(!c.contains("a.md"), "{c}");
    }
}
