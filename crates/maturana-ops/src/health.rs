use std::time::Duration;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HealthCheck {
    pub ok: bool,
    pub message: String,
}

pub fn http_health(url: &str) -> HealthCheck {
    http_health_with_timeout(url, Duration::from_secs(2))
}

pub fn http_health_with_timeout(url: &str, timeout: Duration) -> HealthCheck {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    match agent.get(url).call() {
        Ok(response) => match response.into_json::<serde_json::Value>() {
            Ok(payload)
                if payload
                    .get("ok")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false) =>
            {
                HealthCheck {
                    ok: true,
                    message: url.to_string(),
                }
            }
            Ok(payload) => HealthCheck {
                ok: false,
                message: format!("unexpected payload from {url}: {payload}"),
            },
            Err(error) => HealthCheck {
                ok: false,
                message: format!("invalid JSON from {url}: {error}"),
            },
        },
        Err(error) => HealthCheck {
            ok: false,
            message: format!("{url}: {error}"),
        },
    }
}
