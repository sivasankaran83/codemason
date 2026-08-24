use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::{GatingConfig, ModelEntry};
use crate::error::Error;

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Built-in router/auto-select id patterns, checked in addition to whatever
/// `[gating] deny_id_patterns` adds. See PLAN.md "Ambiguity 1" — SPEC.md
/// doesn't define detection, this is the agreed default set.
const BUILTIN_ROUTER_PATTERNS: &[RouterPattern] = &[
    RouterPattern::Exact("openrouter/auto"),
    RouterPattern::Suffix("/auto"),
    RouterPattern::Contains("auto-router"),
];

enum RouterPattern {
    Exact(&'static str),
    Suffix(&'static str),
    Contains(&'static str),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogueEntry {
    pub id: String,
    #[serde(default)]
    pub context_length: u64,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CatalogueResponse {
    data: Vec<CatalogueEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    fetched_at: u64, // seconds since UNIX_EPOCH
    entries: Vec<CatalogueEntry>,
}

#[derive(Debug)]
pub struct GateRejection {
    pub reason: String,
}

impl std::fmt::Display for GateRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

/// GET `{base_url}/models` with a bearer token, OpenAI/OpenRouter list-endpoint
/// convention. SPEC.md names the response fields it gates on
/// (`context_length`, `supported_parameters`) but not the path — inference,
/// see PLAN.md risk flag 3.
fn fetch_live(base_url: &str, api_key: &str) -> Result<Vec<CatalogueEntry>, Error> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let response = ureq::get(&url)
        .set("Authorization", &format!("Bearer {api_key}"))
        .call()
        .map_err(|source| Error::CatalogueFetch {
            base_url: base_url.to_string(),
            source: source.into(),
        })?;
    let parsed: CatalogueResponse =
        response
            .into_json()
            .map_err(|source| Error::CatalogueFetch {
                base_url: base_url.to_string(),
                source: source.into(),
            })?;
    Ok(parsed.data)
}

/// The platform cache dir, or `$CODEMASON_CACHE_DIR` when set. The override
/// exists so tests can isolate the 24h cache instead of touching the real
/// machine's cache directory — `dirs::cache_dir()` resolves through the
/// Windows known-folder API, which ignores ordinary env var overrides, so
/// there's no other clean way to sandbox this in an integration test.
fn cache_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEMASON_CACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    dirs::cache_dir()
}

fn cache_path(base_url: &str) -> Option<PathBuf> {
    let cache_dir = cache_root()?;
    let sanitized: String = base_url
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Some(cache_dir.join("codemason").join("catalogue").join(format!("{sanitized}.json")))
}

fn read_cache(path: &std::path::Path) -> Option<CacheFile> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(path: &std::path::Path, cache: &CacheFile) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, text);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fetch the provider's model catalogue, using a 24h on-disk cache so
/// parallel processes against the same `base_url` don't each hit the
/// network. A fetch failure is non-fatal when a cache entry exists at all
/// (present, not necessarily unexpired) — the whole point of falling back is
/// the network being unavailable.
pub fn catalogue(base_url: &str, api_key: &str) -> Result<Vec<CatalogueEntry>, Error> {
    let cache_file = cache_path(base_url);

    if let Some(path) = &cache_file {
        if let Some(cached) = read_cache(path) {
            let age = now_secs().saturating_sub(cached.fetched_at);
            if age < CACHE_TTL.as_secs() {
                return Ok(cached.entries);
            }
        }
    }

    match fetch_live(base_url, api_key) {
        Ok(entries) => {
            if let Some(path) = &cache_file {
                write_cache(
                    path,
                    &CacheFile {
                        fetched_at: now_secs(),
                        entries: entries.clone(),
                    },
                );
            }
            Ok(entries)
        }
        Err(err) => {
            if let Some(path) = &cache_file {
                if let Some(cached) = read_cache(path) {
                    return Ok(cached.entries);
                }
            }
            Err(err)
        }
    }
}

fn is_router_id(id: &str, extra_patterns: &[String]) -> bool {
    let matches_builtin = BUILTIN_ROUTER_PATTERNS.iter().any(|p| match p {
        RouterPattern::Exact(s) => id == *s,
        RouterPattern::Suffix(s) => id.ends_with(s),
        RouterPattern::Contains(s) => id.contains(s),
    });
    matches_builtin || extra_patterns.iter().any(|p| id.contains(p.as_str()))
}

/// Run the full gate sequence for one model id against an already-resolved
/// allowlist and catalogue. See PLAN.md "Ambiguity 2" for why allowlist
/// membership and catalogue presence are two distinct checks:
///
/// 1. Allowlist membership (skippable via `allow_unlisted`) — is `id`
///    present in `models.toml`?
/// 2. Catalogue presence (never skippable) — is `id` in the provider's live
///    response at all? No data, no gate.
/// 3. Tool-calling support (never skippable, under any flag or config value
///    — CLAUDE.md's design invariant: "the model gate has no bypass for
///    tool-calling support").
/// 4. Context length below `gating.min_context_length`.
/// 5. Router/auto-select pseudo-model id.
pub fn check(
    id: &str,
    allow_unlisted: bool,
    allowlist: &[ModelEntry],
    gating: &GatingConfig,
    catalogue: &[CatalogueEntry],
) -> Result<(), GateRejection> {
    let in_allowlist = allowlist.iter().any(|m| m.id == id);
    if !in_allowlist && !allow_unlisted {
        return Err(GateRejection {
            reason: format!(
                "{id} is not in the models.toml allowlist; pass --allow-unlisted-model to use it anyway"
            ),
        });
    }

    let entry = match catalogue.iter().find(|e| e.id == id) {
        Some(entry) => entry,
        None => {
            return Err(GateRejection {
                reason: format!("{id} is not present in the provider's model catalogue"),
            });
        }
    };

    if !entry
        .supported_parameters
        .iter()
        .any(|p| p == "tools")
    {
        return Err(GateRejection {
            reason: format!("{id} does not advertise tool-calling support (\"tools\" absent from supported_parameters)"),
        });
    }

    if entry.context_length < gating.min_context_length {
        return Err(GateRejection {
            reason: format!(
                "{id} context length {} is below the configured minimum {}",
                entry.context_length, gating.min_context_length
            ),
        });
    }

    if is_router_id(id, &gating.deny_id_patterns) {
        return Err(GateRejection {
            reason: format!("{id} denotes a router or auto-select pseudo-model"),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, context_length: u64, tools: bool) -> CatalogueEntry {
        CatalogueEntry {
            id: id.to_string(),
            context_length,
            supported_parameters: if tools {
                vec!["tools".to_string()]
            } else {
                vec![]
            },
        }
    }

    fn allowlist() -> Vec<ModelEntry> {
        vec![ModelEntry {
            id: "vendor/good-model".to_string(),
            role: "primary".to_string(),
        }]
    }

    fn gating_config() -> GatingConfig {
        GatingConfig {
            min_context_length: 8000,
            require_tool_support: true,
            allow_unlisted: false,
            deny_id_patterns: vec![],
        }
    }

    #[test]
    fn passes_when_everything_checks_out() {
        let cat = vec![entry("vendor/good-model", 32000, true)];
        assert!(check("vendor/good-model", false, &allowlist(), &gating_config(), &cat).is_ok());
    }

    #[test]
    fn rejects_missing_tools_with_no_bypass() {
        let cat = vec![entry("vendor/good-model", 32000, false)];
        let err = check("vendor/good-model", false, &allowlist(), &gating_config(), &cat)
            .expect_err("should reject");
        assert!(err.reason.contains("tool-calling"));
    }

    #[test]
    fn rejects_id_absent_from_catalogue_even_with_allow_unlisted() {
        let cat = vec![entry("vendor/good-model", 32000, true)];
        let err = check("vendor/ghost-model", true, &allowlist(), &gating_config(), &cat)
            .expect_err("should reject");
        assert!(err.reason.contains("catalogue"));
    }

    #[test]
    fn rejects_unlisted_id_without_flag() {
        let cat = vec![entry("vendor/unlisted-model", 32000, true)];
        let err = check("vendor/unlisted-model", false, &allowlist(), &gating_config(), &cat)
            .expect_err("should reject");
        assert!(err.reason.contains("allowlist"));
    }

    #[test]
    fn allows_unlisted_id_with_flag_when_catalogue_checks_pass() {
        let cat = vec![entry("vendor/unlisted-model", 32000, true)];
        assert!(check("vendor/unlisted-model", true, &allowlist(), &gating_config(), &cat).is_ok());
    }

    #[test]
    fn rejects_router_id() {
        let cat = vec![entry("openrouter/auto", 200000, true)];
        let err = check("openrouter/auto", true, &allowlist(), &gating_config(), &cat)
            .expect_err("should reject");
        assert!(err.reason.contains("router"));
    }

    #[test]
    fn custom_deny_pattern_extends_builtin_list() {
        let mut gating = gating_config();
        gating.deny_id_patterns.push("shadow-router".to_string());
        let cat = vec![entry("vendor/shadow-router-9000", 32000, true)];
        let err = check(
            "vendor/shadow-router-9000",
            true,
            &allowlist(),
            &gating,
            &cat,
        )
        .expect_err("should reject");
        assert!(err.reason.contains("router"));
    }

    #[test]
    fn rejects_below_minimum_context_length() {
        let cat = vec![entry("vendor/good-model", 4000, true)];
        let err = check("vendor/good-model", false, &allowlist(), &gating_config(), &cat)
            .expect_err("should reject");
        assert!(err.reason.contains("context length"));
    }
}
