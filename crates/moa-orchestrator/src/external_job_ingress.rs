//! Private non-Restate boundary for asynchronous-provider callbacks.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use moa_core::error::MoaError;
use moa_execution::repository::external_job::{
    ExecutionExternalJobCallback, ExecutionExternalJobCallbackOutcome,
    ExecutionExternalJobCallbackWrite, ExecutionExternalJobRecord,
};
use moa_execution::repository::{ExecutionRepository, ExecutionScope};
use reqwest::{Client, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::services::execution_dispatcher::DispatchExecutionsRequest;
use crate::services::tool_executor::{
    ExecutionExternalJobAdapterRegistry, ExecutionExternalJobCallbackAuthentication,
};

/// Exact private callback route served outside Restate.
pub const EXTERNAL_JOB_CALLBACK_INGRESS_ROUTE: &str = "/internal/v1/execution/external-jobs/{external_job_uid}/generations/{job_generation}/callbacks/{provider_event_id}";
/// Maximum callback body accepted before adapter authentication or parsing.
pub const MAX_EXTERNAL_JOB_CALLBACK_INGRESS_BODY_BYTES: usize = 256 * 1024;
/// Maximum number of provider callback headers.
pub const MAX_EXTERNAL_JOB_CALLBACK_INGRESS_HEADERS: usize = 64;
/// Maximum aggregate provider callback-header bytes.
pub const MAX_EXTERNAL_JOB_CALLBACK_INGRESS_HEADER_BYTES: usize = 32 * 1024;
const MAX_PROVIDER_EVENT_ID_BYTES: usize = 512;

/// Public-safe callback ingress failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExternalJobIngressError {
    /// Selectors, headers, or parsed callback fields were invalid.
    #[error("invalid external-job callback")]
    InvalidRequest,
    /// Raw callback evidence exceeded a fixed limit.
    #[error("external-job callback exceeds the size limit")]
    RequestTooLarge,
    /// Provider authentication failed.
    #[error("external-job callback authentication failed")]
    Unauthorized,
    /// Persistence or a provider authentication dependency was unavailable.
    #[error("external-job callback service unavailable")]
    Unavailable,
}

/// Stable disposition returned after all transient callback bytes are dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalJobCallbackDisposition {
    /// The exact generation advanced.
    Applied,
    /// The provider event was already durably accepted.
    Duplicate,
    /// The path generation or provider job identity was stale.
    Stale,
    /// The job had already reached a terminal state.
    AlreadyTerminal,
}

/// Persistence seam for the callback boundary.
#[async_trait]
pub trait ExternalJobCallbackStore: Send + Sync {
    /// Loads canonical job metadata before adapter selection and authentication.
    async fn load(
        &self,
        external_job_uid: Uuid,
    ) -> Result<Option<ExecutionExternalJobRecord>, moa_execution::Error>;

    /// Atomically persists the callback receipt, transition, and controller wake.
    async fn apply(
        &self,
        config: &moa_config::ExecutionConfig,
        callback: ExecutionExternalJobCallback,
    ) -> Result<ExecutionExternalJobCallbackWrite, moa_execution::Error>;
}

#[async_trait]
impl ExternalJobCallbackStore for ExecutionRepository {
    async fn load(
        &self,
        external_job_uid: Uuid,
    ) -> Result<Option<ExecutionExternalJobRecord>, moa_execution::Error> {
        self.load_external_job(ExecutionScope::ControlPlane, external_job_uid)
            .await
    }

    async fn apply(
        &self,
        config: &moa_config::ExecutionConfig,
        callback: ExecutionExternalJobCallback,
    ) -> Result<ExecutionExternalJobCallbackWrite, moa_execution::Error> {
        self.apply_external_job_callback_and_activate(
            ExecutionScope::ControlPlane,
            config,
            callback,
        )
        .await
    }
}

/// Best-effort persist-then-send dispatcher wake.
#[async_trait]
pub trait ExternalJobDispatcherKick: Send + Sync {
    /// Requests one bounded outbox drain using a stable idempotency key.
    async fn kick(&self, idempotency_key: &str) -> Result<(), ExternalJobDispatcherKickError>;
}

/// Sanitized dispatcher acceptance failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("execution dispatcher did not accept callback wake")]
pub struct ExternalJobDispatcherKickError;

/// Restate HTTP implementation of the best-effort dispatcher wake.
pub struct RestateExternalJobDispatcherKick {
    http: Client,
    dispatch_url: Url,
}

