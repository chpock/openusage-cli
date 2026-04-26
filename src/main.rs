use anyhow::{Context, Result, anyhow};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use directories::ProjectDirs;
use globset::{Glob, GlobSet, GlobSetBuilder};
use indoc::formatdoc;
use openusage_cli::config;
use openusage_cli::daemon::{CachedPluginSnapshot, DaemonState};
use openusage_cli::discovery::PublishedDiscovery;
use openusage_cli::http_api::{self, ApiState, LifecycleCommand, RuntimeConfig};
use openusage_cli::instance_control::{self, ExistingInstancePolicy, ServiceMode};
use openusage_cli::plugin_engine::manifest;
use openusage_cli::plugin_engine::runtime::MetricLine;
use std::ffi::{OsStr, OsString};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;

const SYSTEM_PLUGINS_DIR: &str = "/usr/share/openusage-cli/openusage-plugins";
const SYSTEM_PLUGIN_OVERRIDES_DIR: &str = "/usr/share/openusage-cli/plugin-overrides";
const USER_SYSTEMD_SERVICE_NAME: &str = "openusage-cli.service";
const EXISTING_INSTANCE_SHUTDOWN_TIMEOUT_SECS: u64 = 15;
const SYSTEMD_RESTART_EXIT_CODE: i32 = 75;
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

// NOTE: When adding new value-taking arguments here, also update
// `option_consumes_separate_value` to keep pre-parser positional detection in sync.
#[derive(Debug, Clone, Default, Args)]
#[command(next_help_heading = HELP_HEADING_MODE_OPTIONS)]
struct QueryArgs {
    /// HTTP host to bind to [default: 127.0.0.1]
    #[arg(long)]
    host: Option<String>,

    /// HTTP port to bind to (0 = random free port) [default: 0]
    #[arg(long)]
    port: Option<u16>,

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

    /// Background refresh interval in seconds (0 = disable) [default: 300]
    #[arg(long)]
    refresh_interval_secs: Option<u64>,
}

// NOTE: When adding new value-taking arguments here, also update
// `option_consumes_separate_value` to keep pre-parser positional detection in sync.
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = HELP_HEADING_MODE_OPTIONS)]
struct RunDaemonArgs {
    #[command(flatten)]
    runtime: QueryArgs,

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

    /// Query usage data (default mode; one-shot JSON output)
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
#[command(about = "HTTP daemon for AI usage limit plugins")]
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

fn parse_cli_with_default_mode(raw_args: &[OsString]) -> Cli {
    if let Some(message) = unknown_command_error(raw_args) {
        eprintln!("{message}");
        std::process::exit(2);
    }

    Cli::parse_from(cli_args_with_default_mode(raw_args))
}

fn unknown_command_error(raw_args: &[OsString]) -> Option<String> {
    if contains_global_help_or_version_flag(raw_args) {
        return None;
    }

    let command = first_positional_token(raw_args)?;
    if KNOWN_COMMANDS.contains(&command.as_str()) {
        return None;
    }

    let known_commands = KNOWN_COMMANDS.join(", ");
    let suggestion = find_similar_command(&command);

    Some(match suggestion {
        Some(similar) => {
            format!(
                "unknown command {command}. Did you mean {similar}? Known commands: {known_commands}"
            )
        }
        None => {
            format!("unknown command {command}. Use one of the known commands: {known_commands}")
        }
    })
}

fn first_positional_token(raw_args: &[OsString]) -> Option<String> {
    let mut index = 0;
    while index < raw_args.len() {
        let token = raw_args[index].to_string_lossy();

        if token == "--" {
            return raw_args
                .get(index + 1)
                .map(|value| value.to_string_lossy().into_owned());
        }

        if !token.starts_with('-') {
            return Some(token.into_owned());
        }

        if option_consumes_separate_value(&token) && !token.contains('=') {
            index += 1;
        }

        index += 1;
    }

    None
}

fn find_similar_command(input: &str) -> Option<&'static str> {
    let input_lower = input.to_ascii_lowercase();

    KNOWN_COMMANDS
        .iter()
        .copied()
        .map(|candidate| {
            let distance = levenshtein_distance(&input_lower, candidate);
            (candidate, distance)
        })
        .min_by_key(|(_, distance)| *distance)
        .and_then(|(candidate, distance)| {
            let max_len = input_lower.chars().count().max(candidate.chars().count());
            let threshold = match max_len {
                0..=4 => 1,
                5..=8 => 2,
                _ => 3,
            };

            if distance <= threshold {
                Some(candidate)
            } else {
                None
            }
        })
}

fn levenshtein_distance(left: &str, right: &str) -> usize {
    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();

    if left_chars.is_empty() {
        return right_chars.len();
    }
    if right_chars.is_empty() {
        return left_chars.len();
    }

    let mut previous_row: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current_row = vec![0; right_chars.len() + 1];

    for (i, left_char) in left_chars.iter().enumerate() {
        current_row[0] = i + 1;

        for (j, right_char) in right_chars.iter().enumerate() {
            let substitution_cost = if left_char == right_char { 0 } else { 1 };
            let delete_cost = previous_row[j + 1] + 1;
            let insert_cost = current_row[j] + 1;
            let substitute_cost = previous_row[j] + substitution_cost;

            current_row[j + 1] = delete_cost.min(insert_cost).min(substitute_cost);
        }

        std::mem::swap(&mut previous_row, &mut current_row);
    }

    previous_row[right_chars.len()]
}

