//! Bitwarden secret resolution for pipelock references.
//!
//! A pipelock entry may store a *reference* to a Bitwarden secret instead of a
//! literal value:
//!   - `bitwarden://<secret-id>`   — Bitwarden Secrets Manager (the `bws` CLI)
//!   - `bw://<item-id>[/<field>]`  — Bitwarden Password Manager (the `bw` CLI);
//!                                   `<field>` defaults to `password`.
//!
//! At injection time the host resolves the reference to the real value using a
//! token the operator stored in pipelock (`bitwarden/access-token` for Secrets
//! Manager, `bitwarden/session` for the password manager) or the matching
//! environment variable. Resolution runs on the host and the value is cached
//! briefly in-process — the raw secret never reaches a guest VM or the browser,
//! which keeps the zero-trust boundary intact while letting Bitwarden be the
//! source of truth for credentials.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::pipelock::PipelockVault;

const CACHE_TTL: Duration = Duration::from_secs(300);

/// Whether a stored pipelock value is a Bitwarden reference (vs a literal).
pub fn is_reference(value: &str) -> bool {
    value.starts_with("bitwarden://") || value.starts_with("bw://")
}

#[derive(Debug, PartialEq, Eq)]
enum Reference {
    /// Bitwarden Secrets Manager secret, resolved via `bws`.
    Bws { id: String },
    /// Bitwarden Password Manager item field, resolved via `bw`.
    Bw { id: String, field: String },
}

fn parse(value: &str) -> anyhow::Result<Reference> {
    if let Some(rest) = value.strip_prefix("bitwarden://") {
        let id = rest.trim();
        if id.is_empty() {
            anyhow::bail!("bitwarden:// reference is missing a secret id");
        }
        return Ok(Reference::Bws { id: id.to_string() });
    }
    if let Some(rest) = value.strip_prefix("bw://") {
        let rest = rest.trim();
        let (id, field) = match rest.split_once('/') {
            Some((id, field)) => (id.trim(), field.trim()),
            None => (rest, "password"),
        };
        if id.is_empty() {
            anyhow::bail!("bw:// reference is missing an item id");
        }
        let field = if field.is_empty() { "password" } else { field };
        return Ok(Reference::Bw {
            id: id.to_string(),
            field: field.to_string(),
        });
    }
    anyhow::bail!("not a bitwarden reference: {value}")
}

fn cache() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a Bitwarden reference to its secret value, using a token from `vault`
/// (or the matching env var). Cached in-process for [`CACHE_TTL`].
pub fn resolve(reference: &str, vault: &PipelockVault) -> anyhow::Result<String> {
    if let Some((value, at)) = cache().lock().unwrap().get(reference) {
        if at.elapsed() < CACHE_TTL {
            return Ok(value.clone());
        }
    }
    let value = match parse(reference)? {
        Reference::Bws { id } => resolve_bws(&id, vault)?,
        Reference::Bw { id, field } => resolve_bw(&id, &field, vault)?,
    };
    cache()
        .lock()
        .unwrap()
        .insert(reference.to_string(), (value.clone(), Instant::now()));
    Ok(value)
}

/// A token, preferring an env var (e.g. a process the operator started with it)
/// over the pipelock vault. Never logged.
fn token(vault: &PipelockVault, env_var: &str, vault_name: &str) -> anyhow::Result<String> {
    if let Ok(value) = std::env::var(env_var) {
        if !value.trim().is_empty() {
            return Ok(value);
        }
    }
    vault.get(vault_name).map_err(|_| {
        anyhow::anyhow!(
            "no Bitwarden credential found — set ${env_var} or run `maturana pipelock set {vault_name} <token>`"
        )
    })
}

fn resolve_bws(id: &str, vault: &PipelockVault) -> anyhow::Result<String> {
    let access = token(vault, "BWS_ACCESS_TOKEN", "bitwarden/access-token")?;
    let output = Command::new("bws")
        .args(["secret", "get", id, "--output", "json"])
        .env("BWS_ACCESS_TOKEN", access)
        .output()
        .map_err(|e| {
            anyhow::anyhow!("failed to run `bws` (install the Bitwarden Secrets Manager CLI): {e}")
        })?;
    if !output.status.success() {
        anyhow::bail!(
            "`bws secret get {id}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| anyhow::anyhow!("`bws` returned non-JSON output: {e}"))?;
    json.get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Bitwarden secret {id} has no 'value' field"))
}

fn resolve_bw(id: &str, field: &str, vault: &PipelockVault) -> anyhow::Result<String> {
    let session = token(vault, "BW_SESSION", "bitwarden/session")?;
    let output = Command::new("bw")
        .args(["get", field, id, "--session", &session, "--raw"])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run `bw` (install the Bitwarden CLI): {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "`bw get {field} {id}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_references() {
        assert!(is_reference("bitwarden://abc-123"));
        assert!(is_reference("bw://item-9/password"));
        assert!(!is_reference("literal-secret"));
        assert!(!is_reference("pipelock:foo"));
    }

    #[test]
    fn parses_secrets_manager_reference() {
        assert_eq!(
            parse("bitwarden://9f8e-secret-id").unwrap(),
            Reference::Bws {
                id: "9f8e-secret-id".to_string()
            }
        );
    }

    #[test]
    fn parses_password_manager_reference_with_default_and_explicit_field() {
        assert_eq!(
            parse("bw://item-42").unwrap(),
            Reference::Bw {
                id: "item-42".to_string(),
                field: "password".to_string()
            }
        );
        assert_eq!(
            parse("bw://item-42/username").unwrap(),
            Reference::Bw {
                id: "item-42".to_string(),
                field: "username".to_string()
            }
        );
    }

    #[test]
    fn rejects_empty_ids_and_non_references() {
        assert!(parse("bitwarden://").is_err());
        assert!(parse("bw://").is_err());
        assert!(parse("nope").is_err());
    }
}
