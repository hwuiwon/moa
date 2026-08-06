//! Dedicated Restate bootstrap and one-way Session state cutover.

use std::time::Duration;

use anyhow::{Context as AnyhowContext, Result, bail};
use moa_core::types::identifiers::SessionId;
use reqwest::Client;
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;

use crate::objects::session_status_migrator::{
    SessionStatusIdleMigrationRequest, SessionStatusIdleMigrationResponse,
};
use crate::runtime::endpoint::{DeploymentListResponse, services_registered};
use crate::runtime::jobs::install_default_cron_jobs;

const BOOTSTRAP_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const REGISTRATION_ATTEMPTS: u32 = 180;
const REGISTRATION_INTERVAL: Duration = Duration::from_secs(2);
const SESSION_STATUS_IDLE_CUTOVER: &str = "session_status_idle_v54";
const STATUS_MIGRATION_SERVICES: &[&str] = &["Session", "StatusMigrationDispatcher"];

#[derive(Debug, Deserialize)]
struct RestateQueryResponse<T> {
    rows: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct ActiveInvocationRow {
    id: String,
    status: String,
}

/// Explicit capabilities granted to the dedicated bootstrap process.
#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    /// Restate Admin API used only to observe Operator-owned registration.
    pub admin_url: String,
    /// Restate ingress used for health, cron configuration, and migration dispatch.
    pub ingress_url: String,
    /// Privileged Postgres URL required to enumerate all sessions and verify cutover.
    pub database_url: String,
    /// Migration-only handler URI registered for the raw-state stage.
    pub migration_deployment_uri: String,
    /// Steady-state handler URI to register after cutover when no Operator is present.
    pub runtime_deployment_uri: Option<String>,
}

/// Verifiable summary of a completed bootstrap pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    /// Number of Session virtual objects inspected by the one-way migration.
    pub sessions_migrated: usize,
    /// Number of standalone Session status keys rewritten.
    pub status_keys_rewritten: usize,
    /// Number of Session metadata mirrors rewritten.
    pub meta_statuses_rewritten: usize,
}

/// Waits for Operator registration, applies the one-way status cutover, and installs cron jobs.
pub async fn run(options: BootstrapOptions) -> Result<BootstrapReport> {
    let client = Client::builder()
        .timeout(BOOTSTRAP_HTTP_TIMEOUT)
        .build()
        .context("build Restate bootstrap HTTP client")?;
    let admin_url = options.admin_url.trim_end_matches('/');
    let ingress_url = options.ingress_url.trim_end_matches('/');

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(BOOTSTRAP_HTTP_TIMEOUT)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET search_path TO moa, public")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&options.database_url)
        .await
        .context("connect bootstrap database pool")?;
    moa_migrations::validate_complete_history(&pool)
        .await
        .context("exact-image database migration runner has not completed")?;

    let report = if let Some(report) = cutover_receipt_report(&pool).await? {
        verify_postgres_cutover(&pool).await?;
        report
    } else {
        register_deployment(&client, admin_url, &options.migration_deployment_uri).await?;
        let migration_deployment = wait_for_status_migration_services(
            &client,
            admin_url,
            Some(&options.migration_deployment_uri),
        )
        .await?;
        let session_ids =
            sqlx::query_scalar::<_, SessionId>("SELECT id FROM public.sessions ORDER BY id")
                .fetch_all(&pool)
                .await
                .context("enumerate Postgres sessions for Restate state cutover")?;
        let mut report = BootstrapReport {
            sessions_migrated: session_ids.len(),
            status_keys_rewritten: 0,
            meta_statuses_rewritten: 0,
        };
        for session_id in session_ids {
            let migration = migrate_session(&client, ingress_url, session_id).await?;
            validate_session_migration(session_id, &migration)?;
            report.status_keys_rewritten += usize::from(migration.status_rewritten);
            report.meta_statuses_rewritten += usize::from(migration.meta_status_rewritten);
        }

        verify_postgres_cutover(&pool).await?;
        deregister_deployment(&client, admin_url, &migration_deployment.id).await?;
        record_cutover_receipt(&pool, &report).await?;
        report
    };
    if let Some(runtime_deployment_uri) = options.runtime_deployment_uri.as_deref() {
        register_deployment(&client, admin_url, runtime_deployment_uri).await?;
    }

    wait_for_expected_services(&client, admin_url).await?;
    check_public_health(&client, ingress_url).await?;
    install_default_cron_jobs(ingress_url).await?;
    Ok(report)
}