impl RestateExternalJobDispatcherKick {
    /// Builds the dispatcher client from an origin-only Restate ingress URL.
    pub fn new(restate_ingress_origin: impl AsRef<str>) -> Result<Self, MoaError> {
        let mut dispatch_url = Url::parse(restate_ingress_origin.as_ref())
            .map_err(|_| MoaError::ConfigError("invalid Restate ingress origin".to_string()))?;
        if !matches!(dispatch_url.scheme(), "http" | "https")
            || dispatch_url.host_str().is_none()
            || !dispatch_url.username().is_empty()
            || dispatch_url.password().is_some()
            || dispatch_url.query().is_some()
            || dispatch_url.fragment().is_some()
            || !matches!(dispatch_url.path(), "" | "/")
        {
            return Err(MoaError::ConfigError(
                "Restate ingress must be an origin-only HTTP(S) URL".to_string(),
            ));
        }
        dispatch_url.set_path("/restate/send/ExecutionDispatcher/dispatch");
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        Ok(Self { http, dispatch_url })
    }
}

#[async_trait]
impl ExternalJobDispatcherKick for RestateExternalJobDispatcherKick {
    async fn kick(&self, idempotency_key: &str) -> Result<(), ExternalJobDispatcherKickError> {
        let response = self
            .http
            .post(self.dispatch_url.clone())
            .header("idempotency-key", idempotency_key)
            .json(&DispatchExecutionsRequest::default())
            .send()
            .await
            .map_err(|_| ExternalJobDispatcherKickError)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(ExternalJobDispatcherKickError)
        }
    }
}

/// Host-local callback controller that owns all transient provider evidence.
#[derive(Clone)]
pub struct ExternalJobCallbackIngress {
    store: Arc<dyn ExternalJobCallbackStore>,
    adapters: ExecutionExternalJobAdapterRegistry,
    config: moa_config::ExecutionConfig,
    dispatcher: Arc<dyn ExternalJobDispatcherKick>,
}

impl ExternalJobCallbackIngress {
    /// Builds a production callback boundary over Postgres and Restate HTTP.
    pub fn new(
        pool: sqlx::PgPool,
        adapters: ExecutionExternalJobAdapterRegistry,
        config: moa_config::ExecutionConfig,
        restate_ingress_origin: impl AsRef<str>,
    ) -> Result<Self, MoaError> {
        Ok(Self {
            store: Arc::new(ExecutionRepository::new(pool)),
            adapters,
            config,
            dispatcher: Arc::new(RestateExternalJobDispatcherKick::new(
                restate_ingress_origin,
            )?),
        })
    }

    #[cfg(test)]
    fn with_dependencies(
        store: Arc<dyn ExternalJobCallbackStore>,
        adapters: ExecutionExternalJobAdapterRegistry,
        dispatcher: Arc<dyn ExternalJobDispatcherKick>,
    ) -> Self {
        Self {
            store,
            adapters,
            config: moa_config::ExecutionConfig::default(),
            dispatcher,
        }
    }

