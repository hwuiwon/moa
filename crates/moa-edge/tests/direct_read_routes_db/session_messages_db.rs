//! DB-backed behavior coverage for the contact session-message SSE route.
//!
//! The Session admission fence itself lives in the orchestrator; these tests pin the edge
//! half of the contract: a message cannot reach admission without a caller retry identity,
//! the caller's identity and the edge's own pre-admission cursor are what get submitted,
//! and the response the fence returns — including on a `Last-Event-ID` reconnect — is what
//! the stream is built from.

use super::*;
use axum::http::StatusCode as AxumStatusCode;
use axum::response::IntoResponse;

/// Upstream double standing in for `Contacts`, with the fence behavior the edge depends on.
struct ContactsUpstream {
    base_url: String,
    /// Every `send_message` body the edge forwarded, in order.
    submissions: Arc<Mutex<Vec<Value>>>,
    /// Sequence number of the newest event the next `progress` poll reports.
    stream_head: Arc<Mutex<u64>>,
    server: JoinHandle<()>,
}

#[derive(Clone)]
struct ContactsUpstreamState {
    submissions: Arc<Mutex<Vec<Value>>>,
    stream_head: Arc<Mutex<u64>>,
    /// Admissions the fence has recorded, keyed by client message id.
    admissions: Arc<Mutex<Vec<(String, Value)>>>,
    /// Client message id the fence must answer with a typed conflict.
    conflicting_id: Option<String>,
}

/// Starts an upstream that records submissions and replays admitted responses.
///
/// The double models exactly the guarantees the edge relies on: one recorded response per
/// client message id, replayed verbatim on a retry no matter what cursor the retry
/// carried, and a typed 409 for an id that was admitted for different work.
async fn start_contacts_upstream(conflicting_id: Option<&str>) -> ContactsUpstream {
    let submissions = Arc::new(Mutex::new(Vec::new()));
    let stream_head = Arc::new(Mutex::new(0_u64));
    let state = ContactsUpstreamState {
        submissions: submissions.clone(),
        stream_head: stream_head.clone(),
        admissions: Arc::new(Mutex::new(Vec::new())),
        conflicting_id: conflicting_id.map(ToOwned::to_owned),
    };
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        let state = state.clone();
        async move {
            let path = request.uri().path().to_string();
            let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                .await
                .expect("read upstream body");
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if path.ends_with("/Contacts/progress") {
                let head = *state.stream_head.lock().await;
                let session_id: SessionId = serde_json::from_value(body["session_id"].clone())
                    .expect("upstream progress request carries a session id");
                // Built from the real projection types so the double cannot drift from the
                // shape the edge actually decodes.
                let progress = moa_wire::turn::SessionProgress {
                    snapshot: moa_wire::turn::SessionSnapshot {
                        session_id: session_id.to_string(),
                        active_turn_id: None,
                        pending_message_count: 0,
                        last_outcome: None,
                        active_execution_run_uids: Vec::new(),
                    },
                    active_turn_progress: None,
                    active_execution_progress: Vec::new(),
                    events: vec![moa_core::types::events_stream::EventRecord {
                        id: Uuid::now_v7(),
                        session_id,
                        sequence_num: head,
                        event_type: moa_core::events::EventType::UserMessage,
                        event: Event::UserMessage {
                            text: "seed".to_string(),
                            attachments: Vec::new(),
                        },
                        timestamp: Utc::now(),
                        brain_id: None,
                        hand_id: None,
                        token_count: None,
                    }],
                    child_progress: Vec::new(),
                };
                return axum::Json(progress).into_response();
            }
            if !path.ends_with("/Contacts/send_message") {
                return (AxumStatusCode::NOT_FOUND, "unexpected upstream path").into_response();
            }

            let client_message_id = body["client_message_id"]
                .as_str()
                .expect("edge must forward a client message id")
                .to_string();
            state.submissions.lock().await.push(body.clone());
            if state.conflicting_id.as_deref() == Some(client_message_id.as_str()) {
                return (
                    AxumStatusCode::CONFLICT,
                    "client message id was already admitted for a different request",
                )
                    .into_response();
            }
            let mut admissions = state.admissions.lock().await;
            if let Some((_, recorded)) = admissions.iter().find(|(id, _)| id == &client_message_id)
            {
                return axum::Json(recorded.clone()).into_response();
            }
            let response = json!({
                "session_id": body["session_id"],
                "queued": false,
                "started_turn_id": format!("turn-for-{client_message_id}"),
                "stream_cursor": body["stream_cursor"],
            });
            admissions.push((client_message_id, response.clone()));
            axum::Json(response).into_response()
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind contacts upstream");
    let addr = listener.local_addr().expect("read contacts upstream addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve contacts upstream");
    });
    ContactsUpstream {
        base_url: format!("http://{addr}"),
        submissions,
        stream_head,
        server,
    }
}

