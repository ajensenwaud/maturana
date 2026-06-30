use std::{fs, path::PathBuf};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HostdStatus {
    pub url: String,
    pub reachable: bool,
    pub token_present: bool,
    pub error: Option<String>,
}

pub fn hostd_status() -> anyhow::Result<HostdStatus> {
    let health_url = hostd_url("/health");
    let token_present = hostd_token()?.is_some();
    match ureq::get(&health_url).call() {
        Ok(response) => {
            let payload: serde_json::Value = response.into_json()?;
            let reachable = payload
                .get("ok")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            Ok(HostdStatus {
                url: health_url,
                reachable,
                token_present,
                error: if reachable {
                    None
                } else {
                    Some(format!("unexpected health payload: {payload}"))
                },
            })
        }
        Err(error) => Ok(HostdStatus {
            url: health_url,
            reachable: false,
            token_present,
            error: Some(error.to_string()),
        }),
    }
}

pub fn live_agent_ip(agent_id: &str) -> anyhow::Result<Option<String>> {
    let payload = hostd_vms_payload()?;
    let expected_name = format!("maturana-{agent_id}");
    let ip = payload
        .get("vms")
        .and_then(|value| value.as_array())
        .and_then(|vms| {
            vms.iter().find_map(|vm| {
                let name = vm.get("name").and_then(|value| value.as_str())?;
                if name != expected_name {
                    return None;
                }
                vm.get("ipv4")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(ToString::to_string)
            })
        });
    Ok(ip)
}

pub fn hostd_vms() -> anyhow::Result<Vec<serde_json::Value>> {
    Ok(hostd_vms_payload()?
        .get("vms")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default())
}

pub fn hostd_get(path: &str) -> anyhow::Result<ureq::Response> {
    let mut request = ureq::get(&hostd_url(path));
    if let Some(token) = hostd_token()? {
        request = request.set("X-Maturana-Hostd-Token", &token);
    }
    Ok(request.call()?)
}

pub fn hostd_url(path: &str) -> String {
    format!(
        "{}{}",
        std::env::var("MATURANA_HOSTD_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:47832".to_string())
            .trim_end_matches('/'),
        path
    )
}

pub fn hostd_token() -> anyhow::Result<Option<String>> {
    if let Ok(token) = std::env::var("MATURANA_HOSTD_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(Some(token.trim().to_string()));
        }
    }
    let path = std::env::var("MATURANA_HOSTD_TOKEN_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".maturana/hostd/token"));
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    if path.exists() {
        let token = fs::read_to_string(path)?;
        let token = token.trim();
        if !token.is_empty() {
            return Ok(Some(token.to_string()));
        }
    }
    Ok(None)
}

fn hostd_vms_payload() -> anyhow::Result<serde_json::Value> {
    let response = hostd_get("/vms")?;
    let payload: serde_json::Value = response.into_json()?;
    if !payload
        .get("ok")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        anyhow::bail!("hostd /vms returned an error: {payload}");
    }
    Ok(payload)
}