    /// Authenticates, parses, and atomically applies one bounded raw callback.
    pub async fn handle(
        &self,
        external_job_uid: Uuid,
        job_generation: u64,
        provider_event_id: String,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<ExternalJobCallbackDisposition, ExternalJobIngressError> {
        validate_selectors(external_job_uid, job_generation, &provider_event_id)?;
        if body.is_empty() {
            return Err(ExternalJobIngressError::InvalidRequest);
        }
        if body.len() > MAX_EXTERNAL_JOB_CALLBACK_INGRESS_BODY_BYTES {
            return Err(ExternalJobIngressError::RequestTooLarge);
        }
        let authentication = callback_authentication(&headers, &body)?;
        let Some(job) = self.store.load(external_job_uid).await.map_err(|_| {
            tracing::warn!("callback authentication dependency failed");
            ExternalJobIngressError::Unauthorized
        })?
        else {
            tracing::debug!("callback authentication rejected");
            return Err(ExternalJobIngressError::Unauthorized);
        };
        let Some(provider) = job.provider.as_deref() else {
            tracing::debug!("callback authentication rejected");
            return Err(ExternalJobIngressError::Unauthorized);
        };
        let Some(callback_auth_reference) = job.callback_auth_reference.as_deref() else {
            tracing::debug!("callback authentication rejected");
            return Err(ExternalJobIngressError::Unauthorized);
        };
        let adapter = self.adapters.require(provider).map_err(|_| {
            tracing::debug!("callback authentication rejected");
            ExternalJobIngressError::Unauthorized
        })?;
        let authenticated = adapter
            .authenticate_callback(callback_auth_reference, &authentication, &body)
            .await
            .map_err(|_| {
                tracing::warn!("callback authentication dependency failed");
                ExternalJobIngressError::Unauthorized
            })?;
        if !authenticated {
            return Err(ExternalJobIngressError::Unauthorized);
        }
        let parsed = adapter
            .parse_callback(&authentication, &body)
            .await
            .map_err(map_adapter_parse_error)?;
        if parsed.provider_event_id != provider_event_id {
            return Err(ExternalJobIngressError::InvalidRequest);
        }
        let dispatcher_idempotency_key =
            dispatcher_idempotency_key(external_job_uid, job_generation, &provider_event_id);
        let callback = ExecutionExternalJobCallback {
            external_job_uid,
            job_generation,
            provider: provider.to_string(),
            provider_job_id: parsed.provider_job_id,
            provider_event_id,
            update: parsed.outcome.into(),
        };
        let write = self
            .store
            .apply(&self.config, callback)
            .await
            .map_err(map_execution_error)?;
        let disposition = match write.outcome {
            ExecutionExternalJobCallbackOutcome::Applied(job) => {
                let needs_dispatch = write.activation.is_some() || job.next_reconcile_at.is_some();
                if needs_dispatch
                    && self
                        .dispatcher
                        .kick(&dispatcher_idempotency_key)
                        .await
                        .is_err()
                {
                    tracing::warn!(
                        external_job_uid = %external_job_uid,
                        job_generation,
                        "execution dispatcher wake failed after durable external-job callback; reconciliation will repair delivery"
                    );
                }
                ExternalJobCallbackDisposition::Applied
            }
            ExecutionExternalJobCallbackOutcome::Duplicate => {
                ExternalJobCallbackDisposition::Duplicate
            }
            ExecutionExternalJobCallbackOutcome::StaleGeneration => {
                ExternalJobCallbackDisposition::Stale
            }
            ExecutionExternalJobCallbackOutcome::AlreadyTerminal => {
                ExternalJobCallbackDisposition::AlreadyTerminal
            }
            ExecutionExternalJobCallbackOutcome::NotFound => {
                return Err(ExternalJobIngressError::Unauthorized);
            }
        };
        Ok(disposition)
    }
}

#[derive(Debug, Deserialize)]
struct ExternalJobCallbackPath {
    external_job_uid: Uuid,
    job_generation: u64,
    provider_event_id: String,
}

/// Builds the exact private non-Restate callback surface.
pub fn router(ingress: ExternalJobCallbackIngress) -> Router {
    Router::new()
        .route(EXTERNAL_JOB_CALLBACK_INGRESS_ROUTE, post(callback_handler))
        .layer(DefaultBodyLimit::max(
            MAX_EXTERNAL_JOB_CALLBACK_INGRESS_BODY_BYTES,
        ))
        .with_state(ingress)
}

// SAFETY: provider authentication uses the bounded raw headers and body before parsing or persistence.
async fn callback_handler(
    State(ingress): State<ExternalJobCallbackIngress>,
    Path(path): Path<ExternalJobCallbackPath>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match ingress
        .handle(
            path.external_job_uid,
            path.job_generation,
            path.provider_event_id,
            headers,
            body,
        )
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => ingress_error_response(error),
    }
}

fn ingress_error_response(error: ExternalJobIngressError) -> Response {
    let status = match error {
        ExternalJobIngressError::InvalidRequest => StatusCode::BAD_REQUEST,
        ExternalJobIngressError::RequestTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ExternalJobIngressError::Unauthorized => StatusCode::UNAUTHORIZED,
        ExternalJobIngressError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };
    status.into_response()
}

fn validate_selectors(
    external_job_uid: Uuid,
    job_generation: u64,
    provider_event_id: &str,
) -> Result<(), ExternalJobIngressError> {
    if external_job_uid.is_nil()
        || job_generation == 0
        || provider_event_id.trim().is_empty()
        || provider_event_id.len() > MAX_PROVIDER_EVENT_ID_BYTES
        || provider_event_id.chars().any(char::is_control)
    {
        return Err(ExternalJobIngressError::InvalidRequest);
    }
    Ok(())
}

