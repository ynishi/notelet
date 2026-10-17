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
//! Headings carry an `id`, a `¶` link and, for a note page, the reference of
//! the section ([`note_markdown`]); all of it is generated here from the
//! escaped heading line, never from raw HTML.
//!
//! Links are not resolved against the store: a relative link in a note is
//! whatever the browser makes of it.

use std::collections::HashSet;
use std::ops::Range;

use pulldown_cmark::{html, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};

/// The reference of a whole note: `notelet:<path>@v<version>`.
pub fn note_ref(path: &str, version: i64) -> String {
    format!("notelet:{path}@v{version}")
}

/// Render a Markdown body to an HTML fragment without section references
/// (no surrounding page). Tests only; the note page uses [`note_markdown`].
///
/// Every heading gets a GitHub-style `id` and a `¶` link to it; see
/// [`note_markdown`] for the section references.
#[cfg(test)]
pub fn markdown(body: &str) -> String {
    render(body, None)
}

/// Render a Markdown body to an HTML fragment (no surrounding page). Every
/// heading gets a GitHub-style `id` and a `¶` link to it, and right after
/// the heading element (not inside it, so the heading's text stays the
/// heading) comes the reference of its section, `notelet:<path>@v<version>#L<first>-L<last>
/// "<heading line>"`, as `<code class="ref">` (the page's CSS makes one
/// click select it).
///
/// Lines are 1-based lines of `body` as `note_get` returns it. A section
/// runs from its heading line to the line before the next heading of the
/// same or a higher level, or to the last line of the body. `<heading
/// line>` is the heading's source line as written (the text line of a
/// setext heading), trimmed and HTML-escaped. A body without headings gets
/// no section reference. Ids are the lowercase text of the heading with
/// spaces as `-`, punctuation other than `-` and `_` dropped and non-ASCII
/// letters and digits kept; a repeat gets `-1`, `-2`, ...; an empty slug is
/// `section`.
pub fn note_markdown(body: &str, path: &str, version: i64) -> String {
    render(body, Some((path, version)))
}

/// One heading found in the body.
struct Heading {
    level: usize,
    id: String,
    /// The heading's source line, trimmed.
    line: String,
    first: usize,
    last: usize,
}

/// 1-based line number of byte offset `at` in `body`.
fn line_of(body: &str, at: usize) -> usize {
    body.as_bytes()[..at]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// GitHub-style slug of heading text (before de-duplication).
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if c == '-' || c == '_' {
            out.push(c);
        } else if c.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        "section".to_string()
    } else {
        out
    }
}

