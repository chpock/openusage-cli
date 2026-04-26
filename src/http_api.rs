use crate::daemon::{DaemonState, PluginMeta};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::oneshot;
use tower_http::cors::{Any, CorsLayer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleCommand {
    Shutdown,
    Restart,
}

#[derive(Clone)]
pub struct ApiState {
    pub daemon: Arc<DaemonState>,
    pub app_version: String,
    pub config: RuntimeConfig,
    pub lifecycle_tx: Option<Arc<tokio::sync::Mutex<Option<oneshot::Sender<LifecycleCommand>>>>>,
}

/// Configuration information exposed via HTTP API
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub app_version: String,
    pub host: String,
    pub port: u16,
    pub service_mode: String,
    pub existing_instance_policy: String,
    pub plugins_dir: Option<PathBuf>,
    pub enabled_plugins: String,
    pub app_data_dir: Option<PathBuf>,
    pub plugin_overrides_dir: Option<PathBuf>,
    pub refresh_interval_secs: u64,
    pub log_level: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    app_version: String,
    plugins_loaded: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageQuery {
    refresh: Option<bool>,
    plugin_ids: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshQuery {
    refresh: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeRequest {
    plugin_ids: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

pub fn router(state: ApiState) -> Router {
    log::debug!(
        "building HTTP router for app_version={} with {} plugins",
        state.app_version,
        state.daemon.plugin_count()
    );

    Router::new()
        .route("/health", get(health))
        .route("/v1/plugins", get(get_plugins))
        .route("/v1/usage", get(get_usage_collection))
        .route("/v1/usage/{provider}", get(get_usage_single))
        .route("/v1/probe", post(post_probe))
        .route("/v1/config", get(get_config))
        .route("/v1/shutdown", post(post_shutdown))
        .route("/v1/restart", post(post_restart))
        .layer(middleware::from_fn(log_http_request))
        .with_state(Arc::new(state))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers(Any)
                .allow_methods(Any),
        )
}

async fn health(State(state): State<Arc<ApiState>>) -> Json<HealthResponse> {
    log::debug!("HTTP GET /health");
    Json(HealthResponse {
        status: "ok",
        app_version: state.app_version.clone(),
        plugins_loaded: state.daemon.plugin_count(),
    })
}

async fn get_plugins(State(state): State<Arc<ApiState>>) -> Json<Vec<PluginMeta>> {
    log::debug!("HTTP GET /v1/plugins");
    Json(state.daemon.plugins_meta())
}

async fn get_usage_collection(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<UsageQuery>,
) -> Response {
    let plugin_ids = parse_plugin_ids(query.plugin_ids);
    let force_refresh = query.refresh.unwrap_or(false);

    log::debug!(
        "HTTP GET /v1/usage refresh={} plugin_ids={:?}",
        force_refresh,
        plugin_ids
    );

    let snapshots = if force_refresh {
        match state.daemon.refresh(plugin_ids.clone()).await {
            Ok(value) => value,
            Err(err) => return internal_error(err),
        }
    } else {
        state.daemon.cached(plugin_ids.as_deref()).await
    };

    Json(snapshots).into_response()
}

async fn get_usage_single(
    State(state): State<Arc<ApiState>>,
    Path(provider): Path<String>,
    Query(query): Query<RefreshQuery>,
) -> Response {
    let force_refresh = query.refresh.unwrap_or(false);
    log::debug!("HTTP GET /v1/usage/{} refresh={}", provider, force_refresh);

    if !state.daemon.has_plugin(&provider) {
        log::debug!("provider {} not found", provider);
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "provider_not_found".to_string(),
            }),
        )
            .into_response();
    }

    if force_refresh && let Err(err) = state.daemon.refresh(Some(vec![provider.clone()])).await {
        return internal_error(err);
    }

    match state.daemon.cached_one(&provider).await {
        Some(snapshot) => {
            log::debug!("returning cached snapshot for provider {}", provider);
            Json(snapshot).into_response()
        }
        None => {
            log::debug!("provider {} has no cached snapshot", provider);
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

async fn post_probe(
    State(state): State<Arc<ApiState>>,
    Json(body): Json<ProbeRequest>,
) -> Response {
    log::debug!("HTTP POST /v1/probe plugin_ids={:?}", body.plugin_ids);
    match state.daemon.refresh(body.plugin_ids).await {
        Ok(result) => Json(result).into_response(),
        Err(err) => internal_error(err),
    }
}

async fn get_config(State(state): State<Arc<ApiState>>) -> Json<RuntimeConfig> {
    log::debug!("HTTP GET /v1/config");
    Json(state.config.clone())
}

async fn post_shutdown(
    State(state): State<Arc<ApiState>>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = validate_control_request(
        &remote_addr,
        &headers,
        "shutdown",
        "shutdown_forbidden_remote",
        "shutdown_forbidden_origin",
    ) {
        return response;
    }

    trigger_lifecycle_command(
        &state,
        LifecycleCommand::Shutdown,
        "shutdown",
        "shutting_down",
        "shutdown_already_triggered",
    )
    .await
}

async fn post_restart(
    State(state): State<Arc<ApiState>>,
    ConnectInfo(remote_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = validate_control_request(
        &remote_addr,
        &headers,
        "restart",
        "restart_forbidden_remote",
        "restart_forbidden_origin",
    ) {
        return response;
    }

    trigger_lifecycle_command(
        &state,
        LifecycleCommand::Restart,
        "restart",
        "restarting",
        "restart_already_triggered",
    )
    .await
}

fn validate_control_request(
    remote_addr: &SocketAddr,
    headers: &HeaderMap,
    action: &str,
    remote_error: &str,
    origin_error: &str,
) -> Option<Response> {
    if !remote_addr.ip().is_loopback() {
        log::warn!(
            "rejecting {} request from non-loopback address {}",
            action,
            remote_addr
        );
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse {
                    error: remote_error.to_string(),
                }),
            )
                .into_response(),
        );
    }

    if !is_control_origin_allowed(headers) {
        let origin = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("<invalid>");
        log::warn!(
            "rejecting {} request with non-local origin '{}'",
            action,
            origin
        );
        return Some(
            (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse {
                    error: origin_error.to_string(),
                }),
            )
                .into_response(),
        );
    }

    None
}

