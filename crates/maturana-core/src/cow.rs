//! Copy-on-write VM storage — instant provisioning + snapshot/rewind of
//! Firecracker rootfs images on CoW filesystems.
//!
//! A Firecracker agent boots from a per-agent ext4 rootfs FILE. Today that file
//! is produced by a full byte-for-byte copy of a golden image — multiple GB per
//! agent, seconds to minutes of disk churn. On a copy-on-write filesystem a
//! *reflink* copy (`cp --reflink`) instead shares the underlying extents: the
//! clone is near-instant and costs almost no space until one side is written.
//!
//! That single primitive gives three things beyond what Hermes documents (it
//! only describes Docker):
//!   - **fast provisioning** — clone a golden rootfs to a new agent in
//!     milliseconds instead of copying GBs;
//!   - **snapshot** — reflink the live rootfs aside before a risky turn;
//!   - **rewind** — reflink a snapshot back over the live rootfs.
//!
//! Btrfs is the primary, in-kernel path (works on a stock Ubuntu); XFS and
//! OpenZFS 2.2+ (block cloning) also support reflink. On a non-CoW filesystem
//! (ext4) every operation falls back to a full copy, so behavior is correct
//! everywhere — just not instant. The filesystem is auto-detected from the path,
//! so nothing in a spec needs to change.

use std::path::Path;
use std::process::Command;

/// Whether a clone/snapshot used a shared-extent reflink (instant) or fell back
/// to a full byte copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CowKind {
    Reflink,
    FullCopy,
}

impl CowKind {
    pub fn label(self) -> &'static str {
        match self {
            CowKind::Reflink => "reflink (copy-on-write)",
            CowKind::FullCopy => "full copy",
        }
    }
}

/// Filesystems whose files support reflink copies (shared-extent CoW).
pub fn fstype_supports_reflink(fstype: &str) -> bool {
    matches!(
        fstype.trim().to_ascii_lowercase().as_str(),
        "btrfs" | "xfs" | "zfs"
    )
}

/// Parse the single-line output of `findmnt -n -o FSTYPE --target <path>`.
pub fn parse_fstype(findmnt_stdout: &str) -> Option<String> {
    let value = findmnt_stdout.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Detect the filesystem type backing `path` (Linux only; `None` elsewhere or if
/// `findmnt` is unavailable).
pub fn detect_fstype(path: &Path) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let output = Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "--target"])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_fstype(&String::from_utf8_lossy(&output.stdout))
}

/// True if `path` lives on a reflink-capable copy-on-write filesystem.
pub fn is_cow(path: &Path) -> bool {
    detect_fstype(path)
        .map(|fstype| fstype_supports_reflink(&fstype))
        .unwrap_or(false)
}

/// Copy `src` to `dest`, preferring an instant reflink (copy-on-write) and
/// falling back to a full byte copy when the filesystem can't reflink. Creates
/// `dest`'s parent directory. Returns which path was taken.
pub fn cow_copy(src: &Path, dest: &Path) -> anyhow::Result<CowKind> {
    if !src.exists() {
        anyhow::bail!("source image not found: {}", src.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Try a reflink first — instant + space-shared on Btrfs/XFS/ZFS-2.2+. `-f`
    // overwrites an existing dest (needed for rollback). If the filesystem does
    // not support reflink, `cp` exits non-zero and we fall back.
    if cfg!(target_os = "linux") {
        if let Ok(output) = Command::new("cp")
            .args(["--reflink=always", "-f"])
            .arg(src)
            .arg(dest)
            .output()
        {
            if output.status.success() {
                return Ok(CowKind::Reflink);
            }
        }
    }
    // Fallback: a normal full copy (correct everywhere, just not instant).
    std::fs::copy(src, dest)?;
    Ok(CowKind::FullCopy)
}

/// Clone a golden rootfs to a new agent's rootfs path (provisioning).
pub fn provision_clone(golden: &Path, dest: &Path) -> anyhow::Result<CowKind> {
    cow_copy(golden, dest)
}

/// Snapshot a live rootfs aside (e.g. before a risky turn).
pub fn snapshot(live: &Path, snap: &Path) -> anyhow::Result<CowKind> {
    cow_copy(live, snap)
}

/// Roll a live rootfs back to a previously taken snapshot (overwrites live).
pub fn rollback(snap: &Path, live: &Path) -> anyhow::Result<CowKind> {
    if !snap.exists() {
        anyhow::bail!("snapshot not found: {}", snap.display());
    }
    cow_copy(snap, live)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflink_capable_filesystems() {
        for fs in ["btrfs", "XFS", "zfs", " Btrfs "] {
            assert!(fstype_supports_reflink(fs), "{fs} should support reflink");
        }
        for fs in ["ext4", "ext3", "ntfs", "vfat", "overlay", ""] {
            assert!(!fstype_supports_reflink(fs), "{fs} should not");
        }
    }

    #[test]
    fn parses_findmnt_output() {
        assert_eq!(parse_fstype("btrfs\n").as_deref(), Some("btrfs"));
        assert_eq!(parse_fstype("   ext4  ").as_deref(), Some("ext4"));
        assert_eq!(parse_fstype("\n"), None);
        assert_eq!(parse_fstype(""), None);
    }

    #[test]
    fn cow_copy_round_trips_and_reports_kind() {
        let base = std::env::temp_dir().join(format!("maturana-cow-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let src = base.join("golden.img");
        let dest = base.join("agent/rootfs.img");
        std::fs::write(&src, b"rootfs-bytes").unwrap();

        let kind = cow_copy(&src, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"rootfs-bytes");
        // On the CI/test host (Windows/ext4) this is a full copy; on Btrfs it
        // would reflink. Either way the bytes must match.
        assert!(matches!(kind, CowKind::Reflink | CowKind::FullCopy));

        // Rollback requires an existing snapshot and overwrites the live file.
        let snap = base.join("snap.img");
        snapshot(&dest, &snap).unwrap();
        std::fs::write(&dest, b"mutated").unwrap();
        rollback(&snap, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"rootfs-bytes");

        assert!(rollback(&base.join("missing.img"), &dest).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