/// Waits until the admin-verified one-way Session status cutover receipt exists.
///
/// Runtime database roles intentionally cannot perform a global RLS-bypassing
/// scan of every Session. The dedicated bootstrap identity performs that scan
/// before it is allowed to write this receipt.
pub async fn wait_for_session_status_cutover(database_url: &str) -> Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(BOOTSTRAP_HTTP_TIMEOUT)
        .connect(database_url)
        .await
        .context("connect cutover-gate database pool")?;
    let mut last_error = "cutover receipt has not been queried".to_string();
    for _attempt in 1..=REGISTRATION_ATTEMPTS {
        match cutover_receipt_report(&pool).await {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => last_error = "cutover receipt is absent".to_string(),
            Err(error) => last_error = error.to_string(),
        }
        tokio::time::sleep(REGISTRATION_INTERVAL).await;
    }
    bail!("timed out waiting for pre-runtime {SESSION_STATUS_IDLE_CUTOVER} cutover: {last_error}")
}

fn validate_session_migration(
    expected_session_id: SessionId,
    migration: &SessionStatusIdleMigrationResponse,
) -> Result<()> {
    if migration.session_id != expected_session_id
        || migration.retired_values_remaining != 0
        || migration.status.as_deref() == Some("paused")
        || migration.meta_status.as_deref() == Some("paused")
    {
        bail!(
            "Session {expected_session_id} migration returned mismatched or retired state: \
             {migration:?}"
        );
    }
    Ok(())
}

async fn register_deployment(client: &Client, admin_url: &str, deployment_uri: &str) -> Result<()> {
    let mut last_error = "no registration attempt".to_string();
    for attempt in 1..=REGISTRATION_ATTEMPTS {
        match client
            .post(format!("{admin_url}/deployments"))
            .header("content-type", "application/json")
            .json(&serde_json::json!({ "uri": deployment_uri }))
            .send()
            .await
        {
            Ok(response)
                if response.status().is_success()
                    || response.status() == reqwest::StatusCode::CONFLICT =>
            {
                tracing::info!(attempt, "registered local Restate deployment");
                return Ok(());
            }
            Ok(response) => {
                let status = response.status();
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|error| format!("<failed to read response body: {error}>"));
                last_error = format!("status {status}: {body}");
            }
            Err(error) => last_error = error.to_string(),
        }
        tokio::time::sleep(REGISTRATION_INTERVAL).await;
    }
    bail!(
        "timed out after {REGISTRATION_ATTEMPTS} attempts registering local Restate deployment: \
         {last_error}"
    )
}

async fn wait_for_status_migration_services(
    client: &Client,
    admin_url: &str,
    deployment_uri: Option<&str>,
) -> Result<crate::runtime::endpoint::RegisteredDeployment> {
    let mut last_error = "no migration deployment observation".to_string();
    for _attempt in 1..=REGISTRATION_ATTEMPTS {
        match client.get(format!("{admin_url}/deployments")).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<DeploymentListResponse>().await {
                    Ok(payload) => {
                        if let Some(deployment) =
                            payload.deployments.into_iter().find(|deployment| {
                                is_status_migration_deployment(deployment, deployment_uri)
                            })
                        {
                            return Ok(deployment);
                        }
                        last_error =
                            "migration-only Session services are not registered".to_string();
                    }
                    Err(error) => last_error = format!("decode deployment list: {error}"),
                },
                Err(error) => last_error = format!("Admin API status: {error}"),
            },
            Err(error) => last_error = format!("reach Admin API: {error}"),
        }
        tokio::time::sleep(REGISTRATION_INTERVAL).await;
    }
    bail!("timed out waiting for migration-only Restate endpoint: {last_error}")
}

