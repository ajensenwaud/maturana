use crate::{pipelock::PipelockVault, secrets::resolve_secret_source_with_home, spec::AgentSpec};
use anyhow::Context;
use chrono::{DateTime, Utc};
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};
use serde::Serialize;
use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{Arc, Once, RwLock},
    thread,
    time::Duration,
};

/// Idle bound for an established tunnel. Streaming model responses can sit
/// silent for minutes while the model thinks, so this must be much larger than
/// the 15s request-parse timeout, while still reclaiming threads whose peer
/// vanished without closing the connection.
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub home_root: PathBuf,
    /// Durable allowlist from the spec (+ MCP egress hosts). Never revoked at
    /// runtime.
    pub allowlist: Vec<String>,
    /// When true, the proxy permits ANY host (egress governance off) — set by the
    /// spec's `network.egress_allow_all` or the `--allow-all` CLI flag. Traffic
    /// still flows through the proxy (header injection + audit keep working); it is
    /// just never denied, and every request audits with `grant_source=allow_all`.
    pub allow_all: bool,
    pub injections: Vec<HeaderInjection>,
    pub audit_path: PathBuf,
    /// Live, additive grants reloaded from `<home>/pipelock/runtime-allow.json`
    /// by a watcher thread — lets the cockpit approve a host without a restart.
    /// Shared (Arc) so every per-connection clone sees the same set.
    pub runtime_allow: Arc<RwLock<Vec<String>>>,
}

/// Where a request's host was permitted (drives the audit `grant_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowSource {
    Spec,
    Runtime,
    /// Egress governance is OFF for this agent (the `egress_allow_all` flag or a
    /// `*` wildcard grant): every host is permitted. Kept distinct from Spec/
    /// Runtime so each request is audited as `allow_all` and the open boundary is
    /// never mistaken for a scoped grant.
    AllowAll,
    Denied,
}

impl AllowSource {
    pub fn label(self) -> &'static str {
        match self {
            AllowSource::Spec => "spec",
            AllowSource::Runtime => "runtime",
            AllowSource::AllowAll => "allow_all",
            AllowSource::Denied => "denied",
        }
    }
}

impl ProxyConfig {
    pub fn from_spec(
        home_root: impl Into<PathBuf>,
        spec: &AgentSpec,
        audit_path: impl Into<PathBuf>,
    ) -> anyhow::Result<Self> {
        let proxy = spec
            .network
            .proxy
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("MATURANA spec does not declare network.proxy"))?;
        if !proxy.enabled {
            anyhow::bail!("network.proxy is disabled");
        }
        // MCP servers run in-guest and make their own outbound calls; permit
        // their declared hosts without a separate allowlist edit.
        let mut allowlist = spec.network.egress_allowlist.clone();
        for server in &spec.mcp_servers {
            for host in &server.egress_hosts {
                if !allowlist.iter().any(|h| h.eq_ignore_ascii_case(host)) {
                    allowlist.push(host.clone());
                }
            }
        }
        Ok(Self {
            home_root: home_root.into(),
            allowlist,
            allow_all: spec.network.egress_allow_all,
            injections: proxy
                .inject_headers
                .iter()
                .map(|injection| HeaderInjection {
                    host: injection.host.trim().to_ascii_lowercase(),
                    header: injection.header.trim().to_string(),
                    source: injection.source.trim().to_string(),
                    prefix: injection.prefix.clone(),
                })
                .collect(),
            audit_path: audit_path.into(),
            runtime_allow: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// File the watcher reloads live grants from.
    pub fn runtime_allow_path(&self) -> PathBuf {
        self.home_root.join("pipelock").join("runtime-allow.json")
    }

    /// Classify a (already normalized) host: allow-all first (the flag or a `*`
    /// wildcard in either layer), then the durable allowlist, then the live
    /// runtime grants. Allow-all stays a distinct source so the open boundary is
    /// always visible in the audit, never masked as a scoped spec/runtime grant.
    fn classify_host(&self, host: &str) -> AllowSource {
        if self.allow_all || self.allowlist.iter().any(|h| h.trim() == "*") {
            return AllowSource::AllowAll;
        }
        if host_allowed(host, &self.allowlist) {
            return AllowSource::Spec;
        }
        let runtime = self.runtime_allow.read().expect("runtime_allow poisoned");
        if runtime.iter().any(|h| h.trim() == "*") {
            return AllowSource::AllowAll;
        }
        if host_allowed(host, &runtime) {
            return AllowSource::Runtime;
        }
        AllowSource::Denied
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderInjection {
    pub host: String,
    pub header: String,
    pub source: String,
    /// Literal prepended to the resolved secret (e.g. `"Bearer "` so one
    /// stored API key serves both direct calls and proxy injection).
    pub prefix: Option<String>,
}

impl HeaderInjection {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let (host, rest) = raw
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("header injection must be host:Header=pipelock:name"))?;
        let (header, source) = rest
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("header injection must be host:Header=pipelock:name"))?;
        if host.trim().is_empty() || header.trim().is_empty() || source.trim().is_empty() {
            anyhow::bail!("header injection must be host:Header=pipelock:name");
        }
        if !source.starts_with("pipelock:") {
            anyhow::bail!("header injection source must use pipelock:");
        }
        Ok(Self {
            host: host.trim().to_ascii_lowercase(),
            header: header.trim().to_string(),
            source: source.trim().to_string(),
            prefix: None,
        })
    }
}

pub fn run_proxy(bind: &str, config: ProxyConfig) -> anyhow::Result<()> {
    // Seed live grants from disk and keep them current via a watcher, so the
    // cockpit can approve a host into the running proxy without a restart.
    load_runtime_allow(&config.runtime_allow_path(), &config.runtime_allow);
    spawn_runtime_allow_watcher(config.runtime_allow_path(), config.runtime_allow.clone());
    let listener = TcpListener::bind(bind).with_context(|| format!("failed to bind {bind}"))?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Handle each connection on its own thread: a CONNECT tunnel
                // stays open for the whole upstream exchange (minutes for a
                // streaming model response), and the harnesses open several
                // connections at once. Serving inline on the accept thread
                // serializes every guest connection behind the current tunnel.
                let config = config.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_proxy_stream(stream, &config) {
                        let _ = append_audit(
                            &config.audit_path,
                            &ProxyAuditEvent::denied_raw(error.to_string()),
                        );
                    }
                });
            }
            Err(error) => {
                append_audit(
                    &config.audit_path,
                    &ProxyAuditEvent::denied_raw(error.to_string()),
                )?;
            }
        }
    }
    Ok(())
}

