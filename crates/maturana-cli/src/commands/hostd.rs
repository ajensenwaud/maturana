use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use anyhow::Context;
use chrono::Utc;
use clap::{Args, Subcommand};
use rand::{distributions::Alphanumeric, Rng};

#[derive(Debug, Args)]
pub(crate) struct HostdCommand {
    #[command(subcommand)]
    command: HostdSubcommand,
}

#[derive(Debug, Subcommand)]
enum HostdSubcommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Serve {
        #[arg(long, default_value = "http://127.0.0.1:47832/")]
        bind_prefix: String,
        #[arg(long, default_value = ".maturana/hostd/token")]
        token_path: PathBuf,
        #[arg(long, default_value = ".maturana/logs/hostd.log")]
        log_path: PathBuf,
    },
}

pub(crate) fn handle_hostd(command: HostdCommand) -> anyhow::Result<()> {
    match command.command {
        HostdSubcommand::Status { json } => {
            let status = maturana_ops::hostd::hostd_status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("hostd.url: {}", status.url);
                println!("hostd.reachable: {}", status.reachable);
                println!("hostd.token_present: {}", status.token_present);
                if let Some(error) = status.error {
                    println!("hostd.error: {error}");
                }
            }
        }
        HostdSubcommand::Serve {
            bind_prefix,
            token_path,
            log_path,
        } => {
            run_hostd_server(&bind_prefix, &token_path, &log_path)?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct HostdHttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug, serde::Deserialize)]
struct HostdLaunchBody {
    agent_id: Option<String>,
    harness: Option<String>,
    base_vhdx_path: Option<PathBuf>,
    switch_name: Option<String>,
    ssh_user: Option<String>,
    ssh_key_path: Option<PathBuf>,
    cloud_init_user_data_path: Option<PathBuf>,
    cloud_init_meta_data_path: Option<PathBuf>,
    disk_size_gb: Option<u32>,
    vcpu: Option<u8>,
    memory_mib: Option<u32>,
    provision_existing: Option<bool>,
    force: Option<bool>,
}

#[derive(Debug)]
struct HostdRouteResponse {
    status: u16,
    body: serde_json::Value,
}

fn run_hostd_server(bind_prefix: &str, token_path: &Path, log_path: &Path) -> anyhow::Result<()> {
    if !cfg!(windows) {
        anyhow::bail!("hostd serve is only supported on Windows hosts");
    }
    assert_windows_elevated()?;
    let token_path = absolute_or_cwd(token_path.to_path_buf())?;
    let log_path = absolute_or_cwd(log_path.to_path_buf())?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let token = ensure_hostd_server_token(&token_path)?;
    let bind = parse_hostd_bind_prefix(bind_prefix)?;
    hostd_log(
        &log_path,
        &format!("maturana rust hostd listening on {bind_prefix}"),
    )?;
    let listener = TcpListener::bind(bind).with_context(|| format!("failed to bind {bind}"))?;
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_hostd_stream(&mut stream, &token, &log_path) {
                    let _ = hostd_log(&log_path, &format!("request failed: {error:#}"));
                    let _ = write_hostd_json(
                        &mut stream,
                        500,
                        serde_json::json!({ "ok": false, "error": error.to_string() }),
                    );
                }
            }
            Err(error) => {
                hostd_log(&log_path, &format!("accept failed: {error}"))?;
            }
        }
    }
    Ok(())
}

fn assert_windows_elevated() -> anyhow::Result<()> {
    let script = "$identity=[Security.Principal.WindowsIdentity]::GetCurrent();$principal=[Security.Principal.WindowsPrincipal]::new($identity);if($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){exit 0}else{exit 1}";
    let status = ProcessCommand::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .status()
        .context("failed to check Windows elevation")?;
    if !status.success() {
        anyhow::bail!("Run maturana hostd serve from an elevated shell or scheduled task");
    }
    Ok(())
}

fn ensure_hostd_server_token(path: &Path) -> anyhow::Result<String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        let token: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();
        fs::write(path, &token)?;
    }
    let token = fs::read_to_string(path)?.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("hostd token file is empty: {}", path.display());
    }
    Ok(token)
}

