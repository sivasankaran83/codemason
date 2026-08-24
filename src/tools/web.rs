//! `web_search` — the seventh tool. See SPEC.md's "Amendment: the seventh
//! tool" for why the six-tool cap was raised deliberately rather than
//! quietly.
//!
//! Provider-agnostic by design, for the same reason the model client is:
//! this binary does not hardcode a vendor. A search provider is configured
//! through two environment variables and its JSON response is read
//! structurally rather than against one vendor's schema.
//!
//! No new crate enters the tree for this — `ureq` already drives the model
//! client and `regex` already ships with the engine.

use once_cell::sync::Lazy;
use regex::Regex;

use super::ToolOutcome;

/// Endpoint of an OpenAPI-ish JSON search provider. Documented for Brave
/// (`https://api.search.brave.com/res/v1/web/search`), Tavily and Serper —
/// all of which have a free tier — but nothing here is specific to any of
/// them.
pub const SEARCH_URL_ENV: &str = "CODEMASON_SEARCH_URL";
pub const SEARCH_KEY_ENV: &str = "CODEMASON_SEARCH_API_KEY";

/// Header a provider expects its key in. Brave uses `X-Subscription-Token`,
/// Serper uses `X-API-KEY`, Tavily takes the key in the body; defaulting to
/// Brave's is a convenience, not a lock-in.
pub const SEARCH_KEY_HEADER_ENV: &str = "CODEMASON_SEARCH_KEY_HEADER";
const DEFAULT_KEY_HEADER: &str = "X-Subscription-Token";

const MAX_RESULTS_CAP: i64 = 10;
const DEFAULT_MAX_RESULTS: i64 = 5;
const TIMEOUT_SECONDS: u64 = 20;

fn clamp_results(requested: i64) -> usize {
    if requested <= 0 {
        DEFAULT_MAX_RESULTS as usize
    } else {
        requested.min(MAX_RESULTS_CAP) as usize
    }
}

/// One result, flattened out of whatever shape the provider returned.
struct Hit {
    title: String,
    url: String,
    snippet: String,
}

/// Pull results out of a provider response without knowing its schema.
///
/// Every provider worth using nests its hits under *some* array of objects
/// carrying a title/url/description triple; the key names differ
/// (`description` vs `snippet` vs `content`, `url` vs `link`). Rather than
/// encode one vendor's layout, walk the JSON for the first array whose
/// objects carry a URL-ish field and read the neighbouring fields by any of
/// their common names.
fn extract_hits(value: &serde_json::Value, out: &mut Vec<Hit>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                extract_hits(item, out, limit);
                if out.len() >= limit {
                    return;
                }
            }
        }
        serde_json::Value::Object(map) => {
            let pick = |keys: &[&str]| -> Option<String> {
                keys.iter()
                    .find_map(|k| map.get(*k).and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
            };
            let url = pick(&["url", "link", "href"]);
            if let Some(url) = url {
                if url.starts_with("http") {
                    out.push(Hit {
                        title: pick(&["title", "name", "heading"]).unwrap_or_default(),
                        url,
                        snippet: pick(&["description", "snippet", "content", "text", "excerpt"])
                            .unwrap_or_default(),
                    });
                    if out.len() >= limit {
                        return;
                    }
                }
            }
            for (_, v) in map {
                extract_hits(v, out, limit);
                if out.len() >= limit {
                    return;
                }
            }
        }
        _ => {}
    }
}

