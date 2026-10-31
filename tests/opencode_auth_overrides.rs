mod support;

use openusage_cli::{
    config::{DEFAULT_AUTH_SOURCES, EnabledAuthSources},
    plugin_engine::{manifest, runtime},
};
use serde_json::{Value, json};
use std::{fs, path::Path};
use support::hermetic::{self, HermeticPaths};
use support::override_runner::{
    ProviderOutcome, ProviderSpec, run_provider_probe, run_provider_probe_with_auth_sources,
};

const AUTH: &str = ".local/share/opencode/auth.json";

fn write(paths: &HermeticPaths, relative: &str, text: &str) {
    let path = paths.root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn auth() -> Value {
    json!({
        "anthropic": {"type":"oauth", "access":"oc-claude", "refresh":"oc-refresh", "expires":4102444800000_u64},
        "openrouter": {"type":"api", "key":"oc-router"},
        "unrelated": {"type":"api", "key":"untouched"}
    })
}

fn install(paths: &HermeticPaths, id: &str, options: Value) -> manifest::LoadedPlugin {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut plugin = manifest::load_plugins_from_dir(&root.join("vendor/crossusage/plugins"))
        .into_iter()
        .find(|plugin| plugin.manifest.id == id)
        .unwrap();
    let source = root.join("plugin-overrides").join(format!("{id}.js"));
    if source.exists() {
        fs::copy(source, paths.overrides.join(format!("{id}.js"))).unwrap();
    }
    let observation = paths.root.join("observed.json");
    let harness = format!(
        r#";(function () {{
            var ctx = globalThis.__openusage_ctx;
            var options = {options}, path = {path};
            var observed = {{requests: [], writes: [], subscriptions: [], localUsage: [], violations: []}};
            var write = ctx.host.fs.writeText;
            var read = ctx.host.fs.readText;
            var subscribe = ctx.host.fs.subscribeFile;
            function persist() {{ write(path, JSON.stringify(observed)); }}
            ctx.host.fs.subscribeFile = function (file) {{
                observed.subscriptions.push(file); persist();
                return subscribe(file);
            }};
            ctx.host.fs.writeText = function (file, text) {{
                observed.writes.push({{path:file, text:text}}); persist();
                if (options.writeFailure && file === '~/.local/share/opencode/auth.json') throw new Error('fixture write failure');
                return write(file, text);
            }};
            ctx.host.keychain = ctx.host.keychain || {{}};
            ctx.host.ccusage = ctx.host.ccusage || {{}};
            ctx.host.keychain.readGenericPassword = function () {{ return null; }};
            ctx.host.keychain.readGenericPasswordForCurrentUser = function () {{ return null; }};
            ctx.host.keychain.writeGenericPassword = function () {{
                observed.violations.push('keychain write'); persist();
                throw new Error('unexpected keychain write');
            }};
            ctx.host.ccusage.query = function (opts) {{
                observed.localUsage.push(opts); persist();
                return {{status:'ok', data:{{daily:[{{date:new Date().toISOString().slice(0,10), totalTokens:100, totalCost:1}}]}}}};
            }};
            ctx.host.http.request = function (request) {{
                observed.requests.push(request); persist();
                var url = request.url;
                if (url === 'https://platform.claude.com/v1/oauth/token') {{
                    if (options.changeDuringRefresh) {{
                        var doc = JSON.parse(read('~/.local/share/opencode/auth.json'));
                        doc.concurrentSibling = {{key:'preserve-latest'}};
                        write('~/.local/share/opencode/auth.json', JSON.stringify(doc));
                    }}
                    if (options.corruptDuringRefresh) write('~/.local/share/opencode/auth.json', '{{bad');
                    if (options.replaceLoginDuringRefresh) {{
                        var doc = JSON.parse(read('~/.local/share/opencode/auth.json'));
                        doc.anthropic = {{type:'oauth',access:'different-login',refresh:'different-refresh',expires:4102444800000}};
                        write('~/.local/share/opencode/auth.json', JSON.stringify(doc));
                    }};
                    var body = {{access_token:'oc-renewed', expires_in:3600}};
                    if (!options.noRotation) body.refresh_token = 'oc-rotated';
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify(body)}};
                }}
                var token = request.headers.Authorization;
                if (options.reject && token.indexOf('oc-') >= 0) return {{status:401, headers:{{}}, bodyText:'{{}}'}};
                if (options.retry && token === 'Bearer oc-claude' && url === 'https://api.anthropic.com/api/oauth/usage') {{
                    return {{status:401, headers:{{}}, bodyText:'{{}}'}};
                }}
                if (url === 'https://api.anthropic.com/api/oauth/usage') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{
                        five_hour:{{utilization:token === 'Bearer native-claude' ? 11 : 37, resets_at:'2099-01-01T00:00:00Z'}},
                        cedar_ember:{{}}
                    }})}};
                }}
                if (url === 'https://api.anthropic.com/api/oauth/profile') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{organization:{{organization_type:'claude_pro'}}}})}};
                }}
                if (url === 'https://openrouter.ai/api/v1/credits') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{data:{{total_credits:10, total_usage:token === 'Bearer native-router' ? 2.5 : 4}}}})}};
                }}
                if (url === 'https://openrouter.ai/api/v1/key') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{data:{{usage_daily:1.25}}}})}};
                }}
                observed.violations.push(request); persist();
                throw new Error('unexpected HTTP fixture request');
            }};
            persist();
        }})();
        "#,
        options = options,
        path = json!(observation.to_string_lossy()),
    );
    plugin.entry_script = harness + plugin.entry_script.as_str();
    plugin
}