/// Posts one session message body to the edge SSE route.
async fn post_session_message(
    client: &reqwest::Client,
    edge: &EdgeServer,
    session_id: SessionId,
    body: &Value,
    last_event_id: Option<u64>,
) -> reqwest::Response {
    let mut request = client
        .post(format!(
            "{}/v1/sessions/{session_id}/messages",
            edge.base_url
        ))
        .header("accept", "text/event-stream")
        .json(body);
    if let Some(last_event_id) = last_event_id {
        request = request.header("last-event-id", last_event_id.to_string());
    }
    request.send().await.expect("send session message")
}

/// Reads the first server-sent event frame and returns its name and decoded payload.
async fn first_sse_frame(mut response: reqwest::Response) -> (String, Value) {
    let mut buffer = String::new();
    while let Some(chunk) = response.chunk().await.expect("read sse chunk") {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(frame) = buffer
            .split("\n\n")
            .next()
            .filter(|_| buffer.contains("\n\n"))
        {
            let mut event = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    event = value.trim().to_string();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data.push_str(value.trim());
                }
            }
            let payload = serde_json::from_str(&data).expect("decode sse frame payload");
            return (event, payload);
        }
    }
    panic!("session message stream closed before its first frame");
}

/// Builds a JSON session-message body.
fn message_body(tenant_id: TenantId, client_message_id: Option<&str>, text: &str) -> Value {
    let mut body = json!({
        "tenant_id": tenant_id.0,
        "contact_token": "contact-token",
        "user_message": text,
    });
    if let Some(client_message_id) = client_message_id {
        body["client_message_id"] = json!(client_message_id);
    }
    body
}

