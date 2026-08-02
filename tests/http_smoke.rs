use openusage_cli::daemon::DaemonState;
use openusage_cli::http_api::{self, ApiState, AvailablePlugins, LifecycleCommand, RuntimeConfig};
use openusage_cli::plugin_engine::manifest;
use serde_json::Value;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn vendor_plugins_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/openusage/plugins")
}

fn load_mock_only_plugins() -> Vec<manifest::LoadedPlugin> {
    manifest::load_plugins_from_dir(&vendor_plugins_dir())
        .into_iter()
        .filter(|plugin| plugin.manifest.id == "mock")
        .collect()
}

#[tokio::test]
async fn http_api_smoke_for_plugins_and_usage_refresh() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let plugins = load_mock_only_plugins();
    assert_eq!(plugins.len(), 1, "expected only mock plugin in test setup");

    let daemon = Arc::new(DaemonState::new(
        plugins,
        tmp.path().to_path_buf(),
        "0.1.0-test".to_string(),
        None,
        None,
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let runtime_config = RuntimeConfig {
        app_version: "0.1.0-test".to_string(),
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        service_mode: "standalone".to_string(),
        existing_instance_policy: "error".to_string(),
        plugins_dir: Some(vendor_plugins_dir()),
        enabled_plugins: vec!["mock".to_string()],
        available_plugins: AvailablePlugins {
            active: vec!["mock".to_string()],
            inactive: vec![
                "claude".to_string(),
                "codex".to_string(),
                "cursor".to_string(),
            ],
        },
        app_data_dir: Some(tmp.path().to_path_buf()),
        plugin_overrides_dir: None,
        refresh_interval_secs: 180,
        aggressive_refresh_interval_secs: 10,
        log_level: "error".to_string(),
    };

    let (lifecycle_tx, lifecycle_rx) = oneshot::channel::<LifecycleCommand>();
    let lifecycle_tx = Arc::new(tokio::sync::Mutex::new(Some(lifecycle_tx)));

    let app = http_api::router(ApiState {
        daemon,
        app_version: "0.1.0-test".to_string(),
        config: runtime_config,
        lifecycle_tx: Some(lifecycle_tx),
    });

    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = lifecycle_rx.await;
        })
        .await;
    });

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);

    tokio::time::sleep(Duration::from_millis(25)).await;

    let plugins_resp = client
        .get(format!("{}/v1/plugins", base))
        .send()
        .await
        .expect("plugins response");
    assert_eq!(plugins_resp.status(), reqwest::StatusCode::OK);
    let plugins_json: Value = plugins_resp.json().await.expect("plugins json");
    let plugins_array = plugins_json.as_array().expect("plugins array");
    assert_eq!(plugins_array.len(), 1);
    assert_eq!(plugins_array[0]["id"], "mock");

    let empty_usage_resp = client
        .get(format!("{}/v1/usage", base))
        .send()
        .await
        .expect("empty usage response");
    assert_eq!(empty_usage_resp.status(), reqwest::StatusCode::OK);
    let empty_usage_json: Value = empty_usage_resp.json().await.expect("empty usage json");
    assert_eq!(
        empty_usage_json
            .as_array()
            .expect("empty usage array")
            .len(),
        0
    );

    let uncached_single_resp = client
        .get(format!("{}/v1/usage/mock", base))
        .send()
        .await
        .expect("uncached single response");
    assert_eq!(
        uncached_single_resp.status(),
        reqwest::StatusCode::NO_CONTENT
    );

    let usage_resp = client
        .get(format!("{}/v1/usage?refresh=true", base))
        .send()
        .await
        .expect("usage response");
    assert_eq!(usage_resp.status(), reqwest::StatusCode::OK);
    let usage_json: Value = usage_resp.json().await.expect("usage json");
    let usage_array = usage_json.as_array().expect("usage array");
    assert_eq!(usage_array.len(), 1);
    assert_eq!(usage_array[0]["providerId"], "mock");
    assert!(usage_array[0]["lines"].is_array());
    assert!(usage_array[0].get("fetchedAt").is_some());
    assert_eq!(
        usage_array[0]["account"],
        serde_json::json!({ "id": "default", "origin": "native" }),
        "every usage snapshot must carry the default account"
    );

    // Request with pluginIds filter, asserting account on selected snapshots
    let filtered_resp = client
        .get(format!("{}/v1/usage?refresh=true&pluginIds=mock", base))
        .send()
        .await
        .expect("filtered usage response");
    assert_eq!(filtered_resp.status(), reqwest::StatusCode::OK);
    let filtered_json: Value = filtered_resp.json().await.expect("filtered usage json");
    let filtered_array = filtered_json.as_array().expect("filtered usage array");
    assert_eq!(filtered_array.len(), 1, "filtered to mock only");
    assert_eq!(
        filtered_array[0]["account"],
        serde_json::json!({ "id": "default", "origin": "native" }),
        "filtered response must carry the default account"
    );

    let single_resp = client
        .get(format!("{}/v1/usage/mock", base))
        .send()
        .await
        .expect("single response");
    assert_eq!(single_resp.status(), reqwest::StatusCode::OK);
    let single_json: Value = single_resp.json().await.expect("single json");
    assert_eq!(single_json["providerId"], "mock");
    assert_eq!(
        single_json["account"],
        serde_json::json!({ "id": "default", "origin": "native" }),
        "single provider response must carry the default account"
    );

    let probe_resp = client
        .post(format!("{}/v1/probe", base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("probe response");
    assert_eq!(probe_resp.status(), reqwest::StatusCode::OK);
    let probe_json: Value = probe_resp.json().await.expect("probe json");
    let probe_array = probe_json.as_array().expect("probe array");
    assert!(!probe_array.is_empty(), "probe should return snapshots");
    assert_eq!(
        probe_array[0]["account"],
        serde_json::json!({ "id": "default", "origin": "native" }),
        "probe response must carry the default account"
    );

    let missing_resp = client
        .get(format!("{}/v1/usage/unknown-provider", base))
        .send()
        .await
        .expect("missing response");
    assert_eq!(missing_resp.status(), reqwest::StatusCode::NOT_FOUND);
    let missing_json: Value = missing_resp.json().await.expect("missing json");
    assert_eq!(missing_json["error"], "provider_not_found");

    // Test config endpoint
    let config_resp = client
        .get(format!("{}/v1/config", base))
        .send()
        .await
        .expect("config response");
    assert_eq!(config_resp.status(), reqwest::StatusCode::OK);
    let config_json: Value = config_resp.json().await.expect("config json");
    assert_eq!(config_json["appVersion"], "0.1.0-test");
    assert_eq!(config_json["host"], "127.0.0.1");
    assert_eq!(config_json["port"], serde_json::json!(addr.port()));
    assert_eq!(config_json["serviceMode"], "standalone");
    assert_eq!(config_json["existingInstancePolicy"], "error");
    assert_eq!(config_json["enabledPlugins"], serde_json::json!(["mock"]));
    assert_eq!(
        config_json["availablePlugins"]["active"],
        serde_json::json!(["mock"])
    );
    assert_eq!(
        config_json["availablePlugins"]["inactive"],
        serde_json::json!(["claude", "codex", "cursor"])
    );
    assert_eq!(config_json["logLevel"], "error");
    assert!(config_json["refreshIntervalSecs"].is_number());
    assert!(config_json["aggressiveRefreshIntervalSecs"].is_number());

    let shutdown_with_foreign_origin_resp = client
        .post(format!("{}/v1/shutdown", base))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .expect("shutdown with foreign origin response");
    assert_eq!(
        shutdown_with_foreign_origin_resp.status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let shutdown_with_foreign_origin_json: Value = shutdown_with_foreign_origin_resp
        .json()
        .await
        .expect("shutdown with foreign origin json");
    assert_eq!(
        shutdown_with_foreign_origin_json["error"],
        "shutdown_forbidden_origin"
    );

    let restart_with_foreign_origin_resp = client
        .post(format!("{}/v1/restart", base))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .expect("restart with foreign origin response");
    assert_eq!(
        restart_with_foreign_origin_resp.status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let restart_with_foreign_origin_json: Value = restart_with_foreign_origin_resp
        .json()
        .await
        .expect("restart with foreign origin json");
    assert_eq!(
        restart_with_foreign_origin_json["error"],
        "restart_forbidden_origin"
    );

    // Test shutdown endpoint
    let shutdown_resp = client
        .post(format!("{}/v1/shutdown", base))
        .send()
        .await
        .expect("shutdown response");
    assert_eq!(shutdown_resp.status(), reqwest::StatusCode::OK);
    let shutdown_json: Value = shutdown_resp.json().await.expect("shutdown json");
    assert_eq!(shutdown_json["status"], "shutting_down");

    // Give the server time to start shutting down
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _ = server.await;
}

#[tokio::test]
async fn single_provider_returns_sole_custom_account() {
    // A provider that returns a sole custom account must be returned by
    // GET /v1/usage/{provider} with that custom account, not 204.
    let tmp = tempfile::tempdir().expect("temp dir");

    let custom_plugin = manifest::LoadedPlugin {
        manifest: manifest::PluginManifest {
            schema_version: 1,
            id: "custom-account-provider".to_string(),
            name: "Custom Account".to_string(),
            version: "0.0.0".to_string(),
            entry: "plugin.js".to_string(),
            icon: "icon.svg".to_string(),
            brand_color: None,
            lines: vec![],
            links: vec![],
        },
        plugin_dir: PathBuf::from("."),
        entry_script: r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "my-account" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
        "#
        .to_string(),
        icon_data_url: "data:image/svg+xml;base64,".to_string(),
    };

    let daemon = Arc::new(DaemonState::new(
        vec![custom_plugin],
        tmp.path().to_path_buf(),
        "0.1.0-test".to_string(),
        None,
        None,
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (lifecycle_tx, lifecycle_rx) = oneshot::channel::<LifecycleCommand>();
    let lifecycle_tx = Arc::new(tokio::sync::Mutex::new(Some(lifecycle_tx)));

    let app = http_api::router(ApiState {
        daemon: Arc::clone(&daemon),
        app_version: "0.1.0-test".to_string(),
        config: RuntimeConfig {
            app_version: "0.1.0-test".to_string(),
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            service_mode: "standalone".to_string(),
            existing_instance_policy: "error".to_string(),
            plugins_dir: None,
            enabled_plugins: vec!["custom-account-provider".to_string()],
            available_plugins: AvailablePlugins {
                active: vec!["custom-account-provider".to_string()],
                inactive: vec![],
            },
            app_data_dir: Some(tmp.path().to_path_buf()),
            plugin_overrides_dir: None,
            refresh_interval_secs: 180,
            aggressive_refresh_interval_secs: 10,
            log_level: "error".to_string(),
        },
        lifecycle_tx: Some(lifecycle_tx),
    });

    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = lifecycle_rx.await;
        })
        .await;
    });

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);
    tokio::time::sleep(Duration::from_millis(25)).await;

    // First refresh the provider so it has a cached snapshot.
    let refresh_resp = client
        .get(format!(
            "{}/v1/usage?refresh=true&pluginIds=custom-account-provider",
            base
        ))
        .send()
        .await
        .expect("refresh response");
    assert_eq!(refresh_resp.status(), reqwest::StatusCode::OK);

    // Now GET /v1/usage/{provider} should return the custom account, not 204.
    let single_resp = client
        .get(format!("{}/v1/usage/custom-account-provider", base))
        .send()
        .await
        .expect("single provider response");
    assert_eq!(
        single_resp.status(),
        reqwest::StatusCode::OK,
        "sole custom account provider must return 200, not 204"
    );
    let single_json: Value = single_resp.json().await.expect("single json");
    assert_eq!(
        single_json["account"],
        serde_json::json!({ "id": "default", "origin": "native" }),
        "single-account mode must return default account (probe result.account ignored)"
    );

    // Shutdown
    let _ = client.post(format!("{}/v1/shutdown", base)).send().await;
    let _ = server.await;
}

