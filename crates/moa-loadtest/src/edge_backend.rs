//! moa-edge SSE backend: drives MOA through the production entry path.
//!
//! Unlike the ingress backend (trusted headers straight to Restate), this
//! target authenticates like a real integration: an Argon2-hashed API key row
//! per caller, a contact token minted through `POST /v1/contacts/tokens`, a
//! session created through `POST /v1/sessions`, and turns driven through the
//! `POST /v1/sessions/{id}/messages` SSE stream. TTFT is the time to the
//! first `response` frame; completion is the terminal `done` frame.
//!
//! Measurement caveat: the edge synthesizes SSE frames by polling
//! `/Contacts/progress` at a 1-3s adaptive interval, so observed latency has
//! a polling-granularity floor. That is the real production entry behavior,
//! which is exactly what this target certifies.
//!
//! Verification reads (session meta, event ranges) are not part of the
//! public edge surface; they go through the ingress client and stay off the
//! measured hot path.

use chrono::Utc;
use eventsource_stream::Eventsource as _;
use futures_util::StreamExt as _;
use moa_auth_providers::api_keys::{self, Env as ApiKeyEnv, KeyOwner, NewApiKey};
use moa_core::types::agent::SYSTEM_DEFAULT_AGENT_REVISION_UID;
use moa_core::{
    types::agent::AgentSessionSelection, types::channel::ChannelRef,
    types::contact::ContactSessionChannelRequest, types::contact::ContactSessionInitRequest,
    types::contact::ContactSessionInitResponse, types::contact::ContactTokenIssueRequest,
    types::contact::ContactTokenIssueResponse,
};
use secrecy::ExposeSecret as _;
use sqlx::postgres::PgPoolOptions;

use crate::*;

/// One edge-authenticated caller driving sessions over SSE.
pub(crate) struct EdgeTarget {
    http: reqwest::Client,
    edge_endpoint: String,
    tenant_id: TenantId,
    contact_token: String,
    model: String,
    /// Ingress-side reader for meta/event verification, off the hot path.
    reads: RemoteTarget,
}

impl EdgeTarget {
    async fn failure_after_active_turn(
        &self,
        session_id: SessionId,
        kind: TurnFailureKind,
        message: String,
    ) -> TurnFailure {
        let cleanup = self.reads.cancel_active_turn_and_wait(session_id).await;
        let cleanup_message = match &cleanup {
            Ok(()) => "cooperative cancellation reached an idle session".to_string(),
            Err(error) => format!("cooperative cancellation did not settle: {error}"),
        };
        TurnFailure {
            kind,
            message: format!("{message}; {cleanup_message}"),
            replacement_safe: cleanup.is_ok(),
        }
    }
}

