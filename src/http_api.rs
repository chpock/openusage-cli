use crate::daemon::{DaemonState, PluginMeta};
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tower_http::cors::{Any, CorsLayer};

#[derive(Clone)]
pub struct ApiState {
    pub daemon: Arc<DaemonState>,
    pub app_version: String,
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
