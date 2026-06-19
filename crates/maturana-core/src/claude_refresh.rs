//! Host-owned Claude (claude-code) OAuth refresh.
//!
//! claude-code's access token expires ~8h and its refresh token is single-use
//! (rotates on every refresh). Left to the guest, a long-running agent's token
//! eventually expires; and any host re-push of a stale `.credentials.json`
//! clobbers a token the guest already rotated. The fix: the host refreshes the
//! token before expiry, writes the rotated creds (preserving the rest of the
//! file), and re-pushes — staying the single source of truth.
//!
//! `.credentials.json` shape (only the oauth block is ours to touch):
//! ```json
//! { "claudeAiOauth": { "accessToken", "refreshToken", "expiresAt"(ms), ... },
//!   "mcpOAuth": { ... } }   // preserved verbatim
//! ```
//!
//! The refresh endpoint/params are community-documented and UNVERIFIED here;
//! `maturana claude-refresh probe` confirms them against a real token before
//! the scheduler is trusted.

use std::path::Path;
use std::time::Duration;

/// OAuth client id claude-code presents for the refresh grant.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Refresh this long before expiry so a turn never starts on a dead token.
pub const REFRESH_SKEW: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCreds {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
}

/// Read the oauth block from a `.credentials.json`.
pub fn read_claude_creds(path: &Path) -> anyhow::Result<ClaudeCreds> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    parse_claude_creds(&value)
}

fn parse_claude_creds(value: &serde_json::Value) -> anyhow::Result<ClaudeCreds> {
    let oauth = value
        .get("claudeAiOauth")
        .ok_or_else(|| anyhow::anyhow!(".credentials.json has no claudeAiOauth block"))?;
    Ok(ClaudeCreds {
        access_token: str_field(oauth, "accessToken")?,
        refresh_token: str_field(oauth, "refreshToken")?,
        expires_at_ms: oauth
            .get("expiresAt")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("claudeAiOauth.expiresAt missing or not an integer"))?,
    })
}

fn str_field(obj: &serde_json::Value, key: &str) -> anyhow::Result<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("claudeAiOauth.{key} missing"))
}

/// The (url, json-body) for a refresh — pure, so tests can assert the shape.
pub fn refresh_request(refresh_token: &str) -> (String, serde_json::Value) {
    (
        TOKEN_URL.to_string(),
        serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        }),
    )
}

/// Parse a token-endpoint response into rotated creds. `now_ms` is passed in so
/// the function stays pure/testable.
pub fn parse_refresh_response(body: &str, now_ms: i64) -> anyhow::Result<ClaudeCreds> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("refresh response is not JSON: {e}"))?;
    let access_token = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("refresh response missing access_token"))?
        .to_string();
    // Some providers omit a rotated refresh_token (then the old one stays valid).
    let refresh_token = value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let expires_in = value
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(8 * 3600);
    Ok(ClaudeCreds {
        access_token,
        refresh_token: refresh_token.unwrap_or_default(),
        expires_at_ms: now_ms + expires_in * 1000,
    })
}

/// Perform a refresh over the network. Returns rotated creds (carrying the old
/// refresh token forward if the endpoint didn't rotate it).
pub fn refresh_claude_token(creds: &ClaudeCreds) -> anyhow::Result<ClaudeCreds> {
    let (url, body) = refresh_request(&creds.refresh_token);
    let text = match ureq::post(&url)
        .timeout(Duration::from_secs(30))
        .set("Accept", "application/json")
        .set("anthropic-beta", "oauth-2025-04-20")
        .send_json(body)
    {
        Ok(response) => response.into_string()?,
        // Surface the endpoint's error body (e.g. invalid_grant vs
        // invalid_client) so a dead token is distinguishable from a wrong
        // endpoint. The body never contains our secret.
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            anyhow::bail!("claude oauth refresh failed: HTTP {code}: {body}");
        }
        Err(e) => anyhow::bail!("claude oauth refresh transport error: {e}"),
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut rotated = parse_refresh_response(&text, now_ms)?;
    if rotated.refresh_token.is_empty() {
        rotated.refresh_token = creds.refresh_token.clone();
    }
    Ok(rotated)
}

