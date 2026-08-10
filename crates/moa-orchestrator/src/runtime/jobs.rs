//! Background job wiring for orchestrator startup.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as AnyhowContext, Result, bail};
use moa_authz::{AwakeableResolver, FgaClient};
use moa_config::AsyncAuthzKind;
use moa_config::MoaConfig;
use moa_hands::{HandLeaseReaper, HandLeaseReaperConfig, PostgresExpiredHandLeaseClaims};
use reqwest::Client;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::services::authz_challenges_reaper::{AuthzChallengeReaper, AuthzChallengeReaperHandle};

use crate::services::action_reviews_reaper::{ActionReviewReaper, ActionReviewReaperHandle};

const DEFAULT_RESTATE_INGRESS_PORT: u16 = 8080;
/// Initial delay before retrying default cron jobs that failed a reconcile pass.
const CRON_RECONCILE_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
/// Upper bound on the exponential backoff between cron reconcile passes.
const CRON_RECONCILE_MAX_BACKOFF: Duration = Duration::from_secs(300);
/// How often configured MCP connectors are re-discovered.
const MCP_CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Starts the OpenFGA outbox poller that drains queued authorization tuple changes.
pub fn start_authz_outbox_poller(pool: &PgPool, fga_client: FgaClient) -> moa_authz::PollerHandle {
    let outbox_poller =
        moa_authz::OutboxPoller::new(pool.clone(), fga_client, moa_authz::PollerConfig::default());
    let poller_handle = outbox_poller.spawn();
    tracing::info!("authz outbox poller started");
    poller_handle
}

/// Starts the builtin async-authorization challenge reaper when configured.
pub fn start_authz_challenge_reaper_if_configured(
    pool: &PgPool,
    config: &MoaConfig,
    resolver: Arc<dyn AwakeableResolver>,
) -> Result<Option<AuthzChallengeReaperHandle>> {
    if config.async_authz.provider != AsyncAuthzKind::Builtin {
        return Ok(None);
    }
    let handle = AuthzChallengeReaper::new(pool.clone()).spawn(resolver);
    tracing::info!("authz challenge reaper started");
    Ok(Some(handle))
}

/// Starts the tenant action-review timeout reaper and queue-gauge sampler.
pub fn start_action_review_reaper(
    pool: &PgPool,
    restate_ingress_url: String,
) -> ActionReviewReaperHandle {
    let handle =
        ActionReviewReaper::with_restate_ingress(pool.clone(), restate_ingress_url).spawn();
    tracing::info!("action review reaper started");
    handle
}