fn format_hits(query: &str, hits: &[Hit], source: &str) -> String {
    if hits.is_empty() {
        return format!("no results for {query:?} (via {source})");
    }
    let mut out = format!("{} result(s) for {query:?} (via {source}):\n", hits.len());
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, h.title.trim(), h.url));
        let snippet = collapse_whitespace(&h.snippet);
        if !snippet.is_empty() {
            let trimmed: String = snippet.chars().take(300).collect();
            out.push_str(&format!("   {trimmed}\n"));
        }
    }
    out
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Configured provider: a JSON endpoint plus a key.
fn search_configured(
    url: &str,
    key: &str,
    key_header: &str,
    query: &str,
    limit: usize,
) -> Result<String, String> {
    let response = ureq::get(url)
        .query("q", query)
        .query("count", &limit.to_string())
        .set(key_header, key)
        .set("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECONDS))
        .call();

    let body = match response {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| format!("could not read search response: {e}"))?,
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            let detail: String = collapse_whitespace(&detail).chars().take(200).collect();
            return Err(format!("search provider returned HTTP {code}: {detail}"));
        }
        Err(e) => return Err(format!("search request failed: {e}")),
    };

    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("search provider did not return JSON: {e}"))?;

    let mut hits = Vec::new();
    extract_hits(&value, &mut hits, limit);
    Ok(format_hits(query, &hits, "configured provider"))
}

// DuckDuckGo's lite endpoint returns plain anchors; these pull the href and
// the visible text back out. Deliberately narrow — this is a best-effort
// fallback, not a parser anyone should rely on.
static DDG_LINK: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)<a[^>]+class="result-link"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
        .expect("valid DDG link regex")
});
static DDG_SNIPPET: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)<td[^>]*class="result-snippet"[^>]*>(.*?)</td>"#)
        .expect("valid DDG snippet regex")
});
static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]*>").expect("valid tag regex"));

fn strip_tags(s: &str) -> String {
    let text = TAG.replace_all(s, " ");
    collapse_whitespace(&text)
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Keyless DuckDuckGo fallback. **Best-effort and expected to fail
/// intermittently**: the endpoint rate-limits aggressively and answers with
/// HTTP 202 and a challenge page rather than results when it decides to. It
/// is here so the tool does something useful with no configuration at all,
/// not because it is dependable — configure a real provider for anything
/// that matters.
fn search_duckduckgo(query: &str, limit: usize) -> Result<String, String> {
    let response = ureq::get("https://lite.duckduckgo.com/lite/")
        .query("q", query)
        .set("User-Agent", "Mozilla/5.0 (compatible; codemason)")
        .set("Accept", "text/html")
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECONDS))
        .call();

    let (status, body) = match response {
        Ok(resp) => (
            resp.status(),
            resp.into_string()
                .map_err(|e| format!("could not read DuckDuckGo response: {e}"))?,
        ),
        Err(ureq::Error::Status(code, _)) => {
            return Err(format!(
                "DuckDuckGo returned HTTP {code} (the keyless fallback is rate-limited). \
                 Configure a search provider via {SEARCH_URL_ENV} and {SEARCH_KEY_ENV} for reliable results."
            ));
        }
        Err(e) => return Err(format!("DuckDuckGo request failed: {e}")),
    };

    let mut hits: Vec<Hit> = DDG_LINK
        .captures_iter(&body)
        .take(limit)
        .map(|c| Hit {
            title: strip_tags(&c[2]),
            url: c[1].to_string(),
            snippet: String::new(),
        })
        .collect();

    for (hit, cap) in hits.iter_mut().zip(DDG_SNIPPET.captures_iter(&body)) {
        hit.snippet = strip_tags(&cap[1]);
    }

    if hits.is_empty() {
        return Err(format!(
            "DuckDuckGo returned no parseable results (HTTP {status}); the keyless fallback is \
             best-effort and rate-limited. Configure {SEARCH_URL_ENV} and {SEARCH_KEY_ENV} for \
             reliable results."
        ));
    }

    Ok(format_hits(query, &hits, "duckduckgo (best-effort)"))
}

