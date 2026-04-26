use crate::discovery::discover_daemon_endpoint_with_override;
use anyhow::{Context, Result, anyhow};
use clap::ValueEnum;
use serde::Deserialize;
use std::fmt;
use std::path::Path;
use std::time::Duration;

const HTTP_TIMEOUT_SECS: u64 = 5;
const SHUTDOWN_POLL_INTERVAL_MS: u64 = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ExistingInstancePolicy {
    Error,
    Ignore,
    Replace,
}

impl ExistingInstancePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Ignore => "ignore",
            Self::Replace => "replace",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "error" => Ok(Self::Error),
            "ignore" => Ok(Self::Ignore),
            "replace" => Ok(Self::Replace),
            _ => anyhow::bail!(
                "invalid existing_instance value '{}'; expected one of: error, ignore, replace",
                raw
            ),
        }
    }
}

impl fmt::Display for ExistingInstancePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ServiceMode {
    Standalone,
    Systemd,
}

impl ServiceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Systemd => "systemd",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "standalone" => Ok(Self::Standalone),
            "systemd" => Ok(Self::Systemd),
            _ => anyhow::bail!(
                "invalid service mode '{}'; expected one of: standalone, systemd",
                raw
            ),
        }
    }
}

impl fmt::Display for ServiceMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct RunningInstance {
    pub base_url: String,
    pub service_mode: ServiceMode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeConfigProbe {
    app_version: Option<String>,
    service_mode: Option<String>,
}

pub async fn discover_running_instance(
    runtime_dir_override: Option<&Path>,
) -> Option<RunningInstance> {
    let daemon_url = discover_daemon_endpoint_with_override(runtime_dir_override)?;

    match fetch_running_instance(&daemon_url).await {
        Ok(instance) => Some(instance),
        Err(err) => {
            log::warn!(
                "daemon endpoint discovered at {} but instance validation failed: {}; ignoring stale discovery",
                daemon_url,
                err
            );
            None
        }
    }
}

async fn fetch_running_instance(base_url: &str) -> Result<RunningInstance> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("failed to create HTTP client")?;
    let probe_url = format!("{}/v1/config", base_url);
    let response = client
        .get(&probe_url)
        .send()
        .await
        .with_context(|| format!("failed to query running instance config at {}", probe_url))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "running instance config endpoint returned status {}",
            response.status()
        );
    }

    let payload: RuntimeConfigProbe = response
        .json()
        .await
        .context("running instance config returned invalid JSON")?;
    if payload.app_version.is_none() {
        anyhow::bail!("running instance config payload missing appVersion");
    }

    let service_mode = match payload.service_mode {
        Some(value) => ServiceMode::parse(&value)
            .with_context(|| format!("invalid remote service mode '{}'", value))?,
        None => ServiceMode::Standalone,
    };

    Ok(RunningInstance {
        base_url: base_url.to_string(),
        service_mode,
    })
}

pub async fn request_shutdown(base_url: &str) -> Result<()> {
    let client = build_http_client()?;
    let shutdown_url = control_url(base_url, "shutdown");
    let response = send_control_request(&client, &shutdown_url, "shutdown").await?;
    ensure_control_success("shutdown", &shutdown_url, response).await
}

pub async fn request_restart(base_url: &str) -> Result<()> {
    let client = build_http_client()?;
    let restart_url = control_url(base_url, "restart");
    let restart_response = send_control_request(&client, &restart_url, "restart").await?;
    ensure_control_success("restart", &restart_url, restart_response).await
}

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("failed to create HTTP client")
}

fn control_url(base_url: &str, action: &str) -> String {
    format!("{}/v1/{}", base_url, action)
}

async fn send_control_request(
    client: &reqwest::Client,
    action_url: &str,
    action: &str,
) -> Result<reqwest::Response> {
    client
        .post(action_url)
        .send()
        .await
        .with_context(|| format!("failed to request {} via {}", action, action_url))
}

async fn ensure_control_success(
    action: &str,
    action_url: &str,
    response: reqwest::Response,
) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    let body = response.text().await.unwrap_or_default();
    anyhow::bail!(
        "{} endpoint {} returned status {}: {}",
        action,
        action_url,
        status,
        body
    );
}

pub async fn wait_until_unreachable(base_url: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("failed to create HTTP client")?;
    let health_url = format!("{}/health", base_url);

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for existing instance {} to stop",
                base_url
            ));
        }

        let is_reachable = client.get(&health_url).send().await.is_ok();
        if !is_reachable {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(SHUTDOWN_POLL_INTERVAL_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_instance_policy_parse_accepts_all_values() {
        assert_eq!(
            ExistingInstancePolicy::parse("error").expect("parse error policy"),
            ExistingInstancePolicy::Error
        );
        assert_eq!(
            ExistingInstancePolicy::parse("ignore").expect("parse ignore policy"),
            ExistingInstancePolicy::Ignore
        );
        assert_eq!(
            ExistingInstancePolicy::parse("replace").expect("parse replace policy"),
            ExistingInstancePolicy::Replace
        );
    }

    #[test]
    fn service_mode_parse_accepts_all_values() {
        assert_eq!(
            ServiceMode::parse("standalone").expect("parse standalone service mode"),
            ServiceMode::Standalone
        );
        assert_eq!(
            ServiceMode::parse("systemd").expect("parse systemd service mode"),
            ServiceMode::Systemd
        );
    }
}
