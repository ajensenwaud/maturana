//! MaturanaGraph host service: `maturana graph serve`.
//!
//! A small HTTP API in front of the from-scratch `maturana-graph` engine. Graphs
//! are addressed **by name** and opened on demand under `.maturana/graphs/<name>/`,
//! so multiple agents can share one named graph (`personal`, `team`, …) or keep
//! a private one (named after the agent). Access is open to any caller holding
//! the host graph token. It follows the sessiond shape exactly (sync
//! TcpListener, hand-rolled HTTP, `/health` exempt, constant-time bearer token,
//! JSON in/out) and does pure storage + graph/vector math — never a model call.
//! The agent (in its VM) does extraction and embedding and talks to this service
//! via GraphRAG.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Subcommand};
use maturana_core::state::MaturanaHome;
use maturana_graph::{local_query, Edge, LocalQuery, Node, Store};
use serde::Deserialize;

#[derive(Debug, Args)]
pub struct GraphCommand {
    #[command(subcommand)]
    pub command: GraphSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum GraphSubcommand {
    /// Serve the MaturanaGraph API for agents to read/write a knowledge graph.
    Serve {
        #[arg(long, default_value = "0.0.0.0:47835")]
        bind: String,
        #[arg(long, env = "MATURANA_GRAPH_TOKEN")]
        token: Option<String>,
    },
    /// Ingest a document (PDF/PPTX/DOCX/MD/TXT/HTML) or a directory of them into
    /// a named graph via the running service.
    Ingest {
        /// File or directory to ingest.
        path: PathBuf,
        #[arg(long, default_value = "personal")]
        graph: String,
        #[arg(long, default_value = "http://127.0.0.1:47835")]
        url: String,
        #[arg(long, default_value = ".maturana/graph/token")]
        token_path: PathBuf,
        #[arg(long, default_value_t = 1800)]
        chunk_chars: usize,
        /// Recurse into subdirectories when PATH is a directory.
        #[arg(long)]
        recursive: bool,
    },
    /// Run a GraphRAG query against a named graph via the running service and
    /// print the assembled context (host-side keyword query; no embedding).
    Query {
        /// Query terms.
        terms: Vec<String>,
        #[arg(long, default_value = "personal")]
        graph: String,
        #[arg(long, default_value = "http://127.0.0.1:47835")]
        url: String,
        #[arg(long, default_value = ".maturana/graph/token")]
        token_path: PathBuf,
        #[arg(long, default_value_t = 2)]
        depth: usize,
    },
}

pub fn handle_graph(command: GraphCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        GraphSubcommand::Serve { bind, token } => serve_graph(home, &bind, token.as_deref()),
        GraphSubcommand::Ingest {
            path,
            graph,
            url,
            token_path,
            chunk_chars,
            recursive,
        } => ingest_documents(&path, &graph, &url, &token_path, chunk_chars, recursive),
        GraphSubcommand::Query {
            terms,
            graph,
            url,
            token_path,
            depth,
        } => query_graph(&terms, &graph, &url, &token_path, depth),
    }
}

pub(crate) const SUPPORTED_EXTS: &[&str] = &[
    "pdf", "pptx", "docx", "md", "markdown", "txt", "text", "html", "htm", "json",
];

/// Where co-located host processes (the Telegram bridge) reach the service.
pub(crate) const DEFAULT_LOCAL_URL: &str = "http://127.0.0.1:47835";