pub fn handle_proxy_stream(mut client: TcpStream, config: &ProxyConfig) -> anyhow::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(15)))?;
    client.set_write_timeout(Some(Duration::from_secs(15)))?;

    let request = read_http_request(&mut client)?;
    let parsed = parse_proxy_request(&request)?;
    if config.classify_host(&parsed.host) == AllowSource::Denied {
        write_proxy_error(&mut client, 403, "forbidden")?;
        append_audit(
            &config.audit_path,
            &ProxyAuditEvent::denied(&parsed, "host not allowed"),
        )?;
        return Ok(());
    }

    if parsed.method.eq_ignore_ascii_case("CONNECT") {
        if has_injections(&parsed.host, config) {
            return handle_mitm_connect(client, &parsed, config);
        }
        return handle_connect(client, &parsed, config);
    }

    let injected = injected_headers(&parsed.host, config)?;
    let upstream_request = build_upstream_request(&parsed, &request, &injected)?;
    let mut upstream = connect_upstream(&parsed.host, parsed.port)?;
    upstream.write_all(&upstream_request)?;

    let mut total_bytes = 0u64;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = upstream.read(&mut buf)?;
        if read == 0 {
            break;
        }
        total_bytes += read as u64;
        client.write_all(&buf[..read])?;
    }
    append_audit(
        &config.audit_path,
        &ProxyAuditEvent::allowed(
            &parsed,
            injected.len(),
            total_bytes,
            false,
            config.classify_host(&parsed.host).label(),
        ),
    )?;
    Ok(())
}

#[derive(Debug)]
struct ParsedRequest {
    method: String,
    target: String,
    host: String,
    port: u16,
    path: String,
    version: String,
}

fn read_http_request(stream: &mut impl Read) -> anyhow::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let read = stream.read(&mut buf)?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buf[..read]);
        if data.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if data.len() > 64 * 1024 {
            anyhow::bail!("proxy request headers are too large");
        }
    }
    if data.is_empty() {
        anyhow::bail!("empty proxy request");
    }
    Ok(data)
}

fn parse_proxy_request(request: &[u8]) -> anyhow::Result<ParsedRequest> {
    let text = std::str::from_utf8(request).context("proxy request is not UTF-8")?;
    let first_line = text
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("proxy request missing request line"))?;
    let mut parts = first_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("proxy request missing method"))?;
    let target = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("proxy request missing target"))?;
    let version = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("proxy request missing version"))?;

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = match target.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|ch| ch.is_ascii_digit()) => {
                (normalize_host(host)?, port.parse()?)
            }
            _ => (normalize_host(target)?, 443),
        };
        return Ok(ParsedRequest {
            method: method.to_string(),
            target: target.to_string(),
            host,
            port,
            path: target.to_string(),
            version: version.to_string(),
        });
    }
    let rest = target
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("proxy target must be an http:// absolute URL"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|ch| ch.is_ascii_digit()) => {
            (normalize_host(host)?, port.parse()?)
        }
        _ => (normalize_host(authority)?, 80),
    };

    Ok(ParsedRequest {
        method: method.to_string(),
        target: target.to_string(),
        host,
        port,
        path,
        version: version.to_string(),
    })
}

fn handle_connect(
    mut client: TcpStream,
    parsed: &ParsedRequest,
    config: &ProxyConfig,
) -> anyhow::Result<()> {
    let mut upstream = connect_upstream(&parsed.host, parsed.port)?;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;

    // The 15s socket timeouts exist to bound the initial request parse and the
    // upstream connect. An established tunnel must tolerate long idle gaps
    // (streaming model responses sit silent while the model thinks), so relax
    // the timeouts to a generous idle bound before the blocking copy loops;
    // otherwise the tunnel is torn down mid-response and the harness retries
    // the whole request.
    client.set_read_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    client.set_write_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    upstream.set_read_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    upstream.set_write_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;

    let mut client_reader = client.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let client_to_upstream =
        thread::spawn(move || std::io::copy(&mut client_reader, &mut upstream_writer).unwrap_or(0));

    let upstream_to_client = std::io::copy(&mut upstream, &mut client).unwrap_or(0);
    let _ = client.shutdown(Shutdown::Both);
    let client_to_upstream = client_to_upstream.join().unwrap_or(0);
    let total_bytes = upstream_to_client + client_to_upstream;
    append_audit(
        &config.audit_path,
        &ProxyAuditEvent::allowed(
            parsed,
            0,
            total_bytes,
            false,
            config.classify_host(&parsed.host).label(),
        ),
    )?;
    Ok(())
}