fn observed(paths: &HermeticPaths) -> Value {
    let observed: Value = serde_json::from_str(
        &fs::read_to_string(paths.root.join("observed.json")).expect("fixture initialized"),
    )
    .unwrap();
    assert_eq!(observed["violations"], json!([]), "{observed}");
    assert!(
        observed["subscriptions"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("~/{AUTH}")))
    );
    observed
}

fn probe(paths: &HermeticPaths, id: &str, options: Value) -> (Vec<Value>, Value) {
    let plugin = install(paths, id, options);
    let result = runtime::run_probe_in_sandbox(
        &plugin,
        &paths.app_data,
        "test",
        Some(&paths.overrides),
        &paths.root,
    );
    assert!(
        result
            .subscriptions
            .iter()
            .any(|s| s.path == paths.root.join(AUTH)),
        "missing OpenCode subscription: {:?}",
        result.outputs.iter().map(|o| &o.lines).collect::<Vec<_>>()
    );
    let outputs = result
        .outputs
        .iter()
        .map(|o| serde_json::to_value(o).unwrap())
        .collect();
    (outputs, observed(paths))
}

// Minimal plugin contracts isolate core source selection from provider HTTP and filesystem APIs.
// Actual pinned probes and credential persistence are covered by the sandbox fixtures above.
fn selected_routes(id: &str, selection: &str, options: Value) -> (Vec<Value>, Value) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let override_source =
        fs::read_to_string(root.join("plugin-overrides").join(format!("{id}.js"))).unwrap();
    let harness = format!(
        r#"
        var observed = {{requests:[], writes:[], subscriptions:[]}};
        var document = {document};
        var reject = {reject};
        globalThis.__openusage_ctx = {{host:{{fs:{{
            exists: function () {{return true;}},
            readText: function () {{return JSON.stringify(document);}},
            subscribeFile: function (path) {{observed.subscriptions.push(path);}}
        }}}}}};
    "#,
        document = auth(),
        reject = options["reject"] == true
    );
    let loaders = if id == "claude" {
        r#"
        function loadCredentials(ctx) {return {oauth:{accessToken:'native-claude'}};}
        function saveCredentials() {}
        function queryTokenUsage() {}
        function selectedToken(ctx) {return loadCredentials(ctx).oauth.accessToken;}
        "#
    } else {
        r#"
        function loadApiKey(ctx) {return 'native-router';}
        function creditsLines() {return [];}
        function appendSpend() {}
        function selectedToken(ctx) {return loadApiKey(ctx);}
        "#
    };
    let plugin = format!(
        r#"{loaders}
        globalThis.__openusage_plugin = {{probe:function (ctx) {{
            var token = selectedToken(ctx);
            observed.requests.push({{headers:{{Authorization:'Bearer ' + token}}}});
            if (reject && token.indexOf('oc-') === 0) throw 'Selected credentials rejected';
            return {{lines:[{{type:'text',label:'Token',value:token}}]}};
        }}}};
    "#
    );
    let spec = ProviderSpec {
        plugin_id: id,
        plugin_name: id,
        plugin_source: &plugin,
        override_source: Some(&override_source),
        harness: &harness,
        setup: "",
        assertion: Some("JSON.stringify(observed)"),
    };
    let selection: EnabledAuthSources = serde_yaml::from_str(selection).unwrap();
    let outcome = if selection.for_plugin(id) == DEFAULT_AUTH_SOURCES {
        run_provider_probe(&spec)
    } else {
        run_provider_probe_with_auth_sources(&spec, selection.for_plugin(id))
    };
    let ProviderOutcome::Probe { result, assertion } = outcome;
    let outputs = result
        .outputs
        .iter()
        .map(|output| serde_json::to_value(output).unwrap())
        .collect();
    (outputs, assertion.unwrap())
}