fn is_status_migration_deployment(
    deployment: &crate::runtime::endpoint::RegisteredDeployment,
    expected_uri: Option<&str>,
) -> bool {
    expected_uri.is_none_or(|uri| {
        deployment
            .uri
            .as_deref()
            .is_some_and(|registered| endpoint_uris_match(registered, uri))
    }) && STATUS_MIGRATION_SERVICES.iter().all(|expected| {
        deployment
            .services
            .iter()
            .any(|service| service.name == *expected)
    }) && !deployment
        .services
        .iter()
        .any(|service| service.name == "Health")
}

fn endpoint_uris_match(registered: &str, expected: &str) -> bool {
    registered.trim_end_matches('/') == expected.trim_end_matches('/')
}

async fn deregister_deployment(
    client: &Client,
    admin_url: &str,
    deployment_id: &str,
) -> Result<()> {
    deregister_deployment_with_policy(
        client,
        admin_url,
        deployment_id,
        REGISTRATION_ATTEMPTS,
        REGISTRATION_INTERVAL,
    )
    .await
}

async fn deregister_deployment_with_policy(
    client: &Client,
    admin_url: &str,
    deployment_id: &str,
    poll_attempts: u32,
    poll_interval: Duration,
) -> Result<()> {
    let active = active_invocations_pinned_to_deployment(client, admin_url, deployment_id).await?;
    if let Some(invocation) = active.first() {
        bail!(
            "refusing to force-delete migration deployment {deployment_id}: active invocation {} is still pinned with status {}",
            invocation.id,
            invocation.status
        );
    }

    let response = client
        .delete(deployment_delete_url(admin_url, deployment_id))
        .send()
        .await
        .context("deregister migration-only Restate deployment")?;
    if response.status() != reqwest::StatusCode::ACCEPTED {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("<failed to read response body: {error}>"));
        bail!(
            "migration-only Restate deployment deletion returned {status}, expected 202 Accepted: {body}"
        );
    }

    wait_for_deployment_absence(
        client,
        admin_url,
        deployment_id,
        poll_attempts,
        poll_interval,
    )
    .await
}

fn deployment_delete_url(admin_url: &str, deployment_id: &str) -> String {
    format!("{admin_url}/deployments/{deployment_id}?force=true")
}

async fn active_invocations_pinned_to_deployment(
    client: &Client,
    admin_url: &str,
    deployment_id: &str,
) -> Result<Vec<ActiveInvocationRow>> {
    let query = active_invocations_query(deployment_id);
    let response = client
        .post(format!("{admin_url}/query"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await
        .context("query migration deployment invocations")?
        .error_for_status()
        .context("migration deployment invocation query failed")?;
    response
        .json::<RestateQueryResponse<ActiveInvocationRow>>()
        .await
        .context("decode migration deployment invocation query")
        .map(|payload| payload.rows)
}

fn active_invocations_query(deployment_id: &str) -> String {
    let deployment_id = deployment_id.replace('\'', "''");
    format!(
        "SELECT id, status FROM sys_invocation \
         WHERE (pinned_deployment_id = '{deployment_id}' \
         OR last_attempt_deployment_id = '{deployment_id}') \
         AND status NOT IN ('completed', 'killed') LIMIT 1"
    )
}

async fn wait_for_deployment_absence(
    client: &Client,
    admin_url: &str,
    deployment_id: &str,
    poll_attempts: u32,
    poll_interval: Duration,
) -> Result<()> {
    let mut last_error = "deployment deletion has not been observed".to_string();
    for attempt in 1..=poll_attempts {
        match client.get(format!("{admin_url}/deployments")).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<DeploymentListResponse>().await {
                    Ok(payload)
                        if payload
                            .deployments
                            .iter()
                            .all(|deployment| deployment.id != deployment_id) =>
                    {
                        tracing::info!(
                            attempt,
                            deployment_id,
                            "migration-only Restate deployment deletion completed"
                        );
                        return Ok(());
                    }
                    Ok(_) => last_error = format!("deployment {deployment_id} is still registered"),
                    Err(error) => last_error = format!("decode deployment list: {error}"),
                },
                Err(error) => last_error = format!("Admin API status: {error}"),
            },
            Err(error) => last_error = format!("reach Admin API: {error}"),
        }
        tokio::time::sleep(poll_interval).await;
    }
    bail!(
        "timed out after {poll_attempts} attempts waiting for migration deployment {deployment_id} deletion: {last_error}"
    )
}

