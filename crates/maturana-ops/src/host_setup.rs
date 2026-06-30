use anyhow::Context;
use maturana_core::state::MaturanaHome;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub struct UbuntuCloudimgRepair {
    pub home: MaturanaHome,
    pub release: String,
    pub arch: String,
    pub image_url: Option<String>,
    pub sha256sums_url: Option<String>,
    pub qemu_img: Option<PathBuf>,
    pub force: bool,
}

pub fn repair_ubuntu_cloudimg(repair: UbuntuCloudimgRepair) -> anyhow::Result<()> {
    let image_url = repair.image_url.unwrap_or_else(|| {
        format!(
            "https://cloud-images.ubuntu.com/{release}/current/{release}-server-cloudimg-{arch}.img",
            release = repair.release,
            arch = repair.arch
        )
    });
    let sha256sums_url = repair.sha256sums_url.unwrap_or_else(|| {
        format!(
            "https://cloud-images.ubuntu.com/{}/current/SHA256SUMS",
            repair.release
        )
    });

    let image_name = image_url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("image URL has no filename: {image_url}"))?
        .to_string();
    let image_dir = repair
        .home
        .root()
        .join("images")
        .join(format!("ubuntu-{}", repair.release));
    fs::create_dir_all(&image_dir)
        .with_context(|| format!("failed to create {}", image_dir.display()))?;

    let img_path = image_dir.join(&image_name);
    let sha_path = image_dir.join("SHA256SUMS");
    let vhdx_path = image_dir.join(format!(
        "{}-server-cloudimg-{}.vhdx",
        repair.release, repair.arch
    ));

    download_if_needed(
        &image_url,
        &img_path,
        repair.force,
        "official Ubuntu cloud image",
    )?;
    download_if_needed(&sha256sums_url, &sha_path, repair.force, "SHA256SUMS")?;

    let expected = expected_sha256_for_image(&sha_path, &image_name)?;
    let actual = sha256_file_hex(&img_path)?;
    if actual != expected {
        anyhow::bail!(
            "checksum mismatch for {}. Expected {} but got {}.",
            img_path.display(),
            expected,
            actual
        );
    }
    println!("Checksum OK.");

    if repair.force || !vhdx_path.exists() {
        let qemu_img = find_qemu_img(repair.qemu_img)?;
        println!("Converting image to VHDX with {}...", qemu_img.display());
        let status = Command::new(&qemu_img)
            .arg("convert")
            .arg("-p")
            .arg("-O")
            .arg("vhdx")
            .arg("-o")
            .arg("subformat=dynamic")
            .arg(&img_path)
            .arg(&vhdx_path)
            .status()
            .with_context(|| format!("failed to run {}", qemu_img.display()))?;
        if !status.success() {
            anyhow::bail!("qemu-img conversion failed with {status}");
        }
    } else {
        println!("Using existing VHDX {}", vhdx_path.display());
    }

    println!("VHDX: {}", vhdx_path.display());
    Ok(())
}

pub fn ensure_agent_ssh_key(key_path: PathBuf, force: bool) -> anyhow::Result<()> {
    if key_path.exists() && !force {
        println!("Using existing SSH key: {}", key_path.display());
        return Ok(());
    }

    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if force {
        let _ = fs::remove_file(&key_path);
        let _ = fs::remove_file(public_key_path(&key_path));
    }

    let status = Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(&key_path)
        .arg("-C")
        .arg("maturana-agent")
        .status()
        .context("failed to run ssh-keygen")?;
    if !status.success() {
        anyhow::bail!("ssh-keygen failed with {status}");
    }

    tighten_private_key_permissions(&key_path)?;
    println!("SSH key: {}", key_path.display());
    Ok(())
}

pub fn public_key_path(key_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.pub", key_path.display()))
}

pub fn expected_sha256_for_image(sha_path: &Path, image_name: &str) -> anyhow::Result<String> {
    let sha_text = fs::read_to_string(sha_path)
        .with_context(|| format!("failed to read {}", sha_path.display()))?;
    for line in sha_text.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        if parts.any(|part| part.trim_start_matches('*') == image_name) {
            return Ok(hash.to_lowercase());
        }
    }
    anyhow::bail!(
        "no checksum entry for {image_name} in {}",
        sha_path.display()
    )
}

pub fn sha256_file_hex(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn download_if_needed(url: &str, path: &Path, force: bool, label: &str) -> anyhow::Result<()> {
    if path.exists() && !force {
        println!("Using existing {} {}", label, path.display());
        return Ok(());
    }
    println!("Downloading {label}...");
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read response from {url}"))?;
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn find_qemu_img(requested: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = requested {
        let resolved = absolute_or_cwd(path)?;
        if resolved.exists() {
            return Ok(resolved);
        }
        anyhow::bail!("qemu-img not found at {}", resolved.display());
    }

    if let Some(path) = find_on_path(qemu_img_binary()) {
        return Ok(path);
    }

    for candidate in qemu_img_candidates() {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    anyhow::bail!(
        "{} is required to convert the official Ubuntu cloud image to VHDX. Install QEMU for Windows or pass --qemu-img.",
        qemu_img_binary()
    )
}

fn qemu_img_binary() -> &'static str {
    if cfg!(windows) {
        "qemu-img.exe"
    } else {
        "qemu-img"
    }
}

fn qemu_img_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if cfg!(windows) {
        candidates.extend([
            PathBuf::from(r"C:\Program Files\qemu\qemu-img.exe"),
            PathBuf::from(r"C:\Program Files (x86)\qemu\qemu-img.exe"),
            PathBuf::from(r"C:\msys64\mingw64\bin\qemu-img.exe"),
            PathBuf::from(r"C:\msys64\ucrt64\bin\qemu-img.exe"),
        ]);
        if let Some(localappdata) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(localappdata).join(
                r"Microsoft\WinGet\Packages\cloudbase.qemu-img_Microsoft.Winget.Source_8wekyb3d8bbwe\qemu-img.exe",
            ));
        }
    }
    candidates
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn tighten_private_key_permissions(key_path: &PathBuf) -> anyhow::Result<()> {
    let user = std::env::var("USERNAME").context("USERNAME is not set")?;
    let grant = format!("{user}:R");
    let status = Command::new("icacls")
        .arg(key_path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(grant)
        .status()
        .context("failed to run icacls")?;
    if !status.success() {
        anyhow::bail!("icacls failed with {status}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn tighten_private_key_permissions(key_path: &PathBuf) -> anyhow::Result<()> {
    let status = Command::new("chmod")
        .arg("600")
        .arg(key_path)
        .status()
        .context("failed to run chmod")?;
    if !status.success() {
        anyhow::bail!("chmod failed with {status}");
    }
    Ok(())
}

fn absolute_or_cwd(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}