#[tokio::test]
async fn session_message_without_a_valid_client_message_id_never_reaches_admission_db() {
    // Pins: the retry identity is required at the public boundary and is validated before the
    // edge observes a cursor, stores an attachment, or calls Contacts. Admitting a message
    // with no identity would silently give that caller no retry protection at all.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let upstream = start_contacts_upstream(None).await;
    let edge = start_edge_with_upstream(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Contact, tenant_id),
        None,
        &upstream.base_url,
    )
    .await;
    let client = reqwest::Client::new();

    let cases = [
        (
            message_body(tenant_id, None, "hello"),
            "client_message_id is required",
        ),
        (
            message_body(tenant_id, Some(""), "hello"),
            "client_message_id is invalid",
        ),
        (
            message_body(tenant_id, Some(&"x".repeat(257)), "hello"),
            "client_message_id is invalid",
        ),
        (
            message_body(tenant_id, Some("bad\nid"), "hello"),
            "client_message_id is invalid",
        ),
    ];
    for (body, expected) in cases {
        let response = post_session_message(&client, &edge, session_id, &body, None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.text().await.expect("read body"), expected);
    }
    assert!(
        upstream.submissions.lock().await.is_empty(),
        "a rejected message must not reach session admission"
    );

    stop_server(edge.server).await;
    stop_server(upstream.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn retried_session_message_replays_the_admitted_response_and_stored_cursor_db() {
    // Pins: a retry after a lost response returns the original admission — same turn, same
    // pre-admission cursor — even though the stream head has moved on. Using the newly
    // observed head instead would skip every event the first submission already produced.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let upstream = start_contacts_upstream(None).await;
    *upstream.stream_head.lock().await = 7;
    let edge = start_edge_with_upstream(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Contact, tenant_id),
        None,
        &upstream.base_url,
    )
    .await;
    let client = reqwest::Client::new();
    let body = message_body(tenant_id, Some("client-message-retry"), "audit the invoice");

    let first = post_session_message(&client, &edge, session_id, &body, None).await;
    assert_eq!(first.status(), StatusCode::OK);
    let (event, accepted) = first_sse_frame(first).await;
    assert_eq!(event, "accepted");
    assert_eq!(accepted["next_sequence_num"], json!(8));
    assert_eq!(
        accepted["started_turn_id"],
        json!("turn-for-client-message-retry")
    );

    // The session moved on between the lost response and the retry.
    *upstream.stream_head.lock().await = 31;
    let retry = post_session_message(&client, &edge, session_id, &body, None).await;
    assert_eq!(retry.status(), StatusCode::OK);
    let (event, replayed) = first_sse_frame(retry).await;
    assert_eq!(event, "accepted");
    assert_eq!(
        replayed["started_turn_id"],
        json!("turn-for-client-message-retry"),
        "a retry must return the original turn, not a second one"
    );
    assert_eq!(
        replayed["next_sequence_num"],
        json!(8),
        "a retry must resume from the stored pre-admission cursor, not the new stream head"
    );

    let submissions = upstream.submissions.lock().await;
    assert_eq!(submissions.len(), 2, "both attempts must pass the fence");
    assert_eq!(
        submissions[0]["client_message_id"],
        submissions[1]["client_message_id"]
    );
    assert_eq!(submissions[0]["stream_cursor"], json!(8));
    assert_eq!(
        submissions[1]["stream_cursor"],
        json!(32),
        "the edge submits its own freshly observed cursor and lets the fence decide"
    );
    drop(submissions);

    stop_server(edge.server).await;
    stop_server(upstream.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn session_message_admission_conflict_is_surfaced_to_the_caller_db() {
    // Pins: reusing one client message id for different work fails the caller with the
    // fence's typed conflict instead of opening a stream for work that was never admitted.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let upstream = start_contacts_upstream(Some("client-message-conflict")).await;
    let edge = start_edge_with_upstream(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Contact, tenant_id),
        None,
        &upstream.base_url,
    )
    .await;
    let client = reqwest::Client::new();

    let response = post_session_message(
        &client,
        &edge,
        session_id,
        &message_body(tenant_id, Some("client-message-conflict"), "changed text"),
        None,
    )
    .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        upstream.submissions.lock().await.len(),
        1,
        "the conflict is decided by the fence, so exactly one admission attempt is made"
    );

    stop_server(edge.server).await;
    stop_server(upstream.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn session_message_reconnect_passes_the_fence_and_resumes_from_last_event_id_db() {
    // Pins: a `Last-Event-ID` reconnect still goes through admission — it is the only thing
    // that can tell a retry from a new message — and returns the admitted response while
    // resuming from the caller's cursor. Short-circuiting the reconnect would fabricate an
    // empty response for a message that really did start a turn.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let upstream = start_contacts_upstream(None).await;
    *upstream.stream_head.lock().await = 4;
    let edge = start_edge_with_upstream(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Contact, tenant_id),
        None,
        &upstream.base_url,
    )
    .await;
    let client = reqwest::Client::new();
    let body = message_body(tenant_id, Some("client-message-reconnect"), "keep going");

    let first = post_session_message(&client, &edge, session_id, &body, None).await;
    let (_, accepted) = first_sse_frame(first).await;
    assert_eq!(accepted["next_sequence_num"], json!(5));

    let reconnect = post_session_message(&client, &edge, session_id, &body, Some(41)).await;
    assert_eq!(reconnect.status(), StatusCode::OK);
    let (event, resumed) = first_sse_frame(reconnect).await;

    assert_eq!(event, "accepted");
    assert_eq!(
        resumed["started_turn_id"],
        json!("turn-for-client-message-reconnect"),
        "a reconnect must report the turn the original admission started"
    );
    assert_eq!(
        resumed["next_sequence_num"],
        json!(42),
        "an explicit caller cursor wins over the stored pre-admission cursor"
    );
    assert_eq!(
        upstream.submissions.lock().await.len(),
        2,
        "the reconnect must still be validated by the fence"
    );

    stop_server(edge.server).await;
    stop_server(upstream.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}
