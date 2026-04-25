use anyhow::{Context, Result};
use directories::ProjectDirs;
use reqwest::Proxy;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const CONFIG_FILE_NAME: &str = "config.yaml";
pub const RUNTIME_DIR_NAME: &str = "runtime";
pub const DAEMON_ENDPOINT_FILE_NAME: &str = "daemon-endpoint";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 0;
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 300;
pub const DEFAULT_ENABLED_PLUGINS: &str = "*";
pub const DEFAULT_LOG_LEVEL: &str = "error";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonEndpointPath {
    pub dir: PathBuf,
    pub endpoint_file: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub url: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AppConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub plugins_dir: Option<PathBuf>,
    pub enabled_plugins: Option<Vec<String>>,
    pub app_data_dir: Option<PathBuf>,
    pub plugin_overrides_dir: Option<PathBuf>,
    pub refresh_interval_secs: Option<u64>,
    pub daemon: Option<bool>,
    pub existing_instance: Option<String>,
    pub log_level: Option<String>,
    pub proxy: Option<ProxyConfig>,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: AppConfig,
}

#[derive(Debug, Clone)]
pub struct ResolvedProxy {
    pub proxy: Proxy,
}

static RESOLVED_PROXY: OnceLock<Option<ResolvedProxy>> = OnceLock::new();

pub fn get_resolved_proxy() -> Option<&'static ResolvedProxy> {
    RESOLVED_PROXY.get_or_init(load_and_resolve_proxy).as_ref()
}

pub fn load_config_if_exists() -> Result<Option<LoadedConfig>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config: AppConfig = serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse YAML config {}", path.display()))?;
    Ok(Some(LoadedConfig { path, config }))
}

pub fn write_default_config_if_missing() -> Result<(PathBuf, bool)> {
    let path = config_path()?;
    let created = write_default_config_to_path(&path, false)?;
    Ok((path, created))
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(project_dirs) = ProjectDirs::from("com", "openusage", "openusage-cli") {
        return Ok(project_dirs.config_dir().join(CONFIG_FILE_NAME));
    }

    let cwd = std::env::current_dir().context("cannot get current directory")?;
    Ok(cwd.join(".openusage-cli").join(CONFIG_FILE_NAME))
}

pub fn daemon_endpoint_path() -> Result<DaemonEndpointPath> {
    let dir = daemon_runtime_dir()?;
    Ok(DaemonEndpointPath {
        endpoint_file: dir.join(DAEMON_ENDPOINT_FILE_NAME),
        dir,
    })
}

fn daemon_runtime_dir() -> Result<PathBuf> {
    if let Some(project_dirs) = ProjectDirs::from("com", "openusage", "openusage-cli") {
        if let Some(runtime_dir) = project_dirs.runtime_dir() {
            return Ok(runtime_dir.join(RUNTIME_DIR_NAME));
        }
        return Ok(project_dirs.data_local_dir().join(RUNTIME_DIR_NAME));
    }

    let cwd = std::env::current_dir().context("cannot get current directory")?;
    Ok(cwd.join(".openusage-cli").join(RUNTIME_DIR_NAME))
}

fn write_default_config_to_path(path: &Path, overwrite: bool) -> Result<bool> {
    if path.exists() && !overwrite {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }

    std::fs::write(path, default_config_template())
        .with_context(|| format!("failed to write default config file {}", path.display()))?;

    Ok(true)
}

pub fn default_config_template() -> &'static str {
    r#"# openusage-cli configuration.
# Print this template explicitly with: openusage-cli --default-config
# CLI flags (and env vars for supported args) override this file.

# HTTP bind host.
host: 127.0.0.1

# HTTP bind port (0 = random port assigned by OS).
port: 0

# Directory with plugin JS files. null = auto-discovery.
# plugins_dir: /path/to/plugins
plugins_dir: null

# List of glob masks for enabled plugin IDs.
# Examples: ["*"] (all), ["codex", "cursor"], ["c*"]
enabled_plugins:
  - "*"

