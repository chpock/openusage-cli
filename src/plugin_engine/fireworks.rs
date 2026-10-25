//! Fireworks billing exports through an installed, isolated firectl job.

use super::process_io;
use rquickjs::{Ctx, Exception, Function, Object};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const FIRECTL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ExportOptions {
    api_key: String,
    account_id: String,
    start_time: String,
    end_time: String,
}

impl ExportOptions {
    fn parse(raw: &str) -> Option<Self> {
        let options: Self = serde_json::from_str(raw).ok()?;
        for value in [
            &options.api_key,
            &options.account_id,
            &options.start_time,
            &options.end_time,
        ] {
            if value.trim().is_empty() || value.trim().chars().any(char::is_control) {
                return None;
            }
        }
        Some(options)
    }
}

struct Exporter {
    candidates: Vec<PathBuf>,
    environment: Vec<(OsString, OsString)>,
    temp_root: PathBuf,
    timeout: Duration,
}

impl Exporter {
    fn native() -> Self {
        Self {
            candidates: [
                "firectl",
                "/opt/homebrew/bin/firectl",
                "/usr/local/bin/firectl",
            ]
            .map(PathBuf::from)
            .to_vec(),
            environment: helper_environment(
                std::env::vars_os(),
                crate::config::helper_proxy_environment(),
            ),
            temp_root: std::env::temp_dir(),
            timeout: FIRECTL_TIMEOUT,
        }
    }

    fn export(&self, raw: &str) -> Value {
        let Some(options) = ExportOptions::parse(raw) else {
            return json!({"status":"invalid_opts"});
        };
        let mut workspace = tempfile::Builder::new();
        workspace.prefix("openusage-fireworks-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            workspace.permissions(fs::Permissions::from_mode(0o700));
        }
        let Ok(workspace) = workspace.tempdir_in(&self.temp_root) else {
            return json!({"status":"runner_failed"});
        };
        let result = self
            .export_in(&options, workspace.path())
            .unwrap_or_else(|_| json!({"status":"runner_failed"}));
        if workspace.close().is_err() {
            return json!({"status":"runner_failed"});
        }
        result
    }

    fn command(&self, program: &Path, root: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .envs(self.environment.iter().cloned())
            .current_dir(root);
        command.env("HOME", root);
        for (name, directory) in PRIVATE_DIRECTORIES {
            command.env(name, root.join(directory));
        }
        command
    }

    fn export_in(&self, options: &ExportOptions, root: &Path) -> io::Result<Value> {
        for (_, directory) in PRIVATE_DIRECTORIES {
            private_directory(&root.join(directory))?;
        }
        let program = self.candidates.iter().find(|candidate| {
            let mut command = self.command(candidate, root);
            command.arg("--help");
            process_io::run_command(&mut command, None, self.timeout)
                .is_ok_and(|output| output.status.success())
        });
        let Some(program) = program else {
            return Ok(json!({"status":"no_runner"}));
        };

        private_directory(&root.join(".fireworks"))?;
        let mut auth = OpenOptions::new();
        auth.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            auth.mode(0o600);
        }
        auth.open(root.join(".fireworks/auth.ini"))?
            .write_all(format!("[fireworks]\napi_key = {}\n", options.api_key.trim()).as_bytes())?;
        let filename = format!(
            "{}.csv",
            root.file_name()
                .expect("temporary directory has a name")
                .to_string_lossy()
        );
        let mut command = self.command(program, root);
        command
            .env("FIREWORKS_API_KEY", options.api_key.trim())
            .args([
                "billing",
                "export-metrics",
                "--account-id",
                options.account_id.trim(),
                "--start-time",
                options.start_time.trim(),
                "--end-time",
                options.end_time.trim(),
                "--filename",
                &filename,
            ]);
        // Never log helper output: even a failed CLI can echo credentials.
        match process_io::run_command(&mut command, None, self.timeout) {
            Ok(output) if output.status.success() => {
                match fs::read_to_string(root.join(filename)) {
                    Ok(csv) if !csv.trim().is_empty() => Ok(json!({"status":"ok","csv":csv})),
                    _ => Ok(json!({"status":"empty"})),
                }
            }
            Ok(_) => Ok(json!({"status":"runner_failed"})),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                Ok(json!({"status":"timed_out"}))
            }
            Err(_) => Ok(json!({"status":"runner_failed"})),
        }
    }
}

const PRIVATE_DIRECTORIES: [(&str, &str); 7] = [
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_RUNTIME_DIR", "run"),
    ("TMPDIR", "tmp"),
    ("TEMPDIR", "temp"),
];

fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn helper_environment(
    input: impl IntoIterator<Item = (OsString, OsString)>,
    proxy_environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    input
        .into_iter()
        .filter(|(name, _)| {
            matches!(
                name.to_str(),
                Some("PATH" | "SSL_CERT_FILE" | "SSL_CERT_DIR")
            )
        })
        .chain(proxy_environment)
        .collect()
}

pub(super) fn inject<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    sandboxed: bool,
) -> rquickjs::Result<()> {
    inject_exporter(ctx, host, None, sandboxed)
}

fn inject_exporter<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    exporter: Option<Exporter>,
    sandboxed: bool,
) -> rquickjs::Result<()> {
    let object = Object::new(ctx.clone())?;
    let raw = Function::new(ctx.clone(), move |ctx: Ctx<'_>, raw: String| {
        if sandboxed {
            return Err(Exception::throw_message(
                &ctx,
                "Fireworks billing export is disabled in sandbox",
            ));
        }
        let native;
        let exporter = match exporter.as_ref() {
            Some(exporter) => exporter,
            None => {
                native = Exporter::native();
                &native
            }
        };
        Ok::<_, rquickjs::Error>(exporter.export(&raw).to_string())
    })?;
    let wrap: Function = ctx.eval(r#"(function(raw) { return function(opts) { return JSON.parse(raw(JSON.stringify(opts === undefined ? {} : opts))); }; })"#)?;
    object.set(
        "exportBillingMetrics",
        wrap.call::<_, Function>((raw.clone(),))?,
    )?;
    object.set("_exportBillingMetricsRaw", raw)?;
    host.set("fireworks", object)
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
