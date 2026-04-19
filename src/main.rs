use anyhow::{Context, Result, anyhow};
use clap::Parser;
use directories::ProjectDirs;
use globset::{Glob, GlobSet, GlobSetBuilder};
use openusage_cli::config;
use openusage_cli::daemon::{CachedPluginSnapshot, DaemonState};
use openusage_cli::http_api::{self, ApiState};
use openusage_cli::plugin_engine::manifest;
use openusage_cli::plugin_engine::runtime::MetricLine;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

const SYSTEM_PLUGINS_DIR: &str = "/usr/share/openusage-cli/openusage-plugins";
const SYSTEM_PLUGIN_OVERRIDES_DIR: &str = "/usr/share/openusage-cli/plugin-overrides";
const APP_VERSION: &str = match option_env!("OPENUSAGE_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Debug, Parser)]
#[command(name = "openusage-cli")]
#[command(about = "HTTP daemon for AI usage limit plugins")]
#[command(version = APP_VERSION)]
struct Cli {
    #[arg(long)]
    host: Option<String>,

    #[arg(long)]
    port: Option<u16>,

    #[arg(long, env = "OPENUSAGE_PLUGINS_DIR")]
    plugins_dir: Option<PathBuf>,

    #[arg(long, env = "OPENUSAGE_ENABLED_PLUGINS")]
    enabled_plugins: Option<String>,

    #[arg(long, env = "OPENUSAGE_APP_DATA_DIR")]
    app_data_dir: Option<PathBuf>,

    #[arg(long, env = "OPENUSAGE_PLUGIN_OVERRIDES_DIR")]
    plugin_overrides_dir: Option<PathBuf>,

    #[arg(long)]
    refresh_interval_secs: Option<u64>,

    #[arg(long, default_value_t = false)]
    init_config: bool,

    #[arg(long, default_value_t = false)]
    daemon: bool,

    #[arg(long, hide = true, default_value_t = false)]
    daemon_child: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    install_panic_hook();

    let result = run().await;
    match &result {
        Ok(_) => log::info!("openusage-cli shutdown complete"),
        Err(err) => log::error!("openusage-cli exiting with error: {err:#}"),
    }

    result
}

async fn run() -> Result<()> {
    let raw_args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let cli = Cli::parse();

    if cli.init_config {
        let (path, created) =
            config::write_default_config_if_missing().context("failed to write default config")?;
        if created {
            log::info!("wrote default config template to {}", path.display());
        } else {
            log::info!(
                "config file already exists at {}; keeping it",
                path.display()
            );
        }
        return Ok(());
    }

    let loaded_config = config::load_config_if_exists().context("failed to load config")?;
    let runtime = match loaded_config {
        Some(loaded) => {
            log::info!("using config file: {}", loaded.path.display());
            RuntimeCli::from_sources(cli, loaded.config)
        }
        None => {
            let path = config::config_path().context("failed to resolve config path")?;
            log::info!(
                "config file not found at {}; using CLI/env/default values",
                path.display()
            );
            RuntimeCli::from_sources(cli, config::AppConfig::default())
        }
    };
    let app_version = APP_VERSION.to_string();

    log::info!("starting openusage-cli v{}", app_version);
    log::debug!(
        "startup options: host={}, port={}, refresh_interval_secs={}, daemon={}, daemon_child={}, plugins_dir={:?}, enabled_plugins='{}', app_data_dir={:?}, plugin_overrides_dir={:?}",
        runtime.host,
        runtime.port,
        runtime.refresh_interval_secs,
        runtime.daemon,
        runtime.daemon_child,
        runtime.plugins_dir,
        runtime.enabled_plugins,
        runtime.app_data_dir,
        runtime.plugin_overrides_dir
    );

    if runtime.daemon && !runtime.daemon_child {
        let child_pid =
            spawn_daemon_process(&raw_args).context("failed to spawn background daemon process")?;
        log::info!("daemon mode enabled; spawned background process pid={child_pid}");
        return Ok(());
    }

    let app_data_dir = resolve_app_data_dir(runtime.app_data_dir)
        .context("failed to resolve app data directory")?;
    log::info!("using app data dir: {}", app_data_dir.display());
    std::fs::create_dir_all(&app_data_dir).with_context(|| {
        format!(
            "failed to create app data directory {}",
            app_data_dir.display()
        )
    })?;
    log::debug!("ensured app data directory exists");

    let plugins_dir =
        resolve_plugins_dir(runtime.plugins_dir).context("failed to resolve plugins directory")?;
    log::info!("using plugins dir: {}", plugins_dir.display());
    let loaded_plugins = manifest::load_plugins_from_dir(&plugins_dir);
    if loaded_plugins.is_empty() {
        anyhow::bail!(
            "no plugins found in {} (set --plugins-dir or install plugin data under <prefix>/share/openusage-cli/openusage-plugins)",
            plugins_dir.display()
        );
    }

    let loaded_plugin_ids: Vec<String> = loaded_plugins
        .iter()
        .map(|p| p.manifest.id.clone())
        .collect();
    log::info!(
        "loaded {} plugins from {}",
        loaded_plugins.len(),
        plugins_dir.display()
    );
    log::debug!("loaded plugin ids: {:?}", loaded_plugin_ids);

    let enabled_plugins_matcher = EnabledPluginsMatcher::from_csv(&runtime.enabled_plugins)
        .with_context(|| {
            format!(
                "invalid enabled_plugins value '{}' (expected comma-separated glob masks)",
                runtime.enabled_plugins
            )
        })?;
    let total_loaded_plugins = loaded_plugins.len();
    let plugins: Vec<_> = loaded_plugins
        .into_iter()
        .filter(|plugin| enabled_plugins_matcher.is_enabled(&plugin.manifest.id))
        .collect();

    if plugins.is_empty() {
        anyhow::bail!(
            "no plugins enabled after applying enabled_plugins='{}'. loaded plugin ids: {:?}",
            runtime.enabled_plugins,
            loaded_plugin_ids
        );
    }

    let plugin_ids: Vec<String> = plugins.iter().map(|p| p.manifest.id.clone()).collect();
    log::info!(
        "enabled {} of {} plugins (enabled_plugins='{}')",
        plugins.len(),
        total_loaded_plugins,
        runtime.enabled_plugins
    );
    log::debug!("enabled plugin ids: {:?}", plugin_ids);

    let plugin_overrides_dir = resolve_plugin_overrides_dir(runtime.plugin_overrides_dir)
        .context("failed to resolve plugin overrides directory")?;
    if let Some(path) = &plugin_overrides_dir {
        log::info!("using plugin overrides dir: {}", path.display());
    } else {
        log::info!("plugin overrides disabled (no overrides dir found)");
    }

    let daemon = Arc::new(DaemonState::new(
        plugins,
        app_data_dir,
        app_version.clone(),
        plugin_overrides_dir,
    ));

    log::info!("running initial plugin refresh");
    match daemon.refresh(None).await {
        Ok(snapshots) => {
            log::info!("initial plugin refresh completed");
            log_plugin_initialization_summary(&snapshots);
        }
        Err(err) => {
            log::warn!("initial plugin refresh failed: {}", err);
            log_plugin_initialization_failure_summary(&plugin_ids, &err.to_string());
        }
    }

    let refresh_task = if runtime.refresh_interval_secs > 0 {
        let refresh_state = daemon.clone();
        let refresh_every = Duration::from_secs(runtime.refresh_interval_secs);
        log::info!(
            "background refresh enabled (every {}s)",
            runtime.refresh_interval_secs
        );
        Some(tokio::spawn(async move {
            log::debug!("background refresh task started");
            loop {
                tokio::time::sleep(refresh_every).await;
                log::debug!("running background refresh");
                if let Err(err) = refresh_state.refresh(None).await {
                    log::warn!("background refresh failed: {}", err);
                } else {
                    log::debug!("background refresh completed");
                }
            }
        }))
    } else {
        log::info!("background refresh disabled (refresh_interval_secs=0)");
        None
    };

    let addr: SocketAddr = format!("{}:{}", runtime.host, runtime.port)
        .parse()
        .context("invalid bind address")?;
    log::info!("attempting to bind HTTP listener on {}", addr);
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            return Err(anyhow!(
                "failed to bind {}: {}. another process is likely already using this address; stop it or use --port with another value",
                addr,
                err
            ));
        }
    };
    log::debug!("HTTP listener successfully bound on {}", addr);

    let app = http_api::router(ApiState {
        daemon,
        app_version,
    });
    log::debug!("HTTP router initialized");

    log::info!("openusage-cli daemon listening on http://{}", addr);
    log::debug!("waiting for shutdown signal");

    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("http server failed");

    if let Some(task) = refresh_task {
        task.abort();
        match task.await {
            Ok(()) => log::debug!("background refresh task exited"),
            Err(err) if err.is_cancelled() => {
                log::debug!("background refresh task cancelled during shutdown")
            }
            Err(err) => log::warn!("background refresh task ended with error: {}", err),
        }
    }

    server_result?;

    log::info!("HTTP server stopped");
    Ok(())
}