fn handle_hostd_stream(stream: &mut TcpStream, token: &str, log_path: &Path) -> anyhow::Result<()> {
    let request = read_hostd_request(stream)?;
    if request.path != "/health" && !hostd_request_authorized(&request, token) {
        return write_hostd_json(
            stream,
            401,
            serde_json::json!({ "ok": false, "error": "unauthorized" }),
        );
    }
    let response = route_hostd_request(request, log_path)?;
    write_hostd_json(stream, response.status, response.body)
}

fn route_hostd_request(
    request: HostdHttpRequest,
    log_path: &Path,
) -> anyhow::Result<HostdRouteResponse> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => Ok(hostd_ok(serde_json::json!({}))),
        ("GET", "/vms") => hyperv_vms(),
        ("POST", "/agents/launch/ubuntu") => {
            let body: HostdLaunchBody = serde_json::from_slice(&request.body)
                .context("failed to parse launch request body")?;
            hyperv_launch_ubuntu(body, log_path)
        }
        ("POST", "/agents/stop") => {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .context("failed to parse stop request body")?;
            let agent_id = required_json_string(&body, "agent_id")?;
            hyperv_stop(&agent_id)
        }
        ("POST", "/agents/snapshot/take") => {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .context("failed to parse snapshot request body")?;
            let agent_id = required_json_string(&body, "agent_id")?;
            let name = required_json_string(&body, "name")?;
            hyperv_snapshot_take(&agent_id, &name)
        }
        ("POST", "/agents/snapshot/restore") => {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .context("failed to parse snapshot request body")?;
            let agent_id = required_json_string(&body, "agent_id")?;
            let name = required_json_string(&body, "name")?;
            hyperv_snapshot_restore(&agent_id, &name)
        }
        ("GET", "/agents/snapshot/list") => {
            let agent_id = request
                .query
                .get("agent_id")
                .ok_or_else(|| anyhow::anyhow!("agent_id is required"))?;
            hyperv_snapshot_list(agent_id)
        }
        _ => Ok(HostdRouteResponse {
            status: 404,
            body: serde_json::json!({ "ok": false, "error": "unknown endpoint" }),
        }),
    }
}

fn hostd_ok(mut extra: serde_json::Value) -> HostdRouteResponse {
    if let Some(object) = extra.as_object_mut() {
        object.insert("ok".to_string(), serde_json::json!(true));
    }
    HostdRouteResponse {
        status: 200,
        body: extra,
    }
}

fn required_json_string(body: &serde_json::Value, name: &str) -> anyhow::Result<String> {
    body.get(name)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("{name} is required"))
}

fn hyperv_launch_ubuntu(
    body: HostdLaunchBody,
    log_path: &Path,
) -> anyhow::Result<HostdRouteResponse> {
    let agent_id = body.agent_id.unwrap_or_else(|| "codex-demo".to_string());
    validate_hostd_agent_id(&agent_id)?;
    let harness = body.harness.unwrap_or_else(|| "codex".to_string());
    if !matches!(
        harness.as_str(),
        "codex" | "claude-code" | "opencode" | "none"
    ) {
        return Ok(HostdRouteResponse {
            status: 400,
            body: serde_json::json!({ "ok": false, "error": format!("unsupported harness: {harness}") }),
        });
    }
    let repo_root = repo_root()?;
    let launch_log_path = repo_root
        .join(".maturana")
        .join("logs")
        .join(format!("hyperv-launch-{agent_id}.log"));
    if let Some(parent) = launch_log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    hostd_log(
        log_path,
        &format!("launch requested; agent={agent_id} harness={harness}"),
    )?;
    hostd_log(&launch_log_path, "hostd launch started")?;
    let launcher = repo_root
        .join("scripts")
        .join("launch-ubuntu-cloudimg-hyperv.ps1");
    let mut command = ProcessCommand::new("powershell.exe");
    command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
    command.arg(launcher);
    add_process_arg(&mut command, "-AgentId", Some(agent_id.as_str()));
    add_process_arg_path(
        &mut command,
        "-BaseVhdxPath",
        body.base_vhdx_path.as_deref(),
    );
    add_process_arg(&mut command, "-SwitchName", body.switch_name.as_deref());
    add_process_arg(
        &mut command,
        "-SshUser",
        body.ssh_user.as_deref().or(Some("ubuntu")),
    );
    add_process_arg_path(&mut command, "-SshKeyPath", body.ssh_key_path.as_deref());
    add_process_arg_path(
        &mut command,
        "-CloudInitUserDataPath",
        body.cloud_init_user_data_path.as_deref(),
    );
    add_process_arg_path(
        &mut command,
        "-CloudInitMetaDataPath",
        body.cloud_init_meta_data_path.as_deref(),
    );
    let disk_size = body.disk_size_gb.map(|value| value.to_string());
    add_process_arg(&mut command, "-DiskSizeGB", disk_size.as_deref());
    let vcpu = body.vcpu.map(|value| value.to_string());
    add_process_arg(&mut command, "-Vcpu", vcpu.as_deref());
    let memory = body.memory_mib.map(|value| value.to_string());
    add_process_arg(&mut command, "-MemoryMiB", memory.as_deref());
    if body.provision_existing.unwrap_or(false) {
        command.arg("-ProvisionExisting");
    }
    if body.force.unwrap_or(false) {
        command.arg("-Force");
    }
    let output = command.output().context("failed to run Hyper-V launcher")?;
    let lines = command_output_lines(&output);
    let status_code = if output.status.success() { 200 } else { 500 };
    hostd_log(
        &launch_log_path,
        &format!("hostd launch finished exit_code={:?}", output.status.code()),
    )?;
    Ok(HostdRouteResponse {
        status: status_code,
        body: serde_json::json!({
            "ok": output.status.success(),
            "agent_id": agent_id,
            "status": if output.status.success() { "succeeded" } else { "failed" },
            "exit_code": output.status.code(),
            "log": launch_log_path,
            "output": lines,
        }),
    })
}

