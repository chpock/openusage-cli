use anyhow::{Context, Result, anyhow};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use globset::{Glob, GlobSet, GlobSetBuilder};
use openusage_cli::config;
use openusage_cli::daemon::{CachedPluginSnapshot, DaemonState};
use openusage_cli::discovery::{PublishedDiscovery, discover_daemon_endpoint_with_override};
use openusage_cli::http_api::{self, ApiState, AvailablePlugins, LifecycleCommand, RuntimeConfig};
use openusage_cli::instance_control::{self, ExistingInstancePolicy, ServiceMode};
use openusage_cli::plugin_engine::manifest;
use openusage_cli::plugin_engine::runtime::MetricLine;
use openusage_cli::restart_watcher;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;

mod main_cli_preparse;
mod main_path_resolution;
mod main_systemd;

use main_cli_preparse::*;
use main_path_resolution::*;
use main_systemd::*;

const SYSTEM_PLUGINS_DIR: &str = "/usr/share/openusage-cli/openusage-plugins";
const SYSTEM_PLUGIN_OVERRIDES_DIR: &str = "/usr/share/openusage-cli/plugin-overrides";
const USER_SYSTEMD_SERVICE_NAME: &str = "openusage-cli.service";
const EXISTING_INSTANCE_SHUTDOWN_TIMEOUT_SECS: u64 = 15;
const QUERY_DAEMON_HTTP_TIMEOUT_SECS: u64 = 5;
const SYSTEMD_WATCHDOG_SEC: u64 = 30;
const SYSTEMD_TIMEOUT_START_SECS: u64 = 120;
const STARTUP_READINESS_TIMEOUT_SECS: u64 = 10;
const STARTUP_READINESS_POLL_INTERVAL_MS: u64 = 50;
const CMD_RUN_DAEMON: &str = "run-daemon";
const HELP_HEADING_MODE_OPTIONS: &str = "Mode options";
const HELP_HEADING_GLOBAL_OPTIONS: &str = "Global options";
const KNOWN_COMMANDS: &[&str] = &[
    CMD_RUN_DAEMON,
    "query",
    "show-default-config",
    "install-systemd-unit",
    "version",
    "help",
];
const APP_VERSION: &str = match option_env!("OPENUSAGE_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
const VALID_LOG_LEVELS: &str = "error, warn, info, debug, trace";

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "lower")]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
enum QueryType {
    #[value(alias = "data")]
    #[default]
    Usage,
    Plugins,
    Config,
}

impl QueryType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Plugins => "plugins",
            Self::Config => "config",
        }
    }

    fn endpoint_path(self) -> &'static str {
        match self {
            Self::Usage => "v1/usage",
            Self::Plugins => "v1/plugins",
            Self::Config => "v1/config",
        }
    }

    fn requires_local_refresh(self) -> bool {
        matches!(self, Self::Usage)
    }

    fn fallback_description(self) -> &'static str {
        match self {
            Self::Usage => "local plugin execution",
            Self::Plugins => "local plugins response generation",
            Self::Config => "local config response generation",
        }
    }
}

// NOTE: When adding new value-taking arguments here, also update
// `option_requires_separate_value` / `option_optionally_consumes_separate_value`
// to keep pre-parser positional detection in sync.
#[derive(Debug, Clone, Default, Args)]
struct SharedRuntimeArgs {
    /// Directory containing plugin JS files [default: auto-discover]
    #[arg(long)]
    plugins_dir: Option<PathBuf>,

    /// Comma-separated glob patterns for enabled plugin IDs [default: *]
    #[arg(long)]
    enabled_plugins: Option<String>,

    /// Directory for application data and cache [default: platform default]
    #[arg(long)]
    app_data_dir: Option<PathBuf>,

    /// Directory containing plugin override scripts [default: auto-discover]
    #[arg(long)]
    plugin_overrides_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Args)]
#[command(next_help_heading = HELP_HEADING_MODE_OPTIONS)]
struct QueryArgs {
    #[command(flatten)]
    shared: SharedRuntimeArgs,

    /// Query payload type [default: usage]
    #[arg(long = "type", value_enum, default_value_t = QueryType::Usage)]
    query_type: QueryType,

    /// Include query mode metadata in output [default: false]
    #[arg(long, default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    with_state: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryModeState {
    Cache,
    Direct,
}

impl QueryModeState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Direct => "direct",
        }
    }
}

#[derive(Debug, Clone, Default, Args)]
struct DaemonRuntimeArgs {
    /// HTTP host to bind to [default: 127.0.0.1]
    #[arg(long)]
    host: Option<String>,

    /// HTTP port to bind to (0 = random free port) [default: 0]
    #[arg(long)]
    port: Option<u16>,

    #[command(flatten)]
    shared: SharedRuntimeArgs,

    /// Background refresh interval in seconds (0 = disable) [default: 300]
    #[arg(long)]
    refresh_interval_secs: Option<u64>,
}

// NOTE: When adding new value-taking arguments here, also update
// `option_requires_separate_value` / `option_optionally_consumes_separate_value`
// to keep pre-parser positional detection in sync.
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = HELP_HEADING_MODE_OPTIONS)]
struct RunDaemonArgs {
    #[command(flatten)]
    runtime: DaemonRuntimeArgs,

    /// Run daemon in foreground mode (do not spawn background process) [default: false]
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    foreground: Option<bool>,

    /// Behavior when a daemon instance is already running [default: error]
    #[arg(long, value_enum)]
    existing_instance: Option<ExistingInstancePolicy>,

    /// Service manager mode for process lifecycle handling [default: standalone]
    #[arg(long, value_enum)]
    service_mode: Option<ServiceMode>,

    /// Internal flag used when spawning the background daemon child process
    #[arg(long, hide = true, default_value_t = false)]
    daemon_child: bool,
}

#[derive(Debug, Clone, Subcommand)]
enum ModeCommand {
    /// Start the HTTP daemon (background by default)
    #[command(name = CMD_RUN_DAEMON)]
    RunDaemon(RunDaemonArgs),

    /// Query one-shot JSON payload (`--type=usage|plugins|config`; default: usage)
    Query(QueryArgs),

    /// Print the default configuration template to stdout
    #[command(name = "show-default-config")]
    ShowDefaultConfig,

    /// Install a systemd user service unit for the daemon
    #[command(name = "install-systemd-unit")]
    InstallSystemdUnit,

    /// Print version information
    Version,
}

#[derive(Debug, Parser)]
#[command(name = "openusage-cli")]
#[command(about = "Local daemon and CLI for AI provider usage/quota via OpenUsage plugins")]
#[command(version = APP_VERSION)]
#[command(propagate_version = true)]
#[command(disable_help_flag = true)]
#[command(disable_version_flag = true)]
struct Cli {
    /// Set the logging level [default: error]
    #[arg(long, value_enum, global = true, help_heading = HELP_HEADING_GLOBAL_OPTIONS)]
    log_level: Option<LogLevel>,

    #[arg(
        short = 'h',
        long = "help",
        action = ArgAction::Help,
        global = true,
        help_heading = HELP_HEADING_GLOBAL_OPTIONS
    )]
    _help: Option<bool>,

    #[arg(
        short = 'V',
        long = "version",
        action = ArgAction::Version,
        global = true,
        help_heading = HELP_HEADING_GLOBAL_OPTIONS
    )]
    _version: Option<bool>,

    #[arg(long, hide = true, default_value_t = false, global = true)]
    test_mode: bool,

    #[command(subcommand)]
    command: Option<ModeCommand>,
}

#[tokio::main]
async fn main() -> Result<()> {
    install_panic_hook();

    // Parse CLI args first to handle early-exit commands and resolve log level
    let raw_args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let cli = parse_cli_with_default_mode(&raw_args);

    // Handle early-exit commands before logger setup
    if matches!(&cli.command, Some(ModeCommand::ShowDefaultConfig)) {
        print!("{}", config::default_config_template());
        return Ok(());
    }

    if matches!(&cli.command, Some(ModeCommand::InstallSystemdUnit)) {
        install_user_systemd_unit()?;
        return Ok(());
    }

    if matches!(&cli.command, Some(ModeCommand::Version)) {
        println!("{}", APP_VERSION);
        return Ok(());
    }

    // Resolve log level from CLI/config/default values (in that order of precedence)
    let log_level = resolve_log_level(&cli).context("failed to resolve log level")?;

    // Initialize logger with resolved log level
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level.as_str()))
        .init();

    let result = run(cli, &raw_args).await;
    match result {
        Ok(RunOutcome::Completed) => {
            log::info!("openusage-cli shutdown complete");
            Ok(())
        }
        Ok(RunOutcome::RestartSelf) => {
            log::info!("openusage-cli self-restart requested");
            restart_self_process(&raw_args).context("failed to restart openusage-cli process")
        }
        Err(err) => {
            log::error!("openusage-cli exiting with error: {err:#}");
            Err(err)
        }
    }
}

async fn run(cli: Cli, raw_args: &[OsString]) -> Result<RunOutcome> {
    let test_mode = cli.test_mode;
    let loaded_config = if test_mode {
        log::info!("test mode enabled; ignoring config file");
        None
    } else {
        config::load_config_if_exists().context("failed to load config")?
    };

    let runtime = match loaded_config {
        Some(loaded) => {
            log::info!("using config file: {}", loaded.path.display());
            RuntimeCli::from_sources(cli, loaded.config)
                .context("failed to resolve runtime options")?
        }
        None => {
            if !test_mode {
                let path = config::config_path().context("failed to resolve config path")?;
                log::info!(
                    "config file not found at {}; using CLI/config/default values",
                    path.display()
                );
            }
            RuntimeCli::from_sources(cli, config::AppConfig::default())
                .context("failed to resolve runtime options")?
        }
    };
    let app_version = APP_VERSION.to_string();

    log::info!("starting openusage-cli v{}", app_version);
    match &runtime {
        RuntimeCli::Query(query_runtime) => {
            log::debug!(
                "startup options: mode=query, type={}, with_state={}, test_mode={}, plugins_dir={:?}, enabled_plugins='{}', app_data_dir={:?}, plugin_overrides_dir={:?}, log_level={}",
                query_runtime.query_type.as_str(),
                query_runtime.with_state,
                query_runtime.shared.test_mode,
                query_runtime.shared.plugins_dir,
                query_runtime.shared.enabled_plugins,
                query_runtime.shared.app_data_dir,
                query_runtime.shared.plugin_overrides_dir,
                query_runtime.shared.log_level
            );
        }
        RuntimeCli::RunDaemon(daemon_runtime) => {
            log::debug!(
                "startup options: mode={}, foreground={}, host={}, port={}, refresh_interval_secs={}, existing_instance={}, service_mode={}, daemon_child={}, test_mode={}, plugins_dir={:?}, enabled_plugins='{}', app_data_dir={:?}, plugin_overrides_dir={:?}, log_level={}",
                CMD_RUN_DAEMON,
                daemon_runtime.foreground,
                daemon_runtime.host,
                daemon_runtime.port,
                daemon_runtime.refresh_interval_secs,
                daemon_runtime.existing_instance_policy,
                daemon_runtime.service_mode,
                daemon_runtime.daemon_child,
                daemon_runtime.shared.test_mode,
                daemon_runtime.shared.plugins_dir,
                daemon_runtime.shared.enabled_plugins,
                daemon_runtime.shared.app_data_dir,
                daemon_runtime.shared.plugin_overrides_dir,
                daemon_runtime.shared.log_level
            );
        }
    }

    match runtime {
        RuntimeCli::Query(query_runtime) => run_query_mode(query_runtime, &app_version).await,
        RuntimeCli::RunDaemon(daemon_runtime) => {
            run_daemon_mode(daemon_runtime, &app_version, raw_args).await
        }
    }
}