/// Search the web. A configured provider is used when one is set; otherwise
/// the keyless DuckDuckGo fallback is tried.
///
/// Every failure here returns `ToolOutcome::Error`, never a run-ending
/// error: a search that fails is something the model can react to by trying
/// a different query or carrying on without it, which is exactly the
/// "errors the model can act on are not failures" rule.
pub fn web_search(query: &str, max_results: i64) -> ToolOutcome {
    let query = query.trim();
    if query.is_empty() {
        return ToolOutcome::Error("web_search: query must not be empty".to_string());
    }
    let limit = clamp_results(max_results);

    let url = std::env::var(SEARCH_URL_ENV).ok().filter(|s| !s.is_empty());
    let key = std::env::var(SEARCH_KEY_ENV).ok().filter(|s| !s.is_empty());

    match (url, key) {
        (Some(url), Some(key)) => {
            let header = std::env::var(SEARCH_KEY_HEADER_ENV)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_KEY_HEADER.to_string());
            match search_configured(&url, &key, &header, query, limit) {
                Ok(text) => ToolOutcome::Ok(text),
                Err(err) => ToolOutcome::Error(format!("web_search: {err}")),
            }
        }
        _ => match search_duckduckgo(query, limit) {
            Ok(text) => ToolOutcome::Ok(text),
            Err(err) => ToolOutcome::Error(format!("web_search: {err}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_is_an_error_not_a_request() {
        match web_search("   ", 5) {
            ToolOutcome::Error(msg) => assert!(msg.contains("must not be empty"), "{msg}"),
            ToolOutcome::Ok(text) => panic!("expected an error, got: {text}"),
        }
    }

    #[test]
    fn max_results_is_clamped() {
        assert_eq!(clamp_results(0), DEFAULT_MAX_RESULTS as usize);
        assert_eq!(clamp_results(-3), DEFAULT_MAX_RESULTS as usize);
        assert_eq!(clamp_results(3), 3);
        assert_eq!(clamp_results(999), MAX_RESULTS_CAP as usize);
    }

    /// The point of `extract_hits`: read results out of provider shapes it
    /// has never seen, so no vendor is baked into the binary.
    #[test]
    fn hits_are_extracted_from_differing_provider_shapes() {
        // Brave-ish
        let brave = serde_json::json!({
            "web": {"results": [
                {"title": "A", "url": "https://a.example", "description": "first"},
                {"title": "B", "url": "https://b.example", "description": "second"}
            ]}
        });
        let mut hits = Vec::new();
        extract_hits(&brave, &mut hits, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://a.example");
        assert_eq!(hits[0].snippet, "first");

        // Serper-ish: different key names, different nesting
        let serper = serde_json::json!({
            "organic": [
                {"title": "C", "link": "https://c.example", "snippet": "third"}
            ]
        });
        let mut hits = Vec::new();
        extract_hits(&serper, &mut hits, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://c.example");
        assert_eq!(hits[0].snippet, "third");

        // Tavily-ish
        let tavily = serde_json::json!({
            "results": [
                {"title": "D", "url": "https://d.example", "content": "fourth"}
            ]
        });
        let mut hits = Vec::new();
        extract_hits(&tavily, &mut hits, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "fourth");
    }

    #[test]
    fn extraction_respects_the_limit() {
        let many = serde_json::json!({"results": (0..20)
            .map(|i| serde_json::json!({"title": i.to_string(), "url": format!("https://{i}.example")}))
            .collect::<Vec<_>>()});
        let mut hits = Vec::new();
        extract_hits(&many, &mut hits, 3);
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn non_http_urls_are_not_treated_as_results() {
        let value = serde_json::json!({"results": [{"title": "x", "url": "javascript:alert(1)"}]});
        let mut hits = Vec::new();
        extract_hits(&value, &mut hits, 5);
        assert!(hits.is_empty(), "non-http url must not become a result");
    }

    #[test]
    fn tags_and_entities_are_stripped_from_ddg_markup() {
        let raw = "<b>rust</b> &amp; <i>tree-sitter</i>   parser";
        assert_eq!(strip_tags(raw), "rust & tree-sitter parser");
    }

    #[test]
    fn formatting_reports_an_empty_result_set_plainly() {
        let text = format_hits("q", &[], "test");
        assert!(text.contains("no results"), "{text}");
    }
}
