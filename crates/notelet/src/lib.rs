//! # notelet
//!
//! The umbrella crate of **notelet**, a thin hosted store for Markdown notes
//! that coding agents read and write over MCP. It re-exports
//! [`notelet_core`] so that `notelet::NoteService`, `notelet::NotePath` and
//! the rest are reachable under the project's name, and it carries the
//! project README on docs.rs.
//!
//! | crate | what |
//! |---|---|
//! | `notelet` (this) | re-export of `notelet-core`; depend on this from library code |
//! | `notelet-core` | domain, port, service, Postgres and in-memory adapters |
//! | `notelet-mcp` | the server binary: `cargo install notelet-mcp` |
//!
//! There is nothing else here on purpose: the model lives in
//! [`notelet_core`]'s crate documentation, which is the design document.

pub use notelet_core::*;

/// The version of the notelet crates, which move in lockstep.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_the_core_types() {
        let p = crate::NotePath::parse("projects/x/journal.md").expect("valid path");
        assert_eq!(p.as_str(), "projects/x/journal.md");
        assert!(!crate::VERSION.is_empty());
    }
}
