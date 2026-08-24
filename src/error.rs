#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to build the search index: {0}")]
    IndexBuild(#[source] anyhow::Error),

    #[error("find_related needs the `embeddings` feature, which this build was compiled without")]
    EmbeddingsFeatureDisabled,

    #[error("no models.toml found; searched: {}", .searched.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))]
    ConfigNotFound { searched: Vec<std::path::PathBuf> },

    #[error("failed to read {path}: {source}")]
    ConfigRead {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {source}")]
    ConfigParse {
        path: std::path::PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    #[error("failed to fetch model catalogue from {base_url}: {source}")]
    CatalogueFetch {
        base_url: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("model {id} rejected by gating: {reason}")]
    ModelGated { id: String, reason: String },

    #[error("missing {0}: pass the flag or set the environment variable")]
    MissingCredential(&'static str),

    #[error("failed to read task file {path}: {source}")]
    TaskFileRead {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("provider {model} failed after {attempts} attempts, last status {last_status}")]
    ProviderExhausted {
        model: String,
        attempts: u32,
        last_status: u16,
    },

    #[error("provider request for {model} failed: {source}")]
    ProviderRequest {
        model: String,
        #[source]
        source: anyhow::Error,
    },

    #[error("path {path} resolves outside the repository root")]
    PathEscapesRepo { path: String },

    #[error("failed to open event log {path}: {source}")]
    LogOpen {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}
