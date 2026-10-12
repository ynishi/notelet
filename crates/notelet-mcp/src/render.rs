//! Markdown to HTML for the browser view ([`crate::ui`]), and the HTML /
//! URL escaping the view's templates use.
//!
//! A note body is untrusted text, so the renderer produces no active
//! content:
//!
//! - raw HTML in the Markdown (blocks and inline) is shown as escaped text,
//!   never passed through;
//! - a link keeps its `href` only when the URL is relative or uses `http`,
//!   `https` or `mailto`; any other scheme (`javascript:`, `data:`, ...)
//!   drops the link and keeps its text;
//! - an image is never loaded: it becomes a link to its URL (under the same
//!   rule) with the alt text as the label, so viewing a note fetches nothing
//!   from another host.
//!
//! Links are not resolved against the store: a relative link in a note is
//! whatever the browser makes of it.

use pulldown_cmark::{html, Event, LinkType, Options, Parser, Tag, TagEnd};

/// Render a Markdown body to an HTML fragment (no surrounding page).
pub fn markdown(body: &str) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    // For each open link / image: was its start event kept?
    let mut open: Vec<bool> = Vec::new();
    let events = Parser::new_ext(body, opts).filter_map(|ev| match ev {
        Event::Html(s) | Event::InlineHtml(s) => Some(Event::Text(s)),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let keep = safe_href(&dest_url);
            open.push(keep);
            keep.then_some(Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }))
        }
        Event::Start(Tag::Image {
            dest_url,
            title,
            id,
            ..
        }) => {
            let keep = safe_href(&dest_url);
            open.push(keep);
            keep.then_some(Event::Start(Tag::Link {
                link_type: LinkType::Inline,
                dest_url,
                title,
                id,
            }))
        }
        Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => open
            .pop()
            .unwrap_or(false)
            .then_some(Event::End(TagEnd::Link)),
        other => Some(other),
    });
    let mut out = String::with_capacity(body.len() * 3 / 2);
    html::push_html(&mut out, events);
    out
}

/// `true` when `url` is relative (no scheme) or its scheme is `http`,
/// `https` or `mailto`. Browsers ignore ASCII whitespace and control
/// characters inside a scheme (`java\tscript:`), so those are removed
/// before comparing.
pub fn safe_href(url: &str) -> bool {
    let Some(colon) = url.find(':') else {
        return true;
    };
    // A ':' after the first '/', '?' or '#' is part of a relative URL.
    if url[..colon].contains(['/', '?', '#']) {
        return true;
    }
    let scheme: String = url[..colon]
        .chars()
        .filter(|c| !(c.is_ascii_whitespace() || c.is_ascii_control()))
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(scheme.as_str(), "http" | "https" | "mailto")
}

/// Escape text for an HTML element body or a double-quoted attribute.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Percent-encode a query-string value (everything but RFC 3986
/// unreserved characters).
pub fn query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_html_is_escaped_text() {
        let out = markdown("# a\n<script>x</script>");
        assert!(out.contains("<h1>a</h1>"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
        assert!(!out.contains("<script>"), "{out}");
    }

    #[test]
    fn inline_html_is_escaped_text() {
        let out = markdown("hi <img src=x onerror=alert(1)> there");
        assert!(!out.contains("<img"), "{out}");
        assert!(out.contains("&lt;img"), "{out}");
    }

    #[test]
    fn javascript_link_loses_its_href() {
        let out = markdown("[x](javascript:alert(1))");
        assert!(!out.contains("href"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("javascript"), "{out}");
        assert!(out.contains('x'), "{out}");
        let out = markdown("[y](<JaVa\tScript:alert(1)>)");
        assert!(!out.contains("href"), "{out}");
    }

    #[test]
    fn ordinary_link_is_an_anchor() {
        let out = markdown("[x](https://example.com/a)");
        assert!(
            out.contains(r#"<a href="https://example.com/a">x</a>"#),
            "{out}"
        );
        let out = markdown("[r](other.md)");
        assert!(out.contains(r#"<a href="other.md">r</a>"#), "{out}");
    }

    #[test]
    fn image_becomes_a_link_and_is_not_loaded() {
        let out = markdown("![alt](https://example.com/i.png)");
        assert!(!out.contains("<img"), "{out}");
        assert!(
            out.contains(r#"<a href="https://example.com/i.png">alt</a>"#),
            "{out}"
        );
        let out = markdown("![alt](data:image/png;base64,AAAA)");
        assert!(!out.contains("<img") && !out.contains("href"), "{out}");
    }

    #[test]
    fn escape_and_query_value() {
        assert_eq!(
            escape(r#"<a href="x">&'"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
        assert_eq!(query_value("a b&c/d"), "a%20b%26c%2Fd");
    }
}
