use super::*;
use indoc::formatdoc;
use std::ffi::OsStr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

pub(super) fn install_user_systemd_unit() -> Result<()> {
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
        let unit_existed = unit_path.is_file();
        let executable = std::env::current_exe().context("cannot resolve current executable")?;
        let exec_start = systemd_exec_start(executable.as_os_str());
        let unit_content = build_systemd_unit(&exec_start);

        std::fs::write(&unit_path, unit_content)
            .with_context(|| format!("failed to write unit file {}", unit_path.display()))?;

        print!("{}", systemd_unit_install_message(&unit_path, unit_existed));

        Ok(())
    }
}

pub(super) fn systemd_unit_install_message(unit_path: &Path, unit_existed: bool) -> String {
    let unit_file = unit_path.display().to_string();
    let service_name = USER_SYSTEMD_SERVICE_NAME;

    if unit_existed {
        return formatdoc! {"
            Systemd user unit updated.
            Updated files:
              - {unit_file}
            Next commands to apply the updated unit:
              - systemctl --user daemon-reload
              - systemctl --user enable --now {service_name}
              - systemctl --user restart {service_name}
              - systemctl --user status {service_name}
            Service logs:
              - journalctl --user -u {service_name} -f
            ",
            unit_file = unit_file,
            service_name = service_name,
        };
    }

    formatdoc! {"
        Systemd user unit installed.
        Created files:
          - {unit_file}
        Next commands:
          - systemctl --user daemon-reload
          - systemctl --user enable --now {service_name}
          - systemctl --user status {service_name}
        Service logs:
          - journalctl --user -u {service_name} -f
        ",
        unit_file = unit_file,
        service_name = service_name,
    }
}

pub(super) fn build_systemd_unit(exec_start: &str) -> String {
    formatdoc! {"
        [Unit]
        Description=OpenUsage CLI daemon
        After=network.target

        [Service]
        Type=notify
        NotifyAccess=main
        WatchdogSec={SYSTEMD_WATCHDOG_SEC}s
        TimeoutStartSec={SYSTEMD_TIMEOUT_START_SECS}s
        ExecStart={exec_start}
        Restart=on-failure
        RestartSec=2s
        SuccessExitStatus={SYSTEMD_RESTART_EXIT_CODE}
        RestartForceExitStatus={SYSTEMD_RESTART_EXIT_CODE}

        [Install]
        WantedBy=default.target
    "}
}

pub(super) fn quote_systemd_argument(value: &OsStr) -> String {
    let raw = value.to_string_lossy();
    if raw.chars().all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '=' | ',')
    }) {
        return raw.to_string();
    }

    let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

pub(super) fn systemd_exec_start(executable: &OsStr) -> String {
    [
        quote_systemd_argument(executable),
        CMD_RUN_DAEMON.to_string(),
        "--foreground=true".to_string(),
        "--service-mode=systemd".to_string(),
        "--log-level=info".to_string(),
    ]
    .join(" ")
}

pub(super) fn notify_systemd_status(service_mode: ServiceMode, status: &str) {
    if service_mode != ServiceMode::Systemd {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        let has_notify_socket = std::env::var_os("NOTIFY_SOCKET").is_some();
        match sd_notify::notify(false, &[sd_notify::NotifyState::Status(status)]) {
            Ok(()) if has_notify_socket => log::debug!("sent systemd status: {}", status),
            Ok(()) => log::debug!(
                "service_mode=systemd but NOTIFY_SOCKET is unset; skipping STATUS update: {}",
                status
            ),
            Err(err) => log::warn!("failed to send STATUS via sd_notify ({}): {}", status, err),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        log::warn!(
            "service_mode=systemd requested on a non-Linux platform; status update skipped: {}",
            status
        );
    }
}

pub(super) async fn wait_for_http_server_readiness(bound_addr: SocketAddr) -> Result<()> {
    let readiness_addr = http_readiness_probe_addr(bound_addr);
    let readiness_url = format!("http://{}/health", readiness_addr);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .context("failed to create readiness probe HTTP client")?;
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(STARTUP_READINESS_TIMEOUT_SECS);

    loop {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timeout waiting for health endpoint {} during daemon startup",
                readiness_url
            );
        }

        if let Ok(response) = client.get(&readiness_url).send().await
            && response.status().is_success()
        {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(STARTUP_READINESS_POLL_INTERVAL_MS)).await;
    }
}

