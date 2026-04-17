//! HTTP helpers for cloud-mode health checks and runtime observation streams.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_stream::stream;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::stream::Stream;
use moa_core::{
    BrainOrchestrator, BroadcastChannel, LagPolicy, RecvResult, RuntimeEvent, SessionId,
    recv_with_lag_handling,
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Shared API state for HTTP handlers.
#[derive(Clone)]
pub struct ApiState {
    /// Orchestrator used to resolve runtime observation streams.
    pub orchestrator: Arc<dyn BrainOrchestrator>,
}

/// Builds the API router used for cloud health checks and runtime SSE.
pub fn build_api_router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/sessions/{session_id}/stream", get(session_stream))
        .with_state(state)
}

/// Starts the HTTP API server with graceful shutdown driven by the provided watch channel.
pub async fn start_api_server(
    orchestrator: Arc<dyn BrainOrchestrator>,
    bind_host: &str,
    port: u16,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<JoinHandle<Result<()>>> {
    let listener = TcpListener::bind((bind_host, port))
        .await
        .with_context(|| format!("binding API listener on {bind_host}:{port}"))?;
    let router = build_api_router(ApiState { orchestrator });
    Ok(tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                loop {
                    match shutdown_rx.changed().await {
                        Ok(()) if *shutdown_rx.borrow() => break,
                        Ok(()) => continue,
                        Err(_) => break,
                    }
                }
            })
            .await
            .context("running API server")
    }))
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn session_stream(
    Path(session_id): Path<String>,
    State(state): State<ApiState>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, StatusCode> {
    let session_id = SessionId(Uuid::parse_str(&session_id).map_err(|_| StatusCode::BAD_REQUEST)?);
    let receiver = state
        .orchestrator
        .observe_runtime(session_id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let event_stream = runtime_event_stream(session_id, receiver);
    Ok(Sse::new(event_stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

fn runtime_event_stream(
    session_id: SessionId,
    mut receiver: broadcast::Receiver<RuntimeEvent>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    stream! {
        loop {
            match recv_with_lag_handling(
                &mut receiver,
                BroadcastChannel::Runtime,
                &session_id,
                LagPolicy::SkipWithGap,
            ).await {
                RecvResult::Message(event) => {
                    let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
                    yield Ok(
                        SseEvent::default()
                            .event(event.event_type())
                            .data(payload),
                    );
                }
                RecvResult::Gap { count } | RecvResult::BackfillRequested { count } => {
                    let payload = serde_json::to_string(&json!({
                        "count": count,
                        "channel": BroadcastChannel::Runtime.as_str(),
                    }))
                    .unwrap_or_else(|_| "{}".to_string());
                    yield Ok(
                        SseEvent::default()
                            .event("gap")
                            .data(payload),
                    );
                }
                RecvResult::AbortRequested | RecvResult::Closed => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Result;
    use async_trait::async_trait;
    use futures_util::StreamExt;
    use moa_core::{
        BrainOrchestrator, CronHandle, CronSpec, EventStream, ObserveLevel, RuntimeEvent,
        SessionFilter, SessionHandle, SessionId, SessionSignal, SessionSummary,
        StartSessionRequest,
    };
    use reqwest::header::CONTENT_TYPE;
    use tokio::sync::broadcast;

    use super::*;

    #[derive(Clone)]
    struct StubOrchestrator {
        runtime_tx: broadcast::Sender<RuntimeEvent>,
        supports_runtime_stream: bool,
    }

    #[async_trait]
    impl BrainOrchestrator for StubOrchestrator {
        async fn start_session(
            &self,
            _req: StartSessionRequest,
        ) -> moa_core::Result<SessionHandle> {
            Err(moa_core::MoaError::ProviderError("not used".to_string()))
        }

        async fn resume_session(&self, _session_id: SessionId) -> moa_core::Result<SessionHandle> {
            Err(moa_core::MoaError::ProviderError("not used".to_string()))
        }

        async fn signal(
            &self,
            _session_id: SessionId,
            _signal: SessionSignal,
        ) -> moa_core::Result<()> {
            Err(moa_core::MoaError::ProviderError("not used".to_string()))
        }

        async fn list_sessions(
            &self,
            _filter: SessionFilter,
        ) -> moa_core::Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }

        async fn observe(
            &self,
            _session_id: SessionId,
            _level: ObserveLevel,
        ) -> moa_core::Result<EventStream> {
            Ok(EventStream::from_events(Vec::new()))
        }

        async fn observe_runtime(
            &self,
            _session_id: SessionId,
        ) -> moa_core::Result<Option<broadcast::Receiver<RuntimeEvent>>> {
            if self.supports_runtime_stream {
                Ok(Some(self.runtime_tx.subscribe()))
            } else {
                Ok(None)
            }
        }

        async fn schedule_cron(&self, _spec: CronSpec) -> moa_core::Result<CronHandle> {
            Err(moa_core::MoaError::ProviderError("not used".to_string()))
        }
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() -> Result<()> {
        let (runtime_tx, _) = broadcast::channel(8);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let router = build_api_router(ApiState {
            orchestrator: Arc::new(StubOrchestrator {
                runtime_tx,
                supports_runtime_stream: true,
            }),
        });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });

        let response = reqwest::get(format!("http://{address}/health")).await?;
        assert_eq!(response.status(), StatusCode::OK);

        server.abort();
        let _ = server.await;
        Ok(())
    }

    #[tokio::test]
    async fn session_stream_returns_sse_content_type() -> Result<()> {
        let (runtime_tx, _) = broadcast::channel(8);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let router = build_api_router(ApiState {
            orchestrator: Arc::new(StubOrchestrator {
                runtime_tx: runtime_tx.clone(),
                supports_runtime_stream: true,
            }),
        });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });

        let session_id = SessionId::new();
        let response = reqwest::Client::new()
            .get(format!("http://{address}/sessions/{session_id}/stream"))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(content_type.starts_with("text/event-stream"));

        let _ = runtime_tx.send(RuntimeEvent::AssistantStarted);
        server.abort();
        let _ = server.await;
        Ok(())
    }

    #[tokio::test]
    async fn session_stream_returns_not_found_when_runtime_is_unavailable() -> Result<()> {
        let (runtime_tx, _) = broadcast::channel(8);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let router = build_api_router(ApiState {
            orchestrator: Arc::new(StubOrchestrator {
                runtime_tx,
                supports_runtime_stream: false,
            }),
        });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });

        let session_id = SessionId::new();
        let response = reqwest::Client::new()
            .get(format!("http://{address}/sessions/{session_id}/stream"))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        server.abort();
        let _ = server.await;
        Ok(())
    }

    #[tokio::test]
    async fn two_sse_subscribers_receive_the_same_runtime_event() -> Result<()> {
        let (runtime_tx, _) = broadcast::channel(8);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let router = build_api_router(ApiState {
            orchestrator: Arc::new(StubOrchestrator {
                runtime_tx: runtime_tx.clone(),
                supports_runtime_stream: true,
            }),
        });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });

        let session_id = SessionId::new();
        let first = reqwest::Client::new()
            .get(format!("http://{address}/sessions/{session_id}/stream"))
            .send()
            .await?;
        let second = reqwest::Client::new()
            .get(format!("http://{address}/sessions/{session_id}/stream"))
            .send()
            .await?;
        let mut first_stream = first.bytes_stream();
        let mut second_stream = second.bytes_stream();

        let _ = runtime_tx.send(RuntimeEvent::AssistantStarted);

        let first_chunk = tokio::time::timeout(Duration::from_secs(2), first_stream.next())
            .await
            .expect("first SSE chunk should arrive")
            .expect("first stream should produce a chunk")?;
        let second_chunk = tokio::time::timeout(Duration::from_secs(2), second_stream.next())
            .await
            .expect("second SSE chunk should arrive")
            .expect("second stream should produce a chunk")?;

        let first_text = String::from_utf8_lossy(&first_chunk).to_string();
        let second_text = String::from_utf8_lossy(&second_chunk).to_string();
        assert!(first_text.contains("assistant_started"));
        assert!(first_text.contains("data:"));
        assert!(second_text.contains("assistant_started"));
        assert!(second_text.contains("data:"));

        server.abort();
        let _ = server.await;
        Ok(())
    }

    #[tokio::test]
    async fn session_stream_emits_gap_event_when_runtime_subscriber_lags() -> Result<()> {
        let (runtime_tx, _) = broadcast::channel(4);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let router = build_api_router(ApiState {
            orchestrator: Arc::new(StubOrchestrator {
                runtime_tx: runtime_tx.clone(),
                supports_runtime_stream: true,
            }),
        });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });

        let session_id = SessionId::new();
        let response = reqwest::Client::new()
            .get(format!("http://{address}/sessions/{session_id}/stream"))
            .send()
            .await?;
        let mut stream = response.bytes_stream();

        for _ in 0..20 {
            let _ = runtime_tx.send(RuntimeEvent::AssistantStarted);
        }

        let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("gap SSE chunk should arrive")
            .expect("stream should produce a chunk")?;
        let text = String::from_utf8_lossy(&chunk).to_string();
        assert!(text.contains("event: gap"));
        assert!(text.contains("\"count\":16"));

        server.abort();
        let _ = server.await;
        Ok(())
    }
}