fn native(paths: &HermeticPaths) {
    write(paths, ".claude/.credentials.json", &json!({"claudeAiOauth":{
        "accessToken":"native-claude", "refreshToken":"native-refresh", "expiresAt":4102444800000_u64
    }}).to_string());
    write(
        paths,
        ".config/openusage/openrouter.json",
        r#"{"apiKey":"native-router"}"#,
    );
}

fn line<'a>(output: &'a Value, label: &str) -> &'a Value {
    output["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["label"] == label)
        .unwrap_or_else(|| panic!("missing {label}: {output}"))
}

fn error(output: &Value) -> &str {
    line(output, "Error")["text"].as_str().unwrap()
}

fn assert_accounts(outputs: &[Value], origins: &[&str]) {
    assert_eq!(outputs.len(), origins.len(), "{outputs:?}");
    for (output, origin) in outputs.iter().zip(origins) {
        assert_eq!(output["account"]["origin"], *origin);
        assert_eq!(
            output["account"]["id"],
            if *origin == "native" {
                "default"
            } else {
                "opencode-0"
            }
        );
    }
}

#[test]
fn opencode_credentials_keep_native_accounts_and_metrics_separate() {
    let Some(paths) =
        hermetic::enter("opencode_credentials_keep_native_accounts_and_metrics_separate")
    else {
        return;
    };
    native(&paths);
    write(&paths, AUTH, &auth().to_string());
    for id in ["claude", "openrouter"] {
        let (outputs, obs) = probe(&paths, id, json!({}));
        assert_accounts(&outputs, &["native", "opencode"]);
        let (label, native_used, oc_used) = if id == "claude" {
            ("Session", 11.0, 37.0)
        } else {
            ("Credits", 2.5, 4.0)
        };
        assert_eq!(line(&outputs[0], label)["used"], native_used);
        assert_eq!(line(&outputs[1], label)["used"], oc_used);
        assert_eq!(obs["writes"], json!([]));
        if id == "claude" {
            assert_eq!(
                obs["localUsage"].as_array().unwrap().len(),
                1,
                "only native may scan Claude Code logs"
            );
            assert!(
                outputs[1]["lines"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|l| l["label"] != "Today")
            );
        } else {
            assert_eq!(line(&outputs[1], "Balance")["value"], "$6");
            assert_eq!(line(&outputs[1], "Today")["value"], "$1.25");
        }
        let tokens: Vec<_> = obs["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["headers"]["Authorization"].as_str().unwrap())
            .collect();
        assert_eq!(
            tokens,
            if id == "claude" {
                vec![
                    "Bearer native-claude",
                    "Bearer native-claude",
                    "Bearer oc-claude",
                    "Bearer oc-claude",
                ]
            } else {
                vec![
                    "Bearer native-router",
                    "Bearer native-router",
                    "Bearer oc-router",
                    "Bearer oc-router",
                ]
            }
        );
    }
}

#[test]
fn opencode_sources_filter_before_probe_and_never_fall_back() {
    for id in ["claude", "openrouter"] {
        for (selection, origins, requests) in [
            (
                "{default: [native, opencode]}",
                vec!["native", "opencode"],
                2,
            ),
            ("{default: [native]}", vec!["native"], 1),
            ("{default: [opencode]}", vec!["opencode"], 1),
            ("{default: []}", vec![], 0),
        ] {
            let (outputs, obs) = selected_routes(id, selection, json!({}));
            assert_accounts(&outputs, &origins);
            assert_eq!(obs["requests"].as_array().unwrap().len(), requests);
            assert_eq!(obs["writes"], json!([]));
        }
        let (outputs, obs) = selected_routes(id, "{default: [opencode]}", json!({"reject":true}));
        assert_accounts(&outputs, &["opencode"]);
        assert!(!error(&outputs[0]).is_empty());
        assert!(obs["requests"].as_array().unwrap().iter().all(|r| {
            r["headers"]["Authorization"]
                .as_str()
                .is_none_or(|v| !v.contains("native"))
        }));
    }
}

#[test]
fn opencode_only_credentials_work_without_native_login() {
    let Some(paths) = hermetic::enter("opencode_only_credentials_work_without_native_login") else {
        return;
    };
    write(&paths, AUTH, &auth().to_string());
    for id in ["claude", "openrouter"] {
        let (outputs, _) = probe(&paths, id, json!({}));
        // Claude can still display unauthenticated local spend; only native errors are suppressed.
        let output = outputs
            .iter()
            .find(|o| o["account"]["origin"] == "opencode")
            .unwrap();
        assert_eq!(output["account"]["id"], "opencode-0");
        assert!(
            output["lines"]
                .as_array()
                .unwrap()
                .iter()
                .all(|l| l["label"] != "Error")
        );
    }
}

#[test]
fn opencode_missing_empty_and_invalid_inputs_are_source_scoped() {
    let Some(paths) =
        hermetic::enter("opencode_missing_empty_and_invalid_inputs_are_source_scoped")
    else {
        return;
    };
    native(&paths);
    for id in ["claude", "openrouter"] {
        for text in [
            None,
            Some(""),
            Some("   "),
            Some("{}"),
            Some(r#"{"other":{}}"#),
        ] {
            if let Some(text) = text {
                write(&paths, AUTH, text);
            } else {
                let _ = fs::remove_file(paths.root.join(AUTH));
            }
            let (outputs, obs) = probe(&paths, id, json!({}));
            assert_accounts(&outputs, &["native"]);
            assert_eq!(obs["writes"], json!([]));
        }
        let key = if id == "claude" {
            "anthropic"
        } else {
            "openrouter"
        };
        for block in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"type":"wrong","key":"secret","access":"secret"}),
            json!({"type":"api","key":"   "}),
            json!({"type":"oauth","access":"a","refresh":"r","expires":"1793493586905"}),
        ] {
            write(&paths, AUTH, &json!({key:block}).to_string());
            let (outputs, obs) = probe(&paths, id, json!({}));
            assert_accounts(&outputs, &["native", "opencode"]);
            assert!(!error(&outputs[1]).contains("secret"));
            assert_eq!(
                obs["requests"].as_array().unwrap().len(),
                2,
                "invalid source makes no requests"
            );
        }
        for text in ["{bad", "[]", "null"] {
            write(&paths, AUTH, text);
            let (outputs, _) = probe(&paths, id, json!({}));
            assert_accounts(&outputs, &["native", "opencode"]);
            assert!(!error(&outputs[1]).is_empty());
        }
    }
}