fn hyperv_vms() -> anyhow::Result<HostdRouteResponse> {
    let script = r#"
$ErrorActionPreference = 'Stop'
function Get-MaturanaVMIPv4 {
  param([string]$Name)
  $adapter = Get-VMNetworkAdapter -VMName $Name -ErrorAction SilentlyContinue
  if (!$adapter) { return '' }
  $addresses = @($adapter.IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' -and $_ -notlike '169.254.*' -and $_ -notlike '0.*' -and $_ -notlike '127.*' })
  if ($addresses.Count -gt 0) { return $addresses[0] }
  $mac = ($adapter.MacAddress -replace '[^0-9A-Fa-f]', '').ToUpperInvariant()
  if (!$mac) { return '' }
  $neighbor = Get-NetNeighbor -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object {
    ($_.LinkLayerAddress -replace '[^0-9A-Fa-f]', '').ToUpperInvariant() -eq $mac -and
    $_.IPAddress -match '^\d+\.\d+\.\d+\.\d+$' -and
    $_.IPAddress -notlike '169.254.*' -and $_.IPAddress -notlike '0.*' -and $_.IPAddress -notlike '127.*'
  } | Select-Object -First 1
  if ($neighbor) { return $neighbor.IPAddress }
  return ''
}
$vms = @(Get-VM | Where-Object { $_.Name -like 'maturana-*' } | ForEach-Object {
  [pscustomobject]@{
    name = $_.Name
    state = "$($_.State)"
    status = "$($_.Status)"
    uptime = "$($_.Uptime)"
    generation = $_.Generation
    processor_count = $_.ProcessorCount
    memory_startup = $_.MemoryStartup
    ipv4 = Get-MaturanaVMIPv4 -Name $_.Name
  }
})
@{ ok = $true; vms = $vms } | ConvertTo-Json -Compress -Depth 10
"#;
    hyperv_json_script(script)
}

fn hyperv_stop(agent_id: &str) -> anyhow::Result<HostdRouteResponse> {
    let vm_name = hostd_vm_name(agent_id)?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'; if(!(Get-VM -Name '{vm_name}' -ErrorAction SilentlyContinue)){{ @{{ ok=$false; error='VM not found: {vm_name}' }} | ConvertTo-Json -Compress; exit 4 }}; Stop-VM -Name '{vm_name}' -Force -TurnOff; @{{ ok=$true; vm='{vm_name}'; state='stopped' }} | ConvertTo-Json -Compress"#
    );
    hyperv_json_script_with_not_found(&script)
}

