use anyhow::{Context, Result};
use directories::ProjectDirs;
use reqwest::Proxy;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

pub const CONFIG_FILE_NAME: &str = "config.yaml";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 6736;
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 300;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub url: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AppConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub plugins_dir: Option<PathBuf>,
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

pub fn load_or_create_config() -> Result<LoadedConfig> {
    let path = config_path()?;
    ensure_config_exists(&path)?;
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config: AppConfig = serde_yaml::from_str(&contents)
        .with_context(|| format!("failed to parse YAML config {}", path.display()))?;
    Ok(LoadedConfig { path, config })
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(project_dirs) = ProjectDirs::from("com", "openusage", "openusage-cli") {
        return Ok(project_dirs.config_dir().join(CONFIG_FILE_NAME));
    }

    let cwd = std::env::current_dir().context("cannot get current directory")?;
    Ok(cwd.join(".openusage-cli").join(CONFIG_FILE_NAME))
}

fn ensure_config_exists(path: &PathBuf) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }

    std::fs::write(path, default_config_template())
        .with_context(|| format!("failed to write default config file {}", path.display()))
}

fn default_config_template() -> &'static str {
    r#"# openusage-cli configuration.
# File is created automatically on first launch.
# CLI flags (and env vars for supported args) override this file.

# HTTP bind host.
host: 127.0.0.1

# HTTP bind port.
port: 6736

# Directory with plugin JS files. null = auto-discovery.
# plugins_dir: /path/to/plugins
plugins_dir: null

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
    let loaded = load_or_create_config().ok()?;
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

    #[test]
    fn default_template_is_valid_yaml() {
        let parsed: AppConfig =
            serde_yaml::from_str(default_config_template()).expect("default config must parse");

        assert_eq!(parsed.host.as_deref(), Some(DEFAULT_HOST));
        assert_eq!(parsed.port, Some(DEFAULT_PORT));
        assert_eq!(
            parsed.refresh_interval_secs,
            Some(DEFAULT_REFRESH_INTERVAL_SECS)
        );
        assert_eq!(parsed.daemon, Some(false));
        let proxy = parsed.proxy.expect("proxy section must exist");
        assert!(!proxy.enabled);
        assert!(proxy.url.is_empty());
    }
}