#[async_trait]
impl SessionTarget for EdgeTarget {
    async fn start_session(&self, plan: &SessionPlan) -> Result<SessionId> {
        let request = ContactSessionInitRequest {
            tenant_id: self.tenant_id,
            contact_token: self.contact_token.clone(),
            title: Some(plan.title.clone()),
            channel: ContactSessionChannelRequest {
                channel_ref: ChannelRef::Chat {
                    conversation_id: format!("loadtest-{}", Uuid::now_v7()),
                    user_id: None,
                    client_session_id: None,
                },
                reason: None,
            },
            model: self.model.clone(),
            agent: AgentSessionSelection {
                installation_uid: None,
                revision_uid: Some(SYSTEM_DEFAULT_AGENT_REVISION_UID),
            },
        };
        let response = self
            .http
            .post(format!("{}/v1/sessions", self.edge_endpoint))
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("edge session init failed: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MoaError::ProviderError(format!(
                "edge session init returned {status}: {body}"
            )));
        }
        let parsed: ContactSessionInitResponse = serde_json::from_str(&body)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        Ok(parsed.session_id)
    }

    async fn run_turn(
        &self,
        session_id: SessionId,
        turn_ordinal: u64,
        prompt: &str,
        timeout: Duration,
    ) -> std::result::Result<TurnObservation, TurnFailure> {
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        let response = self
            .http
            .post(format!(
                "{}/v1/sessions/{session_id}/messages",
                self.edge_endpoint
            ))
            .header("accept", "text/event-stream")
            .json(&serde_json::json!({
                "tenant_id": self.tenant_id,
                "contact_token": self.contact_token,
                // Derived from the session and this turn's position, so a retried edge
                // submission replays instead of admitting a second paid turn.
                "client_message_id": format!("edge-loadtest-turn:{session_id}:{turn_ordinal}"),
                "user_message": prompt,
                "model": self.model,
                "attachments": [],
            }))
            .send()
            .await
            .map_err(|error| TurnFailure {
                kind: TurnFailureKind::StartFailed,
                message: format!("edge message post failed: {error}"),
                replacement_safe: false,
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let rejected = is_turn_admission_rejection(status, &body);
            return Err(TurnFailure {
                kind: if rejected {
                    TurnFailureKind::Rejected
                } else {
                    TurnFailureKind::StartFailed
                },
                message: format!("edge message post returned {status}: {body}"),
                replacement_safe: rejected,
            });
        }

        let mut ttft = None;
        let mut edge_observation_wait = None;
        let mut stream = response.bytes_stream().eventsource();
        loop {
            let next = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(next) => next,
                Err(_) => {
                    return Err(self
                        .failure_after_active_turn(
                            session_id,
                            TurnFailureKind::Timeout,
                            format!("no terminal done frame within {timeout:?}"),
                        )
                        .await);
                }
            };
            let Some(frame) = next else {
                return Err(self
                    .failure_after_active_turn(
                        session_id,
                        TurnFailureKind::Transport,
                        "SSE stream ended without a done frame".to_string(),
                    )
                    .await);
            };
            let frame = match frame {
                Ok(frame) => frame,
                Err(error) => {
                    return Err(self
                        .failure_after_active_turn(
                            session_id,
                            TurnFailureKind::Transport,
                            format!("SSE stream error: {error}"),
                        )
                        .await);
                }
            };
            match frame.event.as_str() {
                "response" if ttft.is_none() => {
                    ttft = Some(started.elapsed());
                    edge_observation_wait = response_frame_observation_wait(&frame.data);
                }
                "execution_started" => {
                    let Some(run_uid) = execution_started_run_uid(&frame.data) else {
                        return Err(self
                            .failure_after_active_turn(
                                session_id,
                                TurnFailureKind::Transport,
                                "execution_started frame did not contain a typed run UID"
                                    .to_string(),
                            )
                            .await);
                    };
                    return Ok(TurnObservation {
                        kind: TurnObservationKind::ExecutionAdmission { run_uid },
                        ttft: None,
                        edge_observation_wait: response_frame_observation_wait(&frame.data),
                        auto_denied_approvals: 0,
                    });
                }
                "done" => {
                    let status = serde_json::from_str::<serde_json::Value>(&frame.data)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("status")
                                .and_then(|status| status.as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "completed".to_string());
                    return match status.as_str() {
                        // `idle` means the queued message resolved without a
                        // fresh turn; the work still finished.
                        "completed" | "idle" => Ok(TurnObservation {
                            kind: TurnObservationKind::CompletedAnswer,
                            ttft,
                            edge_observation_wait,
                            auto_denied_approvals: 0,
                        }),
                        "cancelled" => Err(TurnFailure {
                            kind: TurnFailureKind::Cancelled,
                            message: "turn cancelled".to_string(),
                            replacement_safe: true,
                        }),
                        other => Err(TurnFailure {
                            kind: TurnFailureKind::Failed,
                            message: format!("turn ended with status {other}"),
                            replacement_safe: true,
                        }),
                    };
                }
                "error" => {
                    return Err(self
                        .failure_after_active_turn(
                            session_id,
                            TurnFailureKind::Transport,
                            format!("edge stream error frame: {}", frame.data),
                        )
                        .await);
                }
                _ => {}
            }
        }
    }

    async fn session_meta(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.reads.session_meta(session_id).await
    }

    async fn session_events_since(
        &self,
        session_id: SessionId,
        after_seq: u64,
    ) -> Result<Vec<EventRecord>> {
        self.reads.session_events_since(session_id, after_seq).await
    }

    async fn recent_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        self.reads.recent_events(session_id).await
    }
}

fn execution_started_run_uid(data: &str) -> Option<Uuid> {
    if let Ok(record) = serde_json::from_str::<EventRecord>(data)
        && let Event::ExecutionRunStarted(started) = record.event
    {
        return Some(started.run_uid);
    }
    let value = serde_json::from_str::<serde_json::Value>(data).ok()?;
    value
        .get("run_uid")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .or_else(|| {
            value
                .pointer("/event/data/run_uid")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
        })
}

fn response_frame_observation_wait(data: &str) -> Option<Duration> {
    let record = serde_json::from_str::<EventRecord>(data).ok()?;
    Utc::now()
        .signed_duration_since(record.timestamp)
        .to_std()
        .ok()
}

