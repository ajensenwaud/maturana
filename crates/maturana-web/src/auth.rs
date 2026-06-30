//! Token login → cookie session auth for the cockpit.
//!
//! The operator proves possession of `<home>/web/token` once at `/login`; the
//! server then issues an HttpOnly `SameSite=Strict` cookie backed by an
//! in-memory session map (sessions die with the process — re-login on restart
//! is the explicit trade-off). CSRF posture: SameSite=Strict cookies, a
//! required `x-maturana-web: 1` header on mutating REST calls, and an
//! Origin==Host check on the WebSocket upgrade. No CORS headers are ever set.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use rand::distributions::Alphanumeric;
use rand::Rng;

use crate::state::AppState;

pub const SESSION_COOKIE: &str = "maturana_web_session";
/// Custom header required on mutating REST requests; forces a CORS preflight
/// that a cross-origin attacker cannot pass (we never answer preflights).
pub const CSRF_HEADER: &str = "x-maturana-web";

/// Read the cockpit login token from `<home>/web/token`, generating one on
/// first run (same shape as the sessiond/graph token files: 43 alphanumeric
/// chars + newline).
pub fn ensure_web_token(home_root: &Path) -> anyhow::Result<String> {
    let path = home_root.join("web").join("token");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let token = random_token();
    std::fs::write(&path, format!("{token}\n"))?;
    Ok(token)
}

fn random_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(43)
        .map(char::from)
        .collect()
}

/// Same 8-line constant-time comparison used by sessiond and the graph
/// service (the repo's established duplication; consolidation tracked
/// separately).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// In-memory session map: cookie value → created-at. Process-lifetime only.
#[derive(Clone, Default)]
pub struct SessionStore {
    inner: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
}

impl SessionStore {
    pub fn create(&self) -> String {
        let id = random_token();
        self.inner
            .lock()
            .expect("session store poisoned")
            .insert(id.clone(), Utc::now());
        id
    }

    pub fn is_valid(&self, id: &str) -> bool {
        self.inner
            .lock()
            .expect("session store poisoned")
            .contains_key(id)
    }

    pub fn remove(&self, id: &str) {
        self.inner
            .lock()
            .expect("session store poisoned")
            .remove(id);
    }
}

/// Extract this server's session cookie value from a Cookie header.
pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let (name, value) = pair.trim().split_once('=')?;
        if name == SESSION_COOKIE {
            return Some(value.trim().to_string());
        }
    }
    None
}

pub fn has_valid_session(state: &AppState, headers: &HeaderMap) -> bool {
    session_cookie(headers)
        .map(|id| state.sessions.is_valid(&id))
        .unwrap_or(false)
}

#[derive(serde::Deserialize)]
pub struct LoginRequest {
    token: String,
}

pub async fn login(State(state): State<AppState>, Json(request): Json<LoginRequest>) -> Response {
    if !constant_time_eq(
        request.token.trim().as_bytes(),
        state.login_token.as_bytes(),
    ) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "ok": false, "error": "invalid token" })),
        )
            .into_response();
    }
    let session = state.sessions.create();
    let cookie = format!("{SESSION_COOKIE}={session}; HttpOnly; SameSite=Strict; Path=/");
    (
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(id) = session_cookie(&headers) {
        state.sessions.remove(&id);
    }
    let cookie = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    (
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

/// Gate everything except the public surface (/health, /login, static assets)
/// behind a valid session. Browsers hitting app pages get redirected to the
/// login page; API callers get a 401. Mutating API calls additionally require
/// the CSRF header.
pub async fn require_session(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let public = path == "/health" || path == "/login" || path.starts_with("/assets/");
    if public {
        return next.run(request).await;
    }

    if !has_valid_session(&state, request.headers()) {
        if path.starts_with("/api/") || path == "/ws" {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "ok": false, "error": "unauthorized" })),
            )
                .into_response();
        }
        return Redirect::temporary("/login").into_response();
    }

    let mutating = !matches!(request.method().as_str(), "GET" | "HEAD" | "OPTIONS");
    if mutating && path.starts_with("/api/") && !request.headers().contains_key(CSRF_HEADER) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "ok": false, "error": "missing x-maturana-web header" })),
        )
            .into_response();
    }

    next.run(request).await
}

/// DNS-rebinding defense for the WS upgrade. The real auth is the SameSite=Strict
/// session cookie (a rebinding attacker's page can't carry it), so this is
/// belt-and-braces: accept a missing Origin (non-browser), a same-hostname Origin
/// (ignoring a proxy's port rewrite), or a trusted-network Origin (loopback /
/// private LAN / tailnet / MagicDNS) — the only contexts this cockpit runs in.
/// Reject only a clearly-external public Origin.
pub fn origin_matches_host(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let origin_authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .unwrap_or(origin);
    let origin_host = origin_authority
        .split(':')
        .next()
        .unwrap_or(origin_authority);
    if let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        let host_name = host.split(':').next().unwrap_or(host);
        if origin_host.eq_ignore_ascii_case(host_name) {
            return true;
        }
    }
    is_trusted_network_host(origin_host)
}

/// True for hosts the cockpit is meant to be reached on: loopback, private LAN,
/// the Tailscale CGNAT range (100.64/10), single-label MagicDNS names, and
/// `*.ts.net` / `*.tailscale.net`.
fn is_trusted_network_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                v4.is_loopback() || v4.is_private() || (o[0] == 100 && (o[1] & 0xC0) == 0x40)
            }
            std::net::IpAddr::V6(v6) => v6.is_loopback(),
        };
    }
    !host.contains('.') || host.ends_with(".ts.net") || host.ends_with(".tailscale.net")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"tok", b"tok"));
        assert!(!constant_time_eq(b"tok", b"toK"));
        assert!(!constant_time_eq(b"tok", b"to"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn ensure_web_token_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("mweb-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = ensure_web_token(&dir).unwrap();
        let second = ensure_web_token(&dir).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 43);
        let on_disk = std::fs::read_to_string(dir.join("web/token")).unwrap();
        assert_eq!(on_disk.trim(), first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_store_lifecycle() {
        let store = SessionStore::default();
        let id = store.create();
        assert!(store.is_valid(&id));
        assert!(!store.is_valid("nope"));
        store.remove(&id);
        assert!(!store.is_valid(&id));
    }

    #[test]
    fn session_cookie_parses_among_other_cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("a=b; {SESSION_COOKIE}=secret123 ; c=d")
                .parse()
                .unwrap(),
        );
        assert_eq!(session_cookie(&headers).as_deref(), Some("secret123"));
        let mut none = HeaderMap::new();
        none.insert(header::COOKIE, "a=b".parse().unwrap());
        assert_eq!(session_cookie(&none), None);
    }

    #[test]
    fn origin_check_accepts_same_host_and_absent_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "cockpit:47836".parse().unwrap());
        assert!(origin_matches_host(&headers)); // no Origin: non-browser client
        headers.insert(header::ORIGIN, "http://cockpit:47836".parse().unwrap());
        assert!(origin_matches_host(&headers));
        headers.insert(header::ORIGIN, "http://evil.example".parse().unwrap());
        assert!(!origin_matches_host(&headers));
    }
}