/// Find the headings of `events` (with their byte ranges) and compute ids
/// and line ranges.
fn headings(body: &str, events: &[(Event<'_>, Range<usize>)]) -> Vec<Heading> {
    let total_lines = body.lines().count().max(1);
    let mut found: Vec<Heading> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    let mut text: Option<String> = None;
    for (ev, range) in events {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                let first = line_of(body, range.start);
                let line = body[range.clone()]
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                found.push(Heading {
                    level: heading_level(*level),
                    id: String::new(),
                    line,
                    first,
                    last: total_lines,
                });
                text = Some(String::new());
            }
            Event::End(TagEnd::Heading(_)) => {
                let base = slug(&text.take().unwrap_or_default());
                let mut id = base.clone();
                let mut n = 0;
                while !used.insert(id.clone()) {
                    n += 1;
                    id = format!("{base}-{n}");
                }
                if let Some(h) = found.last_mut() {
                    h.id = id;
                }
            }
            Event::Text(s) | Event::Code(s) | Event::Html(s) | Event::InlineHtml(s) => {
                if let Some(t) = text.as_mut() {
                    t.push_str(s);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(t) = text.as_mut() {
                    t.push(' ');
                }
            }
            _ => {}
        }
    }
    for i in 0..found.len() {
        let level = found[i].level;
        if let Some(next) = found[i + 1..].iter().find(|h| h.level <= level) {
            found[i].last = next.first.saturating_sub(1).max(found[i].first);
        }
    }
    found
}

fn heading_level(l: HeadingLevel) -> usize {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn render(body: &str, refs: Option<(&str, i64)>) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parsed: Vec<(Event<'_>, Range<usize>)> =
        Parser::new_ext(body, opts).into_offset_iter().collect();
    let found = headings(body, &parsed);
    let mut next_heading = found.iter();
    let mut current: Option<&Heading> = None;
    // For each open link / image: was its start event kept?
    let mut open: Vec<bool> = Vec::new();
    let events = parsed.into_iter().filter_map(|(ev, _)| match ev {
        Event::Start(Tag::Heading { level, .. }) => {
            current = next_heading.next();
            let id = current.map(|h| h.id.as_str()).unwrap_or("section");
            Some(Event::Html(
                format!("<h{} id=\"{}\">", heading_level(level), escape(id)).into(),
            ))
        }
        Event::End(TagEnd::Heading(level)) => {
            let mut tail = String::new();
            let mut reference = String::new();
            if let Some(h) = current.take() {
                let id = escape(&h.id);
                tail.push_str(&format!(" <a class=\"anchor\" href=\"#{id}\">¶</a>"));
                if let Some((path, version)) = refs {
                    reference = format!(
                        "<p class=\"refline\"><code class=\"ref\">{}#L{}-L{} \"{}\"</code></p>\n",
                        escape(&note_ref(path, version)),
                        h.first,
                        h.last,
                        escape(&h.line)
                    );
                }
            }
            tail.push_str(&format!("</h{}>\n", heading_level(level)));
            tail.push_str(&reference);
            Some(Event::Html(tail.into()))
        }
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
        assert!(out.contains(r#"<h1 id="a">a "#), "{out}");
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

    fn sections(body: &str) -> Vec<String> {
        note_markdown(body, "n/a.md", 3)
            .split("<code class=\"ref\">")
            .skip(1)
            .map(|s| s[..s.find("</code>").unwrap()].to_string())
            .collect()
    }

    #[test]
    fn ids_and_duplicates() {
        let out = markdown(
            "# Hello, World!

## a_b-c

## Hello World

## Hello World

## !!

## 移行順
",
        );
        for id in [
            "hello-world",
            "a_b-c",
            "hello-world-1",
            "hello-world-2",
            "section",
            "移行順",
        ] {
            assert!(out.contains(&format!(r##"id="{id}""##)), "{id}: {out}");
            assert!(
                out.contains(&format!(r##"href="#{id}">¶</a>"##)),
                "{id}: {out}"
            );
        }
    }

    #[test]
    fn section_ranges_nest_and_end() {
        let body = "# A
text
## B
b
### C
c
## D
d
# E
e
";
        assert_eq!(
            sections(body),
            [
                "notelet:n/a.md@v3#L1-L8 \"# A\"",
                "notelet:n/a.md@v3#L3-L6 \"## B\"",
                "notelet:n/a.md@v3#L5-L6 \"### C\"",
                "notelet:n/a.md@v3#L7-L8 \"## D\"",
                "notelet:n/a.md@v3#L9-L10 \"# E\"",
            ]
        );
    }

    #[test]
    fn heading_at_line_one_setext_and_no_trailing_newline() {
        let s = sections(
            "Title
=====
x

Sub
---
y",
        );
        assert_eq!(
            s,
            [
                "notelet:n/a.md@v3#L1-L7 \"Title\"",
                "notelet:n/a.md@v3#L5-L7 \"Sub\"",
            ]
        );
    }

    #[test]
    fn no_headings_no_section_reference() {
        let out = note_markdown(
            "just text
",
            "n/a.md",
            1,
        );
        assert!(!out.contains("class=\"ref\""), "{out}");
    }

    #[test]
    fn heading_line_is_escaped_and_quotes_survive() {
        let out = note_markdown("## say \"hi\" <b>x</b>\n", "n/a.md", 2);
        assert!(
            out.contains("#L1-L1 \"## say &quot;hi&quot; &lt;b&gt;x&lt;/b&gt;\"</code>"),
            "{out}"
        );
        assert!(!out.contains("<b>"), "{out}");
    }

    #[test]
    fn example_two_sections() {
        let out = note_markdown(
            "# One
a
## Two
b
",
            "n/a.md",
            7,
        );
        assert!(
            out.starts_with(
                r###"<h1 id="one">One <a class="anchor" href="#one">¶</a></h1>
<p class="refline"><code class="ref">notelet:n/a.md@v7#L1-L4 "# One"</code></p>"###
            ),
            "{out}"
        );
    }
}
