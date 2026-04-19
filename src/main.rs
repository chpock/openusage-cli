use anyhow::{Context, Result, anyhow};
use clap::Parser;
use directories::ProjectDirs;
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

#[derive(Debug, Parser)]
#[command(name = "openusage-cli")]
#[command(about = "HTTP daemon for AI usage limit plugins")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long, default_value_t = 6736)]
    port: u16,

    #[arg(long, env = "OPENUSAGE_PLUGINS_DIR")]
    plugins_dir: Option<PathBuf>,

    #[arg(long, env = "OPENUSAGE_APP_DATA_DIR")]
    app_data_dir: Option<PathBuf>,

    #[arg(long, env = "OPENUSAGE_PLUGIN_OVERRIDES_DIR")]
    plugin_overrides_dir: Option<PathBuf>,

    #[arg(long, default_value_t = 300)]
    refresh_interval_secs: u64,

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
    let app_version = env!("CARGO_PKG_VERSION").to_string();

    log::info!("starting openusage-cli v{}", app_version);
    log::debug!(
        "startup options: host={}, port={}, refresh_interval_secs={}, daemon={}, daemon_child={}, plugins_dir={:?}, app_data_dir={:?}, plugin_overrides_dir={:?}",
        cli.host,
        cli.port,
        cli.refresh_interval_secs,
        cli.daemon,
        cli.daemon_child,
        cli.plugins_dir,
        cli.app_data_dir,
        cli.plugin_overrides_dir
    );

    if cli.daemon && !cli.daemon_child {
        let child_pid =
            spawn_daemon_process(&raw_args).context("failed to spawn background daemon process")?;
        log::info!("daemon mode enabled; spawned background process pid={child_pid}");
        return Ok(());
    }

    let app_data_dir =
        resolve_app_data_dir(cli.app_data_dir).context("failed to resolve app data directory")?;
    log::info!("using app data dir: {}", app_data_dir.display());
    std::fs::create_dir_all(&app_data_dir).with_context(|| {
        format!(
            "failed to create app data directory {}",
            app_data_dir.display()
        )
    })?;
    log::debug!("ensured app data directory exists");

    let plugins_dir =
        resolve_plugins_dir(cli.plugins_dir).context("failed to resolve plugins directory")?;
    log::info!("using plugins dir: {}", plugins_dir.display());
    let plugins = manifest::load_plugins_from_dir(&plugins_dir);
    if plugins.is_empty() {
        anyhow::bail!(
            "no plugins found in {} (expected vendor/openusage/plugins layout)",
            plugins_dir.display()
        );
    }

    log::info!(
        "loaded {} plugins from {}",
        plugins.len(),
        plugins_dir.display()
    );
    let plugin_ids: Vec<String> = plugins.iter().map(|p| p.manifest.id.clone()).collect();
    log::debug!("loaded plugin ids: {:?}", plugin_ids);

    let plugin_overrides_dir = resolve_plugin_overrides_dir(cli.plugin_overrides_dir)
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

    let refresh_task = if cli.refresh_interval_secs > 0 {
        let refresh_state = daemon.clone();
        let refresh_every = Duration::from_secs(cli.refresh_interval_secs);
        log::info!(
            "background refresh enabled (every {}s)",
            cli.refresh_interval_secs
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

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port)
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
    let candidates = [
        cwd.join("vendor/openusage/plugins"),
        cwd.join("plugins"),
        exec_dir.join("vendor/openusage/plugins"),
        exec_dir.join("plugins"),
    ];

    for candidate in candidates {
        log::debug!("checking plugins dir candidate {}", candidate.display());
        if candidate.exists() {
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
    let candidates = [
        cwd.join("plugin-overrides"),
        exec_dir.join("plugin-overrides"),
    ];

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
        if let MetricLine::Badge { label, text, .. } = line {
            if label.eq_ignore_ascii_case("error") {
                return Some(text.clone());
            }
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
