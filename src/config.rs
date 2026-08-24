use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub role: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatingConfig {
    #[serde(default)]
    pub min_context_length: u64,
    #[serde(default = "default_true")]
    pub require_tool_support: bool,
    #[serde(default)]
    pub allow_unlisted: bool,
    #[serde(default)]
    pub deny_id_patterns: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for GatingConfig {
    fn default() -> Self {
        Self {
            min_context_length: 0,
            require_tool_support: true,
            allow_unlisted: false,
            deny_id_patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelsConfig {
    #[serde(rename = "model", default)]
    pub models: Vec<ModelEntry>,
    #[serde(default)]
    pub gating: GatingConfig,
}

impl ModelsConfig {
    /// The first `[[model]]` entry — the default when `--model` isn't given.
    pub fn default_model(&self) -> Option<&ModelEntry> {
        self.models.first()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.models.iter().any(|m| m.id == id)
    }
}

/// Resolve and parse `models.toml`.
///
/// If `explicit` is given (from `--models-config`) it is used as-is, with no
/// fallback — an operator pointing at a specific path gets an error naming
/// that path, not a silent search past it. Otherwise the order is
/// `./models.toml`, then `dirs::config_dir()/codemason/models.toml`.
pub fn resolve(explicit: Option<&Path>) -> Result<(PathBuf, ModelsConfig), Error> {
    if let Some(path) = explicit {
        if !path.exists() {
            return Err(Error::ConfigNotFound {
                searched: vec![path.to_path_buf()],
            });
        }
        return Ok((path.to_path_buf(), load(path)?));
    }

    let mut searched = Vec::new();

    let cwd_path = PathBuf::from("models.toml");
    if cwd_path.exists() {
        return Ok((cwd_path.clone(), load(&cwd_path)?));
    }
    searched.push(cwd_path);

    if let Some(config_dir) = dirs::config_dir() {
        let platform_path = config_dir.join("codemason").join("models.toml");
        if platform_path.exists() {
            return Ok((platform_path.clone(), load(&platform_path)?));
        }
        searched.push(platform_path);
    }

    Err(Error::ConfigNotFound { searched })
}

fn load(path: &Path) -> Result<ModelsConfig, Error> {
    let text = fs::read_to_string(path).map_err(|source| Error::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;

    toml::from_str(&text).map_err(|source| Error::ConfigParse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codemason-config-test-{name}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.toml");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn well_formed_file_preserves_declaration_order() {
        let path = write_temp(
            "ordered",
            r#"
[[model]]
id = "vendor/model-a"
role = "primary"

[[model]]
id = "vendor/model-b"
role = "fallback"

[gating]
min_context_length = 16000
require_tool_support = true
allow_unlisted = false
"#,
        );
        let (_resolved, config) = resolve(Some(&path)).expect("should parse");
        assert_eq!(config.models.len(), 2);
        assert_eq!(config.models[0].id, "vendor/model-a");
        assert_eq!(config.models[1].id, "vendor/model-b");
        assert_eq!(config.default_model().unwrap().id, "vendor/model-a");
        assert_eq!(config.gating.min_context_length, 16000);
    }

    #[test]
    fn malformed_toml_names_file_and_parse_error() {
        let path = write_temp("malformed", "this is not [ valid toml");
        let err = resolve(Some(&path)).expect_err("should fail to parse");
        match err {
            Error::ConfigParse { path: p, .. } => assert_eq!(p, path),
            other => panic!("expected ConfigParse, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_lists_searched_paths() {
        let dir = std::env::temp_dir().join(format!(
            "codemason-config-test-missing-{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let missing = dir.join("does-not-exist.toml");
        let err = resolve(Some(&missing)).expect_err("should fail to find file");
        match err {
            Error::ConfigNotFound { searched } => assert_eq!(searched, vec![missing]),
            other => panic!("expected ConfigNotFound, got {other:?}"),
        }
    }
}
