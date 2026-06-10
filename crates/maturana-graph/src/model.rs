//! The property-graph data model: nodes, edges, and the subgraph returned by
//! traversals and GraphRAG retrieval.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Caller-stable node identifier (e.g. `person:anders`). Stable ids make
/// `upsert` idempotent so re-ingesting the same fact updates rather than
/// duplicates.
pub type NodeId = String;

/// Edge identifier. If a caller leaves it empty, the store derives a
/// deterministic id from `from|type|to` so the same relation collapses to one.
pub type EdgeId = String;

/// A graph node: an entity with labels (types), free-form JSON properties, and
/// an optional embedding vector for similarity search.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Node {
    pub id: NodeId,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub props: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

impl Node {
    pub fn new(id: impl Into<NodeId>) -> Self {
        Self {
            id: id.into(),
            ..Default::default()
        }
    }

    /// A short human-facing name for rendering: the `name` prop, else `title`,
    /// else the id.
    pub fn display_name(&self) -> &str {
        for key in ["name", "title", "label"] {
            if let Some(Value::String(value)) = self.props.get(key) {
                if !value.is_empty() {
                    return value;
                }
            }
        }
        &self.id
    }
}

/// A directed, typed, weighted relationship between two nodes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Edge {
    #[serde(default)]
    pub id: EdgeId,
    pub from: NodeId,
    pub to: NodeId,
    #[serde(rename = "type")]
    pub etype: String,
    #[serde(default)]
    pub props: Map<String, Value>,
    #[serde(default = "default_weight")]
    pub weight: f32,
}

fn default_weight() -> f32 {
    1.0
}

impl Edge {
    pub fn new(from: impl Into<NodeId>, etype: impl Into<String>, to: impl Into<NodeId>) -> Self {
        Self {
            id: String::new(),
            from: from.into(),
            etype: etype.into(),
            to: to.into(),
            props: Map::new(),
            weight: 1.0,
        }
    }

    /// Deterministic id from the relationship triple, used when the caller did
    /// not supply one. Keeps repeated upserts of the same relation idempotent.
    pub fn derived_id(&self) -> EdgeId {
        format!("{}\u{1}{}\u{1}{}", self.from, self.etype, self.to)
    }

    /// The id to store under: the caller's if present, otherwise derived.
    pub fn effective_id(&self) -> EdgeId {
        if self.id.trim().is_empty() {
            self.derived_id()
        } else {
            self.id.clone()
        }
    }
}

/// A bundle of nodes and edges — the result of a neighborhood traversal or a
/// GraphRAG query.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl Subgraph {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }
}