#[derive(Debug, Clone)]
struct RuntimeCli {
    host: String,
    port: u16,
    plugins_dir: Option<PathBuf>,
    enabled_plugins: String,
    app_data_dir: Option<PathBuf>,
    plugin_overrides_dir: Option<PathBuf>,
    refresh_interval_secs: u64,
    daemon: bool,
    daemon_child: bool,
}

impl RuntimeCli {
    fn from_sources(cli: Cli, config: config::AppConfig) -> Self {
        let host = cli
            .host
            .or(config.host)
            .unwrap_or_else(|| config::DEFAULT_HOST.to_string());
        let port = cli.port.or(config.port).unwrap_or(config::DEFAULT_PORT);
        let refresh_interval_secs = cli
            .refresh_interval_secs
            .or(config.refresh_interval_secs)
            .unwrap_or(config::DEFAULT_REFRESH_INTERVAL_SECS);
        let daemon = if cli.daemon {
            true
        } else {
            config.daemon.unwrap_or(false)
        };
        let enabled_plugins = cli
            .enabled_plugins
            .or(config.enabled_plugins)
            .unwrap_or_else(|| config::DEFAULT_ENABLED_PLUGINS.to_string());

        Self {
            host,
            port,
            plugins_dir: cli.plugins_dir.or(config.plugins_dir),
            enabled_plugins,
            app_data_dir: cli.app_data_dir.or(config.app_data_dir),
            plugin_overrides_dir: cli.plugin_overrides_dir.or(config.plugin_overrides_dir),
            refresh_interval_secs,
            daemon,
            daemon_child: cli.daemon_child,
        }
    }
}

