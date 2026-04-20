use crate::config::{self, DaemonEndpointPath};
use anyhow::{Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct PublishedDiscovery {
    endpoint_file: PathBuf,
    endpoint_url: String,
}

impl PublishedDiscovery {
    pub fn publish(bind_addr: SocketAddr, runtime_dir_override: Option<&Path>) -> Result<Self> {
        let endpoint_path = match runtime_dir_override {
            Some(dir) => DaemonEndpointPath {
                endpoint_file: dir.join(config::DAEMON_ENDPOINT_FILE_NAME),
                dir: dir.to_path_buf(),
            },
            None => {
                config::daemon_endpoint_path().context("failed to resolve daemon endpoint path")?
            }
        };
        publish_with_path(endpoint_path, bind_addr)
    }

    pub fn endpoint_file(&self) -> &Path {
        &self.endpoint_file
    }

    pub fn base_url(&self) -> &str {
        &self.endpoint_url
    }
}

impl Drop for PublishedDiscovery {
    fn drop(&mut self) {
        remove_discovery_file(&self.endpoint_file);
    }
}

fn publish_with_path(
    endpoint_path: DaemonEndpointPath,
    bind_addr: SocketAddr,
) -> Result<PublishedDiscovery> {
    std::fs::create_dir_all(&endpoint_path.dir).with_context(|| {
        format!(
            "failed to create daemon endpoint dir {}",
            endpoint_path.dir.display()
        )
    })?;
    set_discovery_dir_permissions_if_supported(&endpoint_path.dir)?;

    let endpoint_url =
        format_http_base_url(&connect_host_for_bind_ip(bind_addr.ip()), bind_addr.port());
    let file_payload = format!("{endpoint_url}\n");
    write_file_atomic(&endpoint_path.endpoint_file, file_payload.as_bytes()).with_context(
        || {
            format!(
                "failed to write daemon endpoint file {}",
                endpoint_path.endpoint_file.display()
            )
        },
    )?;

    Ok(PublishedDiscovery {
        endpoint_file: endpoint_path.endpoint_file,
        endpoint_url,
    })
}

fn connect_host_for_bind_ip(bind_ip: IpAddr) -> String {
    match bind_ip {
        IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.to_string(),
        IpAddr::V6(ip) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.to_string(),
        _ => bind_ip.to_string(),
    }
}

fn format_http_base_url(host: &str, port: u16) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("http://{host}:{port}")
}

fn write_file_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("path has no parent: {}", path.display()))?;
    let temp_path = temp_file_path(parent, path);

    std::fs::write(&temp_path, contents)
        .with_context(|| format!("failed to write temp file {}", temp_path.display()))?;
    set_discovery_file_permissions_if_supported(&temp_path)?;

    std::fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to replace {} with {}",
            path.display(),
            temp_path.display()
        )
    })?;
    Ok(())
}

fn temp_file_path(parent: &Path, destination: &Path) -> PathBuf {
    let nanos = unix_timestamp_nanos();
    let file_name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("daemon-endpoint.tmp");

    parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nanos))
}

fn unix_timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(unix)]
fn set_discovery_dir_permissions_if_supported(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to set permissions for {}", dir.display()))
}

#[cfg(not(unix))]
fn set_discovery_dir_permissions_if_supported(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_discovery_file_permissions_if_supported(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to set permissions for {}", path.display()))
}

#[cfg(not(unix))]
fn set_discovery_file_permissions_if_supported(_path: &Path) -> Result<()> {
    Ok(())
}

fn remove_discovery_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => log::warn!(
            "failed to remove daemon endpoint file {} during shutdown: {}",
            path.display(),
            err
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_endpoint_path(base: &Path) -> DaemonEndpointPath {
        let dir = base.join("runtime");
        DaemonEndpointPath {
            endpoint_file: dir.join("daemon-endpoint"),
            dir,
        }
    }

    #[test]
    fn publish_writes_endpoint_url_and_removes_file_on_drop() {
        let temp = tempdir().expect("temp dir");
        let endpoint_path = test_endpoint_path(temp.path());
        let addr: SocketAddr = "127.0.0.1:6737".parse().expect("socket address");

        let handle = publish_with_path(endpoint_path.clone(), addr).expect("publish endpoint");

        let file_contents =
            std::fs::read_to_string(&endpoint_path.endpoint_file).expect("read endpoint file");
        assert_eq!(file_contents, "http://127.0.0.1:6737\n");
        assert_eq!(handle.base_url(), "http://127.0.0.1:6737");

        drop(handle);
        assert!(!endpoint_path.endpoint_file.exists());
    }

    #[test]
    fn unspecified_bind_host_maps_to_localhost_connect_url() {
        let temp = tempdir().expect("temp dir");
        let endpoint_path = test_endpoint_path(temp.path());
        let addr: SocketAddr = "0.0.0.0:6737".parse().expect("socket address");

        let handle = publish_with_path(endpoint_path.clone(), addr).expect("publish endpoint");

        let file_contents =
            std::fs::read_to_string(&endpoint_path.endpoint_file).expect("read endpoint file");
        assert_eq!(file_contents, "http://127.0.0.1:6737\n");
        assert_eq!(handle.base_url(), "http://127.0.0.1:6737");
    }

    #[test]
    fn ipv6_base_url_is_bracketed() {
        let result = format_http_base_url("::1", 6737);
        assert_eq!(result, "http://[::1]:6737");
    }
}