/// Parse + chunk one document and upsert it into a named graph via the running
/// service (single-writer: all mutations go through the service, never a second
/// `Store` on the same directory). Returns the chunk count.
pub(crate) fn ingest_file_into_service(
    url: &str,
    token: &str,
    graph: &str,
    file: &std::path::Path,
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

/// Run a keyword GraphRAG query via the running service and return the rendered
/// context (host-side: no embedding, pure text seed + graph expansion).
pub(crate) fn query_rendered_context(
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

fn ingest_documents(
    path: &std::path::Path,
    graph: &str,
    url: &str,
    token_path: &std::path::Path,
    chunk_chars: usize,
    recursive: bool,
) -> anyhow::Result<()> {
    let token = read_token(token_path)?;
    let files = collect_files(path, recursive)?;
    if files.is_empty() {
        anyhow::bail!("no ingestible documents found at {}", path.display());
    }
    let mut total_chunks = 0usize;
    let mut ok = 0usize;
    for file in &files {
        match ingest_file_into_service(url, &token, graph, file, chunk_chars) {
            Ok(chunks) => {
                total_chunks += chunks;
                ok += 1;
                println!("ingested {} ({} chunks)", file.display(), chunks);
            }
            Err(error) => eprintln!("skipped {}: {error}", file.display()),
        }
    }
    println!(
        "ingested {ok}/{} document(s), {total_chunks} chunks, into graph '{graph}'",
        files.len()
    );
    Ok(())
}

fn query_graph(
    terms: &[String],
    graph: &str,
    url: &str,
    token_path: &std::path::Path,
    depth: usize,
) -> anyhow::Result<()> {
    let token = read_token(token_path)?;
    let rendered = query_rendered_context(url, &token, graph, terms, depth)?;
    println!("{rendered}");
    Ok(())
}

fn collect_files(path: &std::path::Path, recursive: bool) -> anyhow::Result<Vec<PathBuf>> {
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
                files.extend(collect_files(&p, true)?);
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

fn read_token(token_path: &std::path::Path) -> anyhow::Result<String> {
    std::fs::read_to_string(token_path)
        .map(|s| s.trim().to_string())
        .with_context(|| {
            format!(
                "failed to read graph token {} (is the graph service set up?)",
                token_path.display()
            )
        })
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

const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

fn serve_graph(home: &MaturanaHome, bind: &str, token: Option<&str>) -> anyhow::Result<()> {
    // Binds a public interface (guests reach it), so the token is the only
    // guard — refuse to start without one, mirroring sessiond.
    let token = match token {
        Some(token) if !token.is_empty() => token,
        _ => anyhow::bail!(
            "graph serve requires a token; pass --token or set MATURANA_GRAPH_TOKEN (it binds a public interface)"
        ),
    };
    let listener = TcpListener::bind(bind).with_context(|| format!("failed to bind {bind}"))?;
    println!("maturana graph serving on {bind}");
    // Single-threaded sequential loop (like sessiond), so per-agent stores can
    // be held open across requests without locking.
    let mut stores: HashMap<String, Store> = HashMap::new();
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_request(home, token, &mut stores, &mut stream) {
                    let _ = write_json_response(
                        &mut stream,
                        500,
                        &serde_json::json!({ "ok": false, "error": error.to_string() }),
                    );
                }
            }
            Err(error) => eprintln!("graph accept error: {error}"),
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct UpsertRequest {
    graph: String,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    edges: Vec<Edge>,
}

#[derive(Debug, Deserialize)]
struct QueryRequest {
    graph: String,
    #[serde(flatten)]
    query: LocalQuery,
}

#[derive(Debug, Deserialize)]
struct DeleteRequest {
    graph: String,
    #[serde(default)]
    node_ids: Vec<String>,
    #[serde(default)]
    edge_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GraphRequest {
    graph: String,
}

fn handle_request(
    home: &MaturanaHome,
    token: &str,
    stores: &mut HashMap<String, Store>,
    stream: &mut TcpStream,
) -> anyhow::Result<()> {
    let request = read_http_request(stream)?;
    if request.path != "/health" {
        let actual = request
            .headers
            .get("x-maturana-graph-token")
            .map(String::as_str)
            .unwrap_or("");
        if !constant_time_eq(actual.as_bytes(), token.as_bytes()) {
            return write_json_response(
                stream,
                401,
                &serde_json::json!({ "ok": false, "error": "unauthorized" }),
            );
        }
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => write_json_response(stream, 200, &serde_json::json!({ "ok": true })),
        ("POST", "/graph/upsert") => {
            let body: UpsertRequest = serde_json::from_slice(&request.body)?;
            let store = match resolve_store(home, stores, &body.graph) {
                Ok(store) => store,
                Err(error) => return write_json_response(stream, 400, &error),
            };
            for node in body.nodes {
                store.upsert_node(node)?;
            }
            for edge in body.edges {
                store.upsert_edge(edge)?;
            }
            write_json_response(
                stream,
                200,
                &serde_json::json!({ "ok": true, "stats": store.stats() }),
            )
        }
        ("POST", "/graph/query") => {
            let body: QueryRequest = serde_json::from_slice(&request.body)?;
            let store = match resolve_store(home, stores, &body.graph) {
                Ok(store) => store,
                Err(error) => return write_json_response(stream, 400, &error),
            };
            let result = local_query(store, &body.query);
            write_json_response(stream, 200, &serde_json::json!({ "ok": true, "result": result }))
        }
        ("POST", "/graph/delete") => {
            let body: DeleteRequest = serde_json::from_slice(&request.body)?;
            let store = match resolve_store(home, stores, &body.graph) {
                Ok(store) => store,
                Err(error) => return write_json_response(stream, 400, &error),
            };
            for id in &body.node_ids {
                store.delete_node(id)?;
            }
            for id in &body.edge_ids {
                store.delete_edge(id)?;
            }
            write_json_response(
                stream,
                200,
                &serde_json::json!({ "ok": true, "stats": store.stats() }),
            )
        }
        ("POST", "/graph/stats") => {
            let body: GraphRequest = serde_json::from_slice(&request.body)?;
            let store = match resolve_store(home, stores, &body.graph) {
                Ok(store) => store,
                Err(error) => return write_json_response(stream, 400, &error),
            };
            write_json_response(
                stream,
                200,
                &serde_json::json!({ "ok": true, "stats": store.stats() }),
            )
        }
        _ => write_json_response(
            stream,
            404,
            &serde_json::json!({ "ok": false, "error": "not found" }),
        ),
    }
}

/// Open (cached) the named graph store, validating `graph` first so it can't
/// traverse out of the graphs directory. Named graphs live under
/// `<home>/graphs/<name>/` and are shareable across agents.
fn resolve_store<'a>(
    home: &MaturanaHome,
    stores: &'a mut HashMap<String, Store>,
    graph: &str,
) -> Result<&'a mut Store, serde_json::Value> {
    if !valid_graph_name(graph) {
        return Err(serde_json::json!({ "ok": false, "error": "invalid graph name" }));
    }
    if !stores.contains_key(graph) {
        let dir = home.root().join("graphs").join(graph);
        let store = Store::open(dir)
            .map_err(|e| serde_json::json!({ "ok": false, "error": e.to_string() }))?;
        stores.insert(graph.to_string(), store);
    }
    Ok(stores.get_mut(graph).expect("just inserted"))
}

fn valid_graph_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---- tiny HTTP server (mirrors session.rs) ----

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> anyhow::Result<HttpRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            anyhow::bail!("connection closed while reading request");
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(index) = find_header_end(&data) {
            header_end = index;
            break;
        }
        if data.len() > 1024 * 1024 {
            anyhow::bail!("request headers too large");
        }
    }

    let headers_raw = String::from_utf8_lossy(&data[..header_end]);
    let mut lines = headers_raw.split("\r\n");
    let request_line = lines.next().context("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        anyhow::bail!("request body too large");
    }
    let body_start = header_end + 4;
    while data.len() < body_start + content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
    }
    let body = data
        .get(body_start..body_start + content_length)
        .unwrap_or_default()
        .to_vec();
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let body = serde_json::to_vec(value)?;
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_name_validation_blocks_traversal() {
        assert!(valid_graph_name("personal"));
        assert!(valid_graph_name("team-kb"));
        assert!(valid_graph_name("agent.codex-firecracker"));
        for bad in ["", "..", "../x", "a/b", "a\\b", "/abs"] {
            assert!(!valid_graph_name(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"tok", b"tok"));
        assert!(!constant_time_eq(b"tok", b"toK"));
        assert!(!constant_time_eq(b"tok", b"to"));
    }
}