#[test]
fn claude_opencode_refresh_preserves_file_and_native_credentials() {
    let Some(paths) =
        hermetic::enter("claude_opencode_refresh_preserves_file_and_native_credentials")
    else {
        return;
    };
    native(&paths);
    let native_path = paths.root.join(".claude/.credentials.json");
    let native_before = fs::read(&native_path).unwrap();
    for (retry, no_rotation) in [(false, false), (true, false), (false, true)] {
        let mut doc = auth();
        doc["anthropic"]["extra"] = json!("preserve-entry");
        if !retry {
            doc["anthropic"]["expires"] = json!(0);
        }
        write(&paths, AUTH, &doc.to_string());
        let (outputs, obs) = probe(
            &paths,
            "claude",
            json!({"retry":retry,"noRotation":no_rotation,"changeDuringRefresh":true}),
        );
        assert_accounts(&outputs, &["native", "opencode"]);
        assert_eq!(line(&outputs[1], "Session")["used"], 37.0);
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(paths.root.join(AUTH)).unwrap()).unwrap();
        assert_eq!(saved["anthropic"]["access"], "oc-renewed");
        assert_eq!(
            saved["anthropic"]["refresh"],
            if no_rotation {
                "oc-refresh"
            } else {
                "oc-rotated"
            }
        );
        let expiry = saved["anthropic"]["expires"].as_u64().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(expiry > now + 3_500_000 && expiry <= now + 3_600_000);
        assert_eq!(saved["anthropic"]["extra"], "preserve-entry");
        assert_eq!(saved["openrouter"], doc["openrouter"]);
        assert_eq!(saved["unrelated"], doc["unrelated"]);
        assert_eq!(saved["concurrentSibling"]["key"], "preserve-latest");
        assert_eq!(fs::read(&native_path).unwrap(), native_before);
        let writes = obs["writes"].as_array().unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["path"], format!("~/{AUTH}"));
        let requests = obs["requests"].as_array().unwrap();
        let refresh: Vec<_> = requests.iter().filter(|r| r["method"] == "POST").collect();
        assert_eq!(refresh.len(), 1);
        let body: Value = serde_json::from_str(refresh[0]["bodyText"].as_str().unwrap()).unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "oc-refresh");
        assert!(
            requests
                .iter()
                .any(|r| r["headers"]["Authorization"] == "Bearer oc-renewed")
        );
    }
}

