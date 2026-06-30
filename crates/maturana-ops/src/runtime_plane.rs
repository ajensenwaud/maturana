use anyhow::Context;
use maturana_core::state::MaturanaHome;
use rand::{distributions::Alphanumeric, Rng};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

/// Address the MaturanaGraph service binds to on the Linux host. Guests resolve
/// the URL sentinel to `http://<host-gateway>:47835`, so the port is fixed.
pub const GRAPH_BIND: &str = "0.0.0.0:47835";

pub fn ensure_sessiond_token(path: &Path) -> anyhow::Result<String> {
    ensure_token_file(path)
}

/// Ensure the host MaturanaGraph token (`<home>/graph/token`) exists,
/// generating one on first use.
pub fn ensure_graph_token(home: &MaturanaHome) -> anyhow::Result<String> {
    ensure_token_file(&home.root().join("graph").join("token"))
}

/// Start sessiond as a detached host-plane process.
pub fn start_linux_sessiond(
    home: &MaturanaHome,
    bind: &str,
    token: &str,
    token_path: &Path,
) -> anyhow::Result<u32> {
    let _ = Command::new("pkill")
        .arg("-f")
        .arg("maturana session serve")
        .status();
    let logs_dir = home.root().join("logs");
    fs::create_dir_all(&logs_dir)?;
    if let Some(parent) = token_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stdout = fs::File::create(logs_dir.join("sessiond-linux.out.log"))?;
    let stderr = fs::File::create(logs_dir.join("sessiond-linux.err.log"))?;
    let child = spawn_maturana(
        home.root(),
        &sessiond_args(bind, token),
        stdout,
        stderr,
        "failed to start sessiond",
    )?;
    fs::write(
        home.root().join("sessiond/runner.pid"),
        child.id().to_string(),
    )?;
    let pid = child.id();
    thread::sleep(Duration::from_secs(1));
    Ok(pid)
}

/// Start the MaturanaGraph host service as a detached process.
pub fn start_linux_graph(home: &MaturanaHome, bind: &str, token: &str) -> anyhow::Result<u32> {
    let _ = Command::new("pkill")
        .arg("-f")
        .arg("maturana graph serve")
        .status();
    let logs_dir = home.root().join("logs");
    fs::create_dir_all(&logs_dir)?;
    let stdout = fs::File::create(logs_dir.join("graph-linux.out.log"))?;
    let stderr = fs::File::create(logs_dir.join("graph-linux.err.log"))?;
    let child = spawn_maturana(
        home.root(),
        &graph_args(bind, token),
        stdout,
        stderr,
        "failed to start graph service",
    )?;
    let graph_dir = home.root().join("graph");
    fs::create_dir_all(&graph_dir)?;
    fs::write(graph_dir.join("runner.pid"), child.id().to_string())?;
    let pid = child.id();
    thread::sleep(Duration::from_secs(1));
    Ok(pid)
}

fn ensure_token_file(path: &Path) -> anyhow::Result<String> {
    if path.exists() {
        return Ok(fs::read_to_string(path)?.trim().to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(43)
        .map(char::from)
        .collect();
    fs::write(path, format!("{token}\n"))?;
    Ok(token)
}

fn spawn_maturana(
    home_root: &Path,
    args: &[String],
    stdout: fs::File,
    stderr: fs::File,
    context: &str,
) -> anyhow::Result<std::process::Child> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    Command::new(exe)
        .arg("--home")
        .arg(home_root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .with_context(|| context.to_string())
}

fn sessiond_args(bind: &str, token: &str) -> Vec<String> {
    vec![
        "session".to_string(),
        "serve".to_string(),
        "--bind".to_string(),
        bind.to_string(),
        "--token".to_string(),
        token.to_string(),
    ]
}

fn graph_args(bind: &str, token: &str) -> Vec<String> {
    vec![
        "graph".to_string(),
        "serve".to_string(),
        "--bind".to_string(),
        bind.to_string(),
        "--token".to_string(),
        token.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "maturana-ops-runtime-plane-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn token_file_is_created_and_reused() {
        let path = temp_path("token").join("sessiond/token");
        let first = ensure_sessiond_token(&path).unwrap();
        assert_eq!(first.len(), 43);
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{first}\n"));
        let second = ensure_sessiond_token(&path).unwrap();
        assert_eq!(first, second);
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn graph_token_uses_home_graph_path() {
        let root = temp_path("graph");
        let home = MaturanaHome::new(&root);
        let token = ensure_graph_token(&home).unwrap();
        assert_eq!(token.len(), 43);
        assert_eq!(
            fs::read_to_string(root.join("graph/token")).unwrap(),
            format!("{token}\n")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn service_args_are_narrow_and_stable() {
        assert_eq!(
            sessiond_args("0.0.0.0:47834", "tok"),
            vec![
                "session",
                "serve",
                "--bind",
                "0.0.0.0:47834",
                "--token",
                "tok"
            ]
        );
        assert_eq!(
            graph_args(GRAPH_BIND, "graph-tok"),
            vec![
                "graph",
                "serve",
                "--bind",
                GRAPH_BIND,
                "--token",
                "graph-tok"
            ]
        );
    }
}