/// Starts the durable hand-lease reaper that destroys expired sandboxes.
///
/// This is the destruction owner for every bounded idle timeout and hard
/// maximum lifetime the router admits. It runs independently of request
/// traffic, because the sandboxes that most need destroying belong to sessions
/// that will never send another request. Startup fails when no hand provider is
/// registered: a deployment that provisions sandboxes with no way to destroy
/// them is not a deployment MOA should serve.
pub fn start_hand_lease_reaper(
    pool: &PgPool,
    providers: Vec<Arc<dyn moa_core::traits::HandProvider>>,
) -> Result<JoinHandle<()>> {
    if providers.is_empty() {
        bail!(
            "durable hand-lease reaper requires at least one registered hand provider; \
             without one, expired sandboxes would never be destroyed"
        );
    }
    let provider_names = providers
        .iter()
        .map(|provider| provider.provider_name().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let handle = HandLeaseReaper::new(
        Arc::new(PostgresExpiredHandLeaseClaims::new(pool.clone())),
        providers,
        HandLeaseReaperConfig::default(),
    )
    .spawn();
    tracing::info!(providers = %provider_names, "durable hand lease reaper started");
    Ok(handle)
}

/// Starts the supervised durable workspace maintenance owner.
///
/// The first complete reconciliation/retention/inventory pass must finish
/// before the handle reports ready. Any pass error terminates the task; `main`
/// treats that join as process-fatal instead of serving without a cleanup owner.
pub fn start_workspace_reaper(
    coordinator: Arc<
        moa_hands::core::sandbox_workspace::maintenance::WorkspaceMaintenanceCoordinator,
    >,
    config: &MoaConfig,
) -> Result<moa_hands::core::sandbox_workspace::reaper::WorkspaceReaperHandle> {
    let maximum_age = Duration::from_secs(
        config
            .sandbox_workspaces
            .reaper_heartbeat_maximum_age_seconds,
    );
    let interval = Duration::from_secs((maximum_age.as_secs() / 3).clamp(1, 10));
    let batch_size = i64::from(config.sandbox_checkpoints.retention.gc_batch_size);
    let reaper = coordinator
        .workspace_reaper(8)
        .context("build durable workspace reaper")?;
    let handle = moa_hands::core::sandbox_workspace::reaper::WorkspaceReaperHandle::spawn(
        coordinator,
        reaper,
        interval,
        batch_size,
        maximum_age,
    )
    .context("start durable workspace reaper")?;
    tracing::info!(
        interval_seconds = interval.as_secs(),
        heartbeat_maximum_age_seconds = maximum_age.as_secs(),
        batch_size,
        "durable workspace reaper started"
    );
    Ok(handle)
}

/// Supervised refresh owner for authenticated checkpoint-bucket versioning state.
pub struct CheckpointBucketVersioningRefreshHandle {
    observer:
        moa_hands::core::sandbox_workspace::checkpoint::versioning::CheckpointBucketVersioningObserver,
    shutdown: CancellationToken,
    task: JoinHandle<Result<()>>,
}

impl CheckpointBucketVersioningRefreshHandle {
    /// Waits for the refresh task to exit so the process supervisor can fail closed.
    pub async fn task_result(&mut self) -> Result<()> {
        let result = (&mut self.task)
            .await
            .context("join checkpoint bucket versioning refresh task");
        self.observer.invalidate();
        result?
    }

    /// Invalidates readiness and stops the refresh task during graceful shutdown.
    pub async fn shutdown(self) -> Result<()> {
        self.observer.invalidate();
        self.shutdown.cancel();
        self.task
            .await
            .context("join checkpoint bucket versioning refresh task")?
    }
}

/// Starts periodic authenticated checkpoint-bucket versioning observation.
///
/// Runtime dependency construction has already completed the first observation.
/// Each refresh is scheduled strictly before that observation can become stale.
/// Any ambiguous, versioned, timed-out, or unauthenticated observation exits the
/// task; `main` treats that unexpected exit as process-fatal.
pub fn start_checkpoint_bucket_versioning_refresh(
    observer: moa_hands::core::sandbox_workspace::checkpoint::versioning::CheckpointBucketVersioningObserver,
) -> CheckpointBucketVersioningRefreshHandle {
    let interval = observer.recommended_refresh_interval();
    let shutdown = CancellationToken::new();
    let task_observer = observer.clone();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = task_shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep(interval) => {
                    if let Err(error) = task_observer.observe_unversioned().await {
                        task_observer.invalidate();
                        return Err(anyhow::Error::new(error)
                            .context("refresh authenticated checkpoint bucket versioning state"));
                    }
                }
            }
        }
    });
    tracing::info!(
        interval_seconds = interval.as_secs_f64(),
        "checkpoint bucket versioning refresh started"
    );
    CheckpointBucketVersioningRefreshHandle {
        observer,
        shutdown,
        task,
    }
}

/// Starts periodic MCP connector catalog refresh, when any connector is configured.
///
/// Returns `None` for a deployment with no connectors: there is nothing to
/// rediscover, and a timer that wakes only to find an empty configuration is
/// noise. When it does run, it is what lets an optional connector that was down
/// at startup start serving without a restart, and what re-pins a connector
/// whose published schemas changed.
pub fn start_mcp_catalog_refresh(
    config: &MoaConfig,
    tool_router: Arc<moa_hands::ToolRouter>,
) -> Option<JoinHandle<()>> {
    if config.mcp_servers.is_empty() {
        return None;
    }
    tracing::info!(
        connectors = config.mcp_servers.len(),
        interval_secs = MCP_CATALOG_REFRESH_INTERVAL.as_secs(),
        "MCP connector catalog refresh started"
    );
    Some(moa_hands::spawn_mcp_catalog_refresh(
        tool_router,
        MCP_CATALOG_REFRESH_INTERVAL,
    ))
}