#[derive(Debug, Clone)]
struct RuntimeDirectories {
    app_data_dir: PathBuf,
    test_runtime_dir: Option<PathBuf>,
}

#[derive(Clone)]
struct InitializedRuntimeContext {
    daemon: Arc<DaemonState>,
    app_data_dir: PathBuf,
    plugins_dir: PathBuf,
    plugin_overrides_dir: Option<PathBuf>,
    active_plugin_ids: Vec<String>,
    inactive_plugin_ids: Vec<String>,
}

fn build_restart_watch_inputs(
    runtime: &DaemonRuntimeCli,
    initialized: &InitializedRuntimeContext,
) -> Result<restart_watcher::RestartWatchInputs> {
    let config_file = if runtime.shared.test_mode {
        None
    } else {
        Some(config::config_path().context("failed to resolve config file path")?)
    };
    let binary_file = std::env::current_exe().context("failed to resolve current executable")?;

    Ok(restart_watcher::RestartWatchInputs {
        plugins_dir: initialized.plugins_dir.clone(),
        plugin_overrides_dir: initialized.plugin_overrides_dir.clone(),
        config_file,
        binary_file,
    })
}

fn resolve_runtime_directories(shared: &SharedRuntimeCli) -> Result<RuntimeDirectories> {
    let app_data_dir = resolve_app_data_dir(shared.app_data_dir.clone(), shared.test_mode)
        .context("failed to resolve app data directory")?;
    let test_runtime_dir = shared
        .test_mode
        .then(|| app_data_dir.join(config::RUNTIME_DIR_NAME));

    Ok(RuntimeDirectories {
        app_data_dir,
        test_runtime_dir,
    })
}

async fn initialize_runtime_context(
    shared: &SharedRuntimeCli,
    app_version: &str,
    app_data_dir: PathBuf,
    perform_initial_refresh: bool,
) -> Result<InitializedRuntimeContext> {
    log::info!("using app data dir: {}", app_data_dir.display());
    std::fs::create_dir_all(&app_data_dir).with_context(|| {
        format!(
            "failed to create app data directory {}",
            app_data_dir.display()
        )
    })?;
    log::debug!("ensured app data directory exists");

    let plugins_dir = resolve_plugins_dir(shared.plugins_dir.clone(), shared.test_mode)
        .context("failed to resolve plugins directory")?;
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

    let enabled_plugins_matcher = EnabledPluginsMatcher::from_csv(&shared.enabled_plugins)
        .with_context(|| {
            format!(
                "invalid enabled_plugins value '{}' (expected comma-separated glob masks)",
                shared.enabled_plugins
            )
        })?;
    let total_loaded_plugins = loaded_plugins.len();
    let mut active_plugin_ids = Vec::new();
    let mut inactive_plugin_ids = Vec::new();
    let plugins: Vec<_> = loaded_plugins
        .into_iter()
        .filter_map(|plugin| {
            let plugin_id = plugin.manifest.id.clone();
            if enabled_plugins_matcher.is_enabled(&plugin_id) {
                active_plugin_ids.push(plugin_id);
                Some(plugin)
            } else {
                inactive_plugin_ids.push(plugin_id);
                None
            }
        })
        .collect();

    if plugins.is_empty() {
        anyhow::bail!(
            "no plugins enabled after applying enabled_plugins='{}'. loaded plugin ids: {:?}",
            shared.enabled_plugins,
            loaded_plugin_ids
        );
    }

    let plugin_ids: Vec<String> = plugins.iter().map(|p| p.manifest.id.clone()).collect();
    log::info!(
        "enabled {} of {} plugins (enabled_plugins='{}')",
        plugins.len(),
        total_loaded_plugins,
        shared.enabled_plugins
    );
    log::debug!("enabled plugin ids: {:?}", plugin_ids);

    let plugin_overrides_dir =
        resolve_plugin_overrides_dir(shared.plugin_overrides_dir.clone(), shared.test_mode)
            .context("failed to resolve plugin overrides directory")?;
    if let Some(path) = &plugin_overrides_dir {
        log::info!("using plugin overrides dir: {}", path.display());
    } else {
        log::info!("plugin overrides disabled (no overrides dir found)");
    }

    let daemon = Arc::new(DaemonState::new(
        plugins,
        app_data_dir.clone(),
        app_version.to_string(),
        plugin_overrides_dir.clone(),
    ));

    if perform_initial_refresh {
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
    } else {
        log::debug!("initial plugin refresh skipped for query type without runtime probe");
    }

    Ok(InitializedRuntimeContext {
        daemon,
        app_data_dir,
        plugins_dir,
        plugin_overrides_dir,
        active_plugin_ids,
        inactive_plugin_ids,
    })
}

async fn run_query_mode(runtime: QueryRuntimeCli, app_version: &str) -> Result<RunOutcome> {
    let dirs = resolve_runtime_directories(&runtime.shared)?;
    log::debug!("query mode enabled; attempting to discover running daemon");

    if let Some(daemon_base_url) =
        discover_daemon_endpoint_with_override(dirs.test_runtime_dir.as_deref())
    {
        log::info!(
            "discovered daemon endpoint at {}; querying for {}",
            daemon_base_url,
            runtime.query_type.as_str()
        );
        match query_daemon_via_http(&daemon_base_url, runtime.query_type).await {
            Ok(json_output) => {
                print_query_output(&json_output, runtime.with_state, QueryModeState::Cache)?;
                return Ok(RunOutcome::Completed);
            }
            Err(err) => {
                log::warn!(
                    "failed to query running daemon: {}; falling back to {}",
                    err,
                    runtime.query_type.fallback_description()
                );
            }
        }
    } else {
        log::info!(
            "no running daemon discovered; falling back to {}",
            runtime.query_type.fallback_description()
        );
    }

    let initialized = initialize_runtime_context(
        &runtime.shared,
        app_version,
        dirs.app_data_dir.clone(),
        runtime.query_type.requires_local_refresh(),
    )
    .await?;
    let runtime_config_state =
        RuntimeConfigState::from_query_runtime(app_version, &runtime, &initialized);
    let json_output = build_local_query_output(
        initialized.daemon.as_ref(),
        runtime.query_type,
        &runtime_config_state,
    )
    .await
    .context("failed to build local query response")?;
    print_query_output(&json_output, runtime.with_state, QueryModeState::Direct)?;

    Ok(RunOutcome::Completed)
}

#[derive(Debug, Clone)]
struct RuntimeConfigState {
    app_version: String,
    host: String,
    port: u16,
    service_mode: ServiceMode,
    existing_instance_policy: ExistingInstancePolicy,
    plugins_dir: PathBuf,
    enabled_plugins: String,
    active_plugin_ids: Vec<String>,
    inactive_plugin_ids: Vec<String>,
    app_data_dir: PathBuf,
    plugin_overrides_dir: Option<PathBuf>,
    refresh_interval_secs: u64,
    log_level: String,
}

impl RuntimeConfigState {
    fn enabled_plugins_list(&self) -> Vec<String> {
        self.enabled_plugins
            .split(',')
            .map(str::trim)
            .filter(|mask| !mask.is_empty())
            .map(ToString::to_string)
            .collect()
    }

    fn from_daemon_runtime(
        app_version: &str,
        runtime: &DaemonRuntimeCli,
        initialized: &InitializedRuntimeContext,
        bound_port: u16,
    ) -> Self {
        Self {
            app_version: app_version.to_string(),
            host: runtime.host.clone(),
            port: bound_port,
            service_mode: runtime.service_mode,
            existing_instance_policy: runtime.existing_instance_policy,
            plugins_dir: initialized.plugins_dir.clone(),
            enabled_plugins: runtime.shared.enabled_plugins.clone(),
            active_plugin_ids: initialized.active_plugin_ids.clone(),
            inactive_plugin_ids: initialized.inactive_plugin_ids.clone(),
            app_data_dir: initialized.app_data_dir.clone(),
            plugin_overrides_dir: initialized.plugin_overrides_dir.clone(),
            refresh_interval_secs: runtime.refresh_interval_secs,
            log_level: runtime.shared.log_level.clone(),
        }
    }

    fn from_query_runtime(
        app_version: &str,
        runtime: &QueryRuntimeCli,
        initialized: &InitializedRuntimeContext,
    ) -> Self {
        Self {
            app_version: app_version.to_string(),
            host: runtime.host.clone(),
            port: runtime.port,
            service_mode: runtime.service_mode,
            existing_instance_policy: runtime.existing_instance_policy,
            plugins_dir: initialized.plugins_dir.clone(),
            enabled_plugins: runtime.shared.enabled_plugins.clone(),
            active_plugin_ids: initialized.active_plugin_ids.clone(),
            inactive_plugin_ids: initialized.inactive_plugin_ids.clone(),
            app_data_dir: initialized.app_data_dir.clone(),
            plugin_overrides_dir: initialized.plugin_overrides_dir.clone(),
            refresh_interval_secs: runtime.refresh_interval_secs,
            log_level: runtime.shared.log_level.clone(),
        }
    }

    fn to_api_response(&self) -> RuntimeConfig {
        RuntimeConfig {
            app_version: self.app_version.clone(),
            host: self.host.clone(),
            port: self.port,
            service_mode: self.service_mode.to_string(),
            existing_instance_policy: self.existing_instance_policy.to_string(),
            plugins_dir: Some(self.plugins_dir.clone()),
            enabled_plugins: self.enabled_plugins_list(),
            available_plugins: AvailablePlugins {
                active: self.active_plugin_ids.clone(),
                inactive: self.inactive_plugin_ids.clone(),
            },
            app_data_dir: Some(self.app_data_dir.clone()),
            plugin_overrides_dir: self.plugin_overrides_dir.clone(),
            refresh_interval_secs: self.refresh_interval_secs,
            log_level: self.log_level.clone(),
        }
    }
}