async fn trigger_lifecycle_command(
    state: &ApiState,
    command: LifecycleCommand,
    action: &str,
    status: &str,
    already_triggered_error: &str,
) -> Response {
    log::info!("HTTP POST /v1/{action} - initiating lifecycle action");

    if let Some(tx_mutex) = &state.lifecycle_tx {
        let mut tx_opt = tx_mutex.lock().await;
        if let Some(tx) = tx_opt.take() {
            let _ = tx.send(command);
            return Json(serde_json::json!({"status": status })).into_response();
        }
    }

    log::warn!("{} already triggered or not available", action);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            error: already_triggered_error.to_string(),
        }),
    )
        .into_response()
}

fn is_control_origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin_header) = headers.get(axum::http::header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin_header.to_str() else {
        return false;
    };
    if origin.eq_ignore_ascii_case("null") {
        return false;
    }

    let Ok(uri) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };

    is_loopback_host(host)
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn parse_plugin_ids(raw: Option<String>) -> Option<Vec<String>> {
    let value = raw?;
    let ids = value
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if ids.is_empty() { None } else { Some(ids) }
}

fn internal_error(err: anyhow::Error) -> Response {
    let handler_backtrace = std::backtrace::Backtrace::force_capture();
    log::error!(
        "HTTP handler internal error: {err:#}\nanyhow backtrace:\n{}\nhandler stack:\n{}",
        err.backtrace(),
        handler_backtrace
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: err.to_string(),
        }),
    )
        .into_response()
}

async fn log_http_request(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let started = Instant::now();

    let response = next.run(req).await;
    let status = response.status();
    let elapsed_ms = started.elapsed().as_millis();

    if status.is_server_error() {
        log::error!(
            "HTTP {} {} -> {} ({} ms)",
            method,
            uri,
            status.as_u16(),
            elapsed_ms
        );
    } else if status.is_client_error() {
        log::warn!(
            "HTTP {} {} -> {} ({} ms)",
            method,
            uri,
            status.as_u16(),
            elapsed_ms
        );
    } else {
        log::info!(
            "HTTP {} {} -> {} ({} ms)",
            method,
            uri,
            status.as_u16(),
            elapsed_ms
        );
    }

    response
}