#[test]
fn claude_opencode_refresh_persistence_failure_is_visible() {
    let Some(paths) = hermetic::enter("claude_opencode_refresh_persistence_failure_is_visible")
    else {
        return;
    };
    for options in [
        json!({"writeFailure":true}),
        json!({"corruptDuringRefresh":true}),
    ] {
        let mut doc = auth();
        doc["anthropic"]["expires"] = json!(0);
        write(&paths, AUTH, &doc.to_string());
        let (outputs, obs) = probe(&paths, "claude", options.clone());
        let output = outputs
            .iter()
            .find(|o| o["account"]["origin"] == "opencode")
            .unwrap();
        assert!(error(output).contains("OpenCode"));
        assert!(!error(output).contains("oc-refresh"));
        assert!(
            !obs["requests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["url"] == "https://api.anthropic.com/api/oauth/usage")
        );
        if options["writeFailure"] == true {
            let saved: Value =
                serde_json::from_str(&fs::read_to_string(paths.root.join(AUTH)).unwrap()).unwrap();
            assert_eq!(saved, doc);
        } else {
            assert_eq!(fs::read_to_string(paths.root.join(AUTH)).unwrap(), "{bad");
        }
    }
}

#[test]
fn claude_native_refresh_keeps_its_original_destination() {
    let Some(paths) = hermetic::enter("claude_native_refresh_keeps_its_original_destination")
    else {
        return;
    };
    native(&paths);
    write(&paths, AUTH, &auth().to_string());
    let native_path = paths.root.join(".claude/.credentials.json");
    let mut native_doc: Value = serde_json::from_slice(&fs::read(&native_path).unwrap()).unwrap();
    native_doc["claudeAiOauth"]["expiresAt"] = json!(0);
    native_doc["nativeExtra"] = json!("preserved");
    fs::write(&native_path, native_doc.to_string()).unwrap();
    let opencode_before = fs::read(paths.root.join(AUTH)).unwrap();
    let (outputs, obs) = probe(&paths, "claude", json!({}));
    assert_accounts(&outputs, &["native", "opencode"]);
    assert!(
        outputs[0]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|line| line["label"] != "Error")
    );
    assert_eq!(fs::read(paths.root.join(AUTH)).unwrap(), opencode_before);
    let saved: Value = serde_json::from_slice(&fs::read(native_path).unwrap()).unwrap();
    assert_eq!(saved["nativeExtra"], "preserved");
    assert_eq!(saved["claudeAiOauth"]["accessToken"], "oc-renewed");
    let writes = obs["writes"].as_array().unwrap();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["path"], "~/.claude/.credentials.json");
    let request = obs["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["method"] == "POST")
        .unwrap();
    let body: Value = serde_json::from_str(request["bodyText"].as_str().unwrap()).unwrap();
    assert_eq!(body["refresh_token"], "native-refresh");
}

