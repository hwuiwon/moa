//! Shared helpers for exercising the Restate `SessionStore` service over HTTP.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use moa_core::{
    Event, EventFilter, EventRange, ModelId, SessionId, SessionMeta, SessionStatus, UserMessage,
};
use moa_orchestrator::services::session_store::InitSessionVoRequest;
use moa_orchestrator::services::session_store::{
    AppendEventRequest, GetEventsRequest, SearchEventsRequest, SessionStore as RestateSessionStore,
    SessionStoreImpl, UpdateStatusRequest,
};
use moa_session::{PostgresSessionStore, testing};
use restate_sdk::prelude::*;
use tokio::task::JoinHandle;

/// Test harness exposing a live Restate HTTP endpoint backed by an isolated Postgres schema.
pub struct TestSessionStoreApp {
    /// Base URL of the local handler endpoint.
    pub base_url: String,
    /// Database URL backing the isolated test schema.
    pub database_url: String,
    /// Schema name created for this test instance.
    pub schema_name: String,
    /// Shared backend store used by the service instance.
    pub store: Arc<PostgresSessionStore>,
    server_task: JoinHandle<()>,
}

impl TestSessionStoreApp {
    /// Starts a live `SessionStore` HTTP endpoint backed by a fresh isolated schema.
    pub async fn spawn() -> Result<Self> {
        let (store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .context("create isolated postgres-backed session store")?;
        let store = Arc::new(store);

        let std_listener =
            TcpListener::bind("127.0.0.1:0").context("bind ephemeral listener for test server")?;
        std_listener
            .set_nonblocking(true)
            .context("set listener nonblocking")?;
        let address = std_listener
            .local_addr()
            .context("read listener local address")?;
        let listener = tokio::net::TcpListener::from_std(std_listener)
            .context("convert std listener into tokio listener")?;

        let endpoint = Endpoint::builder()
            .bind(SessionStoreImpl::new(store.clone()).serve())
            .build();
        let server_task = tokio::spawn(async move {
            HttpServer::new(endpoint).serve(listener).await;
        });

        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok(Self {
            base_url: format!("http://{address}"),
            database_url,
            schema_name,
            store,
            server_task,
        })
    }

    /// Returns a full URL for a service handler path.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Stops the HTTP server and drops the isolated Postgres schema.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            database_url,
            schema_name,
            server_task,
            ..
        } = self;

        server_task.abort();
        let _ = server_task.await;
        testing::cleanup_test_schema(&database_url, &schema_name)
            .await
            .context("drop isolated postgres test schema")?;
        Ok(())
    }
}

/// Returns a session metadata payload suitable for `create_session`.
pub fn test_session_meta(workspace_id: &str) -> SessionMeta {
    SessionMeta {
        workspace_id: workspace_id.into(),
        user_id: "user-1".into(),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}

/// Returns a user-message event suitable for append-event tests.
pub fn user_message_event(text: impl Into<String>) -> Event {
    Event::UserMessage {
        text: text.into(),
        attachments: vec![],
    }
}

/// Returns a request payload for `append_event`.
pub fn append_event_request(session_id: SessionId, event: Event) -> AppendEventRequest {
    AppendEventRequest { session_id, event }
}

/// Returns a request payload for `get_events`.
pub fn get_events_request(session_id: SessionId, range: EventRange) -> GetEventsRequest {
    GetEventsRequest { session_id, range }
}

/// Returns a request payload for `update_status`.
pub fn update_status_request(session_id: SessionId, status: SessionStatus) -> UpdateStatusRequest {
    UpdateStatusRequest { session_id, status }
}

/// Returns a request payload for `search_events`.
pub fn search_events_request(query: impl Into<String>, filter: EventFilter) -> SearchEventsRequest {
    SearchEventsRequest {
        query: query.into(),
        filter,
    }
}

/// Returns a request payload for `init_session_vo`.
pub fn init_session_vo_request(session_id: SessionId, meta: SessionMeta) -> InitSessionVoRequest {
    InitSessionVoRequest { session_id, meta }
}

/// Returns a user message payload suitable for `Session/post_message`.
pub fn user_message(text: impl Into<String>) -> UserMessage {
    UserMessage {
        text: text.into(),
        attachments: vec![],
    }
}