fn callback_authentication(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<ExecutionExternalJobCallbackAuthentication, ExternalJobIngressError> {
    if headers.len() > MAX_EXTERNAL_JOB_CALLBACK_INGRESS_HEADERS {
        return Err(ExternalJobIngressError::RequestTooLarge);
    }
    let mut selected = BTreeMap::new();
    let mut total_bytes = 0usize;
    for name in headers.keys() {
        let values = headers.get_all(name);
        let mut values = values.iter();
        let value = values
            .next()
            .ok_or(ExternalJobIngressError::InvalidRequest)?;
        if values.next().is_some() {
            return Err(ExternalJobIngressError::InvalidRequest);
        }
        total_bytes = total_bytes
            .checked_add(name.as_str().len())
            .and_then(|sum| sum.checked_add(value.as_bytes().len()))
            .ok_or(ExternalJobIngressError::RequestTooLarge)?;
        if total_bytes > MAX_EXTERNAL_JOB_CALLBACK_INGRESS_HEADER_BYTES {
            return Err(ExternalJobIngressError::RequestTooLarge);
        }
        if authentication_header(name.as_str()) {
            selected.insert(
                name.as_str().to_ascii_lowercase(),
                value
                    .to_str()
                    .map_err(|_| ExternalJobIngressError::InvalidRequest)?
                    .to_string(),
            );
        }
    }
    Ok(ExecutionExternalJobCallbackAuthentication {
        headers: selected,
        body_sha256: Sha256::digest(body).into(),
    })
}

fn authentication_header(name: &str) -> bool {
    !name.eq_ignore_ascii_case("host")
        && !name.eq_ignore_ascii_case("content-length")
        && !name.eq_ignore_ascii_case("connection")
        && !name.eq_ignore_ascii_case("keep-alive")
        && !name.eq_ignore_ascii_case("proxy-authenticate")
        && !name.eq_ignore_ascii_case("proxy-authorization")
        && !name.eq_ignore_ascii_case("te")
        && !name.eq_ignore_ascii_case("trailers")
        && !name.eq_ignore_ascii_case("transfer-encoding")
        && !name.eq_ignore_ascii_case("upgrade")
        && !name.to_ascii_lowercase().starts_with("x-moa-")
        && !name.eq_ignore_ascii_case(moa_observability::TRACEPARENT_HEADER)
        && !name.eq_ignore_ascii_case(moa_observability::TRACESTATE_HEADER)
}

fn dispatcher_idempotency_key(
    external_job_uid: Uuid,
    job_generation: u64,
    provider_event_id: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(external_job_uid.as_bytes());
    digest.update(job_generation.to_be_bytes());
    digest.update(provider_event_id.as_bytes());
    format!(
        "external-job-callback-dispatch:{}",
        hex::encode(digest.finalize())
    )
}

fn map_adapter_parse_error(error: MoaError) -> ExternalJobIngressError {
    match error {
        MoaError::ValidationError(_)
        | MoaError::SerializationError(_)
        | MoaError::SerdeJson(_)
        | MoaError::Uuid(_) => ExternalJobIngressError::InvalidRequest,
        _ => ExternalJobIngressError::Unavailable,
    }
}

fn map_execution_error(error: moa_execution::Error) -> ExternalJobIngressError {
    match error {
        moa_execution::Error::InvalidRepositoryInput { .. } => {
            ExternalJobIngressError::InvalidRequest
        }
        _ => ExternalJobIngressError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use chrono::Utc;
    use moa_core::types::identifiers::TenantId;
    use moa_core::types::tools::{
        AsyncToolJobCallbackOutcome, AsyncToolJobCancelOutcome, AsyncToolJobTerminalOutcome,
        ExternalJobStartContext,
    };
    use moa_execution::repository::external_job::{
        ExecutionExternalJobCallbackWrite, ExecutionExternalJobOwner, ExecutionExternalJobState,
    };
    use moa_execution::wire::{
        ExecutionExternalJobCancelRequest, ExecutionExternalJobReconcileRequest,
    };
    use tokio::sync::Mutex;

    use super::*;
    use crate::services::tool_executor::{
        ExecutionExternalJobAdapter, ExecutionExternalJobAdapterCallback,
        ExecutionExternalJobStartOutcome, ExecutionExternalJobStartRecovery,
        ExecutionExternalJobStartRequest,
    };

    #[test]
    fn dispatcher_kick_targets_the_coalescing_service_router() {
        // Pins: callbacks outside the SDK enter the same stateless head-coalescing router as
        // generated clients; only that router addresses the ingress-private fleet drain object.
        let kick = RestateExternalJobDispatcherKick::new("http://127.0.0.1:8080")
            .expect("valid fixture Restate ingress origin");
        assert_eq!(
            kick.dispatch_url.as_str(),
            "http://127.0.0.1:8080/restate/send/ExecutionDispatcher/dispatch"
        );
    }

    struct FixtureAdapter {
        authenticated: bool,
        authentication_error: bool,
        parse_calls: AtomicUsize,
    }

    #[async_trait]
    impl ExecutionExternalJobAdapter for FixtureAdapter {
        fn provider_key(&self) -> &'static str {
            "fixture"
        }

        async fn start(
            &self,
            _request: &ExecutionExternalJobStartRequest,
        ) -> moa_core::error::Result<ExecutionExternalJobStartOutcome> {
            Err(MoaError::Unsupported(
                "callback fixture does not start provider jobs".to_string(),
            ))
        }

        async fn recover_start(
            &self,
            _context: &ExternalJobStartContext,
        ) -> moa_core::error::Result<ExecutionExternalJobStartRecovery> {
            Err(MoaError::Unsupported(
                "callback fixture does not recover provider starts".to_string(),
            ))
        }

        async fn authenticate_callback(
            &self,
            callback_auth_reference: &str,
            authentication: &ExecutionExternalJobCallbackAuthentication,
            body: &[u8],
        ) -> moa_core::error::Result<bool> {
            if self.authentication_error {
                return Err(MoaError::ProviderTransport(
                    "fixture authentication dependency failed".to_string(),
                ));
            }
            Ok(self.authenticated
                && callback_auth_reference == "auth-ref"
                && body == b"callback-body"
                && authentication
                    .headers
                    .get("x-fixture-signature")
                    .map(String::as_str)
                    == Some("valid"))
        }

        async fn parse_callback(
            &self,
            _authentication: &ExecutionExternalJobCallbackAuthentication,
            body: &[u8],
        ) -> moa_core::error::Result<ExecutionExternalJobAdapterCallback> {
            self.parse_calls.fetch_add(1, Ordering::SeqCst);
            if body != b"callback-body" {
                return Err(MoaError::ValidationError(
                    "invalid callback body".to_string(),
                ));
            }
            Ok(ExecutionExternalJobAdapterCallback {
                provider_job_id: "provider-job".to_string(),
                provider_event_id: "event-1".to_string(),
                outcome: AsyncToolJobCallbackOutcome::Terminal {
                    outcome: AsyncToolJobTerminalOutcome::Cancelled,
                },
            })
        }

        async fn cancel(
            &self,
            _request: &ExecutionExternalJobCancelRequest,
        ) -> moa_core::error::Result<AsyncToolJobCancelOutcome> {
            Ok(AsyncToolJobCancelOutcome::Cancelled)
        }

        async fn reconcile(
            &self,
            _request: &ExecutionExternalJobReconcileRequest,
        ) -> moa_core::error::Result<AsyncToolJobCallbackOutcome> {
            Err(MoaError::Unsupported("fixture reconcile".to_string()))
        }
    }

    struct FixtureStore {
        job: Option<ExecutionExternalJobRecord>,
        outcome: Mutex<Option<ExecutionExternalJobCallbackOutcome>>,
        activation: bool,
        apply_calls: AtomicUsize,
    }

    #[async_trait]
    impl ExternalJobCallbackStore for FixtureStore {
        async fn load(
            &self,
            _external_job_uid: Uuid,
        ) -> Result<Option<ExecutionExternalJobRecord>, moa_execution::Error> {
            Ok(self.job.clone())
        }

        async fn apply(
            &self,
            _config: &moa_config::ExecutionConfig,
            callback: ExecutionExternalJobCallback,
        ) -> Result<ExecutionExternalJobCallbackWrite, moa_execution::Error> {
            self.apply_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(callback.provider_event_id, "event-1");
            Ok(ExecutionExternalJobCallbackWrite {
                outcome: self
                    .outcome
                    .lock()
                    .await
                    .take()
                    .expect("one fixture outcome"),
                activation: self.activation.then(fixture_activation),
            })
        }
    }

    struct FixtureDispatcher {
        called: AtomicBool,
    }

    #[async_trait]
    impl ExternalJobDispatcherKick for FixtureDispatcher {
        async fn kick(&self, _idempotency_key: &str) -> Result<(), ExternalJobDispatcherKickError> {
            self.called.store(true, Ordering::SeqCst);
            Err(ExternalJobDispatcherKickError)
        }
    }

    #[tokio::test]
    async fn bad_auth_stops_before_parse_or_persistence() {
        // Pins: raw unauthenticated provider bytes never reach parsing or durable storage.
        let adapter = Arc::new(FixtureAdapter {
            authenticated: false,
            authentication_error: false,
            parse_calls: AtomicUsize::new(0),
        });
        let store = Arc::new(fixture_store(
            ExecutionExternalJobCallbackOutcome::Duplicate,
        ));
        let ingress = fixture_ingress(store.clone(), adapter.clone());
        assert_eq!(
            ingress
                .handle(
                    Uuid::from_u128(1),
                    1,
                    "event-1".to_string(),
                    signed_headers(),
                    Bytes::from_static(b"callback-body"),
                )
                .await,
            Err(ExternalJobIngressError::Unauthorized)
        );
        assert_eq!(adapter.parse_calls.load(Ordering::SeqCst), 0);
        assert_eq!(store.apply_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn authentication_dependency_error_is_the_same_unauthorized_boundary() {
        // Pins: an adapter dependency failure before authentication cannot reveal that the
        // supplied external-job and provider identities exist.
        let adapter = Arc::new(FixtureAdapter {
            authenticated: false,
            authentication_error: true,
            parse_calls: AtomicUsize::new(0),
        });
        let store = Arc::new(fixture_store(
            ExecutionExternalJobCallbackOutcome::Duplicate,
        ));
        let ingress = fixture_ingress(store.clone(), adapter.clone());

        assert_eq!(
            ingress
                .handle(
                    Uuid::from_u128(1),
                    1,
                    "event-1".to_string(),
                    signed_headers(),
                    Bytes::from_static(b"callback-body"),
                )
                .await,
            Err(ExternalJobIngressError::Unauthorized)
        );
        assert_eq!(adapter.parse_calls.load(Ordering::SeqCst), 0);
        assert_eq!(store.apply_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_provider_fails_closed_before_authentication() {
        // Pins: a persisted provider key cannot select a placeholder or generic parser.
        let store = Arc::new(fixture_store(
            ExecutionExternalJobCallbackOutcome::Duplicate,
        ));
        let ingress = ExternalJobCallbackIngress::with_dependencies(
            store.clone(),
            ExecutionExternalJobAdapterRegistry::default(),
            Arc::new(FixtureDispatcher {
                called: AtomicBool::new(false),
            }),
        );
        assert_eq!(
            ingress
                .handle(
                    Uuid::from_u128(1),
                    1,
                    "event-1".to_string(),
                    signed_headers(),
                    Bytes::from_static(b"callback-body"),
                )
                .await,
            Err(ExternalJobIngressError::Unauthorized)
        );
        assert_eq!(store.apply_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn missing_job_and_unknown_provider_are_indistinguishable_from_bad_auth() {
        // Pins: unauthenticated callers cannot enumerate durable jobs or installed adapters.
        let mut missing = fixture_store(ExecutionExternalJobCallbackOutcome::Duplicate);
        missing.job = None;
        let ingress = ExternalJobCallbackIngress::with_dependencies(
            Arc::new(missing),
            ExecutionExternalJobAdapterRegistry::default(),
            Arc::new(FixtureDispatcher {
                called: AtomicBool::new(false),
            }),
        );
        assert_eq!(
            ingress
                .handle(
                    Uuid::from_u128(1),
                    1,
                    "event-1".to_string(),
                    signed_headers(),
                    Bytes::from_static(b"callback-body"),
                )
                .await,
            Err(ExternalJobIngressError::Unauthorized)
        );
        assert_eq!(
            ingress_error_response(ExternalJobIngressError::Unauthorized).status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn raw_body_mutation_fails_authentication_before_parsing() {
        // Pins: providers can authenticate the exact raw body; a one-byte
        // mutation cannot pass merely because a derived evidence shape exists.
        let adapter = Arc::new(FixtureAdapter {
            authenticated: true,
            authentication_error: false,
            parse_calls: AtomicUsize::new(0),
        });
        let store = Arc::new(fixture_store(
            ExecutionExternalJobCallbackOutcome::Duplicate,
        ));
        let ingress = fixture_ingress(store.clone(), adapter.clone());
        assert_eq!(
            ingress
                .handle(
                    Uuid::from_u128(1),
                    1,
                    "event-1".to_string(),
                    signed_headers(),
                    Bytes::from_static(b"callback-bodz"),
                )
                .await,
            Err(ExternalJobIngressError::Unauthorized)
        );
        assert_eq!(adapter.parse_calls.load(Ordering::SeqCst), 0);
        assert_eq!(store.apply_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stale_and_duplicate_callbacks_are_accepted_no_ops() {
        // Pins: provider retries and older generations receive success without
        // scheduling duplicate controller work.
        for (outcome, expected) in [
            (
                ExecutionExternalJobCallbackOutcome::StaleGeneration,
                ExternalJobCallbackDisposition::Stale,
            ),
            (
                ExecutionExternalJobCallbackOutcome::Duplicate,
                ExternalJobCallbackDisposition::Duplicate,
            ),
        ] {
            let adapter = Arc::new(FixtureAdapter {
                authenticated: true,
                authentication_error: false,
                parse_calls: AtomicUsize::new(0),
            });
            let store = Arc::new(fixture_store(outcome));
            let dispatcher = Arc::new(FixtureDispatcher {
                called: AtomicBool::new(false),
            });
            let ingress = ExternalJobCallbackIngress::with_dependencies(
                store,
                ExecutionExternalJobAdapterRegistry::new([
                    adapter as Arc<dyn ExecutionExternalJobAdapter>
                ])
                .expect("fixture registry"),
                dispatcher.clone(),
            );
            assert_eq!(
                ingress
                    .handle(
                        Uuid::from_u128(1),
                        1,
                        "event-1".to_string(),
                        signed_headers(),
                        Bytes::from_static(b"callback-body"),
                    )
                    .await,
                Ok(expected)
            );
            assert!(!dispatcher.called.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn applied_progress_kicks_dispatcher_for_rearmed_reconciliation() {
        // Pins: applied provider progress wakes the central dispatcher so a newly earlier
        // sparse-reconciliation deadline cannot remain pending until unrelated traffic.
        let adapter = Arc::new(FixtureAdapter {
            authenticated: true,
            authentication_error: false,
            parse_calls: AtomicUsize::new(0),
        });
        let applied_job = fixture_store(ExecutionExternalJobCallbackOutcome::Duplicate)
            .job
            .expect("fixture job");
        let store = Arc::new(fixture_store(ExecutionExternalJobCallbackOutcome::Applied(
            Box::new(applied_job),
        )));
        let dispatcher = Arc::new(FixtureDispatcher {
            called: AtomicBool::new(false),
        });
        let ingress = ExternalJobCallbackIngress::with_dependencies(
            store,
            ExecutionExternalJobAdapterRegistry::new([
                adapter as Arc<dyn ExecutionExternalJobAdapter>
            ])
            .expect("fixture registry"),
            dispatcher.clone(),
        );
        assert_eq!(
            ingress
                .handle(
                    Uuid::from_u128(1),
                    1,
                    "event-1".to_string(),
                    signed_headers(),
                    Bytes::from_static(b"callback-body"),
                )
                .await,
            Ok(ExternalJobCallbackDisposition::Applied)
        );
        assert!(dispatcher.called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn durable_applied_callback_succeeds_when_best_effort_dispatcher_kick_fails() {
        // Pins: provider acknowledgement follows the committed callback receipt;
        // a transient Restate outage leaves the outbox for bounded reconciliation.
        let adapter = Arc::new(FixtureAdapter {
            authenticated: true,
            authentication_error: false,
            parse_calls: AtomicUsize::new(0),
        });
        let applied_job = fixture_store(ExecutionExternalJobCallbackOutcome::Duplicate)
            .job
            .expect("fixture job");
        let mut store = fixture_store(ExecutionExternalJobCallbackOutcome::Applied(Box::new(
            applied_job,
        )));
        store.activation = true;
        let store = Arc::new(store);
        let dispatcher = Arc::new(FixtureDispatcher {
            called: AtomicBool::new(false),
        });
        let ingress = ExternalJobCallbackIngress::with_dependencies(
            store,
            ExecutionExternalJobAdapterRegistry::new([
                adapter as Arc<dyn ExecutionExternalJobAdapter>
            ])
            .expect("fixture registry"),
            dispatcher.clone(),
        );

        assert_eq!(
            ingress
                .handle(
                    Uuid::from_u128(1),
                    1,
                    "event-1".to_string(),
                    signed_headers(),
                    Bytes::from_static(b"callback-body"),
                )
                .await,
            Ok(ExternalJobCallbackDisposition::Applied)
        );
        assert!(dispatcher.called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn terminal_callback_without_activation_or_reconcile_does_not_kick_dispatcher() {
        // Pins: a terminal callback settled before the active attempt releases is already
        // durable and has no delivery work, so it does not emit a spurious dispatcher wake.
        let adapter = Arc::new(FixtureAdapter {
            authenticated: true,
            authentication_error: false,
            parse_calls: AtomicUsize::new(0),
        });
        let mut applied_job = fixture_store(ExecutionExternalJobCallbackOutcome::Duplicate)
            .job
            .expect("fixture job");
        applied_job.state = ExecutionExternalJobState::Completed;
        applied_job.next_reconcile_at = None;
        applied_job.completed_at = Some(Utc::now());
        let store = Arc::new(fixture_store(ExecutionExternalJobCallbackOutcome::Applied(
            Box::new(applied_job),
        )));
        let dispatcher = Arc::new(FixtureDispatcher {
            called: AtomicBool::new(false),
        });
        let ingress = ExternalJobCallbackIngress::with_dependencies(
            store,
            ExecutionExternalJobAdapterRegistry::new([
                adapter as Arc<dyn ExecutionExternalJobAdapter>
            ])
            .expect("fixture registry"),
            dispatcher.clone(),
        );

        assert_eq!(
            ingress
                .handle(
                    Uuid::from_u128(1),
                    1,
                    "event-1".to_string(),
                    signed_headers(),
                    Bytes::from_static(b"callback-body"),
                )
                .await,
            Ok(ExternalJobCallbackDisposition::Applied)
        );
        assert!(!dispatcher.called.load(Ordering::SeqCst));
    }

    fn fixture_ingress(
        store: Arc<FixtureStore>,
        adapter: Arc<FixtureAdapter>,
    ) -> ExternalJobCallbackIngress {
        ExternalJobCallbackIngress::with_dependencies(
            store,
            ExecutionExternalJobAdapterRegistry::new([
                adapter as Arc<dyn ExecutionExternalJobAdapter>
            ])
            .expect("fixture registry"),
            Arc::new(FixtureDispatcher {
                called: AtomicBool::new(false),
            }),
        )
    }

    fn fixture_store(outcome: ExecutionExternalJobCallbackOutcome) -> FixtureStore {
        FixtureStore {
            job: Some(ExecutionExternalJobRecord {
                external_job_uid: Uuid::from_u128(1),
                tenant_id: TenantId::from(Uuid::from_u128(2)),
                run_uid: Uuid::from_u128(3),
                owner: ExecutionExternalJobOwner::Task {
                    task_id: Uuid::from_u128(4),
                    attempt_generation: 1,
                },
                job_generation: 1,
                declared_provider: "fixture".to_string(),
                provider: Some("fixture".to_string()),
                provider_job_id: Some("provider-job".to_string()),
                idempotency_key: "idempotency".to_string(),
                callback_auth_reference: Some("auth-ref".to_string()),
                state: ExecutionExternalJobState::WaitingReconcile,
                progress_phase: Some("waiting".to_string()),
                cancel_supported: true,
                next_reconcile_at: Some(Utc::now()),
                last_provider_event_id: None,
                output: None,
                error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                completed_at: None,
                provider_contract_violation: None,
            }),
            outcome: Mutex::new(Some(outcome)),
            activation: false,
            apply_calls: AtomicUsize::new(0),
        }
    }

    fn fixture_activation() -> moa_execution::repository::outbox::ExecutionDispatchRecord {
        moa_execution::repository::outbox::ExecutionDispatchRecord {
            dispatch_uid: Uuid::from_u128(10),
            tenant_id: TenantId::from(Uuid::from_u128(2)),
            run_uid: Some(Uuid::from_u128(3)),
            task_id: None,
            compensation_id: None,
            trigger_uid: None,
            external_job_uid: None,
            kind: moa_execution::repository::outbox::ExecutionDispatchKind::RunActivation,
            state: moa_execution::repository::outbox::ExecutionDeliveryState::Pending,
            controller_generation: Some(1),
            wake_epoch: Some(1),
            attempt_generation: None,
            compensation_generation: None,
            compensation_attempt_generation: None,
            not_before_at: Utc::now(),
            payload: serde_json::json!({}),
            delivery_attempts: 0,
            claim_owner: None,
            claimed_at: None,
            claim_expires_at: None,
            delivered_at: None,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn signed_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-fixture-signature", "valid".parse().expect("header"));
        headers
    }
}