#[test]
fn opencode_watch_survives_daemon_clear_discover_replace_lifecycle() {
    let Some(paths) =
        hermetic::enter("opencode_watch_survives_daemon_clear_discover_replace_lifecycle")
    else {
        return;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugins: Vec<_> = manifest::load_plugins_from_dir(&root.join("vendor/crossusage/plugins"))
        .into_iter()
        .filter(|p| ["claude", "openrouter"].contains(&p.manifest.id.as_str()))
        .collect();
    assert_eq!(plugins.len(), 2);
    for plugin in &plugins {
        let file = format!("{}.js", plugin.manifest.id);
        fs::copy(
            root.join("plugin-overrides").join(&file),
            paths.overrides.join(file),
        )
        .unwrap();
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        use openusage_cli::{daemon::DaemonState, restart_watcher::ProviderWatchCommand};
        use std::time::Duration;
        use tokio::sync::mpsc;

        let (tx, mut rx) = mpsc::unbounded_channel();
        // Discovery-only lifecycle: no native credential readers or account network probes.
        let state = DaemonState::new(
            plugins,
            paths.app_data.clone(),
            "test".into(),
            Some(paths.overrides.clone()),
            Some(tx),
        )
        .with_enabled_auth_sources(serde_yaml::from_str("default: []").unwrap());
        let expected_path = paths.root.join(AUTH);
        let monitor = tokio::spawn(async move {
            for exists in [false, true, false] {
                for _ in 0..2 {
                    let clear = rx.recv().await.unwrap();
                    let ProviderWatchCommand::ClearProvider { provider_id, ack } = clear else {
                        panic!("clear before discovery");
                    };
                    ack.send(()).unwrap();
                    let replace = rx.recv().await.unwrap();
                    let ProviderWatchCommand::ReplaceProvider {
                        provider_id: replaced,
                        files,
                        ack,
                    } = replace
                    else {
                        panic!("replace after discovery");
                    };
                    assert_eq!(replaced, provider_id);
                    assert_eq!(files.len(), 1);
                    assert_eq!(files[0].path, expected_path);
                    assert_eq!(files[0].existed_at_declaration, exists);
                    ack.send(()).unwrap();
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            for exists in [false, true, false] {
                if exists {
                    write(&paths, AUTH, &auth().to_string());
                } else {
                    let _ = fs::remove_file(paths.root.join(AUTH));
                }
                assert!(state.refresh(None).await.unwrap().is_empty());
                assert!(state.cached(None).await.is_empty());
            }
            monitor.await.unwrap();
        })
        .await
        .expect("discovery/monitor acknowledgements must finish");
    });
}

#[test]
fn claude_refresh_does_not_overwrite_a_replaced_opencode_login() {
    let Some(paths) =
        hermetic::enter("claude_refresh_does_not_overwrite_a_replaced_opencode_login")
    else {
        return;
    };
    let mut doc = auth();
    doc["anthropic"]["expires"] = json!(0);
    write(&paths, AUTH, &doc.to_string());
    let (outputs, obs) = probe(&paths, "claude", json!({"replaceLoginDuringRefresh":true}));
    assert_accounts(&outputs, &["native", "opencode"]);
    assert_eq!(
        error(&outputs[1]),
        "OpenCode anthropic credentials changed during refresh."
    );
    let saved: Value = serde_json::from_slice(&fs::read(paths.root.join(AUTH)).unwrap()).unwrap();
    assert_eq!(saved["anthropic"]["access"], "different-login");
    assert_eq!(saved["anthropic"]["refresh"], "different-refresh");
    assert_eq!(saved["openrouter"], doc["openrouter"]);
    assert_eq!(obs["writes"], json!([]));
    assert_eq!(obs["requests"].as_array().unwrap().len(), 1);
}

#[test]
fn pinned_providers_reject_opencode_credentials_without_native_fallback() {
    let Some(paths) =
        hermetic::enter("pinned_providers_reject_opencode_credentials_without_native_fallback")
    else {
        return;
    };
    native(&paths);
    for id in ["claude", "openrouter"] {
        write(&paths, AUTH, &auth().to_string());
        let (outputs, obs) = probe(&paths, id, json!({"reject":true}));
        assert_accounts(&outputs, &["native", "opencode"]);
        assert!(
            outputs[0]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .all(|line| line["label"] != "Error")
        );
        assert!(!error(&outputs[1]).is_empty());
        let requests = obs["requests"].as_array().unwrap();
        assert_eq!(requests.len(), if id == "claude" { 5 } else { 4 });
        for request in &requests[2..] {
            assert!(
                !request["headers"]["Authorization"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("native")
            );
        }
        if id == "claude" {
            let refresh: Value =
                serde_json::from_str(requests[3]["bodyText"].as_str().unwrap()).unwrap();
            assert_eq!(refresh["refresh_token"], "oc-refresh");
        } else {
            assert_eq!(obs["writes"], json!([]));
        }
    }
}

#[test]
fn unreadable_opencode_auth_is_an_error_without_hiding_native_success() {
    let Some(paths) =
        hermetic::enter("unreadable_opencode_auth_is_an_error_without_hiding_native_success")
    else {
        return;
    };
    native(&paths);
    // A directory at the auth-file path deterministically exercises a read error, even as root.
    fs::create_dir_all(paths.root.join(AUTH)).unwrap();
    for id in ["claude", "openrouter"] {
        let (outputs, obs) = probe(&paths, id, json!({}));
        assert_accounts(&outputs, &["native", "opencode"]);
        assert_eq!(error(&outputs[1]), "Failed to read OpenCode auth.json.");
        assert_eq!(obs["requests"].as_array().unwrap().len(), 2);
        assert_eq!(obs["writes"], json!([]));
    }
}