fn handle_mitm_connect(
    mut client: TcpStream,
    parsed: &ParsedRequest,
    config: &ProxyConfig,
) -> anyhow::Result<()> {
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;

    let server_config = mitm_server_config(&parsed.host, config)?;
    let server_conn = ServerConnection::new(Arc::new(server_config))?;
    let mut client_tls = StreamOwned::new(server_conn, client);

    let upstream = connect_upstream(&parsed.host, parsed.port)?;
    let client_config = upstream_client_config()?;
    let server_name = ServerName::try_from(parsed.host.clone())
        .map_err(|_| anyhow::anyhow!("invalid TLS server name: {}", parsed.host))?;
    let upstream_conn = ClientConnection::new(Arc::new(client_config), server_name)?;
    let mut upstream_tls = StreamOwned::new(upstream_conn, upstream);

    let request = read_http_request(&mut client_tls)?;
    let tls_request = parse_mitm_http_request(parsed, &request)?;
    let injected = injected_headers(&tls_request.host, config)?;
    let upstream_request = build_upstream_request(&tls_request, &request, &injected)?;
    upstream_tls.write_all(&upstream_request)?;
    upstream_tls.flush()?;

    // As in handle_connect: the request is parsed and sent under the 15s
    // timeouts, but the response may stream with long idle gaps, so relax the
    // socket timeouts before relaying it.
    client_tls
        .sock
        .set_read_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    client_tls
        .sock
        .set_write_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    upstream_tls
        .sock
        .set_read_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    upstream_tls
        .sock
        .set_write_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;

    let mut total_bytes = 0u64;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = match upstream_tls.read(&mut buf) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        };
        if read == 0 {
            break;
        }
        total_bytes += read as u64;
        client_tls.write_all(&buf[..read])?;
    }
    let _ = client_tls.flush();
    append_audit(
        &config.audit_path,
        &ProxyAuditEvent::allowed(
            &tls_request,
            injected.len(),
            total_bytes,
            true,
            config.classify_host(&tls_request.host).label(),
        ),
    )?;
    Ok(())
}

fn host_allowed(host: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|allowed| {
        let allowed = normalize_host_str(allowed);
        host == allowed || host.ends_with(&format!(".{allowed}"))
    })
}

/// Canonicalize a host before any allowlist or injection comparison so the two
/// can never disagree and so common evasions are rejected: lowercase, strip a
/// single trailing FQDN dot (`api.host.com.` == `api.host.com`), and reject
/// any host carrying embedded credentials, ports-in-host, fragments, or control
/// characters/whitespace that could split the comparison from what is dialed.
fn normalize_host(raw: &str) -> anyhow::Result<String> {
    let host = normalize_host_str(raw);
    if host.is_empty() {
        anyhow::bail!("proxy target host is empty");
    }
    if host
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '#' | '/' | '\\'))
    {
        anyhow::bail!("proxy target host contains an illegal character");
    }
    Ok(host)
}

fn normalize_host_str(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn has_injections(host: &str, config: &ProxyConfig) -> bool {
    config
        .injections
        .iter()
        .any(|injection| injection.host == host)
}

fn injected_headers(host: &str, config: &ProxyConfig) -> anyhow::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for injection in &config.injections {
        if injection.host == host {
            let secret = resolve_secret_source_with_home(&injection.source, &config.home_root)?;
            let value = match injection.prefix.as_deref() {
                Some(prefix) => format!("{prefix}{}", secret.expose_for_runtime()),
                None => secret.expose_for_runtime().to_string(),
            };
            // A secret (or header name/prefix) containing CR/LF would let the
            // value smuggle additional upstream headers (header splitting).
            // Refuse it rather than emit a malformed/forgeable request.
            if injection.header.bytes().any(is_header_control)
                || value.bytes().any(is_header_control)
            {
                anyhow::bail!("injected header for {host} contains illegal control characters");
            }
            headers.push((injection.header.clone(), value));
        }
    }
    Ok(headers)
}

fn is_header_control(byte: u8) -> bool {
    byte == b'\r' || byte == b'\n' || byte == 0
}

fn parse_mitm_http_request(
    connect: &ParsedRequest,
    request: &[u8],
) -> anyhow::Result<ParsedRequest> {
    let text = std::str::from_utf8(request).context("TLS HTTP request is not UTF-8")?;
    let first_line = text
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("TLS HTTP request missing request line"))?;
    let mut parts = first_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("TLS HTTP request missing method"))?;
    let target = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("TLS HTTP request missing target"))?;
    let version = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("TLS HTTP request missing version"))?;
    let path = if target.starts_with("https://") {
        let rest = target
            .strip_prefix("https://")
            .ok_or_else(|| anyhow::anyhow!("TLS HTTP target is malformed"))?;
        match rest.split_once('/') {
            Some((_, path)) => format!("/{path}"),
            None => "/".to_string(),
        }
    } else {
        target.to_string()
    };
    Ok(ParsedRequest {
        method: method.to_string(),
        target: format!("https://{}{}", connect.host, path),
        host: connect.host.clone(),
        port: connect.port,
        path,
        version: version.to_string(),
    })
}