#[derive(Debug, Clone)]
struct EnabledPluginsMatcher {
    glob_set: GlobSet,
}

impl EnabledPluginsMatcher {
    fn from_csv(raw: &str) -> Result<Self> {
        let masks = raw
            .split(',')
            .map(str::trim)
            .filter(|mask| !mask.is_empty())
            .collect::<Vec<_>>();

        if masks.is_empty() {
            anyhow::bail!("enabled plugin mask list is empty");
        }

        let mut builder = GlobSetBuilder::new();
        for mask in masks {
            let glob = Glob::new(mask)
                .with_context(|| format!("invalid enabled plugin glob mask '{mask}'"))?;
            builder.add(glob);
        }
        let glob_set = builder
            .build()
            .context("failed to compile enabled plugin glob masks")?;

        Ok(Self { glob_set })
    }

    fn is_enabled(&self, plugin_id: &str) -> bool {
        self.glob_set.is_match(plugin_id)
    }
}

fn plugins_dir_candidates(cwd: &Path, exec_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let source_root = source_checkout_root_from_exec_dir(exec_dir);

    if let Some(source_root) = source_root {
        push_unique_path(
            &mut candidates,
            source_root.join("vendor/openusage/plugins"),
        );
        push_unique_path(&mut candidates, source_root.join("plugins"));
        push_unique_path(&mut candidates, cwd.join("vendor/openusage/plugins"));
        push_unique_path(&mut candidates, cwd.join("plugins"));
        push_unique_path(&mut candidates, exec_dir.join("vendor/openusage/plugins"));
        push_unique_path(&mut candidates, exec_dir.join("plugins"));
    }

    if let Some(packaged_path) = packaged_plugins_dir_from_exec_dir(exec_dir) {
        push_unique_path(&mut candidates, packaged_path);
    }
    push_unique_path(&mut candidates, PathBuf::from(SYSTEM_PLUGINS_DIR));

    candidates
}

