//! MCP resources: the guides under `docs/guides/` baked into the binary
//! with `include_str!`, served as `notelet://guides/<slug>`. The guide is
//! the source of truth for how to use a tool; the README points here rather
//! than repeating it. `tools.md` is the exception: it is generated from the
//! tool router by `just gen-docs`, not written by hand.

use rmcp::model::{ListResourcesResult, ReadResourceResult, Resource, ResourceContents};

/// One bundled guide.
struct Guide {
    uri: &'static str,
    name: &'static str,
    title: &'static str,
    description: &'static str,
    body: &'static str,
}

const GUIDES: &[Guide] = &[
    Guide {
        uri: "notelet://guides/onboarding",
        name: "onboarding",
        title: "Onboarding: the path tree and one pass through the tools",
        description: "Where notes live (projects/, tasks/, lists/), and a first read-write-list-search cycle.",
        body: include_str!("../docs/guides/onboarding.md"),
    },
    Guide {
        uri: "notelet://guides/locking",
        name: "locking",
        title: "Locking: expected_version, 409, and why append needs none",
        description: "How note_put's optimistic lock works, what to do on a conflict, and when to use note_append instead.",
        body: include_str!("../docs/guides/locking.md"),
    },
    Guide {
        uri: "notelet://guides/filters",
        name: "filters",
        title: "Filters: fields, operators, LIKE escaping and paging",
        description: "The note_list filter vocabulary, how values are bound, and how cursors page.",
        body: include_str!("../docs/guides/filters.md"),
    },
    Guide {
        uri: "notelet://guides/tools",
        name: "tools",
        title: "Tools: the list, generated from the tool router",
        description: "Every tool with the first sentence of its description.",
        body: include_str!("../docs/guides/tools.md"),
    },
];

/// All bundled guides, as MCP resources.
pub fn list_all() -> ListResourcesResult {
    let resources: Vec<Resource> = GUIDES
        .iter()
        .map(|g| {
            Resource::new(g.uri, g.name)
                .with_title(g.title)
                .with_description(g.description)
                .with_mime_type("text/markdown")
        })
        .collect();
    ListResourcesResult::with_all_items(resources)
}

/// Read a guide by URI; `None` if no guide has that URI.
pub fn read(uri: &str) -> Option<ReadResourceResult> {
    let g = GUIDES.iter().find(|g| g.uri == uri)?;
    Some(ReadResourceResult::new(vec![ResourceContents::text(
        g.body, g.uri,
    )
    .with_mime_type("text/markdown")]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_guides_and_reads_them() {
        let list = list_all();
        assert_eq!(list.resources.len(), GUIDES.len());
        assert!(read("notelet://guides/tools").is_some());
        for g in GUIDES {
            assert!(read(g.uri).is_some(), "{}", g.uri);
            assert!(!g.body.trim().is_empty(), "{} is empty", g.uri);
        }
        assert!(read("notelet://guides/nope").is_none());
    }
}
