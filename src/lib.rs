pub mod cli;
pub mod config;
pub mod engine;
pub mod error;
pub mod gating;
pub mod index;

pub use engine::{Chunk, DependencyGraph, IndexStats, SearchResult};
pub use error::Error;
pub use index::Index;