fn plugin_overrides_dir_candidates(cwd: &Path, exec_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let source_root = source_checkout_root_from_exec_dir(exec_dir);

    if let Some(source_root) = source_root {
        push_unique_path(&mut candidates, source_root.join("plugin-overrides"));
        push_unique_path(&mut candidates, cwd.join("plugin-overrides"));
        push_unique_path(&mut candidates, exec_dir.join("plugin-overrides"));
    }

    if let Some(packaged_path) = packaged_overrides_dir_from_exec_dir(exec_dir) {
        push_unique_path(&mut candidates, packaged_path);
    }
    push_unique_path(&mut candidates, PathBuf::from(SYSTEM_PLUGIN_OVERRIDES_DIR));

    candidates
}

fn source_checkout_root_from_exec_dir(exec_dir: &Path) -> Option<PathBuf> {
    let profile = exec_dir.file_name()?.to_str()?;
    if profile != "debug" && profile != "release" {
        return None;
    }

    let parent = exec_dir.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("target") {
        return parent.parent().map(Path::to_path_buf);
    }

    let maybe_target = parent.parent()?;
    if maybe_target.file_name().and_then(|name| name.to_str()) == Some("target") {
        return maybe_target.parent().map(Path::to_path_buf);
    }

    None
}

fn packaged_plugins_dir_from_exec_dir(exec_dir: &Path) -> Option<PathBuf> {
    exec_dir
        .parent()
        .map(|prefix| prefix.join("share/openusage-cli/openusage-plugins"))
}

fn packaged_overrides_dir_from_exec_dir(exec_dir: &Path) -> Option<PathBuf> {
    exec_dir
        .parent()
        .map(|prefix| prefix.join("share/openusage-cli/plugin-overrides"))
}

fn push_unique_path(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

fn resolve_app_data_dir(cli_value: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = cli_value {
        log::debug!("app data dir provided via CLI/env: {}", path.display());
        return Ok(path);
    }
    if let Some(project_dirs) = ProjectDirs::from("com", "openusage", "openusage-cli") {
        let resolved = project_dirs.data_local_dir().to_path_buf();
        log::debug!(
            "app data dir resolved via ProjectDirs: {}",
            resolved.display()
        );
        return Ok(resolved);
    }
    let cwd = std::env::current_dir().context("cannot get current directory")?;
    let fallback = cwd.join(".openusage-cli");
    log::debug!(
        "app data dir fallback to current dir: {}",
        fallback.display()
    );
    Ok(fallback)
}

fn resolve_plugins_dir(cli_value: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = cli_value {
        log::debug!("plugins dir provided via CLI/env: {}", path.display());
        return Ok(path);
    }

    let cwd = std::env::current_dir().context("cannot get current directory")?;
    let exec_dir = executable_dir()?;
    let candidates = plugins_dir_candidates(&cwd, &exec_dir);

    for candidate in candidates {
        log::debug!("checking plugins dir candidate {}", candidate.display());
        if candidate.is_dir() {
            log::debug!("plugins dir candidate selected: {}", candidate.display());
            return Ok(candidate);
        }
    }

    anyhow::bail!("plugins directory not found")
}

fn resolve_plugin_overrides_dir(cli_value: Option<PathBuf>) -> Result<Option<PathBuf>> {
    if let Some(path) = cli_value {
        if !path.exists() {
            anyhow::bail!("plugin overrides dir does not exist: {}", path.display());
        }
        if !path.is_dir() {
            anyhow::bail!(
                "plugin overrides path is not a directory: {}",
                path.display()
            );
        }
        log::debug!(
            "plugin overrides dir provided via CLI/env: {}",
            path.display()
        );
        return Ok(Some(path));
    }

    let cwd = std::env::current_dir().context("cannot get current directory")?;
    let exec_dir = executable_dir()?;
    let candidates = plugin_overrides_dir_candidates(&cwd, &exec_dir);

    for candidate in candidates {
        log::debug!(
            "checking plugin overrides dir candidate {}",
            candidate.display()
        );
        if candidate.is_dir() {
            log::debug!(
                "plugin overrides dir candidate selected: {}",
                candidate.display()
            );
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

fn executable_dir() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("cannot resolve executable path")?;
    let dir = executable
        .parent()
        .map(Path::to_path_buf)
        .context("executable has no parent directory")?;
    Ok(dir)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "failed to subscribe to SIGINT/SIGTERM/SIGQUIT/SIGHUP: {}; falling back to Ctrl+C",
                    err
                );
                wait_for_ctrl_c().await;
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "failed to subscribe to SIGINT/SIGTERM/SIGQUIT/SIGHUP: {}; falling back to Ctrl+C",
                    err
                );
                wait_for_ctrl_c().await;
                return;
            }
        };
        let mut sigquit = match signal(SignalKind::quit()) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "failed to subscribe to SIGINT/SIGTERM/SIGQUIT/SIGHUP: {}; falling back to Ctrl+C",
                    err
                );
                wait_for_ctrl_c().await;
                return;
            }
        };
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(v) => v,
            Err(err) => {
                log::warn!(
                    "failed to subscribe to SIGINT/SIGTERM/SIGQUIT/SIGHUP: {}; falling back to Ctrl+C",
                    err
                );
                wait_for_ctrl_c().await;
                return;
            }
        };

        log::debug!("shutdown signal handler waiting for SIGINT/SIGTERM/SIGQUIT/SIGHUP");

        tokio::select! {
            _ = sigint.recv() => log::info!("received SIGINT, starting graceful shutdown"),
            _ = sigterm.recv() => log::info!("received SIGTERM, starting graceful shutdown"),
            _ = sigquit.recv() => log::info!("received SIGQUIT, starting graceful shutdown"),
            _ = sighup.recv() => log::info!("received SIGHUP, starting graceful shutdown"),
        }
    }

    #[cfg(not(unix))]
    {
        wait_for_ctrl_c().await;
    }
}

