use anyhow::{Context, Result};
use directories::ProjectDirs;
use reqwest::Proxy;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const CONFIG_FILE_NAME: &str = "config.yaml";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 6737;
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 300;
pub const DEFAULT_ENABLED_PLUGINS: &str = "*";

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
    pub enabled_plugins: Option<String>,
    pub app_data_dir: Option<PathBuf>,
    pub plugin_overrides_dir: Option<PathBuf>,
    pub refresh_interval_secs: Option<u64>,
    pub daemon: Option<bool>,
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

fn default_config_template() -> &'static str {
    r#"# openusage-cli configuration.
# Generate this file explicitly with: openusage-cli --init-config
# CLI flags (and env vars for supported args) override this file.

# HTTP bind host.
host: 127.0.0.1

# HTTP bind port.
port: 6737

# Directory with plugin JS files. null = auto-discovery.
# plugins_dir: /path/to/plugins
plugins_dir: null

# Comma-separated glob masks for enabled plugin IDs.
# Examples: "*" (all), "codex,cursor", "c*"
enabled_plugins: "*"

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
            parsed.enabled_plugins.as_deref(),
            Some(DEFAULT_ENABLED_PLUGINS)
        );
        assert_eq!(
            parsed.refresh_interval_secs,
            Some(DEFAULT_REFRESH_INTERVAL_SECS)
        );
        assert_eq!(parsed.daemon, Some(false));
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
}