#[tokio::test]
async fn http_two_non_default_accounts_filter_and_single_provider() {
    // A provider with two non-default discovered accounts: the collection
    // and filter endpoint return both in order, while the single-provider
    // route returns 204 (deferred selection).
    let tmp = tempfile::tempdir().expect("temp dir");

    let multi_account_plugin = manifest::LoadedPlugin {
        manifest: manifest::PluginManifest {
            schema_version: 1,
            id: "multi-account-provider".to_string(),
            name: "Multi Account".to_string(),
            version: "0.0.0".to_string(),
            entry: "plugin.js".to_string(),
            icon: "icon.svg".to_string(),
            brand_color: None,
            lines: vec![],
            links: vec![],
        },
        plugin_dir: PathBuf::from("."),
        entry_script: r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        lines: [ctx.line.text({ label: "Account", value: ctx.account.id })]
                    };
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "alpha" },
                        { id: "beta" }
                    ];
                }
            };
        "#
        .to_string(),
        icon_data_url: "data:image/svg+xml;base64,".to_string(),
    };

    let daemon = Arc::new(DaemonState::new(
        vec![multi_account_plugin],
        tmp.path().to_path_buf(),
        "0.1.0-test".to_string(),
        None,
        None,
    ));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let (lifecycle_tx, lifecycle_rx) = oneshot::channel::<LifecycleCommand>();
    let lifecycle_tx = Arc::new(tokio::sync::Mutex::new(Some(lifecycle_tx)));

    let app = http_api::router(ApiState {
        daemon: Arc::clone(&daemon),
        app_version: "0.1.0-test".to_string(),
        config: RuntimeConfig {
            app_version: "0.1.0-test".to_string(),
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            service_mode: "standalone".to_string(),
            existing_instance_policy: "error".to_string(),
            plugins_dir: None,
            enabled_plugins: vec!["multi-account-provider".to_string()],
            available_plugins: AvailablePlugins {
                active: vec!["multi-account-provider".to_string()],
                inactive: vec![],
            },
            app_data_dir: Some(tmp.path().to_path_buf()),
            plugin_overrides_dir: None,
            refresh_interval_secs: 180,
            aggressive_refresh_interval_secs: 10,
            log_level: "error".to_string(),
        },
        lifecycle_tx: Some(lifecycle_tx),
    });

    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = lifecycle_rx.await;
        })
        .await;
    });

    let client = reqwest::Client::new();
    let base = format!("http://{}", addr);
    tokio::time::sleep(Duration::from_millis(25)).await;

    // Refresh the provider so it has two cached snapshots.
    let refresh_resp = client
        .get(format!(
            "{}/v1/usage?refresh=true&pluginIds=multi-account-provider",
            base
        ))
        .send()
        .await
        .expect("refresh response");
    assert_eq!(refresh_resp.status(), reqwest::StatusCode::OK);

    // Collection endpoint returns both accounts in order.
    let usage_resp = client
        .get(format!("{}/v1/usage", base))
        .send()
        .await
        .expect("usage response");
    assert_eq!(usage_resp.status(), reqwest::StatusCode::OK);
    let usage_json: Value = usage_resp.json().await.expect("usage json");
    let usage_array = usage_json.as_array().expect("usage array");
    // The mock plugin also runs (loaded in daemon).
    // multi-account-provider has two entries in order.
    let multi_entries: Vec<&Value> = usage_array
        .iter()
        .filter(|e| e["providerId"] == "multi-account-provider")
        .collect();
    assert_eq!(
        multi_entries.len(),
        2,
        "should have two non-default accounts"
    );
    assert_eq!(multi_entries[0]["account"]["id"], "alpha");
    assert_eq!(multi_entries[1]["account"]["id"], "beta");

    // Filtered collection returns both in order.
    let filtered_resp = client
        .get(format!(
            "{}/v1/usage?pluginIds=multi-account-provider",
            base
        ))
        .send()
        .await
        .expect("filtered response");
    assert_eq!(filtered_resp.status(), reqwest::StatusCode::OK);
    let filtered_json: Value = filtered_resp.json().await.expect("filtered json");
    let filtered_array = filtered_json.as_array().expect("filtered array");
    assert_eq!(
        filtered_array.len(),
        2,
        "filtered should return both accounts"
    );
    assert_eq!(filtered_array[0]["account"]["id"], "alpha");
    assert_eq!(filtered_array[1]["account"]["id"], "beta");

    // Single-provider route returns 204 (multiple non-default accounts).
    let single_resp = client
        .get(format!("{}/v1/usage/multi-account-provider", base))
        .send()
        .await
        .expect("single provider response");
    assert_eq!(
        single_resp.status(),
        reqwest::StatusCode::NO_CONTENT,
        "two non-default accounts must return 204"
    );

    // Shutdown
    let _ = client.post(format!("{}/v1/shutdown", base)).send().await;
    let _ = server.await;
}
