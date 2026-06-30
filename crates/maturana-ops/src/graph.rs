use anyhow::Context;
use std::path::{Path, PathBuf};

pub const SUPPORTED_EXTS: &[&str] = &[
    "pdf", "pptx", "docx", "md", "markdown", "txt", "text", "html", "htm", "json",
];

/// Where co-located host processes reach the graph service.
pub const DEFAULT_LOCAL_URL: &str = "http://127.0.0.1:47835";

pub fn ingest_file_into_service(
    url: &str,
    token: &str,
    graph: &str,
    file: &Path,
    chunk_chars: usize,
) -> anyhow::Result<usize> {
    let ingested = maturana_ingest::ingest(file, chunk_chars)?;
    let body = serde_json::json!({
        "graph": graph,
        "nodes": ingested.nodes,
        "edges": ingested.edges,
    });
    post_json(url, "/graph/upsert", token, &body)?;
    Ok(ingested.chunks)
}

pub fn agent_graph_name(agent_id: &str) -> String {
    let safe: String = agent_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("agent.{}", safe.trim_matches('-'))
}

pub fn query_blended_context(
    url: &str,
    token: &str,
    graphs: &[String],
    terms: &[String],
    depth: usize,
) -> String {
    let mut out = String::new();
    for graph in graphs {
        if let Ok(rendered) = query_rendered_context(url, token, graph, terms, depth) {
            let trimmed = rendered.trim();
            if !trimmed.is_empty() && trimmed != "(no result)" {
                out.push_str(&format!("[{graph}]\n{trimmed}\n\n"));
            }
        }
    }
    if out.trim().is_empty() {
        "(no graph results)".to_string()
    } else {
        out.trim_end().to_string()
    }
}

pub fn query_rendered_context(
    url: &str,
    token: &str,
    graph: &str,
    terms: &[String],
    depth: usize,
) -> anyhow::Result<String> {
    let body = serde_json::json!({ "graph": graph, "query_terms": terms, "depth": depth });
    let response = post_json(url, "/graph/query", token, &body)?;
    Ok(response
        .get("result")
        .and_then(|r| r.get("rendered_context"))
        .and_then(|c| c.as_str())
        .unwrap_or("(no result)")
        .to_string())
}

pub fn collect_ingestible_files(path: &Path, recursive: bool) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(files);
    }
    for entry in std::fs::read_dir(path)
        .with_context(|| format!("failed to read directory {}", path.display()))?
    {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            if recursive {
                files.extend(collect_ingestible_files(&p, true)?);
            }
        } else if p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| SUPPORTED_EXTS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false)
        {
            files.push(p);
        }
    }
    files.sort();
    Ok(files)
}

fn post_json(
    url: &str,
    path: &str,
    token: &str,
    body: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let response = ureq::post(&format!("{url}{path}"))
        .set("x-maturana-graph-token", token)
        .send_json(body)
        .with_context(|| format!("graph request to {path} failed"))?;
    Ok(response.into_json()?)
}