fn cli_args_with_default_mode(raw_args: &[OsString]) -> Vec<OsString> {
    let mut args = Vec::with_capacity(raw_args.len() + 2);
    args.push(OsString::from("openusage-cli"));

    if should_insert_default_query_mode(raw_args) {
        args.push(OsString::from("query"));
    }

    args.extend(raw_args.iter().cloned());
    args
}

fn should_insert_default_query_mode(raw_args: &[OsString]) -> bool {
    if raw_args.is_empty() {
        return true;
    }

    if contains_global_help_or_version_flag(raw_args) {
        return false;
    }

    !raw_args_contains_positional(raw_args)
}

fn contains_global_help_or_version_flag(raw_args: &[OsString]) -> bool {
    raw_args.iter().any(|arg| {
        matches!(
            arg.to_string_lossy().as_ref(),
            "--help" | "-h" | "--version" | "-V"
        )
    })
}

fn raw_args_contains_positional(raw_args: &[OsString]) -> bool {
    let mut index = 0;
    while index < raw_args.len() {
        let token = raw_args[index].to_string_lossy();

        if token == "--" {
            return raw_args.get(index + 1).is_some();
        }

        if !token.starts_with('-') {
            return true;
        }

        if option_consumes_separate_value(&token) && !token.contains('=') {
            index += 1;
        }

        index += 1;
    }

    false
}

fn option_consumes_separate_value(option: &str) -> bool {
    let option_name = option.split('=').next().unwrap_or(option);
    matches!(
        option_name,
        "--host"
            | "--port"
            | "--plugins-dir"
            | "--enabled-plugins"
            | "--app-data-dir"
            | "--plugin-overrides-dir"
            | "--refresh-interval-secs"
            | "--foreground"
            | "--existing-instance"
            | "--service-mode"
            | "--log-level"
    )
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

    // Resolve log level from CLI/env/config (in that order of precedence)
    let env_overrides = if cli.test_mode {
        EnvOverrides::default()
    } else {
        EnvOverrides::from_process()
    };

    let log_level =
        resolve_log_level(&cli, &env_overrides).context("failed to resolve log level")?;

    // Initialize logger with resolved log level
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level.as_str()))
        .init();

    let result = run(cli, env_overrides, &raw_args).await;
    match result {
        Ok(RunOutcome::Completed) => {
            log::info!("openusage-cli shutdown complete");
            Ok(())
        }
        Ok(RunOutcome::ExitWithCode(exit_code)) => {
            log::info!("openusage-cli exiting with requested code {}", exit_code);
            std::process::exit(exit_code);
        }
        Err(err) => {
            log::error!("openusage-cli exiting with error: {err:#}");
            Err(err)
        }
    }
}

