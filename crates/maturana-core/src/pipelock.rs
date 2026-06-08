use anyhow::Context;
use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use chrono::{DateTime, Utc};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

#[derive(Debug, Clone)]
pub struct PipelockVault {
    dir: PathBuf,
}

impl PipelockVault {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn init(&self) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir)?;
        if !self.key_path().exists() {
            let mut key = [0u8; KEY_LEN];
            OsRng.fill_bytes(&mut key);
            fs::write(self.key_path(), STANDARD.encode(key))?;
            restrict_file_best_effort(&self.key_path());
        }
        if !self.vault_path().exists() {
            self.write_vault(&VaultFile::default())?;
        }
        Ok(())
    }

    pub fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        validate_name(name)?;
        self.init()?;
        let key = self.read_key()?;
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let cipher = ChaCha20Poly1305::new_from_slice(&key)
            .map_err(|_| anyhow::anyhow!("invalid pipelock key length"))?;
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), value.as_bytes())
            .map_err(|_| anyhow::anyhow!("failed to encrypt pipelock secret"))?;

        let mut vault = self.read_vault()?;
        vault.secrets.insert(
            name.to_string(),
            VaultEntry {
                nonce: STANDARD.encode(nonce),
                ciphertext: STANDARD.encode(ciphertext),
                updated_at: Utc::now(),
            },
        );
        self.write_vault(&vault)
    }

    pub fn get(&self, name: &str) -> anyhow::Result<String> {
        validate_name(name)?;
        let key = self.read_key()?;
        let vault = self.read_vault()?;
        let entry = vault
            .secrets
            .get(name)
            .with_context(|| format!("pipelock secret not found: {name}"))?;
        let nonce = STANDARD
            .decode(&entry.nonce)
            .context("failed to decode pipelock nonce")?;
        if nonce.len() != NONCE_LEN {
            anyhow::bail!("pipelock nonce must be {NONCE_LEN} bytes");
        }
        let ciphertext = STANDARD
            .decode(&entry.ciphertext)
            .context("failed to decode pipelock ciphertext")?;
        let cipher = ChaCha20Poly1305::new_from_slice(&key)
            .map_err(|_| anyhow::anyhow!("invalid pipelock key length"))?;
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| anyhow::anyhow!("failed to decrypt pipelock secret"))?;
        String::from_utf8(plaintext).context("pipelock secret is not valid UTF-8")
    }

    pub fn list(&self) -> anyhow::Result<Vec<String>> {
        let vault = self.read_vault()?;
        Ok(vault.secrets.keys().cloned().collect())
    }

    pub fn delete(&self, name: &str) -> anyhow::Result<bool> {
        validate_name(name)?;
        let mut vault = self.read_vault()?;
        let removed = vault.secrets.remove(name).is_some();
        self.write_vault(&vault)?;
        Ok(removed)
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.join("key")
    }

    pub fn vault_path(&self) -> PathBuf {
        self.dir.join("vault.json")
    }

    fn read_key(&self) -> anyhow::Result<[u8; KEY_LEN]> {
        let raw = fs::read_to_string(self.key_path()).with_context(|| {
            format!(
                "pipelock is not initialized; run `maturana pipelock init` first ({})",
                self.key_path().display()
            )
        })?;
        let decoded = STANDARD
            .decode(raw.trim())
            .context("failed to decode pipelock key")?;
        decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("pipelock key must be {KEY_LEN} bytes"))
    }

    fn read_vault(&self) -> anyhow::Result<VaultFile> {
        let raw = fs::read_to_string(self.vault_path()).with_context(|| {
            format!(
                "pipelock vault not found; run `maturana pipelock init` first ({})",
                self.vault_path().display()
            )
        })?;
        serde_json::from_str(&raw).context("failed to parse pipelock vault")
    }

    fn write_vault(&self, vault: &VaultFile) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir)?;
        fs::write(self.vault_path(), serde_json::to_string_pretty(vault)?)?;
        restrict_file_best_effort(&self.vault_path());
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct VaultFile {
    version: u8,
    secrets: BTreeMap<String, VaultEntry>,
}

impl Default for VaultFile {
    fn default() -> Self {
        Self {
            version: 1,
            secrets: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct VaultEntry {
    nonce: String,
    ciphertext: String,
    updated_at: DateTime<Utc>,
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("pipelock secret name cannot be empty");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/'))
    {
        anyhow::bail!(
            "pipelock secret names may only contain ASCII letters, digits, '.', '_', '-', and '/'"
        );
    }
    Ok(())
}

fn restrict_file_best_effort(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            let _ = fs::set_permissions(path, permissions);
        }
    }

    #[cfg(windows)]
    {
        if let Ok(user) = std::env::var("USERNAME") {
            let _ = std::process::Command::new("icacls.exe")
                .arg(path)
                .arg("/inheritance:r")
                .arg("/grant:r")
                .arg(format!("{user}:F"))
                .output();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PipelockVault;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn round_trips_secret() {
        let dir = std::env::temp_dir().join(format!("maturana-pipelock-{}", Uuid::new_v4()));
        let vault = PipelockVault::new(&dir);
        vault.init().unwrap();
        vault.set("telegram/bot-token", "secret-value").unwrap();

        assert_eq!(vault.get("telegram/bot-token").unwrap(), "secret-value");
        assert_eq!(
            vault.list().unwrap(),
            vec!["telegram/bot-token".to_string()]
        );
        assert!(vault.delete("telegram/bot-token").unwrap());
        assert!(vault.list().unwrap().is_empty());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_names() {
        let dir = std::env::temp_dir().join(format!("maturana-pipelock-{}", Uuid::new_v4()));
        let vault = PipelockVault::new(&dir);
        let error = vault.set("bad:name", "value").unwrap_err().to_string();
        assert!(error.contains("pipelock secret names"));
        let _ = fs::remove_dir_all(dir);
    }
}
