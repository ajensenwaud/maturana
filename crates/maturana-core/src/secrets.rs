use crate::{pipelock::PipelockVault, state::MaturanaHome};
use std::{env, fs, path::Path};

#[derive(Clone)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn expose_for_runtime(&self) -> &str {
        &self.0
    }
}

// Never render the plaintext through `{:?}`. A derived Debug would leak the
// secret into any log line, panic message, or `.context(format!(..))` that
// happens to capture the value.
impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(***)")
    }
}

pub fn resolve_secret_source(source: &str) -> anyhow::Result<SecretValue> {
    let cwd = env::current_dir()?;
    let home = MaturanaHome::default_for_cwd(&cwd);
    resolve_secret_source_with_home(source, home.root())
}

pub fn resolve_secret_source_with_home(
    source: &str,
    home_root: &Path,
) -> anyhow::Result<SecretValue> {
    if let Some(name) = source.strip_prefix("env:") {
        return Ok(SecretValue(env::var(name)?));
    }

    if let Some(path) = source.strip_prefix("file:") {
        return Ok(SecretValue(fs::read_to_string(path)?.trim().to_string()));
    }

    if let Some(name) = source.strip_prefix("pipelock:") {
        let vault = PipelockVault::new(home_root.join("pipelock"));
        return Ok(SecretValue(vault.get(name)?));
    }

    anyhow::bail!("unsupported secret source; expected env:, file:, or pipelock:")
}

#[cfg(test)]
mod tests {
    use super::resolve_secret_source_with_home;
    use crate::pipelock::PipelockVault;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn resolves_pipelock_source() {
        let home = std::env::temp_dir().join(format!("maturana-home-{}", Uuid::new_v4()));
        let vault = PipelockVault::new(home.join("pipelock"));
        vault.set("telegram/bot-token", "secret-value").unwrap();

        let secret = resolve_secret_source_with_home("pipelock:telegram/bot-token", &home)
            .unwrap()
            .expose_for_runtime()
            .to_string();
        assert_eq!(secret, "secret-value");

        let _ = fs::remove_dir_all(home);
    }
}
