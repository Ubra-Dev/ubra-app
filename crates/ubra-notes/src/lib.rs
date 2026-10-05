//! Ubra Notes: rich notes stored as plain Markdown files.
//!
//! This crate is GUI-free so the app, the `ubra` CLI, and agents' MCP tools
//! share one model, one file format, and one store. The app edits
//! [`doc::Document`] values; everything else is conversion at the file edge.

pub mod doc;
pub mod edit;
pub mod handoff;
pub mod history;
pub mod links;
pub mod markdown;
pub mod mention;
pub mod merge;
pub mod store;
pub mod text_edit;
pub mod work;