fn build_upstream_request(
    parsed: &ParsedRequest,
    raw_request: &[u8],
    injected: &[(String, String)],
) -> anyhow::Result<Vec<u8>> {
    let text = std::str::from_utf8(raw_request)?;
    let (headers, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("proxy request missing header terminator"))?;
    let mut output = Vec::new();
    writeln!(
        output,
        "{} {} {}\r",
        parsed.method, parsed.path, parsed.version
    )?;
    for line in headers.lines().skip(1) {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        // Drop hop-by-hop headers, the client-supplied Host (we pin it to the
        // CONNECT target below so the injected secret can't be redirected to an
        // arbitrary virtual host), and Transfer-Encoding (prevents TE/CL
        // request smuggling since we forward with an explicit Content-Length).
        if lower.starts_with("proxy-connection:")
            || lower.starts_with("connection:")
            || lower.starts_with("host:")
            || lower.starts_with("transfer-encoding:")
        {
            continue;
        }
        if injected
            .iter()
            .any(|(header, _)| lower.starts_with(&format!("{}:", header.to_ascii_lowercase())))
        {
            continue;
        }
        writeln!(output, "{line}\r")?;
    }
    writeln!(output, "Host: {}\r", parsed.host)?;
    output.extend_from_slice(b"Connection: close\r\n");
    for (header, value) in injected {
        writeln!(output, "{header}: {value}\r")?;
    }
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(body.as_bytes());
    Ok(output)
}

fn connect_upstream(host: &str, port: u16) -> anyhow::Result<TcpStream> {
    let addrs = (host, port).to_socket_addrs()?.collect::<Vec<_>>();
    if addrs.is_empty() {
        anyhow::bail!("could not resolve upstream {host}:{port}");
    }
    let mut last_error = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(15)) {
            Ok(stream) => {
                stream.set_read_timeout(Some(Duration::from_secs(15)))?;
                stream.set_write_timeout(Some(Duration::from_secs(15)))?;
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("could not connect upstream {host}:{port}")))
}

fn mitm_server_config(host: &str, config: &ProxyConfig) -> anyhow::Result<ServerConfig> {
    ensure_rustls_crypto_provider();
    let ca = ensure_mitm_ca(&config.home_root)?;
    let leaf_key = KeyPair::generate()?;
    let leaf_params = CertificateParams::new(vec![host.to_string()])?;
    let leaf_cert = leaf_params.signed_by(&leaf_key, &ca.cert, &ca.key)?;
    let cert_chain = vec![CertificateDer::from(leaf_cert.der().to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    Ok(ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?)
}

fn upstream_client_config() -> anyhow::Result<ClientConfig> {
    ensure_rustls_crypto_provider();
    let certs = rustls_native_certs::load_native_certs();
    if !certs.errors.is_empty() {
        anyhow::bail!(
            "failed to load native TLS roots: {:?}",
            certs
                .errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }
    let mut roots = RootCertStore::empty();
    for cert in certs.certs {
        roots.add(cert)?;
    }
    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn ensure_rustls_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

struct MitmCa {
    cert: rcgen::Certificate,
    key: KeyPair,
}

fn ensure_mitm_ca(home_root: &Path) -> anyhow::Result<MitmCa> {
    let cert_path = mitm_ca_cert_path(home_root);
    let key_path = mitm_ca_key_path(home_root);
    if cert_path.exists() && key_path.exists() {
        let cert_pem = fs::read_to_string(&cert_path)?;
        let key_pem = fs::read_to_string(&key_path)?;
        let cert_der = first_cert_der_from_pem(cert_pem.as_bytes())?;
        let ca_params = CertificateParams::from_ca_cert_der(&cert_der)?;
        let key = KeyPair::from_pem(&key_pem)?;
        let cert = ca_params.self_signed(&key)?;
        return Ok(MitmCa { cert, key });
    }

    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec!["Maturana Pipelock Local MITM CA".to_string()])?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "Maturana Pipelock Local MITM CA");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key)?;
    fs::write(&cert_path, cert.pem())?;
    fs::write(&key_path, key.serialize_pem())?;
    restrict_secret_file(&key_path);
    Ok(MitmCa { cert, key })
}

pub fn ensure_mitm_ca_cert(home_root: &Path) -> anyhow::Result<PathBuf> {
    ensure_mitm_ca(home_root)?;
    Ok(mitm_ca_cert_path(home_root))
}

pub fn mitm_ca_cert_path(home_root: &Path) -> PathBuf {
    home_root.join("pipelock").join("mitm-ca-cert.pem")
}

fn mitm_ca_key_path(home_root: &Path) -> PathBuf {
    home_root.join("pipelock").join("mitm-ca-key.pem")
}

fn first_cert_der_from_pem(input: &[u8]) -> anyhow::Result<CertificateDer<'static>> {
    let mut reader = std::io::BufReader::new(input);
    let cert = rustls_pemfile::certs(&mut reader)
        .next()
        .ok_or_else(|| anyhow::anyhow!("PEM file did not contain a certificate"))?
        .map_err(anyhow::Error::from)?;
    Ok(cert)
}

