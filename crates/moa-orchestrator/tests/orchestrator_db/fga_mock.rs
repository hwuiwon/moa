//! Shared lightweight OpenFGA HTTP mock for orchestrator DB tests.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, routing::post};
use moa_authz::{FgaClient, FgaConfig};
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Requests recorded by one isolated mock server.
pub(crate) type RecordedRequests = Arc<Mutex<Vec<Value>>>;

#[derive(Clone)]
struct FgaMockState {
    check_allowed: bool,
    requests: RecordedRequests,
}

/// Starts an isolated OpenFGA mock and returns its client and request log.
pub(crate) async fn spawn_fga_mock(check_allowed: bool) -> Result<(FgaClient, RecordedRequests)> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = FgaMockState {
        check_allowed,
        requests: Arc::clone(&requests),
    };
    let app = Router::new()
        .route("/stores/store-1/list-objects", post(fga_list_objects))
        .route("/stores/store-1/check", post(fga_check))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind FGA mock")?;
    let address = listener.local_addr().context("read FGA mock address")?;
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::debug!(%error, "FGA mock server stopped");
        }
    });

    let client = FgaClient::new(FgaConfig {
        url: format!("http://{address}"),
        preshared_key: "test-token".to_string(),
        store_id: "store-1".to_string(),
        model_id: "model-1".to_string(),
        timeout_ms: 5_000,
    })
    .context("build FGA mock client")?;
    Ok((client, requests))
}

async fn fga_list_objects(
    State(state): State<FgaMockState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.requests.lock().await.push(body);
    Json(json!({ "objects": [] }))
}

async fn fga_check(State(state): State<FgaMockState>, Json(body): Json<Value>) -> Json<Value> {
    state.requests.lock().await.push(body);
    Json(json!({ "allowed": state.check_allowed }))
}