pub(super) fn http_readiness_probe_addr(bound_addr: SocketAddr) -> SocketAddr {
    let ip = match bound_addr.ip() {
        IpAddr::V4(ipv4) if ipv4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ipv6) if ipv6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };

    SocketAddr::new(ip, bound_addr.port())
}

pub(super) fn notify_systemd_ready(service_mode: ServiceMode) -> Result<()> {
    if service_mode != ServiceMode::Systemd {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        let has_notify_socket = std::env::var_os("NOTIFY_SOCKET").is_some();
        sd_notify::notify(false, &[sd_notify::NotifyState::Ready])
            .context("failed to send READY=1 via sd_notify")?;
        if has_notify_socket {
            log::info!("sent READY=1 to systemd");
        } else {
            log::debug!(
                "service_mode=systemd but NOTIFY_SOCKET is unset; skipping READY=1 notification"
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        log::warn!(
            "service_mode=systemd requested on a non-Linux platform; readiness notification skipped"
        );
    }

    Ok(())
}

pub(super) fn notify_systemd_stopping(service_mode: ServiceMode) {
    if service_mode != ServiceMode::Systemd {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        let has_notify_socket = std::env::var_os("NOTIFY_SOCKET").is_some();
        match sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]) {
            Ok(()) if has_notify_socket => log::info!("sent STOPPING=1 to systemd"),
            Ok(()) => log::debug!(
                "service_mode=systemd but NOTIFY_SOCKET is unset; skipping STOPPING=1 notification"
            ),
            Err(err) => {
                log::warn!("failed to send STOPPING=1 via sd_notify: {}", err)
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        log::warn!(
            "service_mode=systemd requested on a non-Linux platform; stop notification skipped"
        );
    }
}

pub(super) fn spawn_systemd_watchdog_task(
    service_mode: ServiceMode,
) -> Option<tokio::task::JoinHandle<()>> {
    let ping_interval = systemd_watchdog_ping_interval(service_mode)?;
    log::info!(
        "systemd watchdog enabled; sending WATCHDOG=1 every {:?}",
        ping_interval
    );

    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(ping_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        ticker.tick().await;

        loop {
            ticker.tick().await;

            #[cfg(target_os = "linux")]
            {
                if let Err(err) = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]) {
                    log::warn!("failed to send WATCHDOG=1 via sd_notify: {}", err);
                } else {
                    log::debug!("sent WATCHDOG=1 to systemd");
                }
            }
        }
    }))
}

pub(super) fn systemd_watchdog_ping_interval(service_mode: ServiceMode) -> Option<Duration> {
    if service_mode != ServiceMode::Systemd {
        return None;
    }

    #[cfg(target_os = "linux")]
    {
        let raw_value = std::env::var("WATCHDOG_USEC").ok()?;
        let watchdog_usec: u64 = match raw_value.trim().parse() {
            Ok(value) => value,
            Err(err) => {
                log::warn!(
                    "service_mode=systemd but WATCHDOG_USEC='{}' is invalid: {}; watchdog disabled",
                    raw_value,
                    err
                );
                return None;
            }
        };

        if watchdog_usec == 0 {
            return None;
        }

        let ping_usec = (watchdog_usec / 2).max(1);
        Some(Duration::from_micros(ping_usec))
    }

    #[cfg(not(target_os = "linux"))]
    {
        log::warn!(
            "service_mode=systemd requested on a non-Linux platform; watchdog support skipped"
        );
        None
    }
}
