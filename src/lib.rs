pub mod cli;
pub mod config;
pub mod engine;
pub mod error;
pub mod gating;
pub mod index;
pub mod llm;
#[path = "loop.rs"]
pub mod r#loop;
pub mod log;
pub mod text;
pub mod tools;

pub use engine::{Chunk, DependencyGraph, IndexStats, SearchResult};
pub use error::Error;
pub use index::Index;
pub use r#loop::{run as run_loop, LoopConfig, LoopExit};