async fn run_daemon_mode(
    runtime: DaemonRuntimeCli,
    app_version: &str,
    raw_args: &[OsString],
) -> Result<RunOutcome> {
    let dirs = resolve_runtime_directories(&runtime.shared)?;

    if should_spawn_daemon_parent(&runtime) {
        if let Some(running_instance) =
            instance_control::discover_running_instance(dirs.test_runtime_dir.as_deref()).await
        {
            match runtime.existing_instance_policy {
                ExistingInstancePolicy::Error => {
                    anyhow::bail!(
                        "a running daemon instance is already discovered at {} (service_mode={}). use --existing-instance=replace to replace it or --existing-instance=ignore to run without discovery registration",
                        running_instance.base_url,
                        running_instance.service_mode
                    );
                }
                ExistingInstancePolicy::Ignore => {
                    log::info!(
                        "running daemon already discovered at {} (service_mode={}); ignoring because --existing-instance=ignore",
                        running_instance.base_url,
                        running_instance.service_mode
                    );
                }
                ExistingInstancePolicy::Replace => match running_instance.service_mode {
                    ServiceMode::Systemd => {
                        log::info!(
                            "requesting restart of existing systemd-managed instance at {}",
                            running_instance.base_url
                        );
                        instance_control::request_restart(&running_instance.base_url)
                            .await
                            .context("failed to request restart for systemd-managed instance")?;
                        println!(
                            "Existing systemd-managed daemon at {} received restart request and will self-restart.",
                            running_instance.base_url
                        );
                        return Ok(RunOutcome::Completed);
                    }
                    ServiceMode::Standalone => {
                        log::info!(
                            "replacing existing standalone instance at {}",
                            running_instance.base_url
                        );
                        instance_control::request_shutdown(&running_instance.base_url)
                            .await
                            .context(
                                "failed to request shutdown for existing standalone instance",
                            )?;
                        instance_control::wait_until_unreachable(
                            &running_instance.base_url,
                            Duration::from_secs(EXISTING_INSTANCE_SHUTDOWN_TIMEOUT_SECS),
                        )
                        .await
                        .context("existing standalone instance did not stop in time")?;
                        log::info!(
                            "existing standalone instance stopped; continuing daemon startup"
                        );
                    }
                },
            }
        }

        let child_pid =
            spawn_daemon_process(raw_args).context("failed to spawn background daemon process")?;
        log::info!("run-daemon enabled; spawned background process pid={child_pid}");
        return Ok(RunOutcome::Completed);
    }

    notify_systemd_status(
        runtime.service_mode,
        "initializing runtime context (plugins and cache)",
    );
    let initialized = initialize_runtime_context(
        &runtime.shared,
        app_version,
        dirs.app_data_dir.clone(),
        true,
    )
    .await?;
    let daemon = initialized.daemon.clone();

    const RESET_CHECK_MARGIN_SECS: u64 = 5;
    const RESET_RETRY_DELAY_SECS: u64 = 30;
    const MAX_RETRY_AGE_SECS: u64 = 300; // Stop retrying after 5 minutes

    let refresh_task = if runtime.refresh_interval_secs > 0 {
        let refresh_state = daemon.clone();
        let refresh_every = Duration::from_secs(runtime.refresh_interval_secs);
        log::info!(
            "background refresh enabled (every {}s, reset-aware)",
            runtime.refresh_interval_secs
        );
        Some(tokio::spawn(async move {
            log::debug!("background refresh task started (reset-aware)");
            let mut interval_timer = create_refresh_interval(refresh_every).await;

            loop {
                let mut proactive_trigger_time: Option<tokio::time::Instant> = None;
                let next_reset = refresh_state
                    .next_reset_with_delay(RESET_CHECK_MARGIN_SECS)
                    .await;

                if let Some((earliest_resets_at, delay)) = next_reset {
                    log::info!(
                        "earliest resetsAt={} observed; aggressive mode will trigger in {}s (margin {}s)",
                        earliest_resets_at,
                        delay.as_secs(),
                        RESET_CHECK_MARGIN_SECS,
                    );

                    tokio::select! {
                        _ = interval_timer.tick() => {
                            log::debug!("running scheduled interval refresh");
                        }
                        _ = tokio::time::sleep(delay) => {
                            log::info!("limit reset time reached (margin {}s), refreshing early", RESET_CHECK_MARGIN_SECS);
                            proactive_trigger_time = Some(tokio::time::Instant::now());
                        }
                    }
                } else {
                    if refresh_state.has_past_resets(RESET_CHECK_MARGIN_SECS).await {
                        log::info!(
                            "no upcoming resetsAt found; cached data already contains past reset times"
                        );
                    } else {
                        log::info!(
                            "no upcoming resetsAt found; aggressive mode will not be proactively armed"
                        );
                    }
                    interval_timer.tick().await;
                    log::debug!("running scheduled interval refresh (no upcoming resets)");
                }

                if let Err(err) = refresh_state.refresh(None).await {
                    log::warn!("background refresh failed: {}", err);
                } else {
                    log::debug!("background refresh completed");
                }

                let _ = run_past_reset_retry_loop(
                    &refresh_state,
                    proactive_trigger_time,
                    RESET_CHECK_MARGIN_SECS,
                    Duration::from_secs(RESET_RETRY_DELAY_SECS),
                    Duration::from_secs(MAX_RETRY_AGE_SECS),
                )
                .await;
            }
        }))
    } else {
        log::info!("background refresh disabled (refresh_interval_secs=0)");
        None
    };

    let addr: SocketAddr = format!("{}:{}", runtime.host, runtime.port)
        .parse()
        .context("invalid bind address")?;
    notify_systemd_status(runtime.service_mode, "binding HTTP listener");
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
    let bound_addr = listener
        .local_addr()
        .context("failed to resolve bound HTTP listener address")?;
    log::debug!("HTTP listener successfully bound on {}", bound_addr);
    notify_systemd_status(
        runtime.service_mode,
        &format!("HTTP listener bound on {}; publishing endpoint", bound_addr),
    );

    let discovery = if runtime.existing_instance_policy == ExistingInstancePolicy::Ignore {
        log::info!("skipping daemon endpoint publication because --existing-instance=ignore");
        None
    } else {
        let discovery = PublishedDiscovery::publish(bound_addr, dirs.test_runtime_dir.as_deref())
            .context("failed to publish daemon endpoint")?;
        log::info!(
            "published daemon endpoint file: {}",
            discovery.endpoint_file().display()
        );
        Some(discovery)
    };

    const LIFECYCLE_NONE: u8 = 0;
    const LIFECYCLE_SHUTDOWN: u8 = 1;
    const LIFECYCLE_RESTART: u8 = 2;

    let (lifecycle_tx, lifecycle_rx) = tokio::sync::oneshot::channel::<LifecycleCommand>();
    let lifecycle_tx = Arc::new(tokio::sync::Mutex::new(Some(lifecycle_tx)));
    let lifecycle_reason = Arc::new(AtomicU8::new(LIFECYCLE_NONE));

    let runtime_config = RuntimeConfigState::from_daemon_runtime(
        app_version,
        &runtime,
        &initialized,
        bound_addr.port(),
    )
    .to_api_response();

    let app = http_api::router(ApiState {
        daemon,
        app_version: app_version.to_string(),
        config: runtime_config,
        lifecycle_tx: Some(Arc::clone(&lifecycle_tx)),
    });
    log::debug!("HTTP router initialized");

    if let Some(discovery) = &discovery {
        log::info!("openusage-cli daemon listening on {}", discovery.base_url());
    } else {
        log::info!("openusage-cli daemon listening on http://{}", bound_addr);
    }
    notify_systemd_status(runtime.service_mode, "starting HTTP server");
    let lifecycle_reason_for_server = Arc::clone(&lifecycle_reason);
    let server_task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown({
            let lifecycle_reason = Arc::clone(&lifecycle_reason_for_server);
            async move {
                tokio::select! {
                    _ = shutdown_signal() => {},
                    command = lifecycle_rx => {
                        match command {
                            Ok(LifecycleCommand::Shutdown) => {
                                lifecycle_reason.store(LIFECYCLE_SHUTDOWN, Ordering::Relaxed);
                                log::info!("shutdown triggered via HTTP API");
                            }
                            Ok(LifecycleCommand::Restart) => {
                                lifecycle_reason.store(LIFECYCLE_RESTART, Ordering::Relaxed);
                                log::info!("restart triggered via HTTP API");
                            }
                            Err(_) => {
                                log::warn!("lifecycle control channel closed before command was received");
                            }
                        }
                    }
                }
            }
        })
        .await
        .context("http server failed")
    });

    if let Err(err) = wait_for_http_server_readiness(bound_addr).await {
        server_task.abort();
        let _ = server_task.await;
        return Err(err).context("HTTP server did not become ready in time");
    }
    notify_systemd_status(
        runtime.service_mode,
        &format!("ready to serve requests on http://{}", bound_addr),
    );
    notify_systemd_ready(runtime.service_mode)
        .context("failed to notify systemd that daemon startup completed")?;
    let watchdog_task = spawn_systemd_watchdog_task(runtime.service_mode);

    let restart_watcher_task = match build_restart_watch_inputs(&runtime, &initialized).and_then(
        |inputs| restart_watcher::spawn_restart_watcher(inputs, Arc::clone(&lifecycle_tx)),
    ) {
        Ok(task) => Some(task),
        Err(err) => {
            log::warn!(
                "filesystem restart watcher is unavailable; continuing without auto-restart-on-change: {}",
                err
            );
            None
        }
    };

    log::debug!("waiting for shutdown signal");

    let server_result = server_task
        .await
        .context("http server task failed to join")?;

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

    if let Some(task) = watchdog_task {
        task.abort();
        match task.await {
            Ok(()) => log::debug!("systemd watchdog task exited"),
            Err(err) if err.is_cancelled() => {
                log::debug!("systemd watchdog task cancelled during shutdown")
            }
            Err(err) => log::warn!("systemd watchdog task ended with error: {}", err),
        }
    }

    if let Some(task) = restart_watcher_task {
        task.abort();
        match task.await {
            Ok(()) => log::debug!("filesystem restart watcher task exited"),
            Err(err) if err.is_cancelled() => {
                log::debug!("filesystem restart watcher task cancelled during shutdown")
            }
            Err(err) => log::warn!("filesystem restart watcher task ended with error: {}", err),
        }
    }

    server_result?;

    notify_systemd_status(runtime.service_mode, "stopping daemon");
    log::info!("HTTP server stopped");
    notify_systemd_stopping(runtime.service_mode);
    if lifecycle_reason.load(Ordering::Relaxed) == LIFECYCLE_RESTART {
        let outcome = restart_outcome_for_service_mode(runtime.service_mode);
        if outcome == RunOutcome::RestartSelf {
            log::info!(
                "restart requested in {} mode; restarting current daemon process",
                runtime.service_mode
            );
        }
        return Ok(outcome);
    }

    Ok(RunOutcome::Completed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOutcome {
    Completed,
    RestartSelf,
}

fn restart_outcome_for_service_mode(_service_mode: ServiceMode) -> RunOutcome {
    RunOutcome::RestartSelf
}

#[derive(Debug, Clone)]
struct SharedRuntimeCli {
    plugins_dir: Option<PathBuf>,
    enabled_plugins: String,
    app_data_dir: Option<PathBuf>,
    plugin_overrides_dir: Option<PathBuf>,
    test_mode: bool,
    log_level: String,
}

#[derive(Debug, Clone)]
struct QueryRuntimeCli {
    shared: SharedRuntimeCli,
    query_type: QueryType,
    with_state: bool,
    host: String,
    port: u16,
    refresh_interval_secs: u64,
    existing_instance_policy: ExistingInstancePolicy,
    service_mode: ServiceMode,
}

#[derive(Debug, Clone)]
struct DaemonRuntimeCli {
    shared: SharedRuntimeCli,
    foreground: bool,
    host: String,
    port: u16,
    refresh_interval_secs: u64,
    existing_instance_policy: ExistingInstancePolicy,
    service_mode: ServiceMode,
    daemon_child: bool,
}

#[derive(Debug, Clone)]
enum RuntimeCli {
    Query(QueryRuntimeCli),
    RunDaemon(DaemonRuntimeCli),
}

impl RuntimeCli {
    fn from_sources(cli: Cli, config: config::AppConfig) -> Result<Self> {
        let Cli {
            log_level: cli_log_level,
            _help: _,
            _version: _,
            test_mode,
            command,
        } = cli;
        let raw_log_level = cli_log_level
            .map(|level| level.as_str().to_string())
            .or(config.log_level.clone())
            .unwrap_or_else(|| config::DEFAULT_LOG_LEVEL.to_string());
        let log_level = normalize_log_level(raw_log_level)?;
        let query_host = config
            .host
            .clone()
            .unwrap_or_else(|| config::DEFAULT_HOST.to_string());
        let query_port = config.port.unwrap_or(config::DEFAULT_PORT);
        let query_refresh_interval_secs = config
            .refresh_interval_secs
            .unwrap_or(config::DEFAULT_REFRESH_INTERVAL_SECS);
        let query_existing_instance_policy = resolve_query_existing_instance_policy(&config);

        match command {
            Some(ModeCommand::Query(args)) => Ok(Self::Query(QueryRuntimeCli {
                shared: resolve_shared_runtime(args.shared, test_mode, log_level, &config),
                query_type: args.query_type,
                with_state: args.with_state,
                host: query_host,
                port: query_port,
                refresh_interval_secs: query_refresh_interval_secs,
                existing_instance_policy: query_existing_instance_policy,
                service_mode: ServiceMode::Standalone,
            })),
            Some(ModeCommand::RunDaemon(args)) => {
                let runtime_args = args.runtime;
                let shared =
                    resolve_shared_runtime(runtime_args.shared, test_mode, log_level, &config);
                let host = runtime_args
                    .host
                    .or(config.host)
                    .unwrap_or_else(|| config::DEFAULT_HOST.to_string());
                let port = runtime_args
                    .port
                    .or(config.port)
                    .unwrap_or(config::DEFAULT_PORT);
                let refresh_interval_secs = runtime_args
                    .refresh_interval_secs
                    .or(config.refresh_interval_secs)
                    .unwrap_or(config::DEFAULT_REFRESH_INTERVAL_SECS);
                let config_existing_instance_policy = match config.existing_instance {
                    Some(value) => {
                        Some(ExistingInstancePolicy::parse(&value).with_context(|| {
                            format!("invalid existing_instance value '{}'", value)
                        })?)
                    }
                    None => None,
                };
                let existing_instance_policy = args
                    .existing_instance
                    .or(config_existing_instance_policy)
                    .unwrap_or(ExistingInstancePolicy::Error);
                let foreground = args.foreground.or(config.foreground).unwrap_or(false);
                let service_mode = args.service_mode.unwrap_or(ServiceMode::Standalone);

                Ok(Self::RunDaemon(DaemonRuntimeCli {
                    shared,
                    foreground,
                    host,
                    port,
                    refresh_interval_secs,
                    existing_instance_policy,
                    service_mode,
                    daemon_child: args.daemon_child,
                }))
            }
            None => {
                let default_query_args = QueryArgs::default();
                Ok(Self::Query(QueryRuntimeCli {
                    shared: resolve_shared_runtime(
                        default_query_args.shared,
                        test_mode,
                        log_level,
                        &config,
                    ),
                    query_type: default_query_args.query_type,
                    with_state: default_query_args.with_state,
                    host: query_host,
                    port: query_port,
                    refresh_interval_secs: query_refresh_interval_secs,
                    existing_instance_policy: query_existing_instance_policy,
                    service_mode: ServiceMode::Standalone,
                }))
            }
            Some(
                ModeCommand::ShowDefaultConfig
                | ModeCommand::InstallSystemdUnit
                | ModeCommand::Version,
            ) => {
                anyhow::bail!("internal error: non-runtime command reached runtime option resolver")
            }
        }
    }
}

fn resolve_shared_runtime(
    mode_args: SharedRuntimeArgs,
    test_mode: bool,
    log_level: String,
    config: &config::AppConfig,
) -> SharedRuntimeCli {
    let enabled_plugins = mode_args
        .enabled_plugins
        .or_else(|| config.enabled_plugins.clone().map(|masks| masks.join(",")))
        .unwrap_or_else(|| config::DEFAULT_ENABLED_PLUGINS.to_string());

    SharedRuntimeCli {
        plugins_dir: mode_args.plugins_dir.or(config.plugins_dir.clone()),
        enabled_plugins,
        app_data_dir: mode_args.app_data_dir.or(config.app_data_dir.clone()),
        plugin_overrides_dir: mode_args
            .plugin_overrides_dir
            .or(config.plugin_overrides_dir.clone()),
        test_mode,
        log_level,
    }
}

fn resolve_query_existing_instance_policy(config: &config::AppConfig) -> ExistingInstancePolicy {
    let Some(raw_value) = config.existing_instance.as_deref() else {
        return ExistingInstancePolicy::Error;
    };

    ExistingInstancePolicy::parse(raw_value).unwrap_or(ExistingInstancePolicy::Error)
}

fn resolve_log_level(cli: &Cli) -> Result<String> {
    let config_log_level = if cli.test_mode {
        None
    } else {
        config::load_config_if_exists()?.and_then(|loaded| loaded.config.log_level)
    };

    let raw_log_level = cli
        .log_level
        .map(|level| level.as_str().to_string())
        .or(config_log_level)
        .unwrap_or_else(|| config::DEFAULT_LOG_LEVEL.to_string());

    normalize_log_level(raw_log_level)
}

fn normalize_log_level(log_level: String) -> Result<String> {
    let normalized = log_level.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "error" | "warn" | "info" | "debug" | "trace" => Ok(normalized),
        _ => anyhow::bail!(
            "invalid log level '{}'; expected one of: {}",
            log_level,
            VALID_LOG_LEVELS
        ),
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

async fn create_refresh_interval(refresh_every: Duration) -> tokio::time::Interval {
    let mut interval_timer = tokio::time::interval(refresh_every);
    interval_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval_timer.tick().await;
    interval_timer
}

async fn run_past_reset_retry_loop(
    refresh_state: &DaemonState,
    proactive_trigger_time: Option<tokio::time::Instant>,
    reset_check_margin_secs: u64,
    reset_retry_delay: Duration,
    max_retry_age: Duration,
) -> usize {
    let aggressive_mode_started_at =
        proactive_trigger_time.unwrap_or_else(tokio::time::Instant::now);
    let proactive_triggered = proactive_trigger_time.is_some();

    let mut retry_attempts = 0usize;
    let mut aggressive_mode_active = false;
    let mut retry_window_expired = false;

    while refresh_state.has_past_resets(reset_check_margin_secs).await {
        if !aggressive_mode_active {
            aggressive_mode_active = true;
            if proactive_triggered {
                log::info!("aggressive refresh mode started: proactive reset trigger detected");
            } else {
                log::info!(
                    "aggressive refresh mode started: past reset times detected in cached data"
                );
            }
        }

        if aggressive_mode_started_at.elapsed() >= max_retry_age {
            retry_window_expired = true;
            log::info!(
                "retry window expired (>{}s), returning to normal interval",
                max_retry_age.as_secs()
            );
            break;
        }

        log::info!(
            "provider data still shows past reset times, retrying in {}s",
            reset_retry_delay.as_secs()
        );
        tokio::time::sleep(reset_retry_delay).await;

        if let Err(err) = refresh_state.refresh(None).await {
            log::warn!("retry refresh failed: {}", err);
        } else {
            log::debug!("retry refresh completed");
        }

        retry_attempts += 1;
    }

    if aggressive_mode_active {
        if retry_window_expired {
            log::info!(
                "aggressive refresh mode ended: retry window expired after {} attempt(s)",
                retry_attempts
            );
        } else {
            log::info!(
                "aggressive refresh mode ended: stale reset timestamps cleared after {} attempt(s)",
                retry_attempts
            );
        }
    } else if proactive_triggered {
        log::info!(
            "aggressive refresh mode not started: no past reset times detected after proactive refresh"
        );
    } else {
        log::info!(
            "aggressive refresh mode not started: no past reset times detected after scheduled refresh"
        );
    }

    retry_attempts
}

fn should_spawn_daemon_parent(runtime: &DaemonRuntimeCli) -> bool {
    !runtime.daemon_child && !runtime.foreground
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
    let forwarded_args = strip_flags_for_daemon_child(raw_args);

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

#[cfg(unix)]
fn restart_self_process(raw_args: &[OsString]) -> Result<()> {
    log::info!(
        "restarting using '{}' strategy (replace current process via exec)",
        self_restart_strategy_tag()
    );
    exec_replacement_process(raw_args)?;
    Ok(())
}

#[cfg(not(unix))]
fn restart_self_process(raw_args: &[OsString]) -> Result<()> {
    log::info!(
        "restarting using '{}' strategy (spawn replacement process)",
        self_restart_strategy_tag()
    );
    let replacement_pid = spawn_replacement_process(raw_args)
        .context("failed to spawn replacement process for self-restart")?;
    log::info!(
        "replacement process started (pid={}); exiting current process",
        replacement_pid
    );
    Ok(())
}

#[cfg(unix)]
fn exec_replacement_process(raw_args: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let executable = std::env::current_exe().context("cannot resolve current executable")?;
    let err = Command::new(executable).args(raw_args).exec();
    Err(anyhow!("failed to exec replacement process: {}", err))
}

fn self_restart_strategy_tag() -> &'static str {
    cfg!(unix).then_some("exec").unwrap_or("spawn")
}

#[cfg(not(unix))]
fn spawn_replacement_process(raw_args: &[OsString]) -> Result<u32> {
    let executable = std::env::current_exe().context("cannot resolve current executable")?;
    let child = Command::new(executable)
        .args(raw_args)
        .spawn()
        .context("failed to spawn replacement process")?;
    Ok(child.id())
}

fn strip_flags_for_daemon_child(raw_args: &[OsString]) -> Vec<OsString> {
    raw_args
        .iter()
        .filter_map(|arg| {
            let value = arg.to_string_lossy();
            if value == "--daemon-child" {
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

async fn build_local_query_output(
    daemon: &DaemonState,
    query_type: QueryType,
    runtime_config_state: &RuntimeConfigState,
) -> Result<String> {
    match query_type {
        QueryType::Usage => {
            let snapshots = daemon.cached(None).await;
            serde_json::to_string(&snapshots).context("failed to serialize local usage query")
        }
        QueryType::Plugins => {
            let plugins = daemon.plugins_meta();
            serde_json::to_string(&plugins).context("failed to serialize local plugins query")
        }
        QueryType::Config => {
            let runtime_config = runtime_config_state.to_api_response();
            serde_json::to_string(&runtime_config).context("failed to serialize local config query")
        }
    }
}

fn print_query_output(payload: &str, with_state: bool, query_mode: QueryModeState) -> Result<()> {
    if !with_state {
        println!("{}", payload);
        return Ok(());
    }

    let data: serde_json::Value = serde_json::from_str(payload)
        .context("failed to parse query payload for --with-state output")?;
    let output = serde_json::json!({
        "data": data,
        "state": {
            "queryMode": query_mode.as_str(),
        },
    });
    let serialized = serde_json::to_string(&output)
        .context("failed to serialize query payload with --with-state")?;
    println!("{}", serialized);

    Ok(())
}

/// Queries a running daemon via HTTP and returns validated JSON payload.
async fn query_daemon_via_http(base_url: &str, query_type: QueryType) -> Result<String> {
    let url = format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        query_type.endpoint_path()
    );
    log::debug!("querying daemon at {}", url);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(QUERY_DAEMON_HTTP_TIMEOUT_SECS))
        .build()
        .context("failed to create HTTP client")?;

    let response = client
        .get(&url)
        .send()
        .await
        .context("failed to connect to daemon")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("daemon returned error: {} - {}", status, body);
    }

    let body = response
        .text()
        .await
        .context("failed to read daemon response")?;

    let payload: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("daemon returned non-JSON {} payload", query_type.as_str()))?;

    match query_type {
        QueryType::Usage | QueryType::Plugins => {
            let entries = payload.as_array().with_context(|| {
                format!(
                    "daemon returned unexpected {} payload shape (expected JSON array)",
                    query_type.as_str()
                )
            })?;
            if !entries.iter().all(serde_json::Value::is_object) {
                anyhow::bail!(
                    "daemon returned unexpected {} payload shape (array entries must be objects)",
                    query_type.as_str()
                );
            }
        }
        QueryType::Config => {
            if !payload.is_object() {
                anyhow::bail!(
                    "daemon returned unexpected {} payload shape (expected JSON object)",
                    query_type.as_str()
                );
            }
        }
    }

    Ok(body)
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
    use std::ffi::OsStr;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, Once};
    use tokio::sync::Mutex as AsyncMutex;

    struct TestLogCollector {
        capture_enabled: AtomicBool,
        entries: Mutex<Vec<String>>,
    }

    impl TestLogCollector {
        const fn new() -> Self {
            Self {
                capture_enabled: AtomicBool::new(false),
                entries: Mutex::new(Vec::new()),
            }
        }

        fn start_capture(&self) {
            let mut entries = self
                .entries
                .lock()
                .expect("test log collector mutex should not be poisoned");
            entries.clear();
            drop(entries);
            self.capture_enabled.store(true, Ordering::Relaxed);
        }

        fn stop_capture(&self) -> Vec<String> {
            self.capture_enabled.store(false, Ordering::Relaxed);
            self.entries
                .lock()
                .expect("test log collector mutex should not be poisoned")
                .clone()
        }
    }

    impl log::Log for TestLogCollector {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() <= log::Level::Info
        }

        fn log(&self, record: &log::Record<'_>) {
            if !self.enabled(record.metadata()) || !self.capture_enabled.load(Ordering::Relaxed) {
                return;
            }

            let mut entries = self
                .entries
                .lock()
                .expect("test log collector mutex should not be poisoned");
            entries.push(format!("{}", record.args()));
        }

        fn flush(&self) {}
    }

    static TEST_LOG_COLLECTOR: TestLogCollector = TestLogCollector::new();
    static TEST_LOGGER_INIT: Once = Once::new();
    static TEST_LOG_CAPTURE_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

    fn init_test_logger() {
        TEST_LOGGER_INIT.call_once(|| {
            log::set_logger(&TEST_LOG_COLLECTOR)
                .expect("test logger should initialize exactly once");
            log::set_max_level(log::LevelFilter::Info);
        });
    }

    fn empty_cli() -> Cli {
        Cli {
            log_level: None,
            _help: None,
            _version: None,
            test_mode: false,
            command: Some(ModeCommand::Query(QueryArgs::default())),
        }
    }

    fn cli_without_mode() -> Cli {
        Cli {
            command: None,
            ..empty_cli()
        }
    }

    fn parse_with_default_mode(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        let raw_args: Vec<OsString> = args.iter().map(OsString::from).collect();
        Cli::try_parse_from(cli_args_with_default_mode(&raw_args))
    }

    fn render_help_text(args: &[&str]) -> String {
        let err = parse_with_default_mode(args).expect_err("help flag should trigger help output");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        err.to_string()
    }

    fn render_root_help() -> String {
        render_help_text(&["--help"])
    }

    fn render_mode_help(mode: &str) -> String {
        render_help_text(&[mode, "--help"])
    }

    fn expect_query_runtime(runtime: RuntimeCli) -> QueryRuntimeCli {
        match runtime {
            RuntimeCli::Query(query_runtime) => query_runtime,
            RuntimeCli::RunDaemon(_) => panic!("expected query runtime"),
        }
    }

    fn expect_daemon_runtime(runtime: RuntimeCli) -> DaemonRuntimeCli {
        match runtime {
            RuntimeCli::RunDaemon(daemon_runtime) => daemon_runtime,
            RuntimeCli::Query(_) => panic!("expected run-daemon runtime"),
        }
    }

    #[test]
    fn cli_accepts_show_default_config_command() {
        let cli = parse_with_default_mode(&["show-default-config"])
            .expect("show-default-config should parse");
        assert!(matches!(cli.command, Some(ModeCommand::ShowDefaultConfig)));
    }

    #[test]
    fn cli_rejects_legacy_init_config_flag() {
        let err = parse_with_default_mode(&["--init-config"])
            .expect_err("--init-config must be rejected");
        assert!(err.to_string().contains("--init-config"));
    }

    #[test]
    fn cli_accepts_install_systemd_unit_command() {
        let cli = parse_with_default_mode(&["install-systemd-unit"])
            .expect("install-systemd-unit should parse");
        assert!(matches!(cli.command, Some(ModeCommand::InstallSystemdUnit)));
    }

    #[test]
    fn cli_accepts_run_daemon_command() {
        let cli = parse_with_default_mode(&["run-daemon"]).expect("run-daemon should parse");
        assert!(matches!(cli.command, Some(ModeCommand::RunDaemon(_))));
    }

    #[test]
    fn cli_accepts_run_daemon_foreground_flag_without_value() {
        let cli = parse_with_default_mode(&["run-daemon", "--foreground"])
            .expect("run-daemon --foreground should parse");
        let run_args = match cli.command {
            Some(ModeCommand::RunDaemon(args)) => args,
            _ => panic!("expected run-daemon command"),
        };
        assert_eq!(run_args.foreground, Some(true));
    }

    #[test]
    fn cli_accepts_run_daemon_foreground_flag_with_explicit_value() {
        let cli = parse_with_default_mode(&["run-daemon", "--foreground", "false"])
            .expect("run-daemon --foreground false should parse");
        let run_args = match cli.command {
            Some(ModeCommand::RunDaemon(args)) => args,
            _ => panic!("expected run-daemon command"),
        };
        assert_eq!(run_args.foreground, Some(false));

        let cli = parse_with_default_mode(&["run-daemon", "--foreground=true"])
            .expect("run-daemon --foreground=true should parse");
        let run_args = match cli.command {
            Some(ModeCommand::RunDaemon(args)) => args,
            _ => panic!("expected run-daemon command"),
        };
        assert_eq!(run_args.foreground, Some(true));
    }

    #[test]
    fn cli_rejects_run_deamon_alias() {
        let err = parse_with_default_mode(&["run-deamon"])
            .expect_err("run-deamon alias should not be supported");
        assert!(err.to_string().contains("run-deamon"));
    }

    #[test]
    fn cli_defaults_to_query_mode_when_mode_is_not_specified() {
        let cli = parse_with_default_mode(&[
            "--plugins-dir",
            "/tmp/plugins-default",
            "--enabled-plugins",
            "mock",
        ])
        .expect("flags without mode should parse as query mode");

        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query mode by default"),
        };
        assert_eq!(
            query_args.shared.plugins_dir,
            Some(PathBuf::from("/tmp/plugins-default"))
        );
        assert_eq!(query_args.shared.enabled_plugins.as_deref(), Some("mock"));
    }

    #[test]
    fn cli_accepts_query_command() {
        let cli = parse_with_default_mode(&["query", "--app-data-dir", "/tmp/query-data"])
            .expect("query command should parse");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };
        assert_eq!(
            query_args.shared.app_data_dir,
            Some(PathBuf::from("/tmp/query-data"))
        );
    }

    #[test]
    fn cli_accepts_query_type_plugins() {
        let cli = parse_with_default_mode(&["query", "--type", "plugins"])
            .expect("query --type plugins should parse");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };
        assert_eq!(query_args.query_type, QueryType::Plugins);
    }

    #[test]
    fn cli_accepts_query_type_usage() {
        let cli = parse_with_default_mode(&["query", "--type", "usage"])
            .expect("query --type usage should parse");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };
        assert_eq!(query_args.query_type, QueryType::Usage);
    }

    #[test]
    fn cli_accepts_query_type_config() {
        let cli = parse_with_default_mode(&["query", "--type", "config"])
            .expect("query --type config should parse");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };
        assert_eq!(query_args.query_type, QueryType::Config);
    }

    #[test]
    fn cli_accepts_with_state_flag_without_value() {
        let cli = parse_with_default_mode(&["query", "--with-state"]).expect("--with-state parses");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };
        assert!(query_args.with_state);
    }

    #[test]
    fn cli_accepts_with_state_with_explicit_boolean_value() {
        let cli = parse_with_default_mode(&["query", "--with-state=false"])
            .expect("--with-state=false parses");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };
        assert!(!query_args.with_state);

        let cli = parse_with_default_mode(&["query", "--with-state=true"])
            .expect("--with-state=true parses");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };
        assert!(query_args.with_state);
    }

    #[test]
    fn cli_defaults_to_query_mode_with_with_state_space_syntax() {
        let cli = parse_with_default_mode(&["--with-state", "true"])
            .expect("--with-state true should default to query mode");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query mode by default"),
        };
        assert!(query_args.with_state);
    }

    #[test]
    fn cli_defaults_to_query_mode_with_with_state_then_type_flag() {
        let cli = parse_with_default_mode(&[
            "--with-state",
            "--type",
            "plugins",
            "--enabled-plugins",
            "mock",
        ])
        .expect("flags without mode should parse as query mode");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query mode by default"),
        };
        assert!(query_args.with_state);
        assert_eq!(query_args.query_type, QueryType::Plugins);
        assert_eq!(query_args.shared.enabled_plugins.as_deref(), Some("mock"));
    }

    #[test]
    fn cli_defaults_to_query_mode_with_type_option() {
        let cli = parse_with_default_mode(&["--type", "plugins", "--enabled-plugins", "mock"])
            .expect("flags without mode should parse as query mode");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query mode by default"),
        };
        assert_eq!(query_args.query_type, QueryType::Plugins);
    }

    #[test]
    fn cli_accepts_query_value_options_with_equals_syntax() {
        let cli = parse_with_default_mode(&[
            "query",
            "--plugins-dir=/tmp/plugins-eq",
            "--enabled-plugins=mock,codex",
            "--app-data-dir=/tmp/data-eq",
            "--plugin-overrides-dir=/tmp/overrides-eq",
            "--log-level=debug",
        ])
        .expect("query options with equals syntax should parse");
        let log_level = cli.log_level;

        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };

        assert_eq!(
            query_args.shared.plugins_dir,
            Some(PathBuf::from("/tmp/plugins-eq"))
        );
        assert_eq!(
            query_args.shared.enabled_plugins.as_deref(),
            Some("mock,codex")
        );
        assert_eq!(
            query_args.shared.app_data_dir,
            Some(PathBuf::from("/tmp/data-eq"))
        );
        assert_eq!(
            query_args.shared.plugin_overrides_dir,
            Some(PathBuf::from("/tmp/overrides-eq"))
        );
        assert!(matches!(log_level, Some(LogLevel::Debug)));
    }

    #[test]
    fn cli_accepts_query_value_options_with_space_syntax() {
        let cli = parse_with_default_mode(&[
            "query",
            "--plugins-dir",
            "/tmp/plugins-space",
            "--enabled-plugins",
            "mock,codex",
            "--app-data-dir",
            "/tmp/data-space",
            "--plugin-overrides-dir",
            "/tmp/overrides-space",
            "--log-level",
            "debug",
        ])
        .expect("query options with space syntax should parse");
        let log_level = cli.log_level;

        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };

        assert_eq!(
            query_args.shared.plugins_dir,
            Some(PathBuf::from("/tmp/plugins-space"))
        );
        assert_eq!(
            query_args.shared.enabled_plugins.as_deref(),
            Some("mock,codex")
        );
        assert_eq!(
            query_args.shared.app_data_dir,
            Some(PathBuf::from("/tmp/data-space"))
        );
        assert_eq!(
            query_args.shared.plugin_overrides_dir,
            Some(PathBuf::from("/tmp/overrides-space"))
        );
        assert!(matches!(log_level, Some(LogLevel::Debug)));
    }

    #[test]
    fn cli_accepts_run_daemon_value_options_with_equals_syntax() {
        let cli = parse_with_default_mode(&[
            "run-daemon",
            "--host=127.0.0.1",
            "--port=7001",
            "--plugins-dir=/tmp/plugins-daemon-eq",
            "--enabled-plugins=mock",
            "--app-data-dir=/tmp/data-daemon-eq",
            "--plugin-overrides-dir=/tmp/overrides-daemon-eq",
            "--refresh-interval-secs=17",
            "--foreground=false",
            "--existing-instance=replace",
            "--service-mode=systemd",
            "--log-level=trace",
        ])
        .expect("run-daemon options with equals syntax should parse");
        let log_level = cli.log_level;

        let run_args = match cli.command {
            Some(ModeCommand::RunDaemon(args)) => args,
            _ => panic!("expected run-daemon command"),
        };

        assert_eq!(run_args.runtime.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(run_args.runtime.port, Some(7001));
        assert_eq!(
            run_args.runtime.shared.plugins_dir,
            Some(PathBuf::from("/tmp/plugins-daemon-eq"))
        );
        assert_eq!(
            run_args.runtime.shared.enabled_plugins.as_deref(),
            Some("mock")
        );
        assert_eq!(
            run_args.runtime.shared.app_data_dir,
            Some(PathBuf::from("/tmp/data-daemon-eq"))
        );
        assert_eq!(
            run_args.runtime.shared.plugin_overrides_dir,
            Some(PathBuf::from("/tmp/overrides-daemon-eq"))
        );
        assert_eq!(run_args.runtime.refresh_interval_secs, Some(17));
        assert_eq!(run_args.foreground, Some(false));
        assert!(matches!(
            run_args.existing_instance,
            Some(ExistingInstancePolicy::Replace)
        ));
        assert!(matches!(run_args.service_mode, Some(ServiceMode::Systemd)));
        assert!(matches!(log_level, Some(LogLevel::Trace)));
    }

    #[test]
    fn cli_accepts_run_daemon_value_options_with_space_syntax() {
        let cli = parse_with_default_mode(&[
            "run-daemon",
            "--host",
            "127.0.0.1",
            "--port",
            "7001",
            "--plugins-dir",
            "/tmp/plugins-daemon-space",
            "--enabled-plugins",
            "mock",
            "--app-data-dir",
            "/tmp/data-daemon-space",
            "--plugin-overrides-dir",
            "/tmp/overrides-daemon-space",
            "--refresh-interval-secs",
            "17",
            "--foreground",
            "false",
            "--existing-instance",
            "replace",
            "--service-mode",
            "systemd",
            "--log-level",
            "trace",
        ])
        .expect("run-daemon options with space syntax should parse");
        let log_level = cli.log_level;

        let run_args = match cli.command {
            Some(ModeCommand::RunDaemon(args)) => args,
            _ => panic!("expected run-daemon command"),
        };

        assert_eq!(run_args.runtime.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(run_args.runtime.port, Some(7001));
        assert_eq!(
            run_args.runtime.shared.plugins_dir,
            Some(PathBuf::from("/tmp/plugins-daemon-space"))
        );
        assert_eq!(
            run_args.runtime.shared.enabled_plugins.as_deref(),
            Some("mock")
        );
        assert_eq!(
            run_args.runtime.shared.app_data_dir,
            Some(PathBuf::from("/tmp/data-daemon-space"))
        );
        assert_eq!(
            run_args.runtime.shared.plugin_overrides_dir,
            Some(PathBuf::from("/tmp/overrides-daemon-space"))
        );
        assert_eq!(run_args.runtime.refresh_interval_secs, Some(17));
        assert_eq!(run_args.foreground, Some(false));
        assert!(matches!(
            run_args.existing_instance,
            Some(ExistingInstancePolicy::Replace)
        ));
        assert!(matches!(run_args.service_mode, Some(ServiceMode::Systemd)));
        assert!(matches!(log_level, Some(LogLevel::Trace)));
    }

    #[test]
    fn unknown_command_error_suggests_similar_command() {
        let raw_args = vec![OsString::from("run-damon")];
        let error = unknown_command_error(&raw_args).expect("must return unknown command error");

        assert!(error.contains("Did you mean run-daemon?"));
        assert!(error.contains("run-daemon"));
        assert!(error.contains("query"));
    }

    #[test]
    fn unknown_command_error_without_suggestion_lists_known_commands() {
        let raw_args = vec![OsString::from("abracadabra")];
        let error = unknown_command_error(&raw_args).expect("must return unknown command error");

        assert!(error.contains("unknown command abracadabra"));
        assert!(error.contains("run-daemon"));
        assert!(error.contains("install-systemd-unit"));
    }

    #[test]
    fn unknown_command_error_ignores_known_commands() {
        let raw_args = vec![OsString::from("query")];
        assert!(unknown_command_error(&raw_args).is_none());
    }

    #[test]
    fn unknown_command_error_ignores_flag_only_inputs() {
        let raw_args = vec![OsString::from("--host"), OsString::from("127.0.0.1")];
        assert!(unknown_command_error(&raw_args).is_none());
    }

    #[test]
    fn unknown_command_error_ignores_inputs_with_help_flag() {
        let raw_args = vec![OsString::from("--help"), OsString::from("abracadabra")];
        assert!(unknown_command_error(&raw_args).is_none());
    }

    #[test]
    fn unknown_command_error_ignores_inputs_with_version_flag() {
        let raw_args = vec![OsString::from("--version"), OsString::from("abracadabra")];
        assert!(unknown_command_error(&raw_args).is_none());
    }

    #[test]
    fn cli_accepts_hidden_test_mode_flag() {
        let cli =
            parse_with_default_mode(&["query", "--test-mode"]).expect("--test-mode should parse");
        assert!(cli.test_mode);
    }

    #[test]
    fn cli_rejects_run_daemon_only_flags_in_query_mode() {
        let err = parse_with_default_mode(&["query", "--existing-instance", "replace"])
            .expect_err("query mode must reject run-daemon-only flags");
        assert!(err.to_string().contains("--existing-instance"));
    }

    #[test]
    fn cli_rejects_query_runtime_flags_for_install_command() {
        let err =
            parse_with_default_mode(&["install-systemd-unit", "--plugins-dir", "/tmp/plugins"])
                .expect_err("install-systemd-unit must reject query flags");
        assert!(err.to_string().contains("--plugins-dir"));
    }

    #[test]
    fn cli_rejects_invalid_log_level_value() {
        let err = parse_with_default_mode(&["query", "--log-level", "inof"])
            .expect_err("invalid log level must be rejected");
        assert!(err.to_string().contains("--log-level"));
    }

    #[test]
    fn cli_accepts_version_command() {
        let cli = parse_with_default_mode(&["version"]).expect("version command should parse");
        assert!(matches!(cli.command, Some(ModeCommand::Version)));
    }

    #[test]
    fn query_help_shows_mode_options_before_global_options() {
        let help = render_mode_help("query");
        let mode_block = help
            .find("Mode options:")
            .expect("query help must include mode options block");
        let global_block = help
            .find("Global options:")
            .expect("query help must include global options block");

        assert!(mode_block < global_block);
        assert!(help.contains("--plugins-dir <PLUGINS_DIR>"));
        assert!(help.contains("--type <QUERY_TYPE>"));
        assert!(help.contains("--with-state [<WITH_STATE>]"));
        assert!(!help.contains("--host <HOST>"));
        assert!(!help.contains("--refresh-interval-secs <REFRESH_INTERVAL_SECS>"));
        assert!(help.contains("--log-level <LOG_LEVEL>"));
        assert!(help.contains("-h, --help"));
        assert!(help.contains("-V, --version"));
        assert!(!help.contains("--existing-instance"));
    }

    #[test]
    fn run_daemon_help_shows_mode_options_before_global_options() {
        let help = render_mode_help(CMD_RUN_DAEMON);
        let mode_block = help
            .find("Mode options:")
            .expect("run-daemon help must include mode options block");
        let global_block = help
            .find("Global options:")
            .expect("run-daemon help must include global options block");

        assert!(mode_block < global_block);
        assert!(help.contains("--foreground"));
        assert!(help.contains("--existing-instance <EXISTING_INSTANCE>"));
        assert!(help.contains("--service-mode <SERVICE_MODE>"));
        assert!(help.contains("--log-level <LOG_LEVEL>"));
        assert!(help.contains("-h, --help"));
        assert!(help.contains("-V, --version"));
    }

    #[test]
    fn global_options_block_is_consistent_for_root_and_modes() {
        let root_help = render_root_help();
        let query_help = render_mode_help("query");
        let daemon_help = render_mode_help(CMD_RUN_DAEMON);

        for help in [root_help, query_help, daemon_help] {
            assert!(help.contains("Global options:"));
            assert!(help.contains("--log-level <LOG_LEVEL>"));
            assert!(help.contains("-h, --help"));
            assert!(help.contains("-V, --version"));
        }
    }

    #[test]
    fn runtime_cli_uses_defaults_when_no_input_values() {
        let runtime = expect_query_runtime(
            RuntimeCli::from_sources(cli_without_mode(), config::AppConfig::default())
                .expect("runtime defaults should resolve"),
        );

        assert_eq!(runtime.shared.plugins_dir, None);
        assert_eq!(runtime.shared.app_data_dir, None);
        assert_eq!(runtime.shared.plugin_overrides_dir, None);
        assert_eq!(
            runtime.shared.enabled_plugins,
            config::DEFAULT_ENABLED_PLUGINS
        );
        assert_eq!(runtime.query_type, QueryType::Usage);
        assert!(!runtime.with_state);
        assert_eq!(runtime.host, config::DEFAULT_HOST);
        assert_eq!(runtime.port, config::DEFAULT_PORT);
        assert_eq!(
            runtime.refresh_interval_secs,
            config::DEFAULT_REFRESH_INTERVAL_SECS
        );
        assert_eq!(
            runtime.existing_instance_policy,
            ExistingInstancePolicy::Error
        );
        assert_eq!(runtime.service_mode, ServiceMode::Standalone);
        assert_eq!(runtime.shared.log_level, config::DEFAULT_LOG_LEVEL);
    }

    #[test]
    fn runtime_cli_query_mode_uses_selected_query_type() {
        let cli = Cli {
            command: Some(ModeCommand::Query(QueryArgs {
                shared: SharedRuntimeArgs::default(),
                query_type: QueryType::Plugins,
                with_state: false,
            })),
            ..empty_cli()
        };

        let runtime = expect_query_runtime(
            RuntimeCli::from_sources(cli, config::AppConfig::default())
                .expect("query type should resolve"),
        );

        assert_eq!(runtime.query_type, QueryType::Plugins);
        assert!(!runtime.with_state);
    }

    #[test]
    fn runtime_cli_uses_config_values_when_cli_is_empty() {
        let app_config = config::AppConfig {
            host: Some("0.0.0.0".to_string()),
            port: Some(9000),
            plugins_dir: Some(PathBuf::from("/tmp/plugins")),
            enabled_plugins: Some(vec!["codex".to_string(), "cur*".to_string()]),
            app_data_dir: Some(PathBuf::from("/tmp/data")),
            plugin_overrides_dir: Some(PathBuf::from("/tmp/overrides")),
            refresh_interval_secs: Some(42),
            foreground: Some(true),
            existing_instance: Some("ignore".to_string()),
            log_level: Some("debug".to_string()),
            proxy: None,
        };
        let runtime = expect_query_runtime(
            RuntimeCli::from_sources(empty_cli(), app_config)
                .expect("runtime config values should resolve"),
        );

        assert_eq!(
            runtime.shared.plugins_dir,
            Some(PathBuf::from("/tmp/plugins"))
        );
        assert_eq!(runtime.shared.enabled_plugins, "codex,cur*");
        assert_eq!(
            runtime.shared.app_data_dir,
            Some(PathBuf::from("/tmp/data"))
        );
        assert_eq!(
            runtime.shared.plugin_overrides_dir,
            Some(PathBuf::from("/tmp/overrides"))
        );
        assert_eq!(runtime.host, "0.0.0.0");
        assert_eq!(runtime.port, 9000);
        assert_eq!(runtime.refresh_interval_secs, 42);
        assert_eq!(
            runtime.existing_instance_policy,
            ExistingInstancePolicy::Ignore
        );
        assert_eq!(runtime.service_mode, ServiceMode::Standalone);
        assert_eq!(runtime.shared.log_level, "debug");
    }

    #[test]
    fn runtime_cli_prioritizes_cli_values_over_config() {
        let cli = Cli {
            log_level: Some(LogLevel::Trace),
            _help: None,
            _version: None,
            test_mode: false,
            command: Some(ModeCommand::RunDaemon(RunDaemonArgs {
                runtime: DaemonRuntimeArgs {
                    host: Some("127.0.0.2".to_string()),
                    port: Some(7001),
                    shared: SharedRuntimeArgs {
                        plugins_dir: Some(PathBuf::from("/cli/plugins")),
                        enabled_plugins: Some("mock".to_string()),
                        app_data_dir: Some(PathBuf::from("/cli/data")),
                        plugin_overrides_dir: Some(PathBuf::from("/cli/overrides")),
                    },
                    refresh_interval_secs: Some(7),
                },
                foreground: Some(true),
                existing_instance: Some(ExistingInstancePolicy::Replace),
                service_mode: Some(ServiceMode::Systemd),
                daemon_child: false,
            })),
        };
        let app_config = config::AppConfig {
            host: Some("0.0.0.0".to_string()),
            port: Some(9000),
            plugins_dir: Some(PathBuf::from("/cfg/plugins")),
            enabled_plugins: Some(vec!["codex".to_string()]),
            app_data_dir: Some(PathBuf::from("/cfg/data")),
            plugin_overrides_dir: Some(PathBuf::from("/cfg/overrides")),
            refresh_interval_secs: Some(60),
            foreground: Some(false),
            existing_instance: Some("error".to_string()),
            log_level: Some("debug".to_string()),
            proxy: None,
        };

        let runtime = expect_daemon_runtime(
            RuntimeCli::from_sources(cli, app_config).expect("runtime CLI values should resolve"),
        );

        assert_eq!(runtime.host, "127.0.0.2");
        assert_eq!(runtime.port, 7001);
        assert_eq!(
            runtime.shared.plugins_dir,
            Some(PathBuf::from("/cli/plugins"))
        );
        assert_eq!(runtime.shared.enabled_plugins, "mock");
        assert_eq!(
            runtime.shared.app_data_dir,
            Some(PathBuf::from("/cli/data"))
        );
        assert_eq!(
            runtime.shared.plugin_overrides_dir,
            Some(PathBuf::from("/cli/overrides"))
        );
        assert_eq!(runtime.refresh_interval_secs, 7);
        assert_eq!(runtime.shared.log_level, "trace");
        assert!(runtime.foreground);
        assert_eq!(
            runtime.existing_instance_policy,
            ExistingInstancePolicy::Replace
        );
        assert_eq!(runtime.service_mode, ServiceMode::Systemd);
    }

    #[test]
    fn runtime_cli_defaults_to_query_mode_without_explicit_command() {
        let runtime = RuntimeCli::from_sources(cli_without_mode(), config::AppConfig::default())
            .expect("runtime mode should resolve");

        assert!(matches!(runtime, RuntimeCli::Query(_)));
    }

    #[test]
    fn runtime_cli_uses_run_daemon_mode_when_selected() {
        let cli = Cli {
            _help: None,
            _version: None,
            command: Some(ModeCommand::RunDaemon(RunDaemonArgs {
                runtime: DaemonRuntimeArgs::default(),
                foreground: None,
                existing_instance: None,
                service_mode: None,
                daemon_child: false,
            })),
            ..empty_cli()
        };

        let runtime = RuntimeCli::from_sources(cli, config::AppConfig::default())
            .expect("runtime mode should resolve");

        assert!(matches!(runtime, RuntimeCli::RunDaemon(_)));
    }

    #[test]
    fn runtime_cli_rejects_invalid_config_existing_instance_value_for_run_daemon() {
        let app_config = config::AppConfig {
            existing_instance: Some("invalid".to_string()),
            ..config::AppConfig::default()
        };

        let cli = Cli {
            command: Some(ModeCommand::RunDaemon(RunDaemonArgs {
                runtime: DaemonRuntimeArgs::default(),
                foreground: None,
                existing_instance: None,
                service_mode: None,
                daemon_child: false,
            })),
            ..empty_cli()
        };

        let err = RuntimeCli::from_sources(cli, app_config)
            .expect_err("invalid existing_instance value must be rejected in run-daemon mode");
        assert!(err.to_string().contains("invalid existing_instance"));
    }

    #[test]
    fn runtime_cli_query_mode_ignores_existing_instance_config() {
        let app_config = config::AppConfig {
            existing_instance: Some("invalid".to_string()),
            ..config::AppConfig::default()
        };

        let runtime = RuntimeCli::from_sources(empty_cli(), app_config)
            .expect("query mode should ignore existing_instance config");
        let query_runtime = expect_query_runtime(runtime);
        assert_eq!(
            query_runtime.existing_instance_policy,
            ExistingInstancePolicy::Error
        );
    }

    #[test]
    fn should_spawn_daemon_parent_respects_foreground_and_child() {
        let shared = SharedRuntimeCli {
            plugins_dir: None,
            enabled_plugins: config::DEFAULT_ENABLED_PLUGINS.to_string(),
            app_data_dir: None,
            plugin_overrides_dir: None,
            test_mode: false,
            log_level: config::DEFAULT_LOG_LEVEL.to_string(),
        };

        let mut runtime = DaemonRuntimeCli {
            shared,
            foreground: false,
            host: config::DEFAULT_HOST.to_string(),
            port: config::DEFAULT_PORT,
            refresh_interval_secs: config::DEFAULT_REFRESH_INTERVAL_SECS,
            existing_instance_policy: ExistingInstancePolicy::Error,
            service_mode: ServiceMode::Standalone,
            daemon_child: false,
        };

        assert!(should_spawn_daemon_parent(&runtime));
        runtime.foreground = true;
        assert!(!should_spawn_daemon_parent(&runtime));
        runtime.foreground = false;
        runtime.daemon_child = true;
        assert!(!should_spawn_daemon_parent(&runtime));
    }

    #[test]
    fn restart_outcome_for_systemd_requests_self_restart() {
        assert_eq!(
            restart_outcome_for_service_mode(ServiceMode::Systemd),
            RunOutcome::RestartSelf
        );
    }

    #[test]
    fn restart_outcome_for_standalone_requests_self_restart() {
        assert_eq!(
            restart_outcome_for_service_mode(ServiceMode::Standalone),
            RunOutcome::RestartSelf
        );
    }

    #[test]
    fn self_restart_strategy_tag_matches_platform() {
        let expected = if cfg!(unix) { "exec" } else { "spawn" };
        assert_eq!(self_restart_strategy_tag(), expected);
    }

    #[test]
    fn runtime_cli_rejects_invalid_config_log_level() {
        let cli = empty_cli();
        let app_config = config::AppConfig {
            log_level: Some("loud".to_string()),
            ..config::AppConfig::default()
        };

        let err = RuntimeCli::from_sources(cli, app_config)
            .expect_err("invalid config log level must be rejected");
        assert!(err.to_string().contains("invalid log level"));
    }

    #[test]
    fn resolve_app_data_dir_requires_explicit_value_in_test_mode() {
        let err =
            resolve_app_data_dir(None, true).expect_err("test mode must require app data dir");
        assert!(err.to_string().contains("--app-data-dir"));
    }

    #[test]
    fn resolve_plugins_dir_requires_explicit_value_in_test_mode() {
        let err = resolve_plugins_dir(None, true).expect_err("test mode must require plugins dir");
        assert!(err.to_string().contains("--plugins-dir"));
    }

    #[test]
    fn resolve_plugin_overrides_dir_is_disabled_in_test_mode_without_explicit_path() {
        let resolved = resolve_plugin_overrides_dir(None, true)
            .expect("test mode should disable auto-discovery for plugin overrides");
        assert!(resolved.is_none());
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
    fn strip_flags_for_daemon_child_removes_internal_flags() {
        let args = vec![
            OsString::from(CMD_RUN_DAEMON),
            OsString::from("--host"),
            OsString::from("127.0.0.1"),
            OsString::from("--daemon-child"),
            OsString::from("--port=6737"),
        ];

        assert_eq!(
            strip_flags_for_daemon_child(&args),
            vec![
                OsString::from(CMD_RUN_DAEMON),
                OsString::from("--host"),
                OsString::from("127.0.0.1"),
                OsString::from("--port=6737"),
            ]
        );
    }

    #[test]
    fn systemd_exec_start_always_uses_foreground_mode_and_log_level_info() {
        assert_eq!(
            systemd_exec_start(OsStr::new("/usr/bin/openusage-cli")),
            "/usr/bin/openusage-cli run-daemon --foreground=true --service-mode=systemd --log-level=info"
        );
    }

    #[test]
    fn systemd_unit_install_messages_for_new_file_keep_enable_flow() {
        let message = systemd_unit_install_message(Path::new("/tmp/openusage-cli.service"), false);

        assert!(message.starts_with("Systemd user unit installed."));
        assert!(message.contains("Created files:"));
        assert!(message.contains("Next commands:"));
        assert!(message.contains(&format!(
            "  - systemctl --user enable --now {}",
            USER_SYSTEMD_SERVICE_NAME
        )));
        assert!(!message.contains("restart"));
    }

    #[test]
    fn systemd_unit_install_messages_for_existing_file_require_restart() {
        let message = systemd_unit_install_message(Path::new("/tmp/openusage-cli.service"), true);

        assert!(message.starts_with("Systemd user unit updated."));
        assert!(message.contains("Updated files:"));
        assert!(message.contains("Next commands to apply the updated unit:"));
        assert!(message.contains(&format!(
            "  - systemctl --user enable --now {}",
            USER_SYSTEMD_SERVICE_NAME
        )));
        assert!(message.contains(&format!(
            "  - systemctl --user restart {}",
            USER_SYSTEMD_SERVICE_NAME
        )));
    }

    #[test]
    fn build_systemd_unit_uses_expected_restart_policy() {
        let unit = build_systemd_unit("/usr/bin/openusage-cli run-daemon --foreground=true");

        assert!(unit.contains("Type=notify"));
        assert!(unit.contains("NotifyAccess=main"));
        assert!(unit.contains("WatchdogSec=30s"));
        assert!(unit.contains("TimeoutStartSec=120s"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("RestartSec=2s"));
        assert!(!unit.contains("SuccessExitStatus="));
        assert!(!unit.contains("RestartForceExitStatus="));
    }

    #[test]
    fn http_readiness_probe_addr_uses_loopback_for_unspecified_ipv4() {
        let bound = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 6737);
        assert_eq!(
            http_readiness_probe_addr(bound),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 6737)
        );
    }

    #[test]
    fn http_readiness_probe_addr_uses_loopback_for_unspecified_ipv6() {
        let bound = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 6737);
        assert_eq!(
            http_readiness_probe_addr(bound),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 6737)
        );
    }

    #[test]
    fn http_readiness_probe_addr_keeps_specific_bind_ip() {
        let bound = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40)), 6737);
        assert_eq!(http_readiness_probe_addr(bound), bound);
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

    fn stale_reset_plugin() -> manifest::LoadedPlugin {
        manifest::LoadedPlugin {
            manifest: manifest::PluginManifest {
                schema_version: 1,
                id: "stale-reset".to_string(),
                name: "Stale Reset".to_string(),
                version: "0.0.0-test".to_string(),
                entry: "plugin.js".to_string(),
                icon: "icon.svg".to_string(),
                brand_color: None,
                lines: Vec::new(),
                links: Vec::new(),
            },
            plugin_dir: PathBuf::from("."),
            entry_script: r#"
                globalThis.__openusage_plugin = {
                    probe() {
                        return {
                            lines: [{
                                type: "progress",
                                label: "Limit",
                                used: 10,
                                limit: 100,
                                format: { kind: "percent" },
                                resetsAt: "2000-01-01T00:00:00Z"
                            }]
                        };
                    }
                };
            "#
            .to_string(),
            icon_data_url: "data:image/svg+xml;base64,".to_string(),
        }
    }

    fn daemon_with_stale_reset_plugin() -> DaemonState {
        DaemonState::new(
            vec![stale_reset_plugin()],
            PathBuf::from("."),
            "0.0.0-test".to_string(),
            None,
        )
    }

    #[tokio::test]
    async fn run_past_reset_retry_loop_stops_after_retry_window() {
        let daemon = daemon_with_stale_reset_plugin();
        daemon
            .refresh(None)
            .await
            .expect("initial refresh should seed stale reset data");

        let result = tokio::time::timeout(
            Duration::from_millis(700),
            run_past_reset_retry_loop(
                &daemon,
                Some(tokio::time::Instant::now()),
                0,
                Duration::from_millis(20),
                Duration::from_millis(120),
            ),
        )
        .await;

        let attempts = result.expect("retry loop should stop when window expires");
        assert!(
            attempts > 0,
            "expected at least one retry attempt before window expiry"
        );
    }

    #[tokio::test]
    async fn run_past_reset_retry_loop_retries_without_proactive_trigger_when_stale() {
        let daemon = daemon_with_stale_reset_plugin();
        daemon
            .refresh(None)
            .await
            .expect("initial refresh should seed stale reset data");

        let result = tokio::time::timeout(
            Duration::from_millis(700),
            run_past_reset_retry_loop(
                &daemon,
                None,
                0,
                Duration::from_millis(20),
                Duration::from_millis(120),
            ),
        )
        .await;

        let attempts = result.expect("retry loop should stop when window expires");
        assert!(
            attempts > 0,
            "without proactive trigger, stale data should still trigger aggressive retries"
        );
    }

    #[tokio::test]
    async fn run_past_reset_retry_loop_logs_aggressive_mode_start_and_end() {
        let _capture_lock = TEST_LOG_CAPTURE_LOCK.lock().await;
        init_test_logger();
        TEST_LOG_COLLECTOR.start_capture();

        let daemon = daemon_with_stale_reset_plugin();
        daemon
            .refresh(None)
            .await
            .expect("initial refresh should seed stale reset data");

        let result = tokio::time::timeout(
            Duration::from_millis(700),
            run_past_reset_retry_loop(
                &daemon,
                None,
                0,
                Duration::from_millis(20),
                Duration::from_millis(120),
            ),
        )
        .await;

        let attempts = result.expect("retry loop should stop when window expires");
        assert!(
            attempts > 0,
            "expected at least one retry attempt for stale cached data"
        );

        let logs = TEST_LOG_COLLECTOR.stop_capture();
        assert!(
            logs.iter().any(|line| {
                line.contains(
                    "aggressive refresh mode started: past reset times detected in cached data",
                )
            }),
            "expected aggressive mode start log message, got logs: {:?}",
            logs
        );
        assert!(
            logs.iter().any(|line| {
                line.contains("aggressive refresh mode ended: retry window expired after")
            }),
            "expected aggressive mode end log message, got logs: {:?}",
            logs
        );
    }

    #[tokio::test]
    async fn run_past_reset_retry_loop_logs_proactive_aggressive_mode_start() {
        let _capture_lock = TEST_LOG_CAPTURE_LOCK.lock().await;
        init_test_logger();
        TEST_LOG_COLLECTOR.start_capture();

        let daemon = daemon_with_stale_reset_plugin();
        daemon
            .refresh(None)
            .await
            .expect("initial refresh should seed stale reset data");

        let result = tokio::time::timeout(
            Duration::from_millis(700),
            run_past_reset_retry_loop(
                &daemon,
                Some(tokio::time::Instant::now()),
                0,
                Duration::from_millis(20),
                Duration::from_millis(120),
            ),
        )
        .await;

        let attempts = result.expect("retry loop should stop when window expires");
        assert!(
            attempts > 0,
            "expected at least one retry attempt for stale cached data"
        );

        let logs = TEST_LOG_COLLECTOR.stop_capture();
        assert!(
            logs.iter().any(|line| line
                .contains("aggressive refresh mode started: proactive reset trigger detected")),
            "expected proactive aggressive mode start log message, got logs: {:?}",
            logs
        );
        assert!(
            logs.iter().any(|line| {
                line.contains("aggressive refresh mode ended: retry window expired after")
            }),
            "expected aggressive mode end log message, got logs: {:?}",
            logs
        );
    }

    #[tokio::test]
    async fn create_refresh_interval_does_not_tick_immediately_after_priming() {
        let mut interval = create_refresh_interval(Duration::from_secs(1)).await;

        let immediate_tick = tokio::time::timeout(Duration::from_millis(10), interval.tick()).await;
        assert!(
            immediate_tick.is_err(),
            "next interval tick should not happen immediately"
        );
    }

    #[tokio::test]
    async fn create_refresh_interval_ticks_after_interval_elapsed() {
        let mut interval = create_refresh_interval(Duration::from_millis(25)).await;

        let delayed_tick = tokio::time::timeout(Duration::from_millis(250), interval.tick()).await;
        assert!(
            delayed_tick.is_ok(),
            "expected interval tick within timeout"
        );
    }
}