async fn wait_for_ctrl_c() {
    log::debug!("shutdown signal handler waiting for Ctrl+C");
    if let Err(err) = tokio::signal::ctrl_c().await {
        log::warn!("shutdown signal handler failed: {}", err);
    }
    log::info!("shutdown signal received, starting graceful shutdown");
}

fn spawn_daemon_process(raw_args: &[OsString]) -> Result<u32> {
    let executable = std::env::current_exe().context("cannot resolve current executable")?;
    let forwarded_args = strip_daemon_flag(raw_args);

    let mut command = Command::new(executable);
    command
        .args(forwarded_args)
        .arg("--daemon-child")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = command.spawn().context("failed to spawn daemon child")?;
    Ok(child.id())
}

fn strip_daemon_flag(raw_args: &[OsString]) -> Vec<OsString> {
    raw_args
        .iter()
        .filter_map(|arg| {
            let value = arg.to_string_lossy();
            if value == "--daemon" || value.starts_with("--daemon=") || value == "--daemon-child" {
                None
            } else {
                Some(arg.clone())
            }
        })
        .collect()
}

fn log_plugin_initialization_summary(snapshots: &[CachedPluginSnapshot]) {
    let mut rows: Vec<(String, String, usize, String)> = snapshots
        .iter()
        .map(|snapshot| {
            let error = snapshot_error(snapshot);
            let status = if error.is_some() { "ERROR" } else { "OK" }.to_string();
            let detail = if let Some(message) = error {
                truncate_for_log(&message, 160)
            } else if let Some(plan) = &snapshot.plan {
                truncate_for_log(&format!("plan={}", plan), 160)
            } else {
                "ready".to_string()
            };

            (
                snapshot.provider_id.clone(),
                status,
                snapshot.lines.len(),
                detail,
            )
        })
        .collect();

    rows.sort_by(|a, b| a.0.cmp(&b.0));

    log::info!(
        "plugin initialization summary: total={} (one line per plugin follows)",
        rows.len()
    );
    for (plugin, status, lines, detail) in rows {
        log::info!(
            "plugin init: plugin={} status={} lines={} info={}",
            plugin,
            status,
            lines,
            detail
        );
    }
}

fn log_plugin_initialization_failure_summary(plugin_ids: &[String], error: &str) {
    let mut rows = plugin_ids.to_vec();
    rows.sort();

    let reason = truncate_for_log(error, 160);
    log::warn!(
        "plugin initialization summary: initial refresh failed before per-plugin results: {}",
        reason
    );
    for plugin in rows {
        log::warn!(
            "plugin init: plugin={} status=REFRESH_NOT_AVAILABLE info=initial refresh failed: {}",
            plugin,
            reason
        );
    }
}

