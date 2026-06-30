//! Web search providers: Brave Search and Tavily. The host-side path
//! (`maturana search`, the cockpit) resolves API keys from pipelock and calls
//! the APIs directly; guests instead curl the same endpoints through the
//! pipelock proxy with header injection so keys never enter the VM (see
//! skills/maturana-web-search). Request building and response parsing are
//! pure functions so they unit-test without network.

use serde::{Deserialize, Serialize};

use crate::secrets::resolve_secret_source_with_home;

pub const BRAVE_KEY_SOURCE: &str = "pipelock:brave/api-key";
pub const TAVILY_KEY_SOURCE: &str = "pipelock:tavily/api-key";
pub const BRAVE_HOST: &str = "api.search.brave.com";
pub const TAVILY_HOST: &str = "api.tavily.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchProviderKind {
    Brave,
    Tavily,
}

impl std::str::FromStr for SearchProviderKind {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "brave" => Ok(Self::Brave),
            "tavily" => Ok(Self::Tavily),
            other => anyhow::bail!("unknown search provider: {other} (brave|tavily)"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub count: usize,
}

/// A fully-described HTTP request, provider-agnostic and pure.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<serde_json::Value>,
}

/// Build the provider request. `api_key` is the bare key — prefixes
/// (`Bearer `) are applied here, mirroring the proxy's `prefix` injection.
pub fn build_request(
    provider: SearchProviderKind,
    request: &SearchRequest,
    api_key: &str,
) -> BuiltRequest {
    let count = request.count.clamp(1, 20);
    match provider {
        SearchProviderKind::Brave => BuiltRequest {
            method: "GET",
            url: format!(
                "https://{BRAVE_HOST}/res/v1/web/search?q={}&count={count}",
                urlencode(&request.query)
            ),
            headers: vec![
                ("X-Subscription-Token".to_string(), api_key.to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            body: None,
        },
        SearchProviderKind::Tavily => BuiltRequest {
            method: "POST",
            url: format!("https://{TAVILY_HOST}/search"),
            headers: vec![("Authorization".to_string(), format!("Bearer {api_key}"))],
            body: Some(serde_json::json!({
                "query": request.query,
                "max_results": count,
            })),
        },
    }
}

/// Parse the provider response body into uniform results.
pub fn parse_response(
    provider: SearchProviderKind,
    body: &str,
) -> anyhow::Result<Vec<SearchResult>> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    let results = match provider {
        SearchProviderKind::Brave => value
            .pointer("/web/results")
            .and_then(|r| r.as_array())
            .map(|results| {
                results
                    .iter()
                    .filter_map(|result| {
                        Some(SearchResult {
                            title: result.get("title")?.as_str()?.to_string(),
                            url: result.get("url")?.as_str()?.to_string(),
                            snippet: result
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        SearchProviderKind::Tavily => value
            .get("results")
            .and_then(|r| r.as_array())
            .map(|results| {
                results
                    .iter()
                    .filter_map(|result| {
                        Some(SearchResult {
                            title: result.get("title")?.as_str()?.to_string(),
                            url: result.get("url")?.as_str()?.to_string(),
                            snippet: result
                                .get("content")
                                .and_then(|c| c.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };
    Ok(results)
}

/// Execute a search host-side: key from pipelock, direct HTTPS call.
pub fn search(
    home_root: &std::path::Path,
    provider: SearchProviderKind,
    request: &SearchRequest,
) -> anyhow::Result<Vec<SearchResult>> {
    let key_source = match provider {
        SearchProviderKind::Brave => BRAVE_KEY_SOURCE,
        SearchProviderKind::Tavily => TAVILY_KEY_SOURCE,
    };
    let key = resolve_secret_source_with_home(key_source, home_root).map_err(|_| {
        anyhow::anyhow!(
            "missing API key: `maturana pipelock set {}` first",
            key_source.trim_start_matches("pipelock:")
        )
    })?;
    let built = build_request(provider, request, key.expose_for_runtime());
    let mut call =
        ureq::request(built.method, &built.url).timeout(std::time::Duration::from_secs(20));
    for (name, value) in &built.headers {
        call = call.set(name, value);
    }
    let response = match &built.body {
        Some(body) => call.send_json(body),
        None => call.call(),
    }
    .map_err(|error| anyhow::anyhow!("search request failed: {error}"))?;
    let body = response.into_string()?;
    parse_response(provider, &body)
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brave_request_shape() {
        let built = build_request(
            SearchProviderKind::Brave,
            &SearchRequest {
                query: "rust axum websockets".into(),
                count: 5,
            },
            "BK123",
        );
        assert_eq!(built.method, "GET");
        assert_eq!(
            built.url,
            "https://api.search.brave.com/res/v1/web/search?q=rust%20axum%20websockets&count=5"
        );
        assert!(built
            .headers
            .contains(&("X-Subscription-Token".to_string(), "BK123".to_string())));
        assert!(built.body.is_none());
    }

    #[test]
    fn tavily_request_uses_bearer_auth_not_key_in_body() {
        let built = build_request(
            SearchProviderKind::Tavily,
            &SearchRequest {
                query: "maturana".into(),
                count: 3,
            },
            "tvly-abc",
        );
        assert_eq!(built.method, "POST");
        assert_eq!(built.url, "https://api.tavily.com/search");
        assert!(built
            .headers
            .contains(&("Authorization".to_string(), "Bearer tvly-abc".to_string())));
        let body = built.body.unwrap();
        assert_eq!(body["query"], "maturana");
        assert_eq!(body["max_results"], 3);
        // The key must never ride in the body — guests rely on proxy header
        // injection, which can only add headers.
        assert!(!body.to_string().contains("tvly-abc"));
    }

    #[test]
    fn count_is_clamped() {
        let built = build_request(
            SearchProviderKind::Brave,
            &SearchRequest {
                query: "q".into(),
                count: 999,
            },
            "k",
        );
        assert!(built.url.ends_with("count=20"));
    }

    #[test]
    fn brave_response_parses() {
        let body = r#"{"web":{"results":[
            {"title":"Axum","url":"https://github.com/tokio-rs/axum","description":"Web framework"},
            {"title":"Docs","url":"https://docs.rs/axum","description":"API docs"}
        ]}}"#;
        let results = parse_response(SearchProviderKind::Brave, body).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Axum");
        assert_eq!(results[1].url, "https://docs.rs/axum");
        assert_eq!(results[0].snippet, "Web framework");
    }

    #[test]
    fn tavily_response_parses() {
        let body = r#"{"query":"x","results":[
            {"title":"Result","url":"https://example.com","content":"Snippet text","score":0.9}
        ]}"#;
        let results = parse_response(SearchProviderKind::Tavily, body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet, "Snippet text");
    }

    #[test]
    fn empty_or_alien_responses_yield_no_results() {
        assert!(parse_response(SearchProviderKind::Brave, "{}")
            .unwrap()
            .is_empty());
        assert!(parse_response(SearchProviderKind::Tavily, "{}")
            .unwrap()
            .is_empty());
        assert!(parse_response(SearchProviderKind::Brave, "not json").is_err());
    }
}
