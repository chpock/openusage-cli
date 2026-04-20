use openusage_cli::config::{DAEMON_ENDPOINT_FILE_NAME, RUNTIME_DIR_NAME};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

struct DaemonProcess {
    child: Child,
    terminated: bool,
}

impl DaemonProcess {
    fn spawn(workspace_root: &Path, home_dir: &Path, app_data_dir: &Path) -> Self {
        let daemon_bin = PathBuf::from(env!("CARGO_BIN_EXE_openusage-cli"));
        let plugins_dir = workspace_root.join("vendor/openusage/plugins");

        let child = Command::new(daemon_bin)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--daemon=false")
            .arg("--refresh-interval-secs")
            .arg("0")
            .arg("--test-mode")
            .arg("--plugins-dir")
            .arg(plugins_dir)
            .arg("--enabled-plugins")
            .arg("mock")
            .arg("--app-data-dir")
            .arg(app_data_dir)
            .env("HOME", home_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn openusage-cli daemon");

        Self {
            child,
            terminated: false,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn terminate_gracefully(&mut self) {
        #[cfg(unix)]
        {
            let status = Command::new("kill")
                .arg("-TERM")
                .arg(self.child.id().to_string())
                .status()
                .expect("send SIGTERM to daemon process");
            assert!(status.success(), "kill -TERM must succeed");
        }

        #[cfg(not(unix))]
        {
            self.child.kill().expect("terminate daemon process");
        }

        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll daemon process exit") {
                assert!(
                    status.success(),
                    "daemon process exited with non-zero status"
                );
                self.terminated = true;
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }

        self.child.kill().expect("force kill daemon process");
        let status = self
            .child
            .wait()
            .expect("wait for force-killed daemon process");
        panic!("daemon did not stop after graceful shutdown request, final status: {status}");
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        if self.terminated {
            return;
        }
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn daemon_writes_single_endpoint_file_and_serves_health_without_user_config() {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temp = tempfile::tempdir().expect("temp dir");
    let home_dir = temp.path().join("home");
    let app_data_dir = temp.path().join("app-data");

    fs::create_dir_all(&home_dir).expect("create HOME dir");
    fs::create_dir_all(&app_data_dir).expect("create app data dir");

    let fake_user_config = home_dir
        .join(".config/openusage/openusage-cli")
        .join("config.yaml");
    fs::create_dir_all(
        fake_user_config
            .parent()
            .expect("fake user config parent dir"),
    )
    .expect("create fake user config parent dir");
    fs::write(&fake_user_config, "not-valid: [yaml\n")
        .expect("write intentionally invalid fake user config");

    let mut daemon = DaemonProcess::spawn(&workspace_root, &home_dir, &app_data_dir);

    let endpoint_path = app_data_dir
        .join(RUNTIME_DIR_NAME)
        .join(DAEMON_ENDPOINT_FILE_NAME);
    wait_for_endpoint_file(&endpoint_path, daemon.child_mut());
    let endpoint_url = read_endpoint_url(&endpoint_path);
    assert!(
        endpoint_url.starts_with("http://"),
        "daemon endpoint must include scheme"
    );

    wait_for_health_ok(&endpoint_url, daemon.child_mut());

    daemon.terminate_gracefully();
    assert!(
        !endpoint_path.exists(),
        "daemon endpoint file must be removed on graceful shutdown"
    );
}

fn wait_for_endpoint_file(endpoint_path: &Path, child: &mut Child) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        assert_process_still_running(child);
        if endpoint_path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "timed out waiting for daemon-endpoint at {}",
        endpoint_path.display()
    );
}

fn wait_for_health_ok(endpoint_url: &str, child: &mut Child) {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .expect("build HTTP client");

    while Instant::now() < deadline {
        assert_process_still_running(child);
        let health_url = format!("{endpoint_url}/health");
        if let Ok(response) = client.get(&health_url).send()
            && response.status().is_success()
        {
            let body: Value = response.json().expect("parse health response JSON");
            assert_eq!(body["status"], Value::String("ok".to_string()));
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }

    panic!("timed out waiting for healthy daemon at {endpoint_url}");
}

fn read_endpoint_url(path: &Path) -> String {
    let raw = fs::read_to_string(path).expect("read daemon endpoint file");
    raw.trim().to_string()
}

fn assert_process_still_running(child: &mut Child) {
    if let Some(status) = child.try_wait().expect("poll daemon process status") {
        panic!("daemon process exited unexpectedly: {status}");
    }
}
