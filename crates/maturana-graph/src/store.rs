//! The in-memory property graph plus its persistence and traversal. One `Store`
//! owns one graph directory (e.g. a single agent's `.maturana/agents/<id>/graph`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use crate::model::{Edge, EdgeId, Node, NodeId, Subgraph};
use crate::vector;
use crate::wal::{self, Mutation, Snapshot, Wal};

/// How many mutations to apply before folding the WAL into a fresh snapshot.
const DEFAULT_SNAPSHOT_EVERY: usize = 512;

/// Result of a neighborhood expansion: the reachable subgraph plus each node's
/// hop distance from the nearest seed (used for scoring).
#[derive(Debug, Default)]
pub struct Expansion {
    pub subgraph: Subgraph,
    pub distance: HashMap<NodeId, usize>,
}

/// Aggregate counts for `GET /graph/stats`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Stats {
    pub nodes: usize,
    pub edges: usize,
    pub embedded_nodes: usize,
    pub labels: usize,
}

pub struct Store {
    dir: PathBuf,
    nodes: HashMap<NodeId, Node>,
    edges: HashMap<EdgeId, Edge>,
    out_adj: HashMap<NodeId, Vec<EdgeId>>,
    in_adj: HashMap<NodeId, Vec<EdgeId>>,
    label_index: HashMap<String, HashSet<NodeId>>,
    wal: Wal,
    mutations_since_snapshot: usize,
    snapshot_every: usize,
}

impl Store {
    /// Open (or create) the graph in `dir`: load the snapshot, replay the WAL,
    /// and reopen the WAL for appends.
    pub fn open(dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let dir = dir.into();
        let mut store = Self {
            wal: Wal::open(&dir)?,
            dir,
            nodes: HashMap::new(),
            edges: HashMap::new(),
            out_adj: HashMap::new(),
            in_adj: HashMap::new(),
            label_index: HashMap::new(),
            mutations_since_snapshot: 0,
            snapshot_every: DEFAULT_SNAPSHOT_EVERY,
        };

        let snapshot = wal::read_snapshot(&store.dir)?;
        for node in snapshot.nodes {
            store.put_node(node);
        }
        for edge in snapshot.edges {
            store.put_edge(edge);
        }
        for mutation in wal::read_wal(&store.dir)? {
            store.apply_in_memory(&mutation);
        }
        Ok(store)
    }

    // ---- public mutations (WAL-first, then memory) ----

    pub fn upsert_node(&mut self, node: Node) -> anyhow::Result<()> {
        let mutation = Mutation::UpsertNode { node };
        self.wal.append(&mutation)?;
        self.apply_in_memory(&mutation);
        self.maybe_snapshot()
    }

    /// Upsert an edge, resolving its id (caller's or derived) before logging so
    /// replay is deterministic. Returns the resolved id.
    pub fn upsert_edge(&mut self, mut edge: Edge) -> anyhow::Result<EdgeId> {
        edge.id = edge.effective_id();
        let id = edge.id.clone();
        let mutation = Mutation::UpsertEdge { edge };
        self.wal.append(&mutation)?;
        self.apply_in_memory(&mutation);
        self.maybe_snapshot()?;
        Ok(id)
    }

    pub fn delete_node(&mut self, id: &str) -> anyhow::Result<()> {
        let mutation = Mutation::DeleteNode { id: id.to_string() };
        self.wal.append(&mutation)?;
        self.apply_in_memory(&mutation);
        self.maybe_snapshot()
    }

    pub fn delete_edge(&mut self, id: &str) -> anyhow::Result<()> {
        let mutation = Mutation::DeleteEdge { id: id.to_string() };
        self.wal.append(&mutation)?;
        self.apply_in_memory(&mutation);
        self.maybe_snapshot()
    }

    // ---- reads ----

    pub fn get_node(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn get_edge(&self, id: &str) -> Option<&Edge> {
        self.edges.get(id)
    }

    pub fn by_label(&self, label: &str) -> Vec<&Node> {
        self.label_index
            .get(label)
            .into_iter()
            .flatten()
            .filter_map(|id| self.nodes.get(id))
            .collect()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            nodes: self.nodes.len(),
            edges: self.edges.len(),
            embedded_nodes: self
                .nodes
                .values()
                .filter(|n| n.embedding.is_some())
                .count(),
            labels: self.label_index.len(),
        }
    }

    /// Exact cosine search over embedded nodes; returns `(node_id, score)`.
    pub fn vector_search(&self, query: &[f32], k: usize, min_score: f32) -> Vec<(NodeId, f32)> {
        let candidates = self
            .nodes
            .values()
            .filter_map(|n| n.embedding.as_deref().map(|e| (n.id.as_str(), e)));
        vector::top_k(query, candidates, k, min_score)
    }

