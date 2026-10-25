#[path = "support/hermetic.rs"]
#[allow(dead_code)]
mod hermetic;

use hermetic::HermeticPaths;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn query(binary: &Path, paths: &HermeticPaths, query_type: &str) -> Value {
    let output = Command::new(binary)
        .args(["query", "--use-daemon=false", "--enabled-plugins=mock"])
        .arg(format!("--type={query_type}"))
        .arg("--app-data-dir")
        .arg(&paths.app_data)
        .arg("--plugin-overrides-dir")
        .arg(&paths.overrides)
        .current_dir(&paths.root)
        .output()
        .expect("run plugin discovery query");
    assert!(
        output.status.success(),
        "query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse query JSON")
}

#[test]
fn installed_binary_discovers_plugins_in_vendor_neutral_directory() {
    let Some(paths) =
        hermetic::enter("installed_binary_discovers_plugins_in_vendor_neutral_directory")
    else {
        return;
    };
    let prefix = paths.root.join("prefix");
    let bin_dir = prefix.join("bin");
    fs::create_dir_all(&bin_dir).expect("create installed binary directory");
    let binary = bin_dir.join("openusage-cli");
    fs::copy(env!("CARGO_BIN_EXE_openusage-cli"), &binary).expect("copy installed binary");

    let plugins_dir = prefix.join("share/openusage-cli/plugins");
    let mock_dir = plugins_dir.join("mock");
    fs::create_dir_all(&mock_dir).expect("create installed plugin directory");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/crossusage/plugins/mock");
    for name in ["plugin.json", "plugin.js", "icon.svg"] {
        fs::copy(source.join(name), mock_dir.join(name)).expect("copy mock plugin file");
    }
    // An old installation must not take precedence over the vendor-neutral path.
    fs::create_dir_all(prefix.join("share/openusage-cli/openusage-plugins"))
        .expect("create legacy plugin directory");

    let config = query(&binary, &paths, "config");
    assert_eq!(config["pluginsDir"], plugins_dir.to_string_lossy().as_ref());
    let plugins = query(&binary, &paths, "plugins");
    let plugins = plugins.as_array().expect("plugin metadata array");
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["id"], "mock");
}

#[test]
fn source_binary_discovers_crossusage_plugins_from_unrelated_working_directory() {
    let Some(paths) = hermetic::enter(
        "source_binary_discovers_crossusage_plugins_from_unrelated_working_directory",
    ) else {
        return;
    };
    let binary = Path::new(env!("CARGO_BIN_EXE_openusage-cli"));
    let expected = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/crossusage/plugins");
    let config = query(binary, &paths, "config");
    assert_eq!(config["pluginsDir"], expected.to_string_lossy().as_ref());
    let plugins = query(binary, &paths, "plugins");
    let plugins = plugins.as_array().expect("plugin metadata array");
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["id"], "mock");
}

#[test]
fn installed_binary_reports_png_icon_from_pinned_plugin() {
    let Some(paths) = hermetic::enter("installed_binary_reports_png_icon_from_pinned_plugin")
    else {
        return;
    };
    let prefix = paths.root.join("prefix");
    let bin_dir = prefix.join("bin");
    fs::create_dir_all(&bin_dir).expect("create installed binary directory");
    let binary = bin_dir.join("openusage-cli");
    fs::copy(env!("CARGO_BIN_EXE_openusage-cli"), &binary).expect("copy installed binary");
    let installed = prefix.join("share/openusage-cli/plugins/cursor-nightly");
    fs::create_dir_all(&installed).expect("create installed plugin directory");
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/crossusage/plugins/cursor-nightly");
    for name in ["plugin.json", "plugin.js", "icon.png"] {
        fs::copy(source.join(name), installed.join(name)).expect("copy pinned plugin file");
    }
    let output = Command::new(&binary)
        .args([
            "query",
            "--use-daemon=false",
            "--enabled-plugins=cursor-nightly",
            "--type=plugins",
        ])
        .arg("--app-data-dir")
        .arg(&paths.app_data)
        .arg("--plugin-overrides-dir")
        .arg(&paths.overrides)
        .current_dir(&paths.root)
        .output()
        .expect("run installed metadata query");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plugins: Value = serde_json::from_slice(&output.stdout).expect("parse metadata JSON");
    let plugins = plugins.as_array().expect("metadata array");
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0]["id"], "cursor-nightly");
    use base64::Engine as _;
    let expected = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD
            .encode(fs::read(source.join("icon.png")).expect("read pinned PNG fixture"))
    );
    assert_eq!(plugins[0]["iconUrl"], expected);
}