fn restrict_secret_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            let _ = fs::set_permissions(path, permissions);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("icacls.exe")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!(
                "{}:R",
                std::env::var("USERNAME").unwrap_or_else(|_| "Users".to_string())
            ))
            .status();
    }
}

fn write_proxy_error(client: &mut TcpStream, code: u16, message: &str) -> anyhow::Result<()> {
    write!(
        client,
        "HTTP/1.1 {code} {message}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{message}",
        message.len()
    )?;
    Ok(())
}

fn append_audit(path: &Path, event: &ProxyAuditEvent) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{}", serde_json::to_string(event)?)?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct ProxyAuditEvent {
    at: DateTime<Utc>,
    action: &'static str,
    method: String,
    host: String,
    port: u16,
    target: String,
    injected_headers: usize,
    tls_intercepted: bool,
    bytes: u64,
    reason: Option<String>,
    /// "spec" (durable allowlist) or "runtime" (live cockpit grant) for allowed
    /// events; "denied" otherwise. Lets the cockpit badge each row.
    grant_source: &'static str,
}

impl ProxyAuditEvent {
    fn allowed(
        parsed: &ParsedRequest,
        injected_headers: usize,
        bytes: u64,
        tls_intercepted: bool,
        grant_source: &'static str,
    ) -> Self {
        Self {
            at: Utc::now(),
            action: "pipelock.proxy.allowed",
            method: parsed.method.clone(),
            host: parsed.host.clone(),
            port: parsed.port,
            target: parsed.target.clone(),
            injected_headers,
            tls_intercepted,
            bytes,
            reason: None,
            grant_source,
        }
    }

    fn denied(parsed: &ParsedRequest, reason: impl Into<String>) -> Self {
        Self {
            at: Utc::now(),
            action: "pipelock.proxy.denied",
            method: parsed.method.clone(),
            host: parsed.host.clone(),
            port: parsed.port,
            target: parsed.target.clone(),
            injected_headers: 0,
            tls_intercepted: false,
            bytes: 0,
            reason: Some(reason.into()),
            grant_source: "denied",
        }
    }

    fn denied_raw(reason: impl Into<String>) -> Self {
        Self {
            at: Utc::now(),
            action: "pipelock.proxy.denied",
            method: String::new(),
            host: String::new(),
            port: 0,
            target: String::new(),
            injected_headers: 0,
            tls_intercepted: false,
            bytes: 0,
            reason: Some(reason.into()),
            grant_source: "denied",
        }
    }
}

/// Load `runtime-allow.json` (a JSON array of hosts) into the live grant set.
/// Missing/invalid file → empty grants (fail closed, never panic).
fn load_runtime_allow(path: &Path, grants: &Arc<RwLock<Vec<String>>>) {
    let hosts = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default();
    if let Ok(mut g) = grants.write() {
        *g = hosts;
    }
}

/// Poll the runtime-allow file's mtime once a second and reload on change, so a
/// cockpit approval reaches the running proxy within ~1s without a restart.
fn spawn_runtime_allow_watcher(path: PathBuf, grants: Arc<RwLock<Vec<String>>>) {
    thread::spawn(move || {
        let mut last: Option<std::time::SystemTime> = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());
        loop {
            thread::sleep(Duration::from_secs(1));
            let current = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            if current != last {
                last = current;
                load_runtime_allow(&path, &grants);
            }
        }
    });
}

