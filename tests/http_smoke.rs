use openusage_cli::daemon::DaemonState;
use openusage_cli::http_api::{self, ApiState, RuntimeConfig};
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
        enabled_plugins: "mock".to_string(),
        app_data_dir: Some(tmp.path().to_path_buf()),
        plugin_overrides_dir: None,
        refresh_interval_secs: 300,
        log_level: "error".to_string(),
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shutdown_tx = Arc::new(tokio::sync::Mutex::new(Some(shutdown_tx)));

    let app = http_api::router(ApiState {
        daemon,
        app_version: "0.1.0-test".to_string(),
        config: runtime_config,
        shutdown_tx: Some(shutdown_tx),
    });

    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
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

    let single_resp = client
        .get(format!("{}/v1/usage/mock", base))
        .send()
        .await
        .expect("single response");
    assert_eq!(single_resp.status(), reqwest::StatusCode::OK);
    let single_json: Value = single_resp.json().await.expect("single json");
    assert_eq!(single_json["providerId"], "mock");

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
    assert_eq!(config_json["enabledPlugins"], "mock");
    assert_eq!(config_json["logLevel"], "error");
    assert!(config_json["refreshIntervalSecs"].is_number());

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