    /// Case-insensitive keyword seeds: nodes whose id, labels, display name, or
    /// string properties contain any of the given terms.
    pub fn text_search(&self, terms: &[String], limit: usize) -> Vec<NodeId> {
        let lower: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        if lower.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(NodeId, usize)> = self
            .nodes
            .values()
            .filter_map(|node| {
                let hay = node_haystack(node);
                let hits = lower.iter().filter(|t| hay.contains(t.as_str())).count();
                (hits > 0).then(|| (node.id.clone(), hits))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.into_iter().take(limit).map(|(id, _)| id).collect()
    }

    /// Breadth-first expand from `seeds` up to `depth` hops, following edges in
    /// either direction, optionally filtered to `edge_types`, capped at
    /// `max_nodes`. Returns the reachable subgraph and per-node hop distances.
    pub fn expand(
        &self,
        seeds: &[NodeId],
        depth: usize,
        edge_types: Option<&[String]>,
        max_nodes: usize,
    ) -> Expansion {
        let mut distance: HashMap<NodeId, usize> = HashMap::new();
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        let mut edge_ids: HashSet<EdgeId> = HashSet::new();

        for seed in seeds {
            if self.nodes.contains_key(seed) && !distance.contains_key(seed) {
                distance.insert(seed.clone(), 0);
                queue.push_back(seed.clone());
            }
        }

        while let Some(current) = queue.pop_front() {
            let hops = distance[&current];
            if hops >= depth || distance.len() >= max_nodes {
                continue;
            }
            for eid in self.incident_edges(&current) {
                let edge = match self.edges.get(&eid) {
                    Some(edge) => edge,
                    None => continue,
                };
                if let Some(types) = edge_types {
                    if !types.iter().any(|t| t == &edge.etype) {
                        continue;
                    }
                }
                let other = if edge.from == current {
                    &edge.to
                } else {
                    &edge.from
                };
                edge_ids.insert(eid.clone());
                if !distance.contains_key(other) && distance.len() < max_nodes {
                    distance.insert(other.clone(), hops + 1);
                    queue.push_back(other.clone());
                }
            }
        }

        let nodes = distance
            .keys()
            .filter_map(|id| self.nodes.get(id).cloned())
            .collect();
        // Keep only edges whose endpoints are both in the result set.
        let edges = edge_ids
            .iter()
            .filter_map(|eid| self.edges.get(eid))
            .filter(|e| distance.contains_key(&e.from) && distance.contains_key(&e.to))
            .cloned()
            .collect();

        Expansion {
            subgraph: Subgraph { nodes, edges },
            distance,
        }
    }

    /// Force a snapshot + WAL truncation (also exposed for tests/maintenance).
    pub fn snapshot(&mut self) -> anyhow::Result<()> {
        let snapshot = Snapshot {
            nodes: self.nodes.values().cloned().collect(),
            edges: self.edges.values().cloned().collect(),
        };
        wal::write_snapshot(&self.dir, &snapshot)?;
        self.wal.truncate()?;
        self.mutations_since_snapshot = 0;
        Ok(())
    }

    // ---- internal mutation/indexing ----

    fn maybe_snapshot(&mut self) -> anyhow::Result<()> {
        self.mutations_since_snapshot += 1;
        if self.mutations_since_snapshot >= self.snapshot_every {
            self.snapshot()?;
        }
        Ok(())
    }

    fn apply_in_memory(&mut self, mutation: &Mutation) {
        match mutation {
            Mutation::UpsertNode { node } => self.put_node(node.clone()),
            Mutation::UpsertEdge { edge } => self.put_edge(edge.clone()),
            Mutation::DeleteNode { id } => self.remove_node(id),
            Mutation::DeleteEdge { id } => self.remove_edge(id),
        }
    }

    fn put_node(&mut self, node: Node) {
        if let Some(existing) = self.nodes.get(&node.id) {
            for label in &existing.labels {
                if let Some(set) = self.label_index.get_mut(label) {
                    set.remove(&node.id);
                }
            }
        }
        for label in &node.labels {
            self.label_index
                .entry(label.clone())
                .or_default()
                .insert(node.id.clone());
        }
        self.nodes.insert(node.id.clone(), node);
    }

    fn put_edge(&mut self, edge: Edge) {
        // Edge id is already resolved by the time it reaches the WAL.
        if let Some(old) = self.edges.get(&edge.id).cloned() {
            detach(&mut self.out_adj, &old.from, &old.id);
            detach(&mut self.in_adj, &old.to, &old.id);
        }
        self.out_adj
            .entry(edge.from.clone())
            .or_default()
            .push(edge.id.clone());
        self.in_adj
            .entry(edge.to.clone())
            .or_default()
            .push(edge.id.clone());
        self.edges.insert(edge.id.clone(), edge);
    }

    fn remove_node(&mut self, id: &str) {
        if let Some(node) = self.nodes.remove(id) {
            for label in &node.labels {
                if let Some(set) = self.label_index.get_mut(label) {
                    set.remove(id);
                }
            }
        }
        let incident = self.incident_edges(id);
        for eid in incident {
            self.remove_edge(&eid);
        }
        self.out_adj.remove(id);
        self.in_adj.remove(id);
    }

    fn remove_edge(&mut self, id: &str) {
        if let Some(edge) = self.edges.remove(id) {
            detach(&mut self.out_adj, &edge.from, id);
            detach(&mut self.in_adj, &edge.to, id);
        }
    }

    fn incident_edges(&self, id: &str) -> Vec<EdgeId> {
        let mut out = self.out_adj.get(id).cloned().unwrap_or_default();
        if let Some(incoming) = self.in_adj.get(id) {
            out.extend(incoming.iter().cloned());
        }
        out
    }
}

fn detach(adj: &mut HashMap<NodeId, Vec<EdgeId>>, node: &str, edge_id: &str) {
    if let Some(list) = adj.get_mut(node) {
        list.retain(|e| e != edge_id);
    }
}

fn node_haystack(node: &Node) -> String {
    let mut hay = node.id.to_lowercase();
    hay.push(' ');
    hay.push_str(&node.labels.join(" ").to_lowercase());
    for value in node.props.values() {
        if let Some(text) = value.as_str() {
            hay.push(' ');
            hay.push_str(&text.to_lowercase());
        }
    }
    hay
}