fn snapshot_error(snapshot: &CachedPluginSnapshot) -> Option<String> {
    snapshot.lines.iter().find_map(|line| {
        if let MetricLine::Badge { label, text, .. } = line
            && label.eq_ignore_ascii_case("error")
        {
            return Some(text.clone());
        }
        None
    })
}

fn truncate_for_log(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let mut result = String::new();
    for ch in value.chars().take(max_chars) {
        result.push(ch);
    }

    if value.chars().count() > max_chars {
        result.push_str("...");
    }

    if result.is_empty() {
        "-".to_string()
    } else {
        result
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        let location = panic_info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let payload = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "non-string panic payload".to_string()
        };

        let backtrace = std::backtrace::Backtrace::force_capture();
        eprintln!(
            "FATAL: unhandled panic at {}: {}\nbacktrace:\n{}",
            location, payload, backtrace
        );
        log::error!(
            "FATAL: unhandled panic at {}: {}\nbacktrace:\n{}",
            location,
            payload,
            backtrace
        );
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_cli() -> Cli {
        Cli {
            host: None,
            port: None,
            plugins_dir: None,
            enabled_plugins: None,
            app_data_dir: None,
            plugin_overrides_dir: None,
            refresh_interval_secs: None,
            init_config: false,
            daemon: false,
            daemon_child: false,
        }
    }

    #[test]
    fn runtime_cli_uses_defaults_when_no_input_values() {
        let runtime = RuntimeCli::from_sources(empty_cli(), config::AppConfig::default());

        assert_eq!(runtime.host, config::DEFAULT_HOST);
        assert_eq!(runtime.port, config::DEFAULT_PORT);
        assert_eq!(
            runtime.refresh_interval_secs,
            config::DEFAULT_REFRESH_INTERVAL_SECS
        );
        assert_eq!(runtime.enabled_plugins, config::DEFAULT_ENABLED_PLUGINS);
        assert!(!runtime.daemon);
    }

    #[test]
    fn runtime_cli_uses_config_values_when_cli_is_empty() {
        let app_config = config::AppConfig {
            host: Some("0.0.0.0".to_string()),
            port: Some(9000),
            plugins_dir: Some(PathBuf::from("/tmp/plugins")),
            enabled_plugins: Some("codex,cur*".to_string()),
            app_data_dir: Some(PathBuf::from("/tmp/data")),
            plugin_overrides_dir: Some(PathBuf::from("/tmp/overrides")),
            refresh_interval_secs: Some(42),
            daemon: Some(true),
            proxy: None,
        };
        let runtime = RuntimeCli::from_sources(empty_cli(), app_config);

        assert_eq!(runtime.host, "0.0.0.0");
        assert_eq!(runtime.port, 9000);
        assert_eq!(runtime.plugins_dir, Some(PathBuf::from("/tmp/plugins")));
        assert_eq!(runtime.enabled_plugins, "codex,cur*");
        assert_eq!(runtime.app_data_dir, Some(PathBuf::from("/tmp/data")));
        assert_eq!(
            runtime.plugin_overrides_dir,
            Some(PathBuf::from("/tmp/overrides"))
        );
        assert_eq!(runtime.refresh_interval_secs, 42);
        assert!(runtime.daemon);
    }

    #[test]
    fn runtime_cli_prioritizes_cli_values_over_config() {
        let cli = Cli {
            host: Some("127.0.0.2".to_string()),
            port: Some(7001),
            plugins_dir: Some(PathBuf::from("/cli/plugins")),
            enabled_plugins: Some("mock".to_string()),
            app_data_dir: Some(PathBuf::from("/cli/data")),
            plugin_overrides_dir: Some(PathBuf::from("/cli/overrides")),
            refresh_interval_secs: Some(7),
            init_config: false,
            daemon: true,
            daemon_child: false,
        };
        let app_config = config::AppConfig {
            host: Some("0.0.0.0".to_string()),
            port: Some(9000),
            plugins_dir: Some(PathBuf::from("/cfg/plugins")),
            enabled_plugins: Some("codex".to_string()),
            app_data_dir: Some(PathBuf::from("/cfg/data")),
            plugin_overrides_dir: Some(PathBuf::from("/cfg/overrides")),
            refresh_interval_secs: Some(60),
            daemon: Some(false),
            proxy: None,
        };

        let runtime = RuntimeCli::from_sources(cli, app_config);

        assert_eq!(runtime.host, "127.0.0.2");
        assert_eq!(runtime.port, 7001);
        assert_eq!(runtime.plugins_dir, Some(PathBuf::from("/cli/plugins")));
        assert_eq!(runtime.enabled_plugins, "mock");
        assert_eq!(runtime.app_data_dir, Some(PathBuf::from("/cli/data")));
        assert_eq!(
            runtime.plugin_overrides_dir,
            Some(PathBuf::from("/cli/overrides"))
        );
        assert_eq!(runtime.refresh_interval_secs, 7);
        assert!(runtime.daemon);
    }

    #[test]
    fn enabled_plugins_matcher_supports_multiple_globs() {
        let matcher = EnabledPluginsMatcher::from_csv("codex,cur*").expect("matcher should parse");

        assert!(matcher.is_enabled("codex"));
        assert!(matcher.is_enabled("cursor"));
        assert!(!matcher.is_enabled("claude"));
    }

    #[test]
    fn enabled_plugins_matcher_rejects_empty_list() {
        let err = EnabledPluginsMatcher::from_csv(" , , ").expect_err("must reject empty list");
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn enabled_plugins_matcher_rejects_invalid_glob() {
        let err = EnabledPluginsMatcher::from_csv("[").expect_err("must reject invalid mask");
        assert!(err.to_string().contains("invalid enabled plugin glob mask"));
    }

    #[test]
    fn source_checkout_root_detects_cargo_target_layouts() {
        assert_eq!(
            source_checkout_root_from_exec_dir(Path::new("/repo/target/debug")),
            Some(PathBuf::from("/repo"))
        );
        assert_eq!(
            source_checkout_root_from_exec_dir(Path::new("/repo/target/release")),
            Some(PathBuf::from("/repo"))
        );
        assert_eq!(
            source_checkout_root_from_exec_dir(Path::new(
                "/repo/target/x86_64-unknown-linux-gnu/release"
            )),
            Some(PathBuf::from("/repo"))
        );
        assert_eq!(
            source_checkout_root_from_exec_dir(Path::new("/usr/bin")),
            None
        );
    }

    #[test]
    fn plugins_dir_candidates_prefer_source_checkout_paths() {
        let cwd = Path::new("/repo");
        let exec_dir = Path::new("/repo/target/debug");

        let candidates = plugins_dir_candidates(cwd, exec_dir);

        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/repo/vendor/openusage/plugins"),
                PathBuf::from("/repo/plugins"),
                PathBuf::from("/repo/target/debug/vendor/openusage/plugins"),
                PathBuf::from("/repo/target/debug/plugins"),
                PathBuf::from("/repo/target/share/openusage-cli/openusage-plugins"),
                PathBuf::from(SYSTEM_PLUGINS_DIR),
            ]
        );
    }

    #[test]
    fn plugins_dir_candidates_for_installed_binary_use_prefix_share() {
        let cwd = Path::new("/home/user");
        let exec_dir = Path::new("/opt/openusage-cli/bin");

        let candidates = plugins_dir_candidates(cwd, exec_dir);

        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/opt/openusage-cli/share/openusage-cli/openusage-plugins"),
                PathBuf::from(SYSTEM_PLUGINS_DIR),
            ]
        );
    }

    #[test]
    fn plugin_overrides_candidates_prefer_source_checkout_paths() {
        let cwd = Path::new("/repo");
        let exec_dir = Path::new("/repo/target/debug");

        let candidates = plugin_overrides_dir_candidates(cwd, exec_dir);

        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/repo/plugin-overrides"),
                PathBuf::from("/repo/target/debug/plugin-overrides"),
                PathBuf::from("/repo/target/share/openusage-cli/plugin-overrides"),
                PathBuf::from(SYSTEM_PLUGIN_OVERRIDES_DIR),
            ]
        );
    }

    #[test]
    fn plugin_overrides_candidates_for_installed_binary_use_prefix_share() {
        let cwd = Path::new("/home/user");
        let exec_dir = Path::new("/opt/openusage-cli/bin");

        let candidates = plugin_overrides_dir_candidates(cwd, exec_dir);

        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/opt/openusage-cli/share/openusage-cli/plugin-overrides"),
                PathBuf::from(SYSTEM_PLUGIN_OVERRIDES_DIR),
            ]
        );
    }
}
