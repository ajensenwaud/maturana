//! MaturanaGraph: a from-scratch, dependency-light property-graph database with
//! a GraphRAG retrieval layer, built for per-agent knowledge graphs.
//!
//! - [`model`]: nodes, edges, subgraphs.
//! - [`store`]: the in-memory graph + crash-safe WAL/snapshot persistence + traversal.
//! - [`vector`]: from-scratch cosine similarity / brute-force search.
//! - [`graph_rag`]: local GraphRAG retrieval (seed → expand → score → render).
//!
//! No third-party graph, vector, or storage engine — only serde/json + uuid +
//! chrono, all already used across the workspace. Model work (extraction,
//! embeddings) is the caller's job (the agent, in its VM); this crate is pure
//! storage and graph/vector math.

pub mod graph_rag;
pub mod model;
pub mod store;
pub mod vector;
pub mod wal;

pub use graph_rag::{local_query, LocalQuery, RagResult, ScoredNode};
pub use model::{Edge, EdgeId, Node, NodeId, Subgraph};
pub use store::{Stats, Store};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "maturana-graph-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn person(id: &str, name: &str) -> Node {
        let mut node = Node::new(id);
        node.labels = vec!["Person".into()];
        node.props.insert("name".into(), json!(name));
        node
    }

    #[test]
    fn upsert_traverse_and_delete() {
        let dir = temp_dir("crud");
        let mut store = Store::open(&dir).unwrap();
        store.upsert_node(person("p:anders", "Anders")).unwrap();
        store.upsert_node(person("p:claude", "Claude")).unwrap();
        let mut org = Node::new("o:maturana");
        org.labels = vec!["Project".into()];
        org.props.insert("name".into(), json!("Maturana"));
        store.upsert_node(org).unwrap();

        store
            .upsert_edge(Edge::new("p:anders", "WORKS_ON", "o:maturana"))
            .unwrap();
        store
            .upsert_edge(Edge::new("p:claude", "WORKS_ON", "o:maturana"))
            .unwrap();

        // Idempotent: re-upserting the same relation does not duplicate it.
        store
            .upsert_edge(Edge::new("p:anders", "WORKS_ON", "o:maturana"))
            .unwrap();
        assert_eq!(store.stats().edges, 2);

        // Neighborhood of the project reaches both people in 1 hop.
        let exp = store.expand(&["o:maturana".into()], 1, None, 100);
        assert_eq!(exp.subgraph.nodes.len(), 3);
        assert_eq!(exp.subgraph.edges.len(), 2);
        assert_eq!(exp.distance["p:anders"], 1);

        assert_eq!(store.by_label("Person").len(), 2);

        // Deleting a node cascades its edges.
        store.delete_node("p:claude").unwrap();
        assert_eq!(store.stats().nodes, 2);
        assert_eq!(store.stats().edges, 1);
        assert!(store.get_edge("p:claude\u{1}WORKS_ON\u{1}o:maturana").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wal_recovers_after_crash_without_snapshot() {
        let dir = temp_dir("wal");
        {
            let mut store = Store::open(&dir).unwrap();
            store.upsert_node(person("p:a", "A")).unwrap();
            store.upsert_node(person("p:b", "B")).unwrap();
            store.upsert_edge(Edge::new("p:a", "KNOWS", "p:b")).unwrap();
            store.delete_node("p:b").unwrap();
            // Drop without an explicit snapshot — only the WAL is on disk.
        }
        // Reopen: state must be reconstructed purely from the WAL.
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.stats().nodes, 1);
        assert_eq!(store.stats().edges, 0);
        assert!(store.get_node("p:a").is_some());
        assert!(store.get_node("p:b").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_then_more_mutations_reload_correctly() {
        let dir = temp_dir("snap");
        {
            let mut store = Store::open(&dir).unwrap();
            store.upsert_node(person("p:a", "A")).unwrap();
            store.snapshot().unwrap(); // fold into snapshot, truncate WAL
            store.upsert_node(person("p:b", "B")).unwrap(); // post-snapshot WAL
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.stats().nodes, 2);
        assert!(store.get_node("p:a").is_some());
        assert!(store.get_node("p:b").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn vector_search_finds_nearest() {
        let dir = temp_dir("vec");
        let mut store = Store::open(&dir).unwrap();
        let mut a = person("p:a", "A");
        a.embedding = Some(vec![1.0, 0.0, 0.0]);
        let mut b = person("p:b", "B");
        b.embedding = Some(vec![0.0, 1.0, 0.0]);
        store.upsert_node(a).unwrap();
        store.upsert_node(b).unwrap();
        let hits = store.vector_search(&[0.9, 0.1, 0.0], 1, 0.0);
        assert_eq!(hits[0].0, "p:a");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn graphrag_local_query_assembles_subgraph_and_text() {
        let dir = temp_dir("rag");
        let mut store = Store::open(&dir).unwrap();
        let mut anders = person("p:anders", "Anders");
        anders.embedding = Some(vec![1.0, 0.0]);
        let mut maturana = Node::new("o:maturana");
        maturana.labels = vec!["Project".into()];
        maturana.props.insert("name".into(), json!("Maturana"));
        maturana
            .props
            .insert("summary".into(), json!("Secure agent orchestration"));
        maturana.embedding = Some(vec![0.8, 0.2]);
        store.upsert_node(anders).unwrap();
        store.upsert_node(maturana).unwrap();
        store
            .upsert_edge(Edge::new("p:anders", "FOUNDED", "o:maturana"))
            .unwrap();

        let query = LocalQuery {
            query_terms: vec!["Maturana".into()],
            query_embedding: Some(vec![0.79, 0.2]),
            depth: 2,
            ..Default::default()
        };
        let result = local_query(&store, &query);
        assert!(!result.seeds.is_empty());
        assert!(result.subgraph.nodes.iter().any(|n| n.id == "p:anders"));
        assert!(result.subgraph.edges.iter().any(|e| e.etype == "FOUNDED"));
        assert!(result.rendered_context.contains("Maturana"));
        assert!(result.rendered_context.contains("FOUNDED"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
