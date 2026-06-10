//! Bespoke crash-safe persistence: an append-only write-ahead log of mutations
//! plus periodic full snapshots. No third-party storage engine.
//!
//! Durability model: every live mutation is appended to `graph.wal` and fsync'd
//! *before* it touches in-memory state, so the WAL is the source of truth. A
//! snapshot writes the whole graph to `graph.snapshot.json` (temp + atomic
//! rename) and then truncates the WAL. Load = read the snapshot, then replay the
//! WAL. Because every mutation is idempotent (upsert replaces, delete removes),
//! a crash that leaves un-truncated WAL records already captured in the snapshot
//! is harmless — replay simply re-applies them to the same result.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::model::{Edge, EdgeId, Node, NodeId};

pub const WAL_FILE: &str = "graph.wal";
pub const SNAPSHOT_FILE: &str = "graph.snapshot.json";

/// One durable change to the graph. JSON-tagged so the WAL stays inspectable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Mutation {
    UpsertNode { node: Node },
    UpsertEdge { edge: Edge },
    DeleteNode { id: NodeId },
    DeleteEdge { id: EdgeId },
}

/// On-disk full snapshot of the graph.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
}

/// Append-only log handle. Owns the open file in append mode.
pub struct Wal {
    path: PathBuf,
    file: File,
}

impl Wal {
    fn open_append(path: &Path) -> anyhow::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open WAL {}", path.display()))
    }

    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let path = dir.join(WAL_FILE);
        let file = Self::open_append(&path)?;
        Ok(Self { path, file })
    }

    /// Append a mutation as one JSON line and fsync it to disk.
    pub fn append(&mut self, mutation: &Mutation) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(mutation)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Truncate the WAL (called after a snapshot has captured all prior state).
    pub fn truncate(&mut self) -> anyhow::Result<()> {
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .with_context(|| format!("failed to truncate WAL {}", self.path.display()))?;
        self.file.sync_all()?;
        Ok(())
    }
}

/// Read the snapshot (if any) for this directory.
pub fn read_snapshot(dir: &Path) -> anyhow::Result<Snapshot> {
    let path = dir.join(SNAPSHOT_FILE);
    if !path.exists() {
        return Ok(Snapshot::default());
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Snapshot::default());
    }
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

/// Read and parse the WAL records in order. A torn final line (from a crash
/// mid-append) is ignored rather than failing the load.
pub fn read_wal(dir: &Path) -> anyhow::Result<Vec<Mutation>> {
    let path = dir.join(WAL_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Mutation>(trimmed) {
            Ok(mutation) => out.push(mutation),
            // A partially-written trailing record from a crash: stop here, the
            // earlier records are intact and complete.
            Err(_) => break,
        }
    }
    Ok(out)
}

/// Atomically write a snapshot: temp file + fsync + rename over the target.
pub fn write_snapshot(dir: &Path, snapshot: &Snapshot) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    let target = dir.join(SNAPSHOT_FILE);
    let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
    let mut file = File::create(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    file.write_all(serde_json::to_string(snapshot)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &target)
        .with_context(|| format!("failed to install snapshot {}", target.display()))?;
    Ok(())
}

/// Build a `Mutation` from a node/edge for the live mutation path.
pub fn upsert_node(node: Node) -> Mutation {
    Mutation::UpsertNode { node }
}
pub fn upsert_edge(edge: Edge) -> Mutation {
    Mutation::UpsertEdge { edge }
}
