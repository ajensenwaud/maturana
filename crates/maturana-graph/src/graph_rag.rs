//! GraphRAG retrieval over the store. "Local" search: pick seed entities by
//! vector similarity and keyword match, expand their neighborhood a few hops,
//! score the reachable nodes, and render a compact subgraph the agent can read.
//!
//! No model lives here — the caller (the agent, in its VM) supplies the query
//! embedding. This module is pure graph + vector math.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::{NodeId, Subgraph};
use crate::store::Store;
use crate::vector::cosine;

/// A GraphRAG retrieval request. `query_embedding` is computed by the agent;
/// `query_terms` are optional keyword seeds (also from the agent).
#[derive(Debug, Clone, Deserialize)]
pub struct LocalQuery {
    #[serde(default)]
    pub query_terms: Vec<String>,
    #[serde(default)]
    pub query_embedding: Option<Vec<f32>>,
    #[serde(default)]
    pub seed_ids: Vec<NodeId>,
    #[serde(default = "default_k")]
    pub k: usize,
    #[serde(default = "default_depth")]
    pub depth: usize,
    #[serde(default)]
    pub edge_types: Option<Vec<String>>,
    #[serde(default = "default_max_nodes")]
    pub max_nodes: usize,
}

fn default_k() -> usize {
    8
}
fn default_depth() -> usize {
    2
}
fn default_max_nodes() -> usize {
    60
}

impl Default for LocalQuery {
    fn default() -> Self {
        Self {
            query_terms: Vec::new(),
            query_embedding: None,
            seed_ids: Vec::new(),
            k: default_k(),
            depth: default_depth(),
            edge_types: None,
            max_nodes: default_max_nodes(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoredNode {
    pub id: NodeId,
    pub score: f32,
    pub hops: usize,
}

/// The retrieval result: the assembled subgraph, per-node scores, the seed ids
/// it started from, and a text rendering for the prompt.
#[derive(Debug, Clone, Serialize)]
pub struct RagResult {
    pub seeds: Vec<NodeId>,
    pub scored: Vec<ScoredNode>,
    pub subgraph: Subgraph,
    pub rendered_context: String,
}

/// Run a local GraphRAG query against `store`.
pub fn local_query(store: &Store, query: &LocalQuery) -> RagResult {
    // 1. Seeds: explicit ids + vector hits + keyword hits.
    let mut seed_sim: HashMap<NodeId, f32> = HashMap::new();
    for id in &query.seed_ids {
        if store.get_node(id).is_some() {
            seed_sim.entry(id.clone()).or_insert(1.0);
        }
    }
    if let Some(embedding) = &query.query_embedding {
        for (id, score) in store.vector_search(embedding, query.k, 0.0) {
            let entry = seed_sim.entry(id).or_insert(0.0);
            *entry = entry.max(score);
        }
    }
    for id in store.text_search(&query.query_terms, query.k) {
        seed_sim.entry(id).or_insert(0.5);
    }

    let seeds: Vec<NodeId> = seed_sim.keys().cloned().collect();

    // 2. Expand the neighborhood.
    let expansion = store.expand(&seeds, query.depth, query.edge_types.as_deref(), query.max_nodes);

    // 3. Score: seed similarity (or query cosine) discounted by hop distance.
    let mut scored: Vec<ScoredNode> = expansion
        .subgraph
        .nodes
        .iter()
        .map(|node| {
            let hops = *expansion.distance.get(&node.id).unwrap_or(&query.depth);
            let base = if let Some(sim) = seed_sim.get(&node.id) {
                *sim
            } else if let (Some(q), Some(e)) = (&query.query_embedding, &node.embedding) {
                cosine(q, e).max(0.0)
            } else {
                0.0
            };
            // Each hop away halves the contribution; a small floor keeps
            // structurally-connected context from scoring exactly zero.
            let score = (base + 0.05) / (1.0 + hops as f32);
            ScoredNode {
                id: node.id.clone(),
                score,
                hops,
            }
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.hops.cmp(&b.hops))
    });

    let rendered_context = render(store, &expansion.subgraph, &scored);

    RagResult {
        seeds,
        scored,
        subgraph: expansion.subgraph,
        rendered_context,
    }
}

/// Render the subgraph as compact text for an agent prompt: entities (by score)
/// then the relationships between them.
fn render(store: &Store, subgraph: &Subgraph, scored: &[ScoredNode]) -> String {
    if subgraph.is_empty() {
        return "(no matching knowledge)".to_string();
    }
    let mut out = String::from("Entities:\n");
    for s in scored {
        if let Some(node) = store.get_node(&s.id) {
            let labels = if node.labels.is_empty() {
                String::new()
            } else {
                format!(" [{}]", node.labels.join(", "))
            };
            out.push_str(&format!("- {}{}\n", node.display_name(), labels));
            // Include the substance: a chunk's text, or an entity's summary,
            // truncated so the rendered context stays compact.
            if let Some(content) = node
                .props
                .get("text")
                .or_else(|| node.props.get("summary"))
                .or_else(|| node.props.get("description"))
                .and_then(|v| v.as_str())
            {
                out.push_str("  ");
                out.push_str(&truncate(content, 600).replace('\n', "\n  "));
                out.push('\n');
            }
        }
    }
    if !subgraph.edges.is_empty() {
        out.push_str("\nRelationships:\n");
        for edge in &subgraph.edges {
            let from = name_of(store, &edge.from);
            let to = name_of(store, &edge.to);
            out.push_str(&format!("- {from} -[{}]-> {to}\n", edge.etype));
        }
    }
    out
}

fn name_of(store: &Store, id: &str) -> String {
    store
        .get_node(id)
        .map(|n| n.display_name().to_string())
        .unwrap_or_else(|| id.to_string())
}

/// Truncate to at most `max` chars on a char boundary, adding an ellipsis.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}
