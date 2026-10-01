mod support;

use openusage_cli::config;
use openusage_cli::config::{AuthSource, DEFAULT_AUTH_SOURCES};
use openusage_cli::plugin_engine::runtime::{MetricLine, ProbeResult};
use serde_json::{Value, json};
use std::fs;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use support::hermetic::{self, HermeticPaths};
use support::override_runner::{
    ProviderOutcome, ProviderSpec, run_provider_probe, run_provider_probe_with_auth_sources,
};

const DISCOVERY_PLUGIN: &str = r#"
var probeCalls = [];
globalThis.__openusage_plugin = {
    discoverAccounts() {
        return [
            { id: "remote", origin: "opencode" },
            { id: "local" }
        ];
    },
    probe(ctx) {
        probeCalls.push(ctx.account.id);
        return { lines: [{ type: "text", label: "Calls", value: JSON.stringify(probeCalls) }] };
    }
};
"#;

const LEGACY_PLUGIN: &str = r#"
globalThis.__openusage_plugin = {
    probe() {
        return { lines: [{ type: "text", label: "Calls", value: '["default"]' }] };
    }
};
"#;

fn write_plugins(paths: &HermeticPaths, legacy_beta: bool) {
    for (id, script) in [
        ("alpha", DISCOVERY_PLUGIN),
        (
            "beta",
            if legacy_beta {
                LEGACY_PLUGIN
            } else {
                DISCOVERY_PLUGIN
            },
        ),
    ] {
        let dir = paths.root.join("plugins").join(id);
        fs::create_dir_all(&dir).expect("create plugin directory");
        fs::write(
            dir.join("plugin.json"),
            json!({
                "schemaVersion": 1, "id": id, "name": id,
                "version": "0.0.0", "entry": "plugin.js", "icon": "icon.svg", "lines": []
            })
            .to_string(),
        )
        .expect("write plugin manifest");
        fs::write(dir.join("plugin.js"), script).expect("write plugin script");
        fs::write(
            dir.join("icon.svg"),
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        )
        .expect("write plugin icon");
    }
}

fn write_config(contents: &str) {
    let path = config::config_path().expect("resolve hermetic config path");
    fs::create_dir_all(path.parent().expect("config parent")).expect("create config directory");
    fs::write(path, contents).expect("write config");
}

fn query(paths: &HermeticPaths, query_type: &str, args: &[&str]) -> Output {
    // Omit the command name to exercise value-taking option pre-parsing too.
    Command::new(env!("CARGO_BIN_EXE_openusage-cli"))
        .args(args)
        .arg("--plugins-dir")
        .arg(paths.root.join("plugins"))
        .arg("--app-data-dir")
        .arg(&paths.app_data)
        .arg("--plugin-overrides-dir")
        .arg(&paths.overrides)
        .arg("--use-daemon=false")
        .arg(format!("--type={query_type}"))
        .output()
        .expect("run local query")
}

fn query_json(paths: &HermeticPaths, query_type: &str, args: &[&str]) -> Value {
    let output = query(paths, query_type, args);
    assert!(
        output.status.success(),
        "query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("query JSON")
}

fn assert_account_calls(output: &Value, id: &str, origin: &str, calls: &[&str]) {
    assert_eq!(output["account"]["id"], id);
    assert_eq!(output["account"]["origin"], origin);
    let observed: Value =
        serde_json::from_str(output["lines"][0]["value"].as_str().expect("call counter"))
            .expect("call counter JSON");
    assert_eq!(observed, json!(calls), "unexpected probe calls");
}

#[test]
fn auth_sources_config_filters_before_probe() {
    let Some(paths) = hermetic::enter("auth_sources_config_filters_before_probe") else {
        return;
    };
    write_plugins(&paths, false);
    write_config("enabled_auth_sources:\n  default: [opencode]\n  beta: [native]\n");

    let usage = query_json(&paths, "usage", &[]);
    let accounts = usage.as_array().expect("usage array");
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0]["providerId"], "alpha");
    assert_account_calls(&accounts[0], "remote", "opencode", &["remote"]);
    assert_eq!(accounts[1]["providerId"], "beta");
    assert_account_calls(&accounts[1], "local", "native", &["local"]);

    let runtime = query_json(&paths, "config", &[]);
    assert_eq!(
        runtime["enabledAuthSources"],
        json!({ "default": ["opencode"], "beta": ["native"] })
    );
}

