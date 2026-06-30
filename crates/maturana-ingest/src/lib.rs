//! Document ingestion for MaturanaGraph.
//!
//! Parses a file (PDF, PPTX, DOCX, MD, TXT, HTML, JSON) into plain text,
//! chunks it, and builds graph nodes/edges: a `Document` node, a `Chunk` node
//! per chunk (with the text as a searchable property), `CONTAINS` edges from the
//! document to its chunks, and `NEXT` edges threading the chunks in order.
//!
//! No model runs here — this is deterministic parsing. Entity/relation
//! extraction and embeddings are added later by an agent (in its VM). The chunk
//! text is immediately keyword-searchable in the graph.

use std::path::Path;

use anyhow::{Context, Result};
use maturana_graph::{Edge, Node};
use serde_json::json;

mod office;

/// A parsed document: extracted plain text plus light metadata.
#[derive(Debug, Clone)]
pub struct Document {
    pub title: String,
    pub source: String,
    pub format: String,
    pub text: String,
}

/// The graph payload built from a document: nodes + edges ready to upsert.
#[derive(Debug, Default)]
pub struct Ingested {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub chunks: usize,
}

/// Parse a file into a [`Document`], dispatching on its extension.
pub fn parse(path: &Path) -> Result<Document> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let source = path.display().to_string();
    let title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document")
        .to_string();

    let text = match ext.as_str() {
        "md" | "markdown" | "txt" | "text" | "" => {
            std::fs::read_to_string(path).with_context(|| format!("failed to read {source}"))?
        }
        "json" => std::fs::read_to_string(path).with_context(|| format!("failed to read {source}"))?,
        "html" | "htm" => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {source}"))?;
            strip_html(&raw)
        }
        "pdf" => pdf_extract::extract_text(path)
            .with_context(|| format!("failed to extract text from PDF {source}"))?,
        "pptx" => office::extract_pptx(path)?,
        "docx" => office::extract_docx(path)?,
        other => anyhow::bail!(
            "unsupported document type '.{other}' ({source}); supported: pdf, pptx, docx, md, txt, html, json"
        ),
    };

    Ok(Document {
        title,
        source,
        format: if ext.is_empty() { "txt".into() } else { ext },
        text: normalize_ws(&text),
    })
}

/// Parse and convert to graph nodes/edges in one step.
pub fn ingest(path: &Path, chunk_chars: usize) -> Result<Ingested> {
    let doc = parse(path)?;
    Ok(to_graph(&doc, chunk_chars))
}

/// Split text into readable chunks near `target_chars`, preferring paragraph
/// (blank-line) boundaries and never splitting mid-word.
pub fn chunk(text: &str, target_chars: usize) -> Vec<String> {
    let target = target_chars.max(400);
    let mut chunks = Vec::new();
    let mut current = String::new();
    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if current.len() + para.len() + 2 > target && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if para.len() > target {
            // A single oversized paragraph: hard-split on word boundaries.
            for word in para.split_whitespace() {
                if current.len() + word.len() + 1 > target && !current.is_empty() {
                    chunks.push(std::mem::take(&mut current));
                }
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            }
        } else {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(para);
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Build the document + chunk subgraph. Ids are derived from the source so
/// re-ingesting the same file upserts (idempotent) rather than duplicates.
pub fn to_graph(doc: &Document, chunk_chars: usize) -> Ingested {
    let chunks = chunk(&doc.text, chunk_chars);
    let doc_slug = slug(&doc.source);
    let doc_id = format!("doc:{doc_slug}");

    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    let mut doc_node = Node::new(doc_id.clone());
    doc_node.labels = vec!["Document".into()];
    doc_node.props.insert("name".into(), json!(doc.title));
    doc_node.props.insert("source".into(), json!(doc.source));
    doc_node.props.insert("format".into(), json!(doc.format));
    doc_node
        .props
        .insert("ingested_at".into(), json!(chrono::Utc::now().to_rfc3339()));
    doc_node.props.insert("chunks".into(), json!(chunks.len()));
    nodes.push(doc_node);

    let mut prev_chunk: Option<String> = None;
    for (i, text) in chunks.iter().enumerate() {
        let chunk_id = format!("chunk:{doc_slug}:{i}");
        let mut node = Node::new(chunk_id.clone());
        node.labels = vec!["Chunk".into()];
        node.props
            .insert("name".into(), json!(format!("{} #{i}", doc.title)));
        node.props.insert("source".into(), json!(doc.source));
        node.props.insert("position".into(), json!(i));
        node.props.insert("text".into(), json!(text));
        nodes.push(node);

        edges.push(Edge::new(doc_id.clone(), "CONTAINS", chunk_id.clone()));
        if let Some(prev) = prev_chunk.take() {
            edges.push(Edge::new(prev, "NEXT", chunk_id.clone()));
        }
        prev_chunk = Some(chunk_id);
    }

    let count = chunks.len();
    Ingested {
        nodes,
        edges,
        chunks: count,
    }
}

fn slug(source: &str) -> String {
    source
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Collapse runs of blank lines/whitespace while preserving paragraph breaks.
fn normalize_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// Minimal HTML-to-text: drop tags and decode a few common entities.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_respect_target_and_paragraphs() {
        let text = "para one.\n\npara two is here.\n\npara three.";
        let chunks = chunk(text, 400);
        assert_eq!(chunks.len(), 1); // all fit in one chunk
        assert!(chunks[0].contains("para one"));
        assert!(chunks[0].contains("para three"));

        let big = "a ".repeat(500);
        let chunks = chunk(&big, 400);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= 420));
    }

    #[test]
    fn to_graph_builds_document_and_chunk_structure() {
        // Two paragraphs that together exceed the chunk target so it splits.
        let para = "word ".repeat(120); // ~600 chars each
        let doc = Document {
            title: "Notes".into(),
            source: "/tmp/notes.md".into(),
            format: "md".into(),
            text: format!("{para}\n\n{para}"),
        };
        let ing = to_graph(&doc, 500); // target 500 -> at least 2 chunks
        assert!(ing.chunks >= 2);
        assert!(ing
            .nodes
            .iter()
            .any(|n| n.labels.contains(&"Document".to_string())));
        let chunk_count = ing
            .nodes
            .iter()
            .filter(|n| n.labels.contains(&"Chunk".to_string()))
            .count();
        assert_eq!(chunk_count, ing.chunks);
        assert!(ing.edges.iter().any(|e| e.etype == "CONTAINS"));
        assert!(ing.edges.iter().any(|e| e.etype == "NEXT"));
        // Chunk text is stored as a searchable property.
        assert!(ing
            .nodes
            .iter()
            .filter(|n| n.labels.contains(&"Chunk".to_string()))
            .all(|n| n.props.contains_key("text")));
    }

    #[test]
    fn html_is_stripped() {
        let text = strip_html("<p>Hello <b>world</b></p>&amp; more");
        assert!(text.contains("Hello world"));
        assert!(text.contains("& more"));
        assert!(!text.contains("<b>"));
    }
}