fn hyperv_snapshot_take(agent_id: &str, name: &str) -> anyhow::Result<HostdRouteResponse> {
    let vm_name = hostd_vm_name(agent_id)?;
    let snapshot = validate_hostd_snapshot_name(name)?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'; if(!(Get-VM -Name '{vm_name}' -ErrorAction SilentlyContinue)){{ @{{ ok=$false; error='VM not found: {vm_name}' }} | ConvertTo-Json -Compress; exit 4 }}; Checkpoint-VM -Name '{vm_name}' -SnapshotName '{snapshot}' | Out-Null; @{{ ok=$true; vm='{vm_name}'; snapshot='{snapshot}' }} | ConvertTo-Json -Compress"#
    );
    hyperv_json_script_with_not_found(&script)
}

fn hyperv_snapshot_restore(agent_id: &str, name: &str) -> anyhow::Result<HostdRouteResponse> {
    let vm_name = hostd_vm_name(agent_id)?;
    let snapshot = validate_hostd_snapshot_name(name)?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'; if(!(Get-VM -Name '{vm_name}' -ErrorAction SilentlyContinue)){{ @{{ ok=$false; error='VM not found: {vm_name}' }} | ConvertTo-Json -Compress; exit 4 }}; $s=Get-VMSnapshot -VMName '{vm_name}' -Name '{snapshot}' -ErrorAction SilentlyContinue; if(!$s){{ @{{ ok=$false; error='Snapshot not found: {snapshot}' }} | ConvertTo-Json -Compress; exit 4 }}; Restore-VMSnapshot -VMSnapshot $s -Confirm:$false; @{{ ok=$true; vm='{vm_name}'; snapshot='{snapshot}'; restored=$true }} | ConvertTo-Json -Compress"#
    );
    hyperv_json_script_with_not_found(&script)
}

fn hyperv_snapshot_list(agent_id: &str) -> anyhow::Result<HostdRouteResponse> {
    let vm_name = hostd_vm_name(agent_id)?;
    let script = format!(
        r#"$ErrorActionPreference='Stop'; if(!(Get-VM -Name '{vm_name}' -ErrorAction SilentlyContinue)){{ @{{ ok=$false; error='VM not found: {vm_name}' }} | ConvertTo-Json -Compress; exit 4 }}; $snapshots=@(Get-VMSnapshot -VMName '{vm_name}' -ErrorAction SilentlyContinue | Select-Object Name, CreationTime, SnapshotType); @{{ ok=$true; vm='{vm_name}'; snapshots=$snapshots }} | ConvertTo-Json -Compress -Depth 10"#
    );
    hyperv_json_script_with_not_found(&script)
}

fn hyperv_json_script(script: &str) -> anyhow::Result<HostdRouteResponse> {
    let output = ProcessCommand::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .output()
        .context("failed to run Hyper-V PowerShell adapter")?;
    let status = if output.status.success() { 200 } else { 500 };
    let body = parse_powershell_json_output(&output).unwrap_or_else(|_| {
        serde_json::json!({
            "ok": false,
            "error": String::from_utf8_lossy(&output.stderr).trim().to_string(),
            "output": command_output_lines(&output),
        })
    });
    Ok(HostdRouteResponse { status, body })
}

fn hyperv_json_script_with_not_found(script: &str) -> anyhow::Result<HostdRouteResponse> {
    let mut response = hyperv_json_script(script)?;
    if response.status >= 400
        && response
            .body
            .get("error")
            .and_then(|value| value.as_str())
            .map(|value| value.contains("not found"))
            .unwrap_or(false)
    {
        response.status = 404;
    }
    Ok(response)
}

fn parse_powershell_json_output(
    output: &std::process::Output,
) -> anyhow::Result<serde_json::Value> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .ok_or_else(|| anyhow::anyhow!("PowerShell adapter did not return JSON"))?;
    Ok(serde_json::from_str(json_line.trim())?)
}

fn command_output_lines(output: &std::process::Output) -> Vec<String> {
    let mut lines = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        lines.push(line.to_string());
    }
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        lines.push(line.to_string());
    }
    lines
}

fn add_process_arg(command: &mut ProcessCommand, name: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        command.arg(name).arg(value);
    }
}

fn add_process_arg_path(command: &mut ProcessCommand, name: &str, value: Option<&Path>) {
    if let Some(value) = value {
        command.arg(name).arg(value);
    }
}

fn hostd_vm_name(agent_id: &str) -> anyhow::Result<String> {
    validate_hostd_agent_id(agent_id)?;
    Ok(format!("maturana-{agent_id}"))
}

