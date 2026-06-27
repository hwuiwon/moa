//! HTTP helpers for fixture calls through Restate ingress.

use super::*;

/// Small test-only HTTP helper for calling Restate ingress directly.
#[derive(Clone, Debug)]
pub struct TestApiClient {
    endpoint: String,
    http: reqwest::Client,
    pub(super) identity: Option<Identity>,
}

impl TestApiClient {
    /// Creates a client for a Restate ingress endpoint.
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        url::Url::parse(&endpoint).with_context(|| format!("parse orchestrator URL {endpoint}"))?;
        Ok(Self {
            endpoint,
            http: reqwest::Client::new(),
            identity: None,
        })
    }

    /// Attaches trusted identity headers to all requests.
    #[must_use]
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Persists one session metadata row.
    pub async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
        self.post_call("/SessionStore/create_session", &meta).await
    }

    /// Initializes one Session virtual object.
    pub async fn init_session_vo(&self, session_id: SessionId, meta: SessionMeta) -> Result<()> {
        self.post_void(
            "/SessionStore/init_session_vo",
            &InitSessionVoRequest { session_id, meta },
        )
        .await
    }

    /// Appends one event to the durable session log.
    pub async fn append_event(&self, session_id: SessionId, event: Event) -> Result<u64> {
        self.post_call(
            "/SessionStore/append_event",
            &AppendEventRequest { session_id, event },
        )
        .await
    }

    /// Loads one session metadata row.
    pub async fn get_session(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.post_call("/SessionStore/get_session", &session_id)
            .await
    }

    /// Loads persisted events for one session.
    pub async fn get_events(
        &self,
        session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        self.post_call(
            "/SessionStore/get_events",
            &GetEventsRequest { session_id, range },
        )
        .await
    }

    /// Returns a handle scoped to one Session virtual object.
    pub fn session(&self, session_id: impl Into<String>) -> TestSessionHandle<'_> {
        TestSessionHandle {
            client: self,
            session_id: session_id.into(),
        }
    }

    /// Sends an authenticated JSON POST request and decodes a JSON response.
    pub async fn post_call<Req, Resp>(&self, path: &str, body: &Req) -> Result<Resp>
    where
        Req: serde::Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        let response = self.authed(
            self.http
                .post(format!("{}{path}", self.endpoint))
                .json(body),
        );
        decode_response(response.send().await.context("send orchestrator request")?).await
    }

    async fn post_call_with_idempotency<Req, Resp>(
        &self,
        path: &str,
        body: &Req,
        idempotency_key: Option<&str>,
    ) -> Result<Resp>
    where
        Req: serde::Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        let mut request = self.authed(
            self.http
                .post(format!("{}{path}", self.endpoint))
                .json(body),
        );
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        decode_response(request.send().await.context("send orchestrator request")?).await
    }

    async fn post_empty_call<Resp>(&self, path: &str) -> Result<Resp>
    where
        Resp: serde::de::DeserializeOwned,
    {
        let response = self.authed(self.http.post(format!("{}{path}", self.endpoint)));
        decode_response(response.send().await.context("send orchestrator request")?).await
    }

    /// Sends an authenticated JSON POST request that must return a success status.
    pub async fn post_void<Req>(&self, path: &str, body: &Req) -> Result<()>
    where
        Req: serde::Serialize + ?Sized,
    {
        let response = self
            .authed(
                self.http
                    .post(format!("{}{path}", self.endpoint))
                    .json(body),
            )
            .send()
            .await
            .context("send orchestrator request")?;
        ensure_success(response).await
    }

    fn authed(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let Some(identity) = &self.identity else {
            return request;
        };
        let identity_type = match identity.identity_type {
            IdentityType::User => "user",
            IdentityType::Agent => "agent",
            IdentityType::Service => "service",
            IdentityType::Contact => "contact",
        };
        let mut request = request
            .header("x-moa-identity-type", identity_type)
            .header("x-moa-identity-id", identity.id.to_string())
            .header("x-moa-tenant-id", identity.tenant_id.to_string());
        if let Some(api_key_id) = identity.api_key_id {
            request = request.header("x-moa-api-key-id", api_key_id.to_string());
        }
        if let Some(user_id) = identity.acting_on_behalf_of {
            request = request.header("x-moa-acting-on-behalf-of", user_id.to_string());
        }
        request
    }
}

/// Test-only handle scoped to one Session virtual object.
pub struct TestSessionHandle<'a> {
    client: &'a TestApiClient,
    session_id: String,
}

impl TestSessionHandle<'_> {
    /// Starts a new turn for the session.
    pub async fn start_turn(
        &self,
        request: StartTurnRequest,
        idempotency_key: Option<&str>,
    ) -> Result<StartTurnResponse> {
        self.client
            .post_call_with_idempotency(
                &format!("/Session/{}/start_turn", self.session_id),
                &request,
                idempotency_key,
            )
            .await
    }

    /// Reads a non-blocking session snapshot.
    pub async fn snapshot(&self) -> Result<SessionSnapshot> {
        self.client
            .post_empty_call(&format!("/Session/{}/snapshot", self.session_id))
            .await
    }

    /// Polls snapshots until the requested turn's terminal outcome is visible.
    pub async fn await_turn_outcome(
        &self,
        turn_id: &str,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<TurnOutcome> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot().await?;
            if let Some(outcome) = snapshot.last_outcome
                && outcome.turn_id == turn_id
            {
                return Ok(outcome);
            }
            if Instant::now() >= deadline {
                bail!("turn {turn_id} did not complete within {timeout:?}");
            }
            tokio::time::sleep(poll_interval).await;
        }
    }
}

async fn decode_response<Resp>(response: reqwest::Response) -> Result<Resp>
where
    Resp: serde::de::DeserializeOwned,
{
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read orchestrator response")?;
    if !status.is_success() {
        bail!("orchestrator returned bad status {status}: {body}");
    }
    serde_json::from_str(&body).context("decode orchestrator response")
}

async fn ensure_success(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read orchestrator response")?;
    if !status.is_success() {
        bail!("orchestrator returned bad status {status}: {body}");
    }
    Ok(())
}