# Directory for provider data/cache. null = platform default.
# app_data_dir: /path/to/app-data
app_data_dir: null

# Directory with plugin override scripts. null = auto-discovery.
# plugin_overrides_dir: /path/to/plugin-overrides
plugin_overrides_dir: null

# Background refresh interval in seconds. 0 disables periodic refresh.
refresh_interval_secs: 300

# Run as background daemon process.
daemon: false

# Behavior when a running daemon instance is already discovered.
# Values: error | ignore | replace
existing_instance: error

# Log level: error, warn, info, debug, trace.
log_level: error

proxy:
  # Enable proxy for outgoing plugin HTTP requests.
  enabled: false
  # Proxy URL, examples:
  # - http://127.0.0.1:7890
  # - socks5h://127.0.0.1:1080
  url: ""
"#
}

fn load_and_resolve_proxy() -> Option<ResolvedProxy> {
    if let Some(proxy) = proxy_from_config() {
        return Some(proxy);
    }
    proxy_from_env()
}

fn proxy_from_config() -> Option<ResolvedProxy> {
    let loaded = load_config_if_exists().ok().flatten()?;
    let proxy_cfg = loaded.config.proxy.as_ref().filter(|p| p.enabled)?;
    build_proxy(&proxy_cfg.url)
}

fn proxy_from_env() -> Option<ResolvedProxy> {
    ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .and_then(|value| build_proxy(value.trim()))
}

fn build_proxy(url: &str) -> Option<ResolvedProxy> {
    if url.is_empty() {
        return None;
    }
    let proxy = Proxy::all(url).ok()?;
    let no_proxy = reqwest::NoProxy::from_string("localhost,127.0.0.1,::1");
    Some(ResolvedProxy {
        proxy: proxy.no_proxy(no_proxy),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_template_is_valid_yaml() {
        let parsed: AppConfig =
            serde_yaml::from_str(default_config_template()).expect("default config must parse");

        assert_eq!(parsed.host.as_deref(), Some(DEFAULT_HOST));
        assert_eq!(parsed.port, Some(DEFAULT_PORT));
        assert_eq!(
            parsed.enabled_plugins,
            Some(vec![DEFAULT_ENABLED_PLUGINS.to_string()])
        );
        assert_eq!(
            parsed.refresh_interval_secs,
            Some(DEFAULT_REFRESH_INTERVAL_SECS)
        );
        assert_eq!(parsed.daemon, Some(false));
        assert_eq!(parsed.existing_instance.as_deref(), Some("error"));
        assert_eq!(parsed.log_level.as_deref(), Some(DEFAULT_LOG_LEVEL));
        let proxy = parsed.proxy.expect("proxy section must exist");
        assert!(!proxy.enabled);
        assert!(proxy.url.is_empty());
    }

    #[test]
    fn write_default_config_to_path_creates_file_with_template() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("nested/config.yaml");

        let created = write_default_config_to_path(&path, false).expect("write default config");
        assert!(created);

        let written = std::fs::read_to_string(&path).expect("read written config");
        assert_eq!(written, default_config_template());
    }

    #[test]
    fn write_default_config_to_path_keeps_existing_file_when_not_overwriting() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "host: 0.0.0.0\n").expect("write existing config");

        let created = write_default_config_to_path(&path, false).expect("write default config");
        assert!(!created);

        let written = std::fs::read_to_string(&path).expect("read existing config");
        assert_eq!(written, "host: 0.0.0.0\n");
    }

    #[test]
    fn daemon_endpoint_path_uses_expected_file_name() {
        let path = daemon_endpoint_path().expect("resolve daemon endpoint path");

        assert_eq!(
            path.endpoint_file
                .file_name()
                .and_then(|value| value.to_str()),
            Some(DAEMON_ENDPOINT_FILE_NAME)
        );
        assert_eq!(path.endpoint_file.parent(), Some(path.dir.as_path()));
    }
}