/// Normalizes the Restate ingress URL used by cron bootstrap and awakeables.
#[must_use]
pub fn restate_ingress_base_url(configured_ingress_url: &str) -> String {
    let trimmed = configured_ingress_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return format!("http://localhost:{DEFAULT_RESTATE_INGRESS_PORT}");
    }
    trimmed.to_string()
}

/// Bounded exponential backoff between default cron reconcile passes.
#[derive(Debug, Clone, Copy)]
struct CronReconcileBackoff {
    initial: Duration,
    max: Duration,
}

impl Default for CronReconcileBackoff {
    fn default() -> Self {
        Self {
            initial: CRON_RECONCILE_INITIAL_BACKOFF,
            max: CRON_RECONCILE_MAX_BACKOFF,
        }
    }
}

/// Builds the HTTP client used to configure default cron jobs.
fn cron_bootstrap_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build cron-bootstrap HTTP client")
}

/// Installs all required Restate cron jobs and keeps retrying incomplete passes.
///
/// This is called only by the dedicated bootstrap process. Runtime replicas do
/// not own deployment registration or cluster bootstrap reconciliation.
pub async fn install_default_cron_jobs(ingress_url: &str) -> Result<()> {
    let client = cron_bootstrap_client()?;
    reconcile_default_cron_jobs(
        &client,
        ingress_url.trim_end_matches('/'),
        default_cron_jobs(),
        CronReconcileBackoff::default(),
    )
    .await;
    Ok(())
}

/// Reconciles every default cron job independently, retrying failures forever.
///
/// Each pass attempts all still-pending jobs; a per-job failure is collected and
/// retried on the next pass rather than aborting the remaining jobs, so a single
/// failing job can no longer silently omit later ones (vector-outbox drains,
/// consolidation, or materialized-view maintenance). The delay between passes
/// grows exponentially up to `backoff.max`. A per-pass warning remains published
/// until every required job is installed. This intentionally never gives up:
/// leaving required maintenance uninstalled until process restart is worse than
/// a bounded-backoff background retry.
async fn reconcile_default_cron_jobs(
    client: &Client,
    ingress_url: &str,
    jobs: Vec<DefaultCronJob>,
    backoff: CronReconcileBackoff,
) {
    let required = jobs.len();
    let mut pending = jobs;
    let mut delay = backoff.initial;
    let mut pass: u32 = 0;
    loop {
        pass += 1;
        pending = configure_pending_cron_jobs(client, ingress_url, pending, pass).await;
        if pending.is_empty() {
            tracing::info!(required, passes = pass, "all default cron jobs installed");
            return;
        }
        tracing::warn!(
            pass,
            outstanding = pending.len(),
            required,
            backoff_secs = delay.as_secs(),
            "default cron installation degraded; retrying uninstalled jobs after backoff"
        );
        tokio::time::sleep(delay).await;
        delay = delay.saturating_mul(2).min(backoff.max);
    }
}

/// Attempts one configure request per pending job, returning the ones that failed.
///
/// Every job is attempted regardless of earlier failures in the same pass; the
/// returned vector preserves the still-failing jobs for the caller to retry.
async fn configure_pending_cron_jobs(
    client: &Client,
    ingress_url: &str,
    jobs: Vec<DefaultCronJob>,
    pass: u32,
) -> Vec<DefaultCronJob> {
    let mut still_failed = Vec::new();
    for job in jobs {
        match configure_cron_job(client, ingress_url, &job).await {
            Ok(()) => tracing::info!(key = job.key, "default cron job configured"),
            Err(error) => {
                metrics::counter!(
                    "moa_cron_bootstrap_configure_failures_total",
                    "job" => job.key
                )
                .increment(1);
                tracing::warn!(
                    key = job.key,
                    pass,
                    error = %error,
                    "failed to configure default cron job; will retry"
                );
                still_failed.push(job);
            }
        }
    }
    still_failed
}

