use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::engine::{self, Chunk, DependencyGraph, SearchResult};
use crate::error::Error;

#[derive(Debug, Clone)]
pub struct BuildStats {
    pub indexed_files: usize,
    pub total_chunks: usize,
    pub languages: HashMap<String, usize>,
    pub build_ms: u128,
}

pub struct Index {
    inner: engine::SembleIndex,
    stats: BuildStats,
}

impl Index {
    /// Build the index over `repo_root`, including documentation files.
    ///
    /// Equivalent to `build_with(repo_root, true)`. Documents are included by
    /// default because a repository's specifications, ADRs and architecture
    /// notes are frequently the only statement of intent that exists — most
    /// obviously on a spec-driven or greenfield repository, where markdown is
    /// the entire input and there is not yet any code to search. An agent that
    /// cannot find them cannot follow them.
    pub fn build(repo_root: impl AsRef<Path>) -> Result<Self, Error> {
        Self::build_with(repo_root, true)
    }

    /// `include_docs: false` restricts the index to source code, which is
    /// what the engine does by default. Worth having for a large repository
    /// whose documentation is voluminous and irrelevant to the task.
    pub fn build_with(repo_root: impl AsRef<Path>, include_docs: bool) -> Result<Self, Error> {
        let started = Instant::now();
        let inner = engine::SembleIndex::from_path(repo_root, None, None, None, include_docs)
            .map_err(Error::IndexBuild)?;
        let build_ms = started.elapsed().as_millis();

        let engine_stats = inner.stats();
        let stats = BuildStats {
            indexed_files: engine_stats.indexed_files,
            total_chunks: engine_stats.total_chunks,
            languages: engine_stats.languages,
            build_ms,
        };

        Ok(Self { inner, stats })
    }

    pub fn search(&self, query: &str, top_k: usize) -> Vec<SearchResult> {
        self.inner.search(query, top_k, None, None, None)
    }

    pub fn chunks(&self) -> &[Chunk] {
        self.inner.chunks()
    }

    pub fn graph(&self) -> &DependencyGraph {
        self.inner.graph()
    }

    pub fn stats(&self) -> &BuildStats {
        &self.stats
    }

    #[cfg(feature = "embeddings")]
    pub fn find_related(&self, chunk: &Chunk, top_k: usize) -> Result<Vec<SearchResult>, Error> {
        self.inner
            .find_related(chunk, top_k)
            .map_err(Error::IndexBuild)
    }

    #[cfg(not(feature = "embeddings"))]
    pub fn find_related(&self, _chunk: &Chunk, _top_k: usize) -> Result<Vec<SearchResult>, Error> {
        Err(Error::EmbeddingsFeatureDisabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real C# repository living in the local `old_source/` reference tree
    // (see PLAN.md risk flag 4). Not part of this crate's build or commit
    // history — these two tests only run on a checkout that has it.
    fn fixture_repo() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("old_source/navex-harness-main/metric-measurement-service")
    }

    #[test]
    fn ac4_builds_against_a_real_csharp_repo_with_a_plausible_chunk_count() {
        let repo = fixture_repo();
        if !repo.exists() {
            eprintln!("skipping: {} not present", repo.display());
            return;
        }
        let index = Index::build(&repo).expect("index build should succeed");
        assert!(index.stats().indexed_files > 0, "expected indexed files > 0");
        assert!(index.stats().total_chunks > 0, "expected chunks > 0");
    }

    #[test]
    fn ac5_search_for_a_known_symbol_returns_its_defining_chunk_first() {
        let repo = fixture_repo();
        if !repo.exists() {
            eprintln!("skipping: {} not present", repo.display());
            return;
        }
        let index = Index::build(&repo).expect("index build should succeed");
        let results = index.search("AgentSchedulerService", 5);
        assert!(!results.is_empty(), "expected at least one search result");
        assert!(
            results[0].chunk.file_path.ends_with("AgentSchedulerService.cs"),
            "top result was {:?}, expected AgentSchedulerService.cs",
            results[0].chunk.file_path
        );
    }

    #[test]
    #[cfg(not(feature = "embeddings"))]
    fn ac6_similarity_call_names_the_missing_feature_instead_of_panicking() {
        let repo = fixture_repo();
        if !repo.exists() {
            eprintln!("skipping: {} not present", repo.display());
            return;
        }
        let index = Index::build(&repo).expect("index build should succeed");
        let chunk = index.chunks()[0].clone();
        let err = index
            .find_related(&chunk, 5)
            .expect_err("find_related without the embeddings feature must error, not panic");
        assert!(matches!(err, Error::EmbeddingsFeatureDisabled));
    }
}