/// Builds one edge target per pool identity: API key row, FGA grants, and a
/// contact token per caller. Requires `MOA_DATABASE_URL` for key minting.
pub(crate) async fn build_edge_backend_pool(
    options: &LoadTestOptions,
    config: &MoaConfig,
    pool: &TenancyPool,
    edge_endpoint: &str,
) -> Result<Vec<Arc<dyn SessionTarget>>> {
    let database_url = std::env::var("MOA_DATABASE_URL").map_err(|_| {
        MoaError::MissingEnvironmentVariable(
            "MOA_DATABASE_URL is required for edge-mode API key minting".to_string(),
        )
    })?;
    let db = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .map_err(|error| MoaError::ProviderError(format!("edge-mode postgres connect: {error}")))?;
    let fga = live_fga_client()?;
    pool.grant_operators(&fga).await?;
    let http = build_http_client(options)?;
    let model = options
        .model
        .clone()
        .unwrap_or_else(|| config.model_for_task(ModelTask::MainLoop).to_string());
    let edge_endpoint = edge_endpoint.trim_end_matches('/').to_string();

    let mut targets: Vec<Arc<dyn SessionTarget>> = Vec::with_capacity(pool.entries().len());
    for entry in pool.entries() {
        let issued = api_keys::create(
            &db,
            NewApiKey {
                tenant_id: entry.tenant_id.0,
                owner: KeyOwner::User(entry.identity.id),
                env: ApiKeyEnv::Dev,
                name: "moa-loadtest",
                description: Some("ephemeral load-test caller"),
            },
        )
        .await
        .map_err(|error| MoaError::ProviderError(format!("edge api key mint: {error}")))?;
        // With api_key_id set, FGA checks run as api_key:<id>; the token-issue
        // route requires tenant operator on that subject.
        grant_raw_tuple(
            &fga,
            format!("api_key:{}", issued.id),
            "operator",
            format!("tenant:{}", entry.tenant_id),
        )
        .await?;

        let token_response = http
            .post(format!("{edge_endpoint}/v1/contacts/tokens"))
            .bearer_auth(issued.key.expose_secret())
            .json(&ContactTokenIssueRequest {
                tenant_id: entry.tenant_id,
                contact_points: Vec::new(),
                display_name: Some("moa-loadtest".to_string()),
                profile: serde_json::Value::Null,
                metadata: serde_json::Value::Null,
                requested_scopes: Vec::new(),
                permissions: serde_json::Value::Null,
                agent_ids: Vec::new(),
            })
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderError(format!("edge contact token mint failed: {error}"))
            })?;
        let status = token_response.status();
        let body = token_response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(MoaError::ProviderError(format!(
                "edge contact token mint returned {status}: {body}"
            )));
        }
        let token: ContactTokenIssueResponse = serde_json::from_str(&body)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;

        let reads = RemoteTarget::new(
            &options.endpoint,
            http.clone(),
            fga.clone(),
            entry.identity.clone(),
            entry.tenant_id,
            ModelId::new(model.clone()),
        )
        .map_err(|error| MoaError::ProviderError(format!("edge reads client: {error}")))?;
        targets.push(Arc::new(EdgeTarget {
            http: http.clone(),
            edge_endpoint: edge_endpoint.clone(),
            tenant_id: entry.tenant_id,
            contact_token: token.token,
            model: model.clone(),
            reads,
        }));
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use chrono::Duration as ChronoDuration;
    use moa_core::types::execution_planning::{ExecutionRunAdmissionStatus, ExecutionRunStarted};
    use moa_core::{events::EventType, types::identifiers::SessionId};

    use super::*;

    #[test]
    fn response_frame_observation_wait_uses_event_timestamp() {
        // Pins: edge-mode observation lag is estimated from the durable response event timestamp,
        // not from the terminal done frame.
        let record = EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId(Uuid::now_v7()),
            sequence_num: 7,
            event_type: EventType::BrainResponse,
            event: Event::BrainResponse {
                text: "hello".to_string(),
                thought_signature: None,
                model: ModelId::new("test-model"),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens_uncached: 1,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 1,
                cost_cents: 0,
                duration_ms: 1,
                llm_ttft_ms: None,
            },
            timestamp: Utc::now() - ChronoDuration::milliseconds(250),
            brain_id: None,
            hand_id: None,
            token_count: None,
        };
        let data = serde_json::to_string(&record).expect("serialize event record");

        let wait = response_frame_observation_wait(&data)
            .expect("event timestamp should produce an observation wait");

        assert!(
            wait >= Duration::from_millis(200),
            "wait should reflect event timestamp age: {wait:?}"
        );
    }

    #[test]
    fn accepted_execution_run_edge_backend_is_successful_admission() {
        // Pins: the named edge frame carries the same durable run identifier used by direct
        // admission accounting; an ordinary JSON envelope cannot be mistaken for success.
        let run_uid = Uuid::now_v7();
        let record = EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId(Uuid::now_v7()),
            sequence_num: 7,
            event_type: EventType::ExecutionRunStarted,
            event: Event::ExecutionRunStarted(ExecutionRunStarted {
                run_uid,
                originating_user_sequence_num: 6,
                plan_revision: 1,
                status: ExecutionRunAdmissionStatus::Queued,
                confirmation: None,
            }),
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        };
        let data = serde_json::to_string(&record).expect("serialize execution-start record");

        assert_eq!(execution_started_run_uid(&data), Some(run_uid));
        assert_eq!(execution_started_run_uid(r#"{"status":"accepted"}"#), None);
    }
}
