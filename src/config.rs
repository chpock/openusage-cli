use reqwest::Proxy;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub proxy: Option<ProxyConfig>,
}

#[derive(Debug, Clone)]
pub struct ResolvedProxy {
    pub proxy: Proxy,
}

static RESOLVED_PROXY: OnceLock<Option<ResolvedProxy>> = OnceLock::new();

pub fn get_resolved_proxy() -> Option<&'static ResolvedProxy> {
    RESOLVED_PROXY.get_or_init(load_and_resolve_proxy).as_ref()
}

fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".openusage").join("config.json"))
}

fn load_and_resolve_proxy() -> Option<ResolvedProxy> {
    if let Some(proxy) = proxy_from_config() {
        return Some(proxy);
    }
    proxy_from_env()
}

fn proxy_from_config() -> Option<ResolvedProxy> {
    let path = config_path()?;
    let contents = std::fs::read_to_string(path).ok()?;
    let config: AppConfig = serde_json::from_str(&contents).ok()?;
    let proxy_cfg = config.proxy.as_ref().filter(|p| p.enabled)?;
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