#[test]
fn auth_sources_cli_replaces_config_object() {
    let Some(paths) = hermetic::enter("auth_sources_cli_replaces_config_object") else {
        return;
    };
    write_plugins(&paths, false);
    write_config("enabled_auth_sources:\n  default: [native]\n  beta: []\n");
    let args = ["--enabled-auth-sources", r#"{"default":["opencode"]}"#];
    let usage = query_json(&paths, "usage", &args);
    let accounts = usage.as_array().expect("usage array");
    assert_eq!(accounts.len(), 2);
    for account in accounts {
        assert_account_calls(account, "remote", "opencode", &["remote"]);
    }
    assert_eq!(
        query_json(&paths, "config", &args)["enabledAuthSources"],
        json!({ "default": ["opencode"] })
    );
}

#[test]
fn auth_sources_missing_default_uses_builtin_sources() {
    let Some(paths) = hermetic::enter("auth_sources_missing_default_uses_builtin_sources") else {
        return;
    };
    write_plugins(&paths, false);
    write_config("enabled_auth_sources:\n  alpha: [opencode]\n");
    let usage = query_json(&paths, "usage", &[]);
    let accounts = usage.as_array().expect("usage array");
    assert_eq!(accounts.len(), 3);
    assert_account_calls(&accounts[0], "remote", "opencode", &["remote"]);
    assert_account_calls(&accounts[1], "remote", "opencode", &["remote"]);
    assert_account_calls(&accounts[2], "local", "native", &["remote", "local"]);
    assert_eq!(
        query_json(&paths, "config", &[])["enabledAuthSources"],
        json!({ "default": ["native", "opencode"], "alpha": ["opencode"] })
    );
}

#[test]
fn auth_sources_empty_list_and_native_only_legacy_plugin() {
    let Some(paths) = hermetic::enter("auth_sources_empty_list_and_native_only_legacy_plugin")
    else {
        return;
    };
    write_plugins(&paths, true);
    for policy in ["[]", "[opencode]", "[native]"] {
        write_config(&format!(
            "enabled_auth_sources:\n  default: {policy}\n  alpha: []\n"
        ));
        let usage = query_json(&paths, "usage", &[]);
        let accounts = usage.as_array().expect("usage array");
        if policy == "[native]" {
            assert_eq!(accounts.len(), 1);
            assert_eq!(accounts[0]["providerId"], "beta");
            assert_account_calls(&accounts[0], "default", "native", &["default"]);
        } else {
            assert!(
                accounts.is_empty(),
                "excluded accounts must not produce errors"
            );
        }
    }
}

#[test]
fn auth_sources_rejects_invalid_config_values() {
    let Some(paths) = hermetic::enter("auth_sources_rejects_invalid_config_values") else {
        return;
    };
    write_plugins(&paths, false);
    for value in [
        "{default: [unknown]}",
        "{default: native}",
        "[native]",
        "{default: [123]}",
    ] {
        write_config(&format!("enabled_auth_sources: {value}\n"));
        let output = query(&paths, "config", &[]);
        assert!(
            !output.status.success(),
            "invalid setting was accepted: {value}"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("enabled_auth_sources"));
    }
}

#[test]
fn auth_sources_plugin_keys_are_validated_before_enabled_plugin_selection() {
    let Some(paths) =
        hermetic::enter("auth_sources_plugin_keys_are_validated_before_enabled_plugin_selection")
    else {
        return;
    };
    write_plugins(&paths, false);
    write_config("enabled_auth_sources:\n  default: [opencode]\n  beta: []\n");
    let usage = query_json(&paths, "usage", &["--enabled-plugins=alpha"]);
    assert_eq!(usage.as_array().expect("usage array").len(), 1);

    write_config("enabled_auth_sources:\n  unknown: [native]\n");
    let output = query(&paths, "config", &[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("enabled_auth_sources contains unknown plugin id 'unknown'")
    );
}

struct TestDaemon(Child);

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn auth_sources_daemon_serves_selected_accounts_and_effective_config() {
    let Some(paths) =
        hermetic::enter("auth_sources_daemon_serves_selected_accounts_and_effective_config")
    else {
        return;
    };
    write_plugins(&paths, false);
    write_config(
        "refresh_interval_secs: 0\nenabled_auth_sources:\n  default: [opencode]\n  beta: []\n",
    );
    let mut daemon = TestDaemon(
        Command::new(env!("CARGO_BIN_EXE_openusage-cli"))
            .args([
                "run-daemon",
                "--foreground=true",
                "--host=127.0.0.1",
                "--port=0",
            ])
            .arg("--plugins-dir")
            .arg(paths.root.join("plugins"))
            .arg("--app-data-dir")
            .arg(&paths.app_data)
            .arg("--plugin-overrides-dir")
            .arg(&paths.overrides)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start daemon"),
    );
    let endpoint_file = config::daemon_endpoint_path()
        .expect("endpoint path")
        .endpoint_file;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("HTTP client");
    let deadline = Instant::now() + Duration::from_secs(20);
    let endpoint = loop {
        assert!(
            daemon.0.try_wait().expect("daemon status").is_none(),
            "daemon exited during startup"
        );
        if let Ok(endpoint) = fs::read_to_string(&endpoint_file) {
            let endpoint = endpoint.trim().to_string();
            if client
                .get(format!("{endpoint}/health"))
                .send()
                .is_ok_and(|r| r.status().is_success())
            {
                break endpoint;
            }
        }
        assert!(Instant::now() < deadline, "daemon startup timeout");
        std::thread::sleep(Duration::from_millis(25));
    };

    let expected_config = json!({"default": ["opencode"], "beta": []});
    let runtime: Value = client
        .get(format!("{endpoint}/v1/config"))
        .send()
        .expect("config response")
        .error_for_status()
        .expect("config status")
        .json()
        .expect("config JSON");
    assert_eq!(runtime["enabledAuthSources"], expected_config);
    for path in ["/v1/usage", "/v1/usage?refresh=true"] {
        let usage: Value = client
            .get(format!("{endpoint}{path}"))
            .send()
            .expect("usage response")
            .error_for_status()
            .expect("usage status")
            .json()
            .expect("usage JSON");
        let accounts = usage.as_array().expect("usage array");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["providerId"], "alpha");
        assert_account_calls(&accounts[0], "remote", "opencode", &["remote"]);
    }
    assert_eq!(
        client
            .get(format!("{endpoint}/v1/usage/beta?refresh=true"))
            .send()
            .expect("empty provider response")
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    let probe: Value = client
        .post(format!("{endpoint}/v1/probe"))
        .json(&json!({"pluginIds": ["alpha", "beta"]}))
        .send()
        .expect("probe response")
        .error_for_status()
        .expect("probe status")
        .json()
        .expect("probe JSON");
    assert_eq!(probe.as_array().expect("probe array").len(), 1);
    assert_account_calls(&probe[0], "remote", "opencode", &["remote"]);

    // A query served by a running daemon reports that daemon's policy.
    let output = Command::new(env!("CARGO_BIN_EXE_openusage-cli"))
        .args([
            "query",
            "--type=config",
            "--use-daemon=true",
            "--enabled-auth-sources={default: [native]}",
        ])
        .output()
        .expect("query running daemon");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let runtime: Value = serde_json::from_slice(&output.stdout).expect("daemon query JSON");
    assert_eq!(runtime["enabledAuthSources"], expected_config);
}

fn probe_script(script: &str, sources: &[AuthSource]) -> (ProbeResult, Value) {
    let spec = ProviderSpec {
        plugin_id: "test",
        plugin_name: "Test",
        plugin_source: script,
        override_source: None,
        harness: "globalThis.__openusage_ctx = {}; globalThis.calls = []; globalThis.discoveryCalls = 0;",
        setup: "",
        assertion: Some(
            "JSON.stringify({ calls: globalThis.calls, discoveryCalls: globalThis.discoveryCalls })",
        ),
    };
    // Exercise the default runner as well as its explicit-source counterpart.
    let outcome = if sources == DEFAULT_AUTH_SOURCES {
        run_provider_probe(&spec)
    } else {
        run_provider_probe_with_auth_sources(&spec, sources)
    };
    let ProviderOutcome::Probe { result, assertion } = outcome;
    (result, assertion.expect("post-probe counters"))
}

#[test]
fn auth_sources_discovery_runs_once_and_only_enabled_accounts_are_probed() {
    let script = r#"
        globalThis.__openusage_plugin = {
            discoverAccounts() {
                discoveryCalls++;
                return [
                    { id: "native-a" },
                    { id: "opencode-a", origin: "opencode" },
                    { id: "future-a", origin: "future" },
                    { id: "native-b", origin: "native" }
                ];
            },
            async probe(ctx) {
                calls.push(ctx.account.id);
                return { lines: [{ type: "text", label: "Status", value: "ok" }] };
            }
        };
    "#;
    for (sources, expected) in [
        (
            DEFAULT_AUTH_SOURCES,
            vec!["native-a", "opencode-a", "native-b"],
        ),
        (&[AuthSource::Native][..], vec!["native-a", "native-b"]),
        (&[AuthSource::Opencode][..], vec!["opencode-a"]),
        (&[][..], vec![]),
    ] {
        let (result, observed) = probe_script(script, sources);
        assert_eq!(observed["discoveryCalls"], 1);
        assert_eq!(observed["calls"], json!(expected));
        let ids: Vec<&str> = result
            .outputs
            .iter()
            .map(|o| o.account.id.as_str())
            .collect();
        assert_eq!(ids, expected);
    }
}

#[test]
fn auth_sources_legacy_native_probe_is_skipped_even_if_it_would_report_opencode() {
    let script = r#"
        globalThis.__openusage_plugin = {
            probe(ctx) {
                calls.push(ctx.account.id);
                return {
                    account: {id: "remote", origin: "opencode"},
                    lines: [{type: "text", label: "Status", value: "ok"}]
                };
            }
        };
    "#;
    for sources in [&[AuthSource::Opencode][..], &[][..]] {
        let (result, observed) = probe_script(script, sources);
        assert!(result.outputs.is_empty());
        assert_eq!(observed["calls"], json!([]));
    }
    let (result, observed) = probe_script(script, &[AuthSource::Native]);
    assert_eq!(observed["calls"], json!(["default"]));
    assert_eq!(result.outputs[0].account.id, "default");
    assert_eq!(result.outputs[0].account.origin, "native");
}

#[test]
fn auth_sources_preserves_canonical_ids_across_source_selection() {
    let script = r#"
        globalThis.__openusage_plugin = {
            discoverAccounts() {
                return [
                    { id: "same", origin: "native" },
                    { id: "same", origin: "opencode" }
                ];
            },
            probe(ctx) {
                calls.push(ctx.account.id);
                return { lines: [{ type: "text", label: "Status", value: "ok" }] };
            }
        };
    "#;
    let (all, _) = probe_script(script, DEFAULT_AUTH_SOURCES);
    assert_eq!(all.outputs.len(), 2);
    assert_ne!(all.outputs[0].account.id, all.outputs[1].account.id);
    for (index, source) in [AuthSource::Native, AuthSource::Opencode]
        .into_iter()
        .enumerate()
    {
        let (selected, observed) = probe_script(script, &[source]);
        assert_eq!(selected.outputs.len(), 1);
        assert_eq!(selected.outputs[0].account, all.outputs[index].account);
        assert_eq!(observed["calls"], json!(["same"]));
    }
}

#[test]
fn auth_sources_soft_fail_counts_only_enabled_descriptors() {
    let script = r#"
        globalThis.__openusage_plugin = {
            discoverAccounts() {
                return [
                    { id: "default", errorPolicy: "hide-if-other-account" },
                    { id: "remote", origin: "opencode" }
                ];
            },
            probe(ctx) { calls.push(ctx.account.id); throw "credential error"; }
        };
    "#;
    let (all, observed) = probe_script(script, DEFAULT_AUTH_SOURCES);
    assert_eq!(observed["calls"], json!(["default", "remote"]));
    assert_eq!(all.outputs.len(), 1);
    assert_eq!(all.outputs[0].account.id, "remote");

    let (native, observed) = probe_script(script, &[AuthSource::Native]);
    assert_eq!(observed["calls"], json!(["default"]));
    assert_eq!(
        native.outputs.len(),
        1,
        "excluded account must not suppress native error"
    );
    assert_eq!(native.outputs[0].account.id, "default");
    assert!(
        matches!(&native.outputs[0].lines[0], MetricLine::Badge { label, .. } if label == "Error")
    );
}

#[test]
fn auth_sources_preserves_discovery_errors_and_full_result_validation() {
    for discovery in [
        "throw 'discovery failed';",
        "return [{ id: '', origin: 'opencode' }];",
        "return Array.from({length: 33}, (_, i) => ({ id: String(i), origin: 'opencode' }));",
        "return [{ id: 'same', origin: 'opencode' }, { id: 'same', origin: 'opencode' }];",
    ] {
        let script = format!(
            r#"
            globalThis.__openusage_plugin = {{
                discoverAccounts() {{ discoveryCalls++; {discovery} }},
                probe(ctx) {{ calls.push(ctx.account.id); throw 'must not be called'; }}
            }};
        "#
        );
        let (result, observed) = probe_script(&script, &[AuthSource::Native]);
        assert_eq!(observed["discoveryCalls"], 1);
        assert_eq!(observed["calls"], json!([]));
        assert_eq!(result.outputs.len(), 1);
        assert!(
            matches!(&result.outputs[0].lines[0], MetricLine::Badge { label, .. } if label == "Error")
        );
    }
}