async fn wait_for_expected_services(client: &Client, admin_url: &str) -> Result<()> {
    let mut last_error = "no registration observation".to_string();
    for attempt in 1..=REGISTRATION_ATTEMPTS {
        match client.get(format!("{admin_url}/deployments")).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<DeploymentListResponse>().await {
                    Ok(payload) if services_registered(&payload.deployments) => {
                        tracing::info!(attempt, "Operator-registered Restate services are ready");
                        return Ok(());
                    }
                    Ok(_) => last_error = "expected services are not registered".to_string(),
                    Err(error) => last_error = format!("decode deployment list: {error}"),
                },
                Err(error) => last_error = format!("Admin API status: {error}"),
            },
            Err(error) => last_error = format!("reach Admin API: {error}"),
        }
        tokio::time::sleep(REGISTRATION_INTERVAL).await;
    }
    bail!(
        "timed out after {REGISTRATION_ATTEMPTS} attempts waiting for Operator registration: {last_error}"
    )
}

async fn check_public_health(client: &Client, ingress_url: &str) -> Result<()> {
    client
        .post(format!("{ingress_url}/restate/call/Health/check"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .context("call public Restate Health/check")?
        .error_for_status()
        .context("public Restate Health/check returned an error")?;
    Ok(())
}

async fn migrate_session(
    client: &Client,
    ingress_url: &str,
    session_id: SessionId,
) -> Result<SessionStatusIdleMigrationResponse> {
    let response = client
        .post(format!(
            "{ingress_url}/restate/call/StatusMigrationDispatcher/migrate"
        ))
        .header("content-type", "application/json")
        .header(
            "idempotency-key",
            format!("session-status-idle-{session_id}"),
        )
        .json(&SessionStatusIdleMigrationRequest { session_id })
        .send()
        .await
        .with_context(|| format!("dispatch Session {session_id} status migration"))?
        .error_for_status()
        .with_context(|| format!("Session {session_id} status migration returned an error"))?;
    response
        .json()
        .await
        .with_context(|| format!("decode Session {session_id} status migration response"))
}

async fn verify_postgres_cutover(pool: &sqlx::PgPool) -> Result<()> {
    let retired: i64 = sqlx::query_scalar(
        "SELECT \
           (SELECT COUNT(*) FROM public.sessions WHERE status = 'paused') + \
           (SELECT COUNT(*) FROM public.events WHERE event_type = 'SessionStatusChanged' \
             AND (payload #>> '{data,from}' = 'paused' OR payload #>> '{data,to}' = 'paused'))",
    )
    .fetch_one(pool)
    .await
    .context("verify Postgres session status cutover")?;
    if retired != 0 {
        bail!("Postgres still contains {retired} retired session lifecycle values");
    }
    Ok(())
}

async fn record_cutover_receipt(pool: &sqlx::PgPool, report: &BootstrapReport) -> Result<()> {
    sqlx::query(
        "INSERT INTO public.deployment_cutover_receipts \
            (cutover_name, sessions_verified, status_keys_rewritten, meta_statuses_rewritten) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (cutover_name) DO UPDATE SET \
            completed_at = NOW(), \
            sessions_verified = EXCLUDED.sessions_verified, \
            status_keys_rewritten = EXCLUDED.status_keys_rewritten, \
            meta_statuses_rewritten = EXCLUDED.meta_statuses_rewritten",
    )
    .bind(SESSION_STATUS_IDLE_CUTOVER)
    .bind(i64::try_from(report.sessions_migrated).context("session count exceeds BIGINT")?)
    .bind(i64::try_from(report.status_keys_rewritten).context("status count exceeds BIGINT")?)
    .bind(i64::try_from(report.meta_statuses_rewritten).context("metadata count exceeds BIGINT")?)
    .execute(pool)
    .await
    .context("record completed Session status cutover")?;
    Ok(())
}

async fn cutover_receipt_report(pool: &sqlx::PgPool) -> Result<Option<BootstrapReport>> {
    let row = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT sessions_verified, status_keys_rewritten, meta_statuses_rewritten \
         FROM public.deployment_cutover_receipts WHERE cutover_name = $1",
    )
    .bind(SESSION_STATUS_IDLE_CUTOVER)
    .fetch_optional(pool)
    .await
    .context("read Session status cutover receipt")?;
    row.map(
        |(sessions_migrated, status_keys_rewritten, meta_statuses_rewritten)| {
            Ok(BootstrapReport {
                sessions_migrated: usize::try_from(sessions_migrated)
                    .context("stored session cutover count is outside usize")?,
                status_keys_rewritten: usize::try_from(status_keys_rewritten)
                    .context("stored status cutover count is outside usize")?,
                meta_statuses_rewritten: usize::try_from(meta_statuses_rewritten)
                    .context("stored metadata cutover count is outside usize")?,
            })
        },
    )
    .transpose()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };

    fn deployment(uri: &str, services: &[&str]) -> crate::runtime::endpoint::RegisteredDeployment {
        crate::runtime::endpoint::RegisteredDeployment {
            id: "dp_test".to_string(),
            uri: Some(uri.to_string()),
            services: services
                .iter()
                .map(|name| crate::runtime::endpoint::RegisteredService {
                    name: (*name).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn migration_validation_rejects_each_retired_or_mismatched_value() {
        // Pins: bootstrap cannot report success if the private Session handler
        // leaves either VO status mirror retired or responds for another key.
        let session_id = SessionId(uuid::Uuid::from_u128(1));
        let clean = SessionStatusIdleMigrationResponse {
            session_id,
            status_rewritten: true,
            meta_status_rewritten: true,
            status: Some("idle".to_string()),
            meta_status: Some("idle".to_string()),
            retired_values_remaining: 0,
        };
        validate_session_migration(session_id, &clean)
            .expect("clean exact-key migration should pass");

        for invalid in [
            SessionStatusIdleMigrationResponse {
                status: Some("paused".to_string()),
                ..clean.clone()
            },
            SessionStatusIdleMigrationResponse {
                meta_status: Some("paused".to_string()),
                ..clean.clone()
            },
            SessionStatusIdleMigrationResponse {
                retired_values_remaining: 1,
                ..clean.clone()
            },
            SessionStatusIdleMigrationResponse {
                session_id: SessionId(uuid::Uuid::from_u128(2)),
                ..clean.clone()
            },
        ] {
            validate_session_migration(session_id, &invalid)
                .expect_err("retired or mismatched Session migration must fail bootstrap");
        }
    }

    #[test]
    fn migration_deployment_removal_uses_restate_forced_delete() {
        // Pins: Restate 1.7 only implements deployment deletion when the
        // explicit force query parameter is present.
        assert_eq!(
            deployment_delete_url("http://restate:9070", "dp_migration"),
            "http://restate:9070/deployments/dp_migration?force=true"
        );
    }

    #[tokio::test]
    async fn migration_deployment_removal_refuses_an_active_pinned_invocation() {
        // Pins: forced deletion is fail-closed when Restate reports even one
        // nonterminal invocation pinned to the migration deployment.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/query"))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "rows": [{"id": "inv_active", "status": "running"}]
            })))
            .mount(&server)
            .await;

        let error = deregister_deployment_with_policy(
            &Client::new(),
            &server.uri(),
            "dp_migration",
            2,
            Duration::ZERO,
        )
        .await
        .expect_err("active pinned invocation must prevent forced deletion");
        assert!(error.to_string().contains("inv_active"));

        let requests = server
            .received_requests()
            .await
            .expect("mock server should retain requests");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = requests[0]
            .body_json()
            .expect("introspection request should contain JSON");
        assert_eq!(
            body,
            serde_json::json!({
                "query": "SELECT id, status FROM sys_invocation WHERE (pinned_deployment_id = 'dp_migration' OR last_attempt_deployment_id = 'dp_migration') AND status NOT IN ('completed', 'killed') LIMIT 1"
            })
        );
    }

    #[tokio::test]
    async fn migration_deployment_removal_waits_for_async_absence() {
        // Pins: Restate 1.7 returns 202 before deletion is visible; bootstrap
        // cannot return to receipt recording until the exact deployment vanishes.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/query"))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"rows": []})))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/deployments/dp_migration"))
            .and(query_param("force", "true"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let observations = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/deployments"))
            .respond_with({
                let observations = observations.clone();
                move |_request: &Request| {
                    let body = if observations.fetch_add(1, Ordering::SeqCst) == 0 {
                        serde_json::json!({
                            "deployments": [{
                                "id": "dp_migration",
                                "services": [],
                                "uri": "http://migration:9080"
                            }]
                        })
                    } else {
                        serde_json::json!({
                            "deployments": [{
                                "id": "dp_runtime",
                                "services": [],
                                "uri": "http://runtime:9080"
                            }]
                        })
                    };
                    ResponseTemplate::new(200).set_body_json(body)
                }
            })
            .mount(&server)
            .await;

        deregister_deployment_with_policy(
            &Client::new(),
            &server.uri(),
            "dp_migration",
            3,
            Duration::ZERO,
        )
        .await
        .expect("accepted deletion should wait until exact deployment is absent");

        let requests = server
            .received_requests()
            .await
            .expect("mock server should retain requests");
        let path_sequence = requests
            .iter()
            .map(|request| request.url.path().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            path_sequence,
            vec![
                "/query",
                "/deployments/dp_migration",
                "/deployments",
                "/deployments"
            ]
        );
        assert_eq!(observations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn migration_deployment_matching_accepts_restate_canonical_trailing_slash() {
        // Pins: Restate 1.7 canonicalizes an HTTP deployment URI with a
        // trailing slash after registration.
        assert!(endpoint_uris_match(
            "http://status-migrator:9080/",
            "http://status-migrator:9080"
        ));
        assert!(!endpoint_uris_match(
            "http://status-migrator:9080/",
            "http://runtime:9080"
        ));
    }

    #[test]
    fn migration_registration_rejects_product_or_wrong_uri_deployments() {
        // Pins: bootstrap must never dispatch the raw cutover through a product
        // deployment that can also satisfy edge Health.
        let migration = deployment(
            "http://migration:9080",
            &["Session", "StatusMigrationDispatcher"],
        );
        assert!(is_status_migration_deployment(
            &migration,
            Some("http://migration:9080")
        ));
        assert!(!is_status_migration_deployment(
            &migration,
            Some("http://other:9080")
        ));

        let product = deployment(
            "http://product:9080",
            &["Session", "StatusMigrationDispatcher", "Health"],
        );
        assert!(!is_status_migration_deployment(&product, None));
    }
}