/// Sends a single idempotent configure request for one default cron job.
async fn configure_cron_job(
    client: &Client,
    ingress_url: &str,
    job: &DefaultCronJob,
) -> Result<()> {
    let response = crate::restate_identity::with_reqwest_trace_headers(
        client
            .post(format!(
                "{ingress_url}/restate/call/CronJob/{}/configure",
                job.key
            ))
            .header(
                "idempotency-key",
                format!("cron-config-{}-{}", job.key, job.version),
            )
            .header("content-type", "application/json")
            .json(&job.body),
    )
    .send()
    .await
    .with_context(|| format!("configure cron job {}", job.key))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<failed to read response body: {error}>"));
        bail!("cron configure {} returned {status}: {text}", job.key);
    }

    Ok(())
}

struct DefaultCronJob {
    key: &'static str,
    body: serde_json::Value,
    version: &'static str,
}

fn default_cron_jobs() -> Vec<DefaultCronJob> {
    vec![
        DefaultCronJob {
            key: "graph_memory_compact",
            body: serde_json::json!({
                "schedule": "0 0 * * * *",
                "timezone": "UTC",
                "target_service": "GraphMemoryMaint",
                "target_handler": "compact",
                "payload": {}
            }),
            version: "v1",
        },
        DefaultCronJob {
            key: "vector_sync_outbox_drain",
            body: serde_json::json!({
                "schedule": "0 * * * * *",
                "timezone": "UTC",
                "target_service": "GraphMemoryMaint",
                "target_handler": "sync_vectors",
                "payload": { "limit": 512 }
            }),
            version: "v1",
        },
        DefaultCronJob {
            key: "segment_materialized_views_refresh",
            body: serde_json::json!({
                "schedule": "0 */15 * * * *",
                "timezone": "UTC",
                "target_service": "SessionStore",
                "target_handler": "refresh_segment_materialized_views",
                "payload": {}
            }),
            version: "v1",
        },
        DefaultCronJob {
            key: "analytics_materialized_views_refresh",
            body: serde_json::json!({
                "schedule": "0 */15 * * * *",
                "timezone": "UTC",
                "target_service": "SessionStore",
                "target_handler": "refresh_analytics_materialized_views",
                "payload": {}
            }),
            version: "v1",
        },
        DefaultCronJob {
            key: "skill_regression_monitor",
            body: serde_json::json!({
                "schedule": "0 */15 * * * *",
                "timezone": "UTC",
                "target_service": "SessionStore",
                "target_handler": "monitor_skill_regressions",
                "payload": {}
            }),
            version: "v1",
        },
        DefaultCronJob {
            key: "task_recurrence_monitor",
            body: serde_json::json!({
                "schedule": "0 */15 * * * *",
                "timezone": "UTC",
                "target_service": "SessionStore",
                "target_handler": "mine_task_recurrences",
                "payload": {}
            }),
            version: "v1",
        },
        DefaultCronJob {
            key: "learning_embeddings_backfill",
            body: serde_json::json!({
                "schedule": "0 */15 * * * *",
                "timezone": "UTC",
                "target_service": "SessionStore",
                "target_handler": "backfill_learning_embeddings",
                "payload": {}
            }),
            version: "v1",
        },
        DefaultCronJob {
            key: "neon_prune_branches",
            body: serde_json::json!({
                "schedule": "0 0 0,6,12,18 * * *",
                "timezone": "UTC",
                "target_service": "NeonMaint",
                "target_handler": "prune_branches",
                "payload": null
            }),
            version: "v1",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::Path, extract::State, http::StatusCode, routing::post};
    use moa_config::{ObjectStoreConfig, ObjectStoreCredentialMode};
    use moa_hands::core::sandbox_workspace::checkpoint::store::CheckpointObjectStore;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Shared state for the mock Restate CronJob configure endpoint.
    struct MockCronState {
        hits: Mutex<HashMap<String, usize>>,
        fail_first_n: HashMap<String, usize>,
    }

    async fn configure_handler(
        State(state): State<Arc<MockCronState>>,
        Path(key): Path<String>,
    ) -> StatusCode {
        let count = {
            let mut hits = state.hits.lock().expect("mock cron hits lock");
            let count = hits.entry(key.clone()).or_insert(0);
            *count += 1;
            *count
        };
        let fail_n = state.fail_first_n.get(&key).copied().unwrap_or(0);
        if count <= fail_n {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        }
    }

    async fn spawn_mock_cron_admin(
        fail_first_n: HashMap<String, usize>,
    ) -> (String, Arc<MockCronState>) {
        let state = Arc::new(MockCronState {
            hits: Mutex::new(HashMap::new()),
            fail_first_n,
        });
        let app = Router::new()
            .route(
                "/restate/call/CronJob/{key}/configure",
                post(configure_handler),
            )
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock cron admin");
        let address = listener.local_addr().expect("mock cron admin address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), state)
    }

    fn fast_backoff() -> CronReconcileBackoff {
        CronReconcileBackoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(1),
        }
    }

    fn hit(state: &Arc<MockCronState>, key: &str) -> usize {
        state
            .hits
            .lock()
            .expect("mock cron hits lock")
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    async fn spawn_s3_versioning_sequence(bodies: [&'static str; 2]) -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind S3 versioning fixture");
        let address = listener.local_addr().expect("S3 fixture address");
        let task = tokio::spawn(async move {
            for body in bodies {
                let (mut stream, _) = listener.accept().await.expect("accept S3 request");
                let mut request = vec![0_u8; 8 * 1024];
                let _ = stream.read(&mut request).await.expect("read S3 request");
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write S3 response");
            }
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn versioning_refresh_invalidates_readiness_and_exits_on_provider_drift() {
        // Pins: a provider that enables versioning after startup closes the
        // exact gate used by checkpoint deletion and terminates the supervised
        // refresh owner instead of leaving readiness on until observation age-out.
        let (endpoint, server) = spawn_s3_versioning_sequence([
            "<VersioningConfiguration/>",
            "<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>",
        ])
        .await;
        let mut config = MoaConfig {
            object_store: ObjectStoreConfig {
                credential_mode: ObjectStoreCredentialMode::Static,
                endpoint: Some(endpoint),
                access_key_id: Some("fixture-access".to_string()),
                secret_access_key: Some("fixture-secret".to_string()),
                allow_http: true,
                ..ObjectStoreConfig::default()
            },
            ..MoaConfig::default()
        };
        config
            .sandbox_checkpoints
            .versioning_observation
            .maximum_age_seconds = 1;
        config
            .sandbox_checkpoints
            .versioning_observation
            .timeout_seconds = 1;
        let (_store, observer) = CheckpointObjectStore::from_config_with_versioning_observer(
            &config,
            Arc::new(moa_crypto::LocalKmsProvider::new()),
        )
        .expect("build checkpoint observer fixture");
        observer
            .observe_unversioned()
            .await
            .expect("initial unversioned observation should open readiness");
        assert!(observer.is_ready());

        let mut handle = start_checkpoint_bucket_versioning_refresh(observer.clone());
        let error = tokio::time::timeout(Duration::from_secs(2), handle.task_result())
            .await
            .expect("refresh owner should exit after versioning drift")
            .expect_err("versioning drift must terminate refresh supervision");

        assert!(error.to_string().contains("refresh authenticated"));
        assert!(!observer.is_ready());
        server.await.expect("join S3 versioning fixture");
    }

    #[tokio::test]
    async fn one_failing_job_does_not_prevent_later_jobs_and_failures_are_collected() {
        // Pins (F17): a single pass attempts every pending job regardless of
        // earlier failures, and returns exactly the still-failing jobs so later
        // jobs are never skipped by a first-error-wins abort.
        let mut fail_first_n = HashMap::new();
        // Fail the first job and a middle job on every attempt this pass.
        fail_first_n.insert("graph_memory_compact".to_string(), usize::MAX);
        fail_first_n.insert("segment_materialized_views_refresh".to_string(), usize::MAX);
        let (base_url, state) = spawn_mock_cron_admin(fail_first_n).await;
        let client = cron_bootstrap_client().expect("client");

        let still_failed =
            configure_pending_cron_jobs(&client, &base_url, default_cron_jobs(), 1).await;

        // Every job was attempted exactly once even though the first one failed.
        for key in [
            "graph_memory_compact",
            "vector_sync_outbox_drain",
            "segment_materialized_views_refresh",
            "neon_prune_branches",
        ] {
            assert_eq!(hit(&state, key), 1, "job {key} should be attempted once");
        }

        // Only the two failing jobs are collected for retry, in original order.
        let failed_keys: Vec<&str> = still_failed.iter().map(|job| job.key).collect();
        assert_eq!(
            failed_keys,
            vec!["graph_memory_compact", "segment_materialized_views_refresh"],
        );
    }

    #[tokio::test]
    async fn reconcile_retries_failed_jobs_until_every_job_is_installed() {
        // Pins (F17): a job that fails its first attempt is retried on a later
        // pass and eventually installed; reconciliation only returns once every
        // required job succeeds.
        let mut fail_first_n = HashMap::new();
        // This job returns 500 on its first attempt, then succeeds.
        fail_first_n.insert("vector_sync_outbox_drain".to_string(), 1);
        let (base_url, state) = spawn_mock_cron_admin(fail_first_n).await;
        let client = cron_bootstrap_client().expect("client");

        // Returning at all proves it did not give up after the first failed pass.
        reconcile_default_cron_jobs(&client, &base_url, default_cron_jobs(), fast_backoff()).await;

        // The flaky job was retried; the others installed on the first pass.
        assert_eq!(
            hit(&state, "vector_sync_outbox_drain"),
            2,
            "flaky job should be retried once after its initial failure"
        );
        for key in [
            "graph_memory_compact",
            "segment_materialized_views_refresh",
            "neon_prune_branches",
        ] {
            assert_eq!(
                hit(&state, key),
                1,
                "already-installed job {key} should not be re-attempted"
            );
        }
    }

    #[test]
    fn default_cron_jobs_include_skill_regression_monitor() {
        // Pins: the post-promotion regression monitor is installed as a 15-minute
        // SessionStore cron job so a regressed skill becomes a reviewed rollback
        // proposal instead of a silent decline.
        let jobs = default_cron_jobs();
        let job = jobs
            .iter()
            .find(|job| job.key == "skill_regression_monitor")
            .expect("default skill regression monitor cron job should be installed");

        assert_eq!(job.version, "v1");
        assert_eq!(job.body["schedule"], "0 */15 * * * *");
        assert_eq!(job.body["target_service"], "SessionStore");
        assert_eq!(job.body["target_handler"], "monitor_skill_regressions");
    }

    #[test]
    fn default_cron_jobs_include_task_recurrence_monitor() {
        // Pins: the exact-fingerprint recurrence monitor is installed as a 15-minute
        // SessionStore cron job so a task that recurs across sub-gate sessions
        // dispatches skill learning instead of being invisible.
        let jobs = default_cron_jobs();
        let job = jobs
            .iter()
            .find(|job| job.key == "task_recurrence_monitor")
            .expect("default task recurrence monitor cron job should be installed");

        assert_eq!(job.version, "v1");
        assert_eq!(job.body["schedule"], "0 */15 * * * *");
        assert_eq!(job.body["target_service"], "SessionStore");
        assert_eq!(job.body["target_handler"], "mine_task_recurrences");
    }

    #[test]
    fn default_cron_jobs_include_learning_embeddings_backfill() {
        // Pins: the learning-embeddings backfill is installed as a 15-minute
        // SessionStore cron job so task-summary and skill-identity embeddings are
        // populated out-of-band, never on the turn path.
        let jobs = default_cron_jobs();
        let job = jobs
            .iter()
            .find(|job| job.key == "learning_embeddings_backfill")
            .expect("default learning embeddings backfill cron job should be installed");

        assert_eq!(job.version, "v1");
        assert_eq!(job.body["schedule"], "0 */15 * * * *");
        assert_eq!(job.body["target_service"], "SessionStore");
        assert_eq!(job.body["target_handler"], "backfill_learning_embeddings");
    }

    #[test]
    fn default_cron_jobs_include_vector_sync_drain() {
        // Pins: external vector backend backlog draining is a background maintenance path,
        // not only a graph-write post-commit side effect.
        let jobs = default_cron_jobs();
        let job = jobs
            .iter()
            .find(|job| job.key == "vector_sync_outbox_drain")
            .expect("default vector sync drain cron job should be installed");

        assert_eq!(job.version, "v1");
        assert_eq!(job.body["target_service"], "GraphMemoryMaint");
        assert_eq!(job.body["target_handler"], "sync_vectors");
        assert_eq!(job.body["payload"]["limit"], 512);
    }
}
