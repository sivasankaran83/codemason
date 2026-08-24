#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to build the search index: {0}")]
    IndexBuild(#[source] anyhow::Error),

    #[error("find_related needs the `embeddings` feature, which this build was compiled without")]
    EmbeddingsFeatureDisabled,
}
