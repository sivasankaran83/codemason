//! The AST aware search engine: chunking, indexing, ranking and the
//! dependency graph.
//!
//! Adopted source rather than a dependency. It began as semble_rs v0.9.1, MIT,
//! and is ours now: maintained here, changed here, with no upstream to track.
//! ORIGIN.md beside this file records where it came from and ADR 0009 records
//! why. LICENSE.semble at the crate root carries the notice the licence
//! requires.
//!
//! Upstream's own command line entry point was dropped on adoption. The
//! harness is the binary, and this is a module it calls.

pub mod bm25;
pub mod chunking;
pub mod csharp;
pub mod digest;
pub mod encoder;
pub mod file_walker;
pub mod filter;
pub mod graph;
pub mod index;
pub mod outline;
pub mod plan;
pub mod ranking;
pub mod search;
pub mod stats;
pub mod tokens;
pub mod tree;
pub mod types;
pub mod utils;

pub use graph::DependencyGraph;
pub use index::SembleIndex;
pub use types::{Chunk, IndexStats, SearchResult};