fn validate_hostd_agent_id(agent_id: &str) -> anyhow::Result<()> {
    if !agent_id.is_empty()
        && agent_id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        Ok(())
    } else {
        anyhow::bail!("invalid agent id: {agent_id}")
    }
}

fn validate_hostd_snapshot_name(name: &str) -> anyhow::Result<&str> {
    if !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        Ok(name)
    } else {
        anyhow::bail!("invalid snapshot name: {name}")
    }
}

fn read_hostd_request(stream: &TcpStream) -> anyhow::Result<HostdHttpRequest> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing HTTP target"))?;
    let (path, query) = parse_http_target(target);
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    // Cap the pre-allocation so a forged Content-Length can't OOM the elevated
    // daemon before any bytes arrive. hostd payloads are small JSON requests.
    if content_length > 1024 * 1024 {
        anyhow::bail!("hostd request body too large");
    }
    let mut body = vec![0; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(HostdHttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn parse_http_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut query = HashMap::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(name.to_string(), percent_decode_minimal(value));
    }
    (path.to_string(), query)
}

fn percent_decode_minimal(value: &str) -> String {
    let mut output = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                output.push(hex);
                index += 3;
                continue;
            }
        }
        output.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&output).to_string()
}

fn hostd_request_authorized(request: &HostdHttpRequest, token: &str) -> bool {
    request
        .headers
        .get("x-maturana-hostd-token")
        .map(|actual| actual == token)
        .unwrap_or(false)
}

fn write_hostd_json(
    stream: &mut TcpStream,
    status: u16,
    body: serde_json::Value,
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let json = serde_json::to_vec(&body)?;
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        json.len()
    )?;
    stream.write_all(&json)?;
    stream.flush()?;
    Ok(())
}

fn parse_hostd_bind_prefix(prefix: &str) -> anyhow::Result<SocketAddr> {
    let rest = prefix
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("hostd bind prefix must start with http://"))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("hostd bind prefix must include a port"))?;
    if !matches!(host, "127.0.0.1" | "localhost") {
        anyhow::bail!("hostd bind must stay on loopback, got {host}");
    }
    let port = port.parse::<u16>()?;
    Ok(SocketAddr::from(([127, 0, 0, 1], port)))
}

fn hostd_log(path: &Path, message: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let line = format!("{} {message}\n", Utc::now().to_rfc3339());
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())?;
    Ok(())
}

fn repo_root() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_dir()?)
}

fn absolute_or_cwd(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_hostd_bind_prefix_stays_loopback() {
        assert_eq!(
            parse_hostd_bind_prefix("http://127.0.0.1:47832/").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 47832))
        );
        assert_eq!(
            parse_hostd_bind_prefix("http://localhost:47832").unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 47832))
        );
        assert!(parse_hostd_bind_prefix("https://127.0.0.1:47832/").is_err());
        assert!(parse_hostd_bind_prefix("http://0.0.0.0:47832/").is_err());
    }

    #[test]
    fn rust_hostd_validates_agent_and_snapshot_names() {
        assert!(validate_hostd_agent_id("codex-demo-1").is_ok());
        assert!(validate_hostd_agent_id("Codex").is_err());
        assert!(validate_hostd_agent_id("../demo").is_err());
        assert_eq!(hostd_vm_name("codex-demo").unwrap(), "maturana-codex-demo");

        assert!(validate_hostd_snapshot_name("before.update-1").is_ok());
        assert!(validate_hostd_snapshot_name("../escape").is_err());
        assert!(validate_hostd_snapshot_name("bad/name").is_err());
    }

    #[test]
    fn rust_hostd_auth_and_target_parsing_are_fixed() {
        let (path, query) = parse_http_target("/agents/snapshot/list?agent_id=codex-demo%201");
        assert_eq!(path, "/agents/snapshot/list");
        assert_eq!(query.get("agent_id").unwrap(), "codex-demo 1");

        let mut request = HostdHttpRequest {
            method: "GET".to_string(),
            path,
            query,
            headers: HashMap::new(),
            body: Vec::new(),
        };
        assert!(!hostd_request_authorized(&request, "secret"));
        request
            .headers
            .insert("x-maturana-hostd-token".to_string(), "secret".to_string());
        assert!(hostd_request_authorized(&request, "secret"));
    }
}