#[allow(dead_code)]
fn _keep_vault_type_reachable(_: &PipelockVault) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::Mutex,
        thread,
    };
    use uuid::Uuid;

    #[test]
    fn host_normalization_and_allowlist_boundaries() {
        let allow = vec!["api.anthropic.com".to_string()];
        // Trailing-dot FQDN form must not evade the allowlist or injection match.
        assert_eq!(
            normalize_host("API.Anthropic.com.").unwrap(),
            "api.anthropic.com"
        );
        assert!(host_allowed(
            &normalize_host("api.anthropic.com.").unwrap(),
            &allow
        ));
        assert!(host_allowed(
            &normalize_host("v1.api.anthropic.com").unwrap(),
            &allow
        ));
        assert!(!host_allowed(
            &normalize_host("api.anthropic.com.evil.com").unwrap(),
            &allow
        ));
        assert!(!host_allowed(&normalize_host("evil.com").unwrap(), &allow));
        // Embedded credentials / fragments / whitespace are rejected outright.
        for bad in [
            "evil.com@api.anthropic.com",
            "api.anthropic.com#x",
            "api.anthropic.com/path",
            "bad host",
            "",
        ] {
            assert!(normalize_host(bad).is_err(), "should reject {bad:?}");
        }
    }

    static TLS_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parses_header_injection() {
        let injection =
            HeaderInjection::parse("api.example.test:Authorization=pipelock:api/token").unwrap();
        assert_eq!(injection.host, "api.example.test");
        assert_eq!(injection.header, "Authorization");
        assert_eq!(injection.source, "pipelock:api/token");
    }

    #[test]
    fn builds_config_from_spec_network_policy() {
        let raw = r#"
identity:
  id: demo
  name: Demo
  purpose: A demo agent with governed egress.
runtime:
  harness: codex
vm:
  provider: hyper-v
network:
  egress_allowlist:
    - api.example.test
  proxy:
    enabled: true
    bind: 0.0.0.0:47833
    inject_headers:
      - host: api.example.test
        header: Authorization
        source: pipelock:api/token
"#;
        let spec: AgentSpec = serde_yaml::from_str(raw).unwrap();
        let home = std::env::temp_dir().join(format!("maturana-proxy-{}", Uuid::new_v4()));
        let audit_path = home.join("audit.jsonl");
        let config = ProxyConfig::from_spec(&home, &spec, &audit_path).unwrap();
        assert_eq!(config.allowlist, vec!["api.example.test"]);
        assert_eq!(
            config.injections,
            vec![HeaderInjection {
                host: "api.example.test".to_string(),
                header: "Authorization".to_string(),
                source: "pipelock:api/token".to_string(),
                prefix: None,
            }]
        );
        assert_eq!(config.audit_path, audit_path);
    }

    #[test]
    fn classify_host_uses_spec_then_runtime_then_denied() {
        let config = ProxyConfig {
            home_root: std::env::temp_dir(),
            allowlist: vec!["api.example.test".to_string()],
            injections: vec![],
            audit_path: std::env::temp_dir().join("a.jsonl"),
            allow_all: false,
            runtime_allow: Default::default(),
        };
        assert_eq!(config.classify_host("api.example.test"), AllowSource::Spec);
        assert_eq!(config.classify_host("api.notion.com"), AllowSource::Denied);
        // A live grant flips it to Runtime without touching the spec allowlist.
        config
            .runtime_allow
            .write()
            .unwrap()
            .push("api.notion.com".to_string());
        assert_eq!(config.classify_host("api.notion.com"), AllowSource::Runtime);
        assert_eq!(config.classify_host("api.example.test"), AllowSource::Spec);
    }

    #[test]
    fn allow_all_flag_permits_any_host_as_allow_all_source() {
        let config = ProxyConfig {
            home_root: std::env::temp_dir(),
            allowlist: vec!["api.example.test".to_string()],
            injections: vec![],
            audit_path: std::env::temp_dir().join("a.jsonl"),
            allow_all: true,
            runtime_allow: Default::default(),
        };
        // Any host — on or off the allowlist — is permitted, and classified as
        // AllowAll (not Spec) so the audit records the open boundary.
        assert_eq!(
            config.classify_host("api.example.test"),
            AllowSource::AllowAll
        );
        assert_eq!(
            config.classify_host("totally.random.example"),
            AllowSource::AllowAll
        );
        assert_eq!(AllowSource::AllowAll.label(), "allow_all");
    }

    #[test]
    fn wildcard_grant_in_either_layer_means_allow_all() {
        // A literal "*" in the durable allowlist opens everything.
        let spec_star = ProxyConfig {
            home_root: std::env::temp_dir(),
            allowlist: vec!["*".to_string()],
            injections: vec![],
            audit_path: std::env::temp_dir().join("a.jsonl"),
            allow_all: false,
            runtime_allow: Default::default(),
        };
        assert_eq!(
            spec_star.classify_host("anything.example"),
            AllowSource::AllowAll
        );

        // A "*" added to the LIVE runtime grants opens everything without a restart.
        let runtime_star = ProxyConfig {
            home_root: std::env::temp_dir(),
            allowlist: vec!["api.example.test".to_string()],
            injections: vec![],
            audit_path: std::env::temp_dir().join("a.jsonl"),
            allow_all: false,
            runtime_allow: Default::default(),
        };
        assert_eq!(
            runtime_star.classify_host("api.notion.com"),
            AllowSource::Denied
        );
        runtime_star
            .runtime_allow
            .write()
            .unwrap()
            .push("*".to_string());
        assert_eq!(
            runtime_star.classify_host("api.notion.com"),
            AllowSource::AllowAll
        );
    }

    #[test]
    fn runtime_allow_file_loads_and_reloads() {
        let dir = std::env::temp_dir().join(format!("rt-allow-{}", Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("pipelock")).unwrap();
        let path = dir.join("pipelock").join("runtime-allow.json");
        let grants: Arc<RwLock<Vec<String>>> = Default::default();
        // Missing file → empty (fail closed).
        load_runtime_allow(&path, &grants);
        assert!(grants.read().unwrap().is_empty());
        std::fs::write(&path, r#"["api.notion.com","api.openai.com"]"#).unwrap();
        load_runtime_allow(&path, &grants);
        assert_eq!(grants.read().unwrap().len(), 2);
        // Garbage → empty, never panics.
        std::fs::write(&path, "not json").unwrap();
        load_runtime_allow(&path, &grants);
        assert!(grants.read().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_spec_folds_mcp_egress_hosts_into_allowlist() {
        let raw = r#"
identity: { id: demo, name: Demo, purpose: Demo agent with an MCP server. }
runtime: { harness: codex }
vm: { provider: firecracker, guest_os: linux }
network:
  egress_allowlist: [api.example.test]
  proxy: { enabled: true, bind: 0.0.0.0:47833 }
mcp_servers:
  - name: notion
    transport: stdio
    command: npx
    egress_hosts: [api.notion.com, api.example.test]
"#;
        let spec: AgentSpec = serde_yaml::from_str(raw).unwrap();
        let home = std::env::temp_dir().join(format!("maturana-proxy-{}", Uuid::new_v4()));
        let config = ProxyConfig::from_spec(&home, &spec, home.join("a.jsonl")).unwrap();
        // api.notion.com is added; api.example.test is not duplicated.
        assert_eq!(config.allowlist, vec!["api.example.test", "api.notion.com"]);
    }

    #[test]
    fn injection_prefix_flows_from_spec_and_prepends_to_secret() {
        // Spec → config: the prefix survives the mapping.
        let raw = r#"
identity:
  id: demo
  name: Demo
  purpose: prefix test
runtime:
  harness: codex
vm:
  provider: firecracker
  guest_os: linux
network:
  egress_allowlist:
    - api.tavily.com
  proxy:
    enabled: true
    bind: 0.0.0.0:47833
    inject_headers:
      - host: api.tavily.com
        header: Authorization
        source: pipelock:tavily/api-key
        prefix: "Bearer "
"#;
        let spec: AgentSpec = serde_yaml::from_str(raw).unwrap();
        let home = std::env::temp_dir().join(format!("maturana-proxy-{}", Uuid::new_v4()));
        let config = ProxyConfig::from_spec(&home, &spec, home.join("a.jsonl")).unwrap();
        assert_eq!(config.injections[0].prefix.as_deref(), Some("Bearer "));

        // Config → wire: the resolved secret gets the literal prefix.
        std::fs::create_dir_all(home.join("pipelock")).unwrap();
        let vault = PipelockVault::new(home.join("pipelock"));
        vault.init().unwrap();
        vault.set("tavily/api-key", "tvly-secret").unwrap();
        let headers = injected_headers("api.tavily.com", &config).unwrap();
        assert_eq!(
            headers,
            vec![(
                "Authorization".to_string(),
                "Bearer tvly-secret".to_string()
            )]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn injects_header_and_audits_allowed_request() {
        let home = std::env::temp_dir().join(format!("maturana-proxy-{}", Uuid::new_v4()));
        let vault = PipelockVault::new(home.join("pipelock"));
        vault.set("api/token", "Bearer secret-token").unwrap();
        let audit_path = home.join("audit.jsonl");

        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let upstream_thread = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let read = stream.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..read]);
                if raw.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(raw).unwrap();
            assert!(request.starts_with("GET /hello HTTP/1.1"));
            assert!(request.contains("Authorization: Bearer secret-token"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .unwrap();
        });

        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let config = ProxyConfig {
            home_root: home.clone(),
            allowlist: vec!["127.0.0.1".to_string()],
            injections: vec![HeaderInjection {
                host: "127.0.0.1".to_string(),
                header: "Authorization".to_string(),
                source: "pipelock:api/token".to_string(),
                prefix: None,
            }],
            audit_path: audit_path.clone(),
            allow_all: false,
            runtime_allow: Default::default(),
        };
        let proxy_thread = thread::spawn(move || {
            let (stream, _) = proxy.accept().unwrap();
            handle_proxy_stream(stream, &config).unwrap();
        });

        let mut client = TcpStream::connect(proxy_addr).unwrap();
        write!(
            client,
            "GET http://127.0.0.1:{}/hello HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            upstream_addr.port()
        )
        .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.contains("200 OK"));

        proxy_thread.join().unwrap();
        upstream_thread.join().unwrap();

        let audit = fs::read_to_string(audit_path).unwrap();
        assert!(audit.contains("pipelock.proxy.allowed"));
        assert!(audit.contains("\"injected_headers\":1"));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn denies_hosts_outside_allowlist() {
        let home = std::env::temp_dir().join(format!("maturana-proxy-{}", Uuid::new_v4()));
        let audit_path = home.join("audit.jsonl");
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let config = ProxyConfig {
            home_root: home.clone(),
            allowlist: vec!["allowed.example".to_string()],
            injections: vec![],
            audit_path: audit_path.clone(),
            allow_all: false,
            runtime_allow: Default::default(),
        };
        let proxy_thread = thread::spawn(move || {
            let (stream, _) = proxy.accept().unwrap();
            handle_proxy_stream(stream, &config).unwrap();
        });

        let mut client = TcpStream::connect(proxy_addr).unwrap();
        client
            .write_all(b"GET http://blocked.example/ HTTP/1.1\r\nHost: blocked.example\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.contains("403 forbidden"));

        proxy_thread.join().unwrap();
        let audit = fs::read_to_string(audit_path).unwrap();
        assert!(audit.contains("pipelock.proxy.denied"));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn connect_tunnels_allowed_hosts_and_audits() {
        let home = std::env::temp_dir().join(format!("maturana-proxy-{}", Uuid::new_v4()));
        let audit_path = home.join("audit.jsonl");

        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let upstream_thread = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"ping");
            stream.write_all(b"pong").unwrap();
        });

        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let config = ProxyConfig {
            home_root: home.clone(),
            allowlist: vec!["127.0.0.1".to_string()],
            injections: vec![],
            audit_path: audit_path.clone(),
            allow_all: false,
            runtime_allow: Default::default(),
        };
        let proxy_thread = thread::spawn(move || {
            let (stream, _) = proxy.accept().unwrap();
            handle_proxy_stream(stream, &config).unwrap();
        });

        let mut client = TcpStream::connect(proxy_addr).unwrap();
        write!(
            client,
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            upstream_addr.port(),
            upstream_addr.port()
        )
        .unwrap();
        let mut response = [0u8; 39];
        client.read_exact(&mut response).unwrap();
        assert!(String::from_utf8_lossy(&response).contains("200 Connection Established"));
        client.write_all(b"ping").unwrap();
        let mut pong = [0u8; 4];
        client.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"pong");
        let _ = client.shutdown(Shutdown::Both);

        proxy_thread.join().unwrap();
        upstream_thread.join().unwrap();

        let audit = fs::read_to_string(audit_path).unwrap();
        assert!(audit.contains("pipelock.proxy.allowed"));
        assert!(audit.contains("\"method\":\"CONNECT\""));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn connect_mitm_injects_headers_and_audits_tls_interception() {
        let _env_guard = TLS_ENV_LOCK.lock().unwrap();
        ensure_rustls_crypto_provider();

        let home = std::env::temp_dir().join(format!("maturana-proxy-{}", Uuid::new_v4()));
        let vault = PipelockVault::new(home.join("pipelock"));
        vault.set("api/token", "Bearer tls-secret").unwrap();
        let audit_path = home.join("audit.jsonl");

        let upstream_ca = test_ca("Maturana Test Upstream CA");
        let upstream_ca_path = home.join("upstream-ca.pem");
        fs::create_dir_all(&home).unwrap();
        fs::write(&upstream_ca_path, upstream_ca.cert.pem()).unwrap();
        let old_ssl_cert_file = std::env::var("SSL_CERT_FILE").ok();
        std::env::set_var("SSL_CERT_FILE", &upstream_ca_path);

        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let upstream_thread = thread::spawn(move || {
            let leaf_key = KeyPair::generate().unwrap();
            let leaf_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
            let leaf_cert = leaf_params
                .signed_by(&leaf_key, &upstream_ca.cert, &upstream_ca.key)
                .unwrap();
            let server_config = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(leaf_cert.der().to_vec())],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
                )
                .unwrap();

            let (stream, _) = upstream.accept().unwrap();
            let server_conn = ServerConnection::new(Arc::new(server_config)).unwrap();
            let mut tls = StreamOwned::new(server_conn, stream);
            let request = read_http_request(&mut tls).unwrap();
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /secure HTTP/1.1"));
            assert!(request.contains("Authorization: Bearer tls-secret"));
            tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .unwrap();
            tls.flush().unwrap();
        });

        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let config = ProxyConfig {
            home_root: home.clone(),
            allowlist: vec!["localhost".to_string()],
            injections: vec![HeaderInjection {
                host: "localhost".to_string(),
                header: "Authorization".to_string(),
                source: "pipelock:api/token".to_string(),
                prefix: None,
            }],
            audit_path: audit_path.clone(),
            allow_all: false,
            runtime_allow: Default::default(),
        };
        ensure_mitm_ca_cert(&home).unwrap();
        let proxy_ca = fs::read(mitm_ca_cert_path(&home)).unwrap();
        let proxy_thread = thread::spawn(move || {
            let (stream, _) = proxy.accept().unwrap();
            handle_proxy_stream(stream, &config).unwrap();
        });

        let mut client = TcpStream::connect(proxy_addr).unwrap();
        write!(
            client,
            "CONNECT localhost:{} HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
            upstream_addr.port(),
            upstream_addr.port()
        )
        .unwrap();
        let mut response = [0u8; 39];
        client.read_exact(&mut response).unwrap();
        assert!(String::from_utf8_lossy(&response).contains("200 Connection Established"));

        let mut roots = RootCertStore::empty();
        roots
            .add(first_cert_der_from_pem(&proxy_ca).unwrap())
            .unwrap();
        let client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name = ServerName::try_from("localhost".to_string()).unwrap();
        let client_conn = ClientConnection::new(Arc::new(client_config), server_name).unwrap();
        let mut tls = StreamOwned::new(client_conn, client);
        tls.write_all(b"GET /secure HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        tls.flush().unwrap();
        let mut body = String::new();
        read_to_string_tolerating_unexpected_eof(&mut tls, &mut body).unwrap();
        assert!(body.contains("200 OK"));

        proxy_thread.join().unwrap();
        upstream_thread.join().unwrap();

        let audit = fs::read_to_string(audit_path).unwrap();
        assert!(audit.contains("pipelock.proxy.allowed"));
        assert!(audit.contains("\"injected_headers\":1"));
        assert!(audit.contains("\"tls_intercepted\":true"));
        if let Some(value) = old_ssl_cert_file {
            std::env::set_var("SSL_CERT_FILE", value);
        } else {
            std::env::remove_var("SSL_CERT_FILE");
        }
        let _ = fs::remove_dir_all(home);
    }

    fn test_ca(common_name: &str) -> MitmCa {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![common_name.to_string()]).unwrap();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, common_name);
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).unwrap();
        MitmCa { cert, key }
    }

    fn read_to_string_tolerating_unexpected_eof(
        reader: &mut impl Read,
        output: &mut String,
    ) -> std::io::Result<usize> {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&buf[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            }
        }
        let text = String::from_utf8_lossy(&bytes);
        output.push_str(&text);
        Ok(bytes.len())
    }
}