async fn run(cli: Cli, env_overrides: EnvOverrides, raw_args: &[OsString]) -> Result<RunOutcome> {
    let loaded_config = if cli.test_mode {
        log::info!(
            "test mode enabled; ignoring config file and OPENUSAGE_* runtime environment overrides"
        );
        None
    } else {
        config::load_config_if_exists().context("failed to load config")?
    };
    let runtime = match loaded_config {
        Some(loaded) => {
            log::info!("using config file: {}", loaded.path.display());
            RuntimeCli::from_sources(cli, env_overrides, loaded.config)
                .context("failed to resolve runtime options")?
        }
        None => {
            if !cli.test_mode {
                let path = config::config_path().context("failed to resolve config path")?;
                log::info!(
                    "config file not found at {}; using CLI/env/default values",
                    path.display()
                );
            }
            RuntimeCli::from_sources(cli, env_overrides, config::AppConfig::default())
                .context("failed to resolve runtime options")?
        }
    };
    let app_version = APP_VERSION.to_string();

    log::info!("starting openusage-cli v{}", app_version);
    log::debug!(
        "startup options: mode={}, foreground={}, host={}, port={}, refresh_interval_secs={}, existing_instance={}, service_mode={}, daemon_child={}, test_mode={}, plugins_dir={:?}, enabled_plugins='{}', app_data_dir={:?}, plugin_overrides_dir={:?}, log_level={}",
        runtime.mode.as_str(),
        runtime.foreground,
        runtime.host,
        runtime.port,
        runtime.refresh_interval_secs,
        runtime.existing_instance_policy,
        runtime.service_mode,
        runtime.daemon_child,
        runtime.test_mode,
        runtime.plugins_dir,
        runtime.enabled_plugins,
        runtime.app_data_dir,
        runtime.plugin_overrides_dir,
        runtime.log_level
    );

    // Resolve app_data_dir early - needed for daemon discovery in test mode
    let app_data_dir = resolve_app_data_dir(runtime.app_data_dir.clone(), runtime.test_mode)
        .context("failed to resolve app data directory")?;
    let test_runtime_dir = runtime
        .test_mode
        .then(|| app_data_dir.join(config::RUNTIME_DIR_NAME));

    // Query mode: try to connect to an existing daemon first
    if runtime.mode == RuntimeMode::Query {
        log::debug!("query mode enabled; attempting to discover running daemon");

        if let Some(running_instance) =
            instance_control::discover_running_instance(test_runtime_dir.as_deref()).await
        {
            log::info!(
                "discovered running daemon at {} (service_mode={}); querying for data",
                running_instance.base_url,
                running_instance.service_mode
            );
            match query_daemon_via_http(&running_instance.base_url).await {
                Ok(json_output) => {
                    println!("{}", json_output);
                    return Ok(RunOutcome::Completed);
                }
                Err(err) => {
                    log::warn!(
                        "failed to query running daemon: {}; falling back to local plugin execution",
                        err
                    );
                }
            }
        } else {
            log::info!("no running daemon discovered; falling back to local plugin execution");
        }
    }

    if should_spawn_daemon_parent(&runtime) {
        if let Some(running_instance) =
            instance_control::discover_running_instance(test_runtime_dir.as_deref()).await
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
                            "Existing systemd-managed daemon at {} received restart request. The systemd unit should restart it automatically.",
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
    log::info!("using app data dir: {}", app_data_dir.display());
    std::fs::create_dir_all(&app_data_dir).with_context(|| {
        format!(
            "failed to create app data directory {}",
            app_data_dir.display()
        )
    })?;
    log::debug!("ensured app data directory exists");

    let plugins_dir = resolve_plugins_dir(runtime.plugins_dir.clone(), runtime.test_mode)
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

    let plugin_overrides_dir =
        resolve_plugin_overrides_dir(runtime.plugin_overrides_dir.clone(), runtime.test_mode)
            .context("failed to resolve plugin overrides directory")?;
    if let Some(path) = &plugin_overrides_dir {
        log::info!("using plugin overrides dir: {}", path.display());
    } else {
        log::info!("plugin overrides disabled (no overrides dir found)");
    }

    let daemon = Arc::new(DaemonState::new(
        plugins,
        app_data_dir.clone(),
        app_version.clone(),
        plugin_overrides_dir.clone(),
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

    if runtime.mode == RuntimeMode::Query {
        let snapshots = daemon.cached(None).await;
        let json_output =
            serde_json::to_string(&snapshots).context("failed to serialize query results")?;
        println!("{}", json_output);
        return Ok(RunOutcome::Completed);
    }

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
                // Track when we triggered refresh due to limit reset (for retry timeout)
                let mut proactive_trigger_time: Option<tokio::time::Instant> = None;

                // Calculate when the next limit reset occurs
                let reset_delay = refresh_state
                    .time_until_next_reset(RESET_CHECK_MARGIN_SECS)
                    .await;

                if let Some(delay) = reset_delay {
                    log::debug!(
                        "next limit reset in {:?} (margin {}s)",
                        delay,
                        RESET_CHECK_MARGIN_SECS
                    );

                    // Wait for either the interval or the reset, whichever comes first
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
                    // No upcoming resets known, just wait for the interval
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

    let discovery = if runtime.existing_instance_policy == ExistingInstancePolicy::Ignore {
        log::info!("skipping daemon endpoint publication because --existing-instance=ignore");
        None
    } else {
        let discovery = PublishedDiscovery::publish(bound_addr, test_runtime_dir.as_deref())
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

    let runtime_config = RuntimeConfig {
        app_version: app_version.clone(),
        host: runtime.host.clone(),
        port: bound_addr.port(),
        service_mode: runtime.service_mode.to_string(),
        existing_instance_policy: runtime.existing_instance_policy.to_string(),
        plugins_dir: Some(plugins_dir.clone()),
        enabled_plugins: runtime.enabled_plugins.clone(),
        app_data_dir: Some(app_data_dir.clone()),
        plugin_overrides_dir: plugin_overrides_dir.clone(),
        refresh_interval_secs: runtime.refresh_interval_secs,
        log_level: runtime.log_level.clone(),
    };

    let app = http_api::router(ApiState {
        daemon,
        app_version,
        config: runtime_config,
        lifecycle_tx: Some(lifecycle_tx),
    });
    log::debug!("HTTP router initialized");

    if let Some(discovery) = &discovery {
        log::info!("openusage-cli daemon listening on {}", discovery.base_url());
    } else {
        log::info!("openusage-cli daemon listening on http://{}", bound_addr);
    }
    log::debug!("waiting for shutdown signal");

    let server_result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown({
        let lifecycle_reason = Arc::clone(&lifecycle_reason);
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
    if lifecycle_reason.load(Ordering::Relaxed) == LIFECYCLE_RESTART {
        if runtime.service_mode == ServiceMode::Systemd {
            log::info!(
                "service_mode=systemd and restart requested; exiting with code {}",
                SYSTEMD_RESTART_EXIT_CODE
            );
            return Ok(RunOutcome::ExitWithCode(SYSTEMD_RESTART_EXIT_CODE));
        }
        log::warn!(
            "restart requested but service_mode={} does not support manager-driven restart; exiting normally",
            runtime.service_mode
        );
    }

    Ok(RunOutcome::Completed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeMode {
    Query,
    RunDaemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOutcome {
    Completed,
    ExitWithCode(i32),
}

impl RuntimeMode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::RunDaemon => CMD_RUN_DAEMON,
        }
    }
}

#[derive(Debug, Clone)]
struct RuntimeCli {
    mode: RuntimeMode,
    foreground: bool,
    host: String,
    port: u16,
    plugins_dir: Option<PathBuf>,
    enabled_plugins: String,
    app_data_dir: Option<PathBuf>,
    plugin_overrides_dir: Option<PathBuf>,
    refresh_interval_secs: u64,
    existing_instance_policy: ExistingInstancePolicy,
    service_mode: ServiceMode,
    daemon_child: bool,
    test_mode: bool,
    log_level: String,
}

impl RuntimeCli {
    fn from_sources(cli: Cli, env: EnvOverrides, config: config::AppConfig) -> Result<Self> {
        let Cli {
            log_level: cli_log_level,
            _help: _,
            _version: _,
            test_mode,
            command,
        } = cli;
        let mode_selection = resolve_runtime_mode_and_args(command)?;
        let RuntimeModeSelection {
            mode,
            runtime_args: mode_args,
            foreground: mode_foreground,
            existing_instance: mode_existing_instance,
            service_mode: mode_service_mode,
            daemon_child,
        } = mode_selection;
        let config_existing_instance = config.existing_instance.clone();
        let host = mode_args
            .host
            .or(config.host)
            .unwrap_or_else(|| config::DEFAULT_HOST.to_string());
        let port = mode_args
            .port
            .or(config.port)
            .unwrap_or(config::DEFAULT_PORT);
        let refresh_interval_secs = mode_args
            .refresh_interval_secs
            .or(config.refresh_interval_secs)
            .unwrap_or(config::DEFAULT_REFRESH_INTERVAL_SECS);
        let config_existing_instance_policy = match config_existing_instance {
            Some(value) => Some(
                ExistingInstancePolicy::parse(&value)
                    .with_context(|| format!("invalid existing_instance value '{}'", value))?,
            ),
            None => None,
        };
        let existing_instance_policy = mode_existing_instance
            .or(config_existing_instance_policy)
            .unwrap_or(ExistingInstancePolicy::Error);
        let foreground = mode_foreground.or(config.foreground).unwrap_or(false);
        let service_mode = mode_service_mode.unwrap_or(ServiceMode::Standalone);
        let enabled_plugins = mode_args
            .enabled_plugins
            .or(env.enabled_plugins)
            .or_else(|| config.enabled_plugins.map(|masks| masks.join(",")))
            .unwrap_or_else(|| config::DEFAULT_ENABLED_PLUGINS.to_string());
        let raw_log_level = cli_log_level
            .map(|level| level.as_str().to_string())
            .or(env.log_level)
            .or(config.log_level)
            .unwrap_or_else(|| config::DEFAULT_LOG_LEVEL.to_string());
        let log_level = normalize_log_level(raw_log_level)?;

        Ok(Self {
            mode,
            foreground,
            host,
            port,
            plugins_dir: mode_args
                .plugins_dir
                .or(env.plugins_dir)
                .or(config.plugins_dir),
            enabled_plugins,
            app_data_dir: mode_args
                .app_data_dir
                .or(env.app_data_dir)
                .or(config.app_data_dir),
            plugin_overrides_dir: mode_args
                .plugin_overrides_dir
                .or(env.plugin_overrides_dir)
                .or(config.plugin_overrides_dir),
            refresh_interval_secs,
            existing_instance_policy,
            service_mode,
            daemon_child,
            test_mode,
            log_level,
        })
    }
}

#[derive(Debug, Clone)]
struct RuntimeModeSelection {
    mode: RuntimeMode,
    runtime_args: QueryArgs,
    foreground: Option<bool>,
    existing_instance: Option<ExistingInstancePolicy>,
    service_mode: Option<ServiceMode>,
    daemon_child: bool,
}

fn resolve_runtime_mode_and_args(command: Option<ModeCommand>) -> Result<RuntimeModeSelection> {
    match command {
        Some(ModeCommand::Query(args)) => Ok(RuntimeModeSelection {
            mode: RuntimeMode::Query,
            runtime_args: args,
            foreground: None,
            existing_instance: None,
            service_mode: None,
            daemon_child: false,
        }),
        Some(ModeCommand::RunDaemon(args)) => Ok(RuntimeModeSelection {
            mode: RuntimeMode::RunDaemon,
            runtime_args: args.runtime,
            foreground: args.foreground,
            existing_instance: args.existing_instance,
            service_mode: args.service_mode,
            daemon_child: args.daemon_child,
        }),
        None => Ok(RuntimeModeSelection {
            mode: RuntimeMode::Query,
            runtime_args: QueryArgs::default(),
            foreground: None,
            existing_instance: None,
            service_mode: None,
            daemon_child: false,
        }),
        Some(
            ModeCommand::ShowDefaultConfig | ModeCommand::InstallSystemdUnit | ModeCommand::Version,
        ) => anyhow::bail!("internal error: non-runtime command reached runtime option resolver"),
    }
}

fn resolve_log_level(cli: &Cli, env_overrides: &EnvOverrides) -> Result<String> {
    let config_log_level = if cli.test_mode {
        None
    } else {
        config::load_config_if_exists()?.and_then(|loaded| loaded.config.log_level)
    };

    let raw_log_level = cli
        .log_level
        .map(|level| level.as_str().to_string())
        .or(env_overrides.log_level.clone())
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

#[derive(Debug, Clone, Default)]
struct EnvOverrides {
    plugins_dir: Option<PathBuf>,
    enabled_plugins: Option<String>,
    app_data_dir: Option<PathBuf>,
    plugin_overrides_dir: Option<PathBuf>,
    log_level: Option<String>,
}

impl EnvOverrides {
    fn from_process() -> Self {
        Self::from_reader(|name| std::env::var_os(name))
    }

    fn from_reader<F>(mut reader: F) -> Self
    where
        F: FnMut(&str) -> Option<OsString>,
    {
        Self {
            plugins_dir: env_path(&mut reader, "OPENUSAGE_PLUGINS_DIR"),
            enabled_plugins: env_string(&mut reader, "OPENUSAGE_ENABLED_PLUGINS"),
            app_data_dir: env_path(&mut reader, "OPENUSAGE_APP_DATA_DIR"),
            plugin_overrides_dir: env_path(&mut reader, "OPENUSAGE_PLUGIN_OVERRIDES_DIR"),
            log_level: env_string(&mut reader, "OPENUSAGE_LOG_LEVEL"),
        }
    }
}

fn env_path<F>(reader: &mut F, name: &str) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<OsString>,
{
    let value = reader(name)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

fn env_string<F>(reader: &mut F, name: &str) -> Option<String>
where
    F: FnMut(&str) -> Option<OsString>,
{
    let value = reader(name)?;
    let value = value.to_string_lossy().trim().to_string();
    if value.is_empty() {
        return None;
    }
    Some(value)
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

fn resolve_app_data_dir(cli_value: Option<PathBuf>, test_mode: bool) -> Result<PathBuf> {
    if let Some(path) = cli_value {
        log::debug!("app data dir provided via CLI/env: {}", path.display());
        return Ok(path);
    }
    if test_mode {
        anyhow::bail!("--app-data-dir is required when --test-mode is enabled");
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

fn resolve_plugins_dir(cli_value: Option<PathBuf>, test_mode: bool) -> Result<PathBuf> {
    if let Some(path) = cli_value {
        log::debug!("plugins dir provided via CLI/env: {}", path.display());
        return Ok(path);
    }
    if test_mode {
        anyhow::bail!("--plugins-dir is required when --test-mode is enabled");
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

fn resolve_plugin_overrides_dir(
    cli_value: Option<PathBuf>,
    test_mode: bool,
) -> Result<Option<PathBuf>> {
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

    if test_mode {
        return Ok(None);
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
    let mut retry_attempts = 0usize;

    while refresh_state.has_past_resets(reset_check_margin_secs).await {
        let should_retry = proactive_trigger_time
            .is_some_and(|trigger_time| trigger_time.elapsed() < max_retry_age);

        if !should_retry {
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

    retry_attempts
}

fn should_spawn_daemon_parent(runtime: &RuntimeCli) -> bool {
    runtime.mode == RuntimeMode::RunDaemon && !runtime.daemon_child && !runtime.foreground
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

fn install_user_systemd_unit() -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("'install-systemd-unit' command is supported only on Linux");
    }

    #[cfg(target_os = "linux")]
    {
        let home_dir = dirs::home_dir().context("cannot resolve current user home directory")?;
        let required_dirs = [
            home_dir.join(".config"),
            home_dir.join(".config/systemd"),
            home_dir.join(".config/systemd/user"),
        ];
        let missing_dirs: Vec<PathBuf> = required_dirs
            .iter()
            .filter(|path| !path.is_dir())
            .cloned()
            .collect();

        if !missing_dirs.is_empty() {
            let missing = missing_dirs
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "cannot install user systemd unit: required directories do not exist: {}",
                missing
            );
        }

        let unit_path = home_dir
            .join(".config/systemd/user")
            .join(USER_SYSTEMD_SERVICE_NAME);
        let executable = std::env::current_exe().context("cannot resolve current executable")?;
        let exec_start = systemd_exec_start(executable.as_os_str());
        let unit_content = build_systemd_unit(&exec_start);

        std::fs::write(&unit_path, unit_content)
            .with_context(|| format!("failed to write unit file {}", unit_path.display()))?;

        println!("Systemd user unit installed.");
        println!("Created files:");
        println!("  - {}", unit_path.display());
        println!("Next commands:");
        println!("  - systemctl --user daemon-reload");
        println!(
            "  - systemctl --user enable --now {}",
            USER_SYSTEMD_SERVICE_NAME
        );
        println!("  - systemctl --user status {}", USER_SYSTEMD_SERVICE_NAME);
        println!("Service logs:");
        println!("  - journalctl --user -u {} -f", USER_SYSTEMD_SERVICE_NAME);

        Ok(())
    }
}

fn build_systemd_unit(exec_start: &str) -> String {
    formatdoc! {"
        [Unit]
        Description=OpenUsage CLI daemon
        After=network.target

        [Service]
        Type=simple
        ExecStart={exec_start}
        Restart=on-failure
        RestartSec=2s
        SuccessExitStatus={SYSTEMD_RESTART_EXIT_CODE}
        RestartForceExitStatus={SYSTEMD_RESTART_EXIT_CODE}

        [Install]
        WantedBy=default.target
    "}
}

fn quote_systemd_argument(value: &OsStr) -> String {
    let raw = value.to_string_lossy();
    if raw.chars().all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '=' | ',')
    }) {
        return raw.to_string();
    }

    let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

fn systemd_exec_start(executable: &OsStr) -> String {
    [
        quote_systemd_argument(executable),
        CMD_RUN_DAEMON.to_string(),
        "--foreground=true".to_string(),
        "--service-mode=systemd".to_string(),
        "--log-level=info".to_string(),
    ]
    .join(" ")
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

/// Queries a running daemon via HTTP and returns validated usage JSON.
async fn query_daemon_via_http(base_url: &str) -> Result<String> {
    let url = format!("{}/v1/usage", base_url);
    log::debug!("querying daemon at {}", url);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
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

    let payload: serde_json::Value =
        serde_json::from_str(&body).context("daemon returned non-JSON usage payload")?;

    let snapshots = payload
        .as_array()
        .context("daemon returned unexpected usage payload shape (expected JSON array)")?;
    if !snapshots.iter().all(serde_json::Value::is_object) {
        anyhow::bail!(
            "daemon returned unexpected usage payload shape (array entries must be objects)"
        );
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
        let cli = parse_with_default_mode(&["--host", "127.0.0.2", "--port", "7001"])
            .expect("flags without mode should parse as query mode");

        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query mode by default"),
        };
        assert_eq!(query_args.host.as_deref(), Some("127.0.0.2"));
        assert_eq!(query_args.port, Some(7001));
    }

    #[test]
    fn cli_accepts_query_command() {
        let cli = parse_with_default_mode(&["query", "--host", "127.0.0.1"])
            .expect("query command should parse");
        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };
        assert_eq!(query_args.host.as_deref(), Some("127.0.0.1"));
    }

    #[test]
    fn cli_accepts_query_value_options_with_equals_syntax() {
        let cli = parse_with_default_mode(&[
            "query",
            "--host=127.0.0.1",
            "--port=7001",
            "--plugins-dir=/tmp/plugins-eq",
            "--enabled-plugins=mock,codex",
            "--app-data-dir=/tmp/data-eq",
            "--plugin-overrides-dir=/tmp/overrides-eq",
            "--refresh-interval-secs=11",
            "--log-level=debug",
        ])
        .expect("query options with equals syntax should parse");
        let log_level = cli.log_level;

        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };

        assert_eq!(query_args.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(query_args.port, Some(7001));
        assert_eq!(
            query_args.plugins_dir,
            Some(PathBuf::from("/tmp/plugins-eq"))
        );
        assert_eq!(query_args.enabled_plugins.as_deref(), Some("mock,codex"));
        assert_eq!(query_args.app_data_dir, Some(PathBuf::from("/tmp/data-eq")));
        assert_eq!(
            query_args.plugin_overrides_dir,
            Some(PathBuf::from("/tmp/overrides-eq"))
        );
        assert_eq!(query_args.refresh_interval_secs, Some(11));
        assert!(matches!(log_level, Some(LogLevel::Debug)));
    }

    #[test]
    fn cli_accepts_query_value_options_with_space_syntax() {
        let cli = parse_with_default_mode(&[
            "query",
            "--host",
            "127.0.0.1",
            "--port",
            "7001",
            "--plugins-dir",
            "/tmp/plugins-space",
            "--enabled-plugins",
            "mock,codex",
            "--app-data-dir",
            "/tmp/data-space",
            "--plugin-overrides-dir",
            "/tmp/overrides-space",
            "--refresh-interval-secs",
            "11",
            "--log-level",
            "debug",
        ])
        .expect("query options with space syntax should parse");
        let log_level = cli.log_level;

        let query_args = match cli.command {
            Some(ModeCommand::Query(args)) => args,
            _ => panic!("expected query command"),
        };

        assert_eq!(query_args.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(query_args.port, Some(7001));
        assert_eq!(
            query_args.plugins_dir,
            Some(PathBuf::from("/tmp/plugins-space"))
        );
        assert_eq!(query_args.enabled_plugins.as_deref(), Some("mock,codex"));
        assert_eq!(
            query_args.app_data_dir,
            Some(PathBuf::from("/tmp/data-space"))
        );
        assert_eq!(
            query_args.plugin_overrides_dir,
            Some(PathBuf::from("/tmp/overrides-space"))
        );
        assert_eq!(query_args.refresh_interval_secs, Some(11));
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
            run_args.runtime.plugins_dir,
            Some(PathBuf::from("/tmp/plugins-daemon-eq"))
        );
        assert_eq!(run_args.runtime.enabled_plugins.as_deref(), Some("mock"));
        assert_eq!(
            run_args.runtime.app_data_dir,
            Some(PathBuf::from("/tmp/data-daemon-eq"))
        );
        assert_eq!(
            run_args.runtime.plugin_overrides_dir,
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
            run_args.runtime.plugins_dir,
            Some(PathBuf::from("/tmp/plugins-daemon-space"))
        );
        assert_eq!(run_args.runtime.enabled_plugins.as_deref(), Some("mock"));
        assert_eq!(
            run_args.runtime.app_data_dir,
            Some(PathBuf::from("/tmp/data-daemon-space"))
        );
        assert_eq!(
            run_args.runtime.plugin_overrides_dir,
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
        let err = parse_with_default_mode(&["install-systemd-unit", "--host", "127.0.0.1"])
            .expect_err("install-systemd-unit must reject query flags");
        assert!(err.to_string().contains("--host"));
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
        assert!(help.contains("--host <HOST>"));
        assert!(help.contains("--port <PORT>"));
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
        let runtime = RuntimeCli::from_sources(
            cli_without_mode(),
            EnvOverrides::default(),
            config::AppConfig::default(),
        )
        .expect("runtime defaults should resolve");

        assert_eq!(runtime.host, config::DEFAULT_HOST);
        assert_eq!(runtime.port, config::DEFAULT_PORT);
        assert_eq!(
            runtime.refresh_interval_secs,
            config::DEFAULT_REFRESH_INTERVAL_SECS
        );
        assert_eq!(runtime.enabled_plugins, config::DEFAULT_ENABLED_PLUGINS);
        assert_eq!(runtime.log_level, config::DEFAULT_LOG_LEVEL);
        assert_eq!(runtime.mode, RuntimeMode::Query);
        assert!(!runtime.foreground);
        assert_eq!(
            runtime.existing_instance_policy,
            ExistingInstancePolicy::Error
        );
        assert_eq!(runtime.service_mode, ServiceMode::Standalone);
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
        let runtime = RuntimeCli::from_sources(empty_cli(), EnvOverrides::default(), app_config)
            .expect("runtime config values should resolve");

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
        assert_eq!(runtime.log_level, "debug");
        assert_eq!(runtime.mode, RuntimeMode::Query);
        assert!(runtime.foreground);
        assert_eq!(
            runtime.existing_instance_policy,
            ExistingInstancePolicy::Ignore
        );
    }

    #[test]
    fn runtime_cli_prioritizes_cli_values_over_config() {
        let cli = Cli {
            log_level: Some(LogLevel::Trace),
            _help: None,
            _version: None,
            test_mode: false,
            command: Some(ModeCommand::RunDaemon(RunDaemonArgs {
                runtime: QueryArgs {
                    host: Some("127.0.0.2".to_string()),
                    port: Some(7001),
                    plugins_dir: Some(PathBuf::from("/cli/plugins")),
                    enabled_plugins: Some("mock".to_string()),
                    app_data_dir: Some(PathBuf::from("/cli/data")),
                    plugin_overrides_dir: Some(PathBuf::from("/cli/overrides")),
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

        let runtime = RuntimeCli::from_sources(cli, EnvOverrides::default(), app_config)
            .expect("runtime CLI values should resolve");

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
        assert_eq!(runtime.log_level, "trace");
        assert_eq!(runtime.mode, RuntimeMode::RunDaemon);
        assert!(runtime.foreground);
        assert_eq!(
            runtime.existing_instance_policy,
            ExistingInstancePolicy::Replace
        );
        assert_eq!(runtime.service_mode, ServiceMode::Systemd);
    }

    #[test]
    fn runtime_cli_defaults_to_query_mode_without_explicit_command() {
        let runtime = RuntimeCli::from_sources(
            cli_without_mode(),
            EnvOverrides::default(),
            config::AppConfig::default(),
        )
        .expect("runtime mode should resolve");

        assert_eq!(runtime.mode, RuntimeMode::Query);
    }

    #[test]
    fn runtime_cli_uses_run_daemon_mode_when_selected() {
        let cli = Cli {
            _help: None,
            _version: None,
            command: Some(ModeCommand::RunDaemon(RunDaemonArgs {
                runtime: QueryArgs::default(),
                foreground: None,
                existing_instance: None,
                service_mode: None,
                daemon_child: false,
            })),
            ..empty_cli()
        };

        let runtime =
            RuntimeCli::from_sources(cli, EnvOverrides::default(), config::AppConfig::default())
                .expect("runtime mode should resolve");

        assert_eq!(runtime.mode, RuntimeMode::RunDaemon);
    }

    #[test]
    fn runtime_cli_rejects_invalid_config_existing_instance_value() {
        let app_config = config::AppConfig {
            existing_instance: Some("invalid".to_string()),
            ..config::AppConfig::default()
        };

        let err = RuntimeCli::from_sources(empty_cli(), EnvOverrides::default(), app_config)
            .expect_err("invalid existing_instance value must be rejected");
        assert!(err.to_string().contains("invalid existing_instance"));
    }

    #[test]
    fn should_spawn_daemon_parent_respects_mode_and_foreground() {
        let mut runtime = RuntimeCli::from_sources(
            empty_cli(),
            EnvOverrides::default(),
            config::AppConfig::default(),
        )
        .expect("runtime values should resolve");

        runtime.mode = RuntimeMode::RunDaemon;
        runtime.daemon_child = false;
        runtime.foreground = false;
        assert!(should_spawn_daemon_parent(&runtime));

        runtime.foreground = true;
        assert!(!should_spawn_daemon_parent(&runtime));
        runtime.foreground = false;

        runtime.mode = RuntimeMode::Query;
        assert!(!should_spawn_daemon_parent(&runtime));

        runtime.mode = RuntimeMode::RunDaemon;
        runtime.daemon_child = true;
        assert!(!should_spawn_daemon_parent(&runtime));
    }

    #[test]
    fn env_overrides_parse_expected_runtime_variables() {
        let env = EnvOverrides::from_reader(|name| match name {
            "OPENUSAGE_PLUGINS_DIR" => Some(OsString::from("/env/plugins")),
            "OPENUSAGE_ENABLED_PLUGINS" => Some(OsString::from("mock,codex")),
            "OPENUSAGE_APP_DATA_DIR" => Some(OsString::from("/env/data")),
            "OPENUSAGE_PLUGIN_OVERRIDES_DIR" => Some(OsString::from("/env/overrides")),
            "OPENUSAGE_LOG_LEVEL" => Some(OsString::from("warn")),
            _ => None,
        });

        assert_eq!(env.plugins_dir, Some(PathBuf::from("/env/plugins")));
        assert_eq!(env.enabled_plugins.as_deref(), Some("mock,codex"));
        assert_eq!(env.app_data_dir, Some(PathBuf::from("/env/data")));
        assert_eq!(
            env.plugin_overrides_dir,
            Some(PathBuf::from("/env/overrides"))
        );
        assert_eq!(env.log_level.as_deref(), Some("warn"));
    }

    #[test]
    fn runtime_cli_uses_env_overrides_between_cli_and_config() {
        let cli = empty_cli();
        let env = EnvOverrides {
            plugins_dir: Some(PathBuf::from("/env/plugins")),
            enabled_plugins: Some("mock".to_string()),
            app_data_dir: Some(PathBuf::from("/env/data")),
            plugin_overrides_dir: Some(PathBuf::from("/env/overrides")),
            log_level: Some("info".to_string()),
        };
        let app_config = config::AppConfig {
            plugins_dir: Some(PathBuf::from("/cfg/plugins")),
            enabled_plugins: Some(vec!["codex".to_string()]),
            app_data_dir: Some(PathBuf::from("/cfg/data")),
            plugin_overrides_dir: Some(PathBuf::from("/cfg/overrides")),
            log_level: Some("debug".to_string()),
            ..config::AppConfig::default()
        };

        let runtime = RuntimeCli::from_sources(cli, env, app_config)
            .expect("runtime env values should resolve");

        assert_eq!(runtime.plugins_dir, Some(PathBuf::from("/env/plugins")));
        assert_eq!(runtime.enabled_plugins, "mock");
        assert_eq!(runtime.app_data_dir, Some(PathBuf::from("/env/data")));
        assert_eq!(
            runtime.plugin_overrides_dir,
            Some(PathBuf::from("/env/overrides"))
        );
        assert_eq!(runtime.log_level, "info");
    }

    #[test]
    fn runtime_cli_rejects_invalid_env_log_level() {
        let cli = empty_cli();
        let env = EnvOverrides {
            log_level: Some("inof".to_string()),
            ..EnvOverrides::default()
        };

        let err = RuntimeCli::from_sources(cli, env, config::AppConfig::default())
            .expect_err("invalid env log level must be rejected");
        assert!(err.to_string().contains("invalid log level"));
    }

    #[test]
    fn runtime_cli_rejects_invalid_config_log_level() {
        let cli = empty_cli();
        let app_config = config::AppConfig {
            log_level: Some("loud".to_string()),
            ..config::AppConfig::default()
        };

        let err = RuntimeCli::from_sources(cli, EnvOverrides::default(), app_config)
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
    fn build_systemd_unit_uses_expected_restart_policy() {
        let unit = build_systemd_unit("/usr/bin/openusage-cli run-daemon --foreground=true");

        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("RestartSec=2s"));
        assert!(unit.contains("SuccessExitStatus=75"));
        assert!(unit.contains("RestartForceExitStatus=75"));
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
    async fn run_past_reset_retry_loop_does_not_retry_without_proactive_trigger() {
        let daemon = daemon_with_stale_reset_plugin();
        daemon
            .refresh(None)
            .await
            .expect("initial refresh should seed stale reset data");

        let attempts = run_past_reset_retry_loop(
            &daemon,
            None,
            0,
            Duration::from_millis(20),
            Duration::from_millis(120),
        )
        .await;

        assert_eq!(
            attempts, 0,
            "without proactive trigger, retries must be disabled"
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