/// Write rotated creds back, preserving every other key (mcpOAuth, etc.).
/// Atomic (temp + rename) and 0600 so a crash can't leave a partial/world-
/// readable token.
pub fn write_claude_creds(path: &Path, creds: &ClaudeCreds) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));
    let oauth = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".credentials.json root is not an object"))?
        .entry("claudeAiOauth".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let oauth = oauth
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("claudeAiOauth is not an object"))?;
    oauth.insert("accessToken".into(), creds.access_token.clone().into());
    oauth.insert("refreshToken".into(), creds.refresh_token.clone().into());
    oauth.insert("expiresAt".into(), creds.expires_at_ms.into());

    let serialized = serde_json::to_vec_pretty(&value)?;
    let tmp = path.with_extension("credentials.tmp");
    std::fs::write(&tmp, &serialized)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Whether the token should be refreshed now (within `skew` of expiry).
pub fn needs_refresh(creds: &ClaudeCreds, now_ms: i64, skew: Duration) -> bool {
    creds.expires_at_ms - now_ms <= skew.as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "claudeAiOauth": {
        "accessToken": "at-old", "refreshToken": "rt-old",
        "expiresAt": 1781102803893, "subscriptionType": "max"
      },
      "mcpOAuth": { "keep": "me" }
    }"#;

    #[test]
    fn parses_credentials() {
        let creds = parse_claude_creds(&serde_json::from_str(SAMPLE).unwrap()).unwrap();
        assert_eq!(creds.access_token, "at-old");
        assert_eq!(creds.refresh_token, "rt-old");
        assert_eq!(creds.expires_at_ms, 1781102803893);
    }

    #[test]
    fn refresh_request_shape() {
        let (url, body) = refresh_request("rt-old");
        assert_eq!(url, TOKEN_URL);
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "rt-old");
        assert_eq!(body["client_id"], CLIENT_ID);
    }

    #[test]
    fn parses_response_and_computes_expiry() {
        let body = r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":28800}"#;
        let creds = parse_refresh_response(body, 1_000_000).unwrap();
        assert_eq!(creds.access_token, "at-new");
        assert_eq!(creds.refresh_token, "rt-new");
        assert_eq!(creds.expires_at_ms, 1_000_000 + 28800 * 1000);
    }

    #[test]
    fn write_preserves_other_keys_and_rotates_oauth() {
        let dir = std::env::temp_dir().join(format!("claude-creds-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        std::fs::write(&path, SAMPLE).unwrap();
        write_claude_creds(
            &path,
            &ClaudeCreds {
                access_token: "at-new".into(),
                refresh_token: "rt-new".into(),
                expires_at_ms: 42,
            },
        )
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["claudeAiOauth"]["accessToken"], "at-new");
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "rt-new");
        assert_eq!(v["claudeAiOauth"]["expiresAt"], 42);
        // Untouched keys survive.
        assert_eq!(v["claudeAiOauth"]["subscriptionType"], "max");
        assert_eq!(v["mcpOAuth"]["keep"], "me");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn needs_refresh_within_skew() {
        let creds = ClaudeCreds {
            access_token: "x".into(),
            refresh_token: "y".into(),
            expires_at_ms: 1_000_000,
        };
        // 20 min before expiry, skew 15 min → not yet.
        assert!(!needs_refresh(&creds, 1_000_000 - 20 * 60 * 1000, REFRESH_SKEW));
        // 10 min before expiry → refresh.
        assert!(needs_refresh(&creds, 1_000_000 - 10 * 60 * 1000, REFRESH_SKEW));
    }

    #[test]
    fn missing_rotated_refresh_token_keeps_old() {
        let body = r#"{"access_token":"at-new","expires_in":100}"#;
        let parsed = parse_refresh_response(body, 0).unwrap();
        assert!(parsed.refresh_token.is_empty()); // refresh_claude_token fills it
    }
}
