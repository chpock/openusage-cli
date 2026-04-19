use openusage_cli::daemon::DaemonState;
use openusage_cli::http_api::{self, ApiState};
use openusage_cli::plugin_engine::manifest;
use serde_json::Value;
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

    let app = http_api::router(ApiState {
        daemon,
        app_version: "0.1.0-test".to_string(),
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
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

    let _ = shutdown_tx.send(());
    let _ = server.await;
}
