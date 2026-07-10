//! End-to-end slow-path ingestion coverage through a local Restate ingress.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use moa_core::{ContactId, SessionId, TenantId, config::MoaConfig};
use moa_memory_ingest::{IngestApplyReport, IngestRuntime, SessionTurn, should_ingest_degraded};
use moa_test_support::postgres::test_database_url;
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::support::restate_runtime::{
    OrchestratorPorts, deployment_endpoint_url, register_deployment, reserve_orchestrator_ports,
    restate_admin_url,
};

#[path = "support/mod.rs"]
mod support;

static LIVE_E2E_LOCK: Mutex<()> = Mutex::const_new(());

struct LiveIngestionHarness {
    client: reqwest::Client,
    pool: PgPool,
    ingress: String,
    child: Child,
    orchestrator_log: PathBuf,
    _memory_dir: TempDir,
    _sandbox_dir: TempDir,
}

impl LiveIngestionHarness {
    async fn start() -> Result<Self> {
        let admin_url = restate_admin_url();
        let ingress = restate_ingress_url();
        let ports = reserve_orchestrator_ports()?;
        let endpoint_url = deployment_endpoint_url(ports.restate);
        let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
        let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
        let orchestrator_log = memory_dir.path().join("orchestrator.log");
        let pool = PgPool::connect(&test_database_url())
            .await
            .context("connect to test Postgres")?;
        let child = spawn_orchestrator(
            ports,
            &admin_url,
            &memory_dir,
            &sandbox_dir,
            &orchestrator_log,
        )?;

        wait_for_live(ports.health).await?;
        register_deployment(&admin_url, &endpoint_url).await?;

        Ok(Self {
            client: reqwest::Client::new(),
            pool,
            ingress,
            child,
            orchestrator_log,
            _memory_dir: memory_dir,
            _sandbox_dir: sandbox_dir,
        })
    }

    async fn ingest(&self, turn: &SessionTurn) -> Result<IngestApplyReport> {
        let response = self
            .client
            .post(object_url(&self.ingress, turn))
            .json(turn)
            .send()
            .await
            .context("call IngestionVO/ingest_turn via restate ingress")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "ingestion request should succeed; status={status} body={body}\n{}",
                orchestrator_log_tail(&self.orchestrator_log)
            );
        }
        response
            .json::<IngestApplyReport>()
            .await
            .context("decode ingestion report")
    }

    async fn shutdown(mut self) {
        self.stop();
        self.pool.close().await;
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveIngestionHarness {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    admin_url: &str,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
    log_path: &Path,
) -> Result<Child> {
    let postgres_url = test_database_url();
    let stdout = File::create(log_path).context("create ingestion e2e orchestrator log")?;
    let stderr = stdout
        .try_clone()
        .context("clone ingestion e2e orchestrator log")?;

    let mut command = Command::new(env!("CARGO_BIN_EXE_moa-orchestrator-bin"));
    command
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .arg("--scim-port")
        .arg(ports.scim.to_string())
        .env("MOA_DATABASE_URL", postgres_url)
        .env("MOA_RESTATE_ADMIN_URL", admin_url)
        .env("MOA_RESTATE_INGRESS_URL", restate_ingress_url())
        .env("MOA_LOCAL_MEMORY_DIR", memory_dir.path())
        .env("MOA_LOCAL_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_LOCAL_DOCKER_ENABLED", "false")
        .env_remove("MOA_COHERE_API_KEY")
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    if let Ok(pii_url) = std::env::var("MOA_PII_SERVICE_URL") {
        command.env("MOA_PII_SERVICE_URL", pii_url);
    }

    command
        .spawn()
        .context("spawn moa-orchestrator binary for ingestion e2e")
}

fn orchestrator_log_tail(log_path: &Path) -> String {
    match fs::read_to_string(log_path) {
        Ok(contents) if contents.trim().is_empty() => {
            format!("orchestrator log {} was empty", log_path.display())
        }
        Ok(contents) => {
            let lines = contents.lines().rev().take(120).collect::<Vec<_>>();
            let tail = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
            format!("orchestrator log tail from {}:\n{tail}", log_path.display())
        }
        Err(error) => format!(
            "failed to read orchestrator log {}: {error}",
            log_path.display()
        ),
    }
}

fn restate_ingress_url() -> String {
    std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:10010".to_string())
}

fn object_url(ingress: &str, turn: &SessionTurn) -> String {
    format!(
        "{ingress}/restate/call/IngestionVO/{}:{}/ingest_turn",
        turn.tenant_id, turn.session_id
    )
}

fn realistic_turn() -> SessionTurn {
    SessionTurn {
        tenant_id: TenantId::new(),
        contact_id: Some(ContactId::new()),
        session_id: SessionId::new(),
        turn_seq: 42,
        transcript: [
            "user: We finished the auth and billing design review.",
            "assistant: I captured the durable facts below.",
            "Fact: auth service uses JWT access tokens",
            "Fact: billing service owns invoice reconciliation",
            "Fact: incident commander escalates payment outage",
            "```",
            "Fact: this code block should stay attached to the surrounding chunk",
            "```",
            "Fact: patient SSN is 123-45-6789",
        ]
        .join("\n"),
        dominant_pii_class: "none".to_string(),
        finalized_at: Utc::now(),
    }
}

fn same_fact_turn(tenant_id: TenantId, session_id: SessionId, turn_seq: u64) -> SessionTurn {
    SessionTurn {
        tenant_id,
        contact_id: Some(ContactId::new()),
        session_id,
        turn_seq,
        transcript: [
            "Fact: auth service uses JWT access tokens",
            "Fact: billing service owns invoice reconciliation",
            "Fact: patient SSN is 123-45-6789",
        ]
        .join("\n"),
        dominant_pii_class: "none".to_string(),
        finalized_at: Utc::now(),
    }
}

fn low_pii_degraded_skip_turn() -> SessionTurn {
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let session_id = SessionId::new();
    for turn_seq in 1..=512 {
        let turn = SessionTurn {
            tenant_id,
            contact_id: Some(contact_id),
            session_id,
            turn_seq,
            transcript: [
                "Fact: search service owns query rewriting",
                "Fact: cache service stores retrieval digests",
            ]
            .join("\n"),
            dominant_pii_class: "none".to_string(),
            finalized_at: Utc::now(),
        };
        if !should_ingest_degraded(&turn) {
            return turn;
        }
    }

    panic!("could not find deterministic degraded skip turn")
}

fn sensitive_degraded_turn() -> SessionTurn {
    SessionTurn {
        tenant_id: TenantId::new(),
        contact_id: Some(ContactId::new()),
        session_id: SessionId::new(),
        turn_seq: 7,
        transcript: [
            "Fact: support runbook stores the patient SSN 123-45-6789",
            "Fact: security team keeps the API secret sk-live-test-value rotated",
        ]
        .join("\n"),
        dominant_pii_class: "pii".to_string(),
        finalized_at: Utc::now(),
    }
}

async fn wait_for_live(health_port: u16) -> Result<()> {
    let url = format!("http://127.0.0.1:{health_port}/_health/live");
    let client = reqwest::Client::new();
    for _attempt in 0..60 {
        if let Ok(response) = client.get(&url).send().await
            && response.status().is_success()
        {
            return Ok(());
        }
        sleep(Duration::from_secs(1)).await;
    }

    bail!("orchestrator live probe did not pass before timeout")
}

async fn wait_for_fact_count(pool: &PgPool, turn: &SessionTurn, expected: i64) -> Result<()> {
    for _attempt in 0..60 {
        let count = fact_count(pool, turn).await?;
        if count == expected {
            return Ok(());
        }
        sleep(Duration::from_secs(1)).await;
    }

    bail!(
        "expected {expected} ingested facts, found {}",
        fact_count(pool, turn).await?
    )
}

async fn fact_count(pool: &PgPool, turn: &SessionTurn) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.node_index
        WHERE storage_partition_id = $1
          AND label = 'Fact'
          AND properties_summary->>'source_session_id' = $2
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.to_string())
    .fetch_one(pool)
    .await
    .context("count ingested fact nodes")
}

async fn pii_fact_count(pool: &PgPool, turn: &SessionTurn) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.node_index
        WHERE storage_partition_id = $1
          AND label = 'Fact'
          AND pii_class <> 'none'
          AND properties_summary->>'source_session_id' = $2
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.to_string())
    .fetch_one(pool)
    .await
    .context("count ingested pii fact nodes")
}

async fn dedup_count(pool: &PgPool, turn: &SessionTurn) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.ingest_dedup
        WHERE storage_partition_id = $1
          AND session_id = $2
          AND turn_seq = $3
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.0)
    .bind(i64::try_from(turn.turn_seq).context("turn sequence fits i64")?)
    .fetch_one(pool)
    .await
    .context("count dedup rows")
}

async fn dlq_count(pool: &PgPool, turn: &SessionTurn) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.ingest_dlq
        WHERE storage_partition_id = $1
          AND session_id = $2
          AND turn_seq = $3
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.0)
    .bind(i64::try_from(turn.turn_seq).context("turn sequence fits i64")?)
    .fetch_one(pool)
    .await
    .context("count dlq rows")
}

async fn dlq_errors(pool: &PgPool, turn: &SessionTurn) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT error
        FROM moa.ingest_dlq
        WHERE storage_partition_id = $1
          AND session_id = $2
          AND turn_seq = $3
        ORDER BY dlq_id
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.0)
    .bind(i64::try_from(turn.turn_seq).context("turn sequence fits i64")?)
    .fetch_all(pool)
    .await
    .context("load dlq errors")
}

async fn changelog_count(pool: &PgPool, turn: &SessionTurn) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.graph_changelog
        WHERE storage_partition_id = $1
          AND target_kind = 'node'
          AND op = 'create'
          AND payload->'after'->>'source_session_id' = $2
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.to_string())
    .fetch_one(pool)
    .await
    .context("count graph changelog rows")
}

async fn fact_summaries(pool: &PgPool, turn: &SessionTurn) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT properties_summary->>'summary'
        FROM moa.node_index
        WHERE storage_partition_id = $1
          AND label = 'Fact'
          AND properties_summary->>'source_session_id' = $2
        ORDER BY properties_summary->>'summary'
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.to_string())
    .fetch_all(pool)
    .await
    .context("load fact summaries")
}

async fn set_slow_path_degraded(pool: &PgPool, tenant_id: TenantId, degraded: bool) -> Result<()> {
    let embedder_metadata = live_ingestion_embedder_metadata(pool.clone())?;
    if let Some(metadata) = embedder_metadata {
        sqlx::query(
            r#"
            INSERT INTO moa.storage_partition_state
                (storage_partition_id, slow_path_degraded, embedding_model,
                 embedding_model_version, embedding_dimension)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (storage_partition_id) DO UPDATE
                SET slow_path_degraded = EXCLUDED.slow_path_degraded,
                    embedding_model = EXCLUDED.embedding_model,
                    embedding_model_version = EXCLUDED.embedding_model_version,
                    embedding_dimension = EXCLUDED.embedding_dimension,
                    updated_at = now()
            "#,
        )
        .bind(tenant_id.to_string())
        .bind(degraded)
        .bind(metadata.model_id)
        .bind(metadata.model_version)
        .bind(metadata.dimensions)
        .execute(pool)
        .await
        .context("set workspace degraded flag with embedder metadata")?;
    } else {
        sqlx::query(
            r#"
            INSERT INTO moa.storage_partition_state (storage_partition_id, slow_path_degraded)
            VALUES ($1, $2)
            ON CONFLICT (storage_partition_id) DO UPDATE
                SET slow_path_degraded = EXCLUDED.slow_path_degraded,
                    updated_at = now()
            "#,
        )
        .bind(tenant_id.to_string())
        .bind(degraded)
        .execute(pool)
        .await
        .context("set workspace degraded flag")?;
    }
    Ok(())
}

struct EmbedderMetadata {
    model_id: String,
    model_version: i32,
    dimensions: i32,
}

fn live_ingestion_embedder_metadata(pool: PgPool) -> Result<Option<EmbedderMetadata>> {
    let config = MoaConfig::load().context("load live ingestion config")?;
    let runtime =
        IngestRuntime::from_config(pool, &config).context("build live ingestion runtime")?;
    let Some(embedder) = runtime.embedder() else {
        return Ok(None);
    };
    let dimensions =
        i32::try_from(embedder.dimensions()).context("embedding dimensions fit i32")?;
    Ok(Some(EmbedderMetadata {
        model_id: embedder.model_id().to_string(),
        model_version: embedder.model_version(),
        dimensions,
    }))
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, and optional PII sidecar"]
async fn complex_ingestion_turn_writes_facts_pii_changelog_and_dedup() -> Result<()> {
    let _guard = LIVE_E2E_LOCK.lock().await;
    let harness = LiveIngestionHarness::start().await?;
    let turn = realistic_turn();

    let result = async {
        let first = harness.ingest(&turn).await?;
        ensure!(first.inserted == 5, "unexpected first report: {first:?}");
        ensure!(first.failed == 0, "unexpected first report: {first:?}");

        wait_for_fact_count(&harness.pool, &turn, 5).await?;
        ensure!(dedup_count(&harness.pool, &turn).await? == 5);
        ensure!(dlq_count(&harness.pool, &turn).await? == 0);
        ensure!(changelog_count(&harness.pool, &turn).await? == 5);
        ensure!(
            pii_fact_count(&harness.pool, &turn).await? >= 1,
            "expected at least one non-none PII fact"
        );

        let summaries = fact_summaries(&harness.pool, &turn).await?;
        ensure!(
            summaries.contains(&"auth service uses JWT access tokens".to_string()),
            "missing auth fact in {summaries:?}"
        );
        ensure!(
            summaries.contains(&"billing service owns invoice reconciliation".to_string()),
            "missing billing fact in {summaries:?}"
        );

        let second = harness.ingest(&turn).await?;
        ensure!(second.inserted == 0, "unexpected replay report: {second:?}");
        ensure!(fact_count(&harness.pool, &turn).await? == 5);
        ensure!(dedup_count(&harness.pool, &turn).await? == 5);

        Ok(())
    }
    .await;

    harness.shutdown().await;
    result
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, and optional PII sidecar"]
async fn repeated_fact_text_in_new_sessions_does_not_collide_on_node_uid() -> Result<()> {
    let _guard = LIVE_E2E_LOCK.lock().await;
    let harness = LiveIngestionHarness::start().await?;
    let tenant_id = TenantId::new();
    let first_turn = same_fact_turn(tenant_id, SessionId::new(), 10);
    let second_turn = same_fact_turn(tenant_id, SessionId::new(), 10);

    let result = async {
        let first = harness.ingest(&first_turn).await?;
        ensure!(first.inserted == 3, "unexpected first report: {first:?}");
        ensure!(first.failed == 0, "unexpected first report: {first:?}");

        let second = harness.ingest(&second_turn).await?;
        ensure!(second.inserted == 3, "unexpected second report: {second:?}");
        ensure!(second.failed == 0, "unexpected second report: {second:?}");

        ensure!(fact_count(&harness.pool, &first_turn).await? == 3);
        ensure!(fact_count(&harness.pool, &second_turn).await? == 3);
        ensure!(dedup_count(&harness.pool, &first_turn).await? == 3);
        ensure!(dedup_count(&harness.pool, &second_turn).await? == 3);
        ensure!(dlq_count(&harness.pool, &first_turn).await? == 0);
        ensure!(dlq_count(&harness.pool, &second_turn).await? == 0);

        Ok(())
    }
    .await;

    harness.shutdown().await;
    result
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, and optional PII sidecar"]
async fn degraded_workspace_skips_sampled_low_pii_turn_without_side_effects() -> Result<()> {
    let _guard = LIVE_E2E_LOCK.lock().await;
    let harness = LiveIngestionHarness::start().await?;
    let turn = low_pii_degraded_skip_turn();

    let result = async {
        set_slow_path_degraded(&harness.pool, turn.tenant_id, true).await?;

        let report = harness.ingest(&turn).await?;
        ensure!(
            report.inserted == 0,
            "unexpected degraded report: {report:?}"
        );
        ensure!(
            report.skipped == 1,
            "unexpected degraded report: {report:?}"
        );
        ensure!(report.failed == 0, "unexpected degraded report: {report:?}");
        ensure!(fact_count(&harness.pool, &turn).await? == 0);
        ensure!(dedup_count(&harness.pool, &turn).await? == 0);
        ensure!(dlq_count(&harness.pool, &turn).await? == 0);

        Ok(())
    }
    .await;

    harness.shutdown().await;
    result
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, and optional PII sidecar"]
async fn degraded_workspace_still_ingests_sensitive_turn() -> Result<()> {
    let _guard = LIVE_E2E_LOCK.lock().await;
    let harness = LiveIngestionHarness::start().await?;
    let turn = sensitive_degraded_turn();

    let result = async {
        set_slow_path_degraded(&harness.pool, turn.tenant_id, true).await?;

        let report = harness.ingest(&turn).await?;
        let dlq_errors = dlq_errors(&harness.pool, &turn).await?;
        ensure!(
            report.inserted == 2,
            "unexpected sensitive report: {report:?}; dlq={dlq_errors:?}"
        );
        ensure!(
            report.failed == 0,
            "unexpected sensitive report: {report:?}; dlq={dlq_errors:?}"
        );
        wait_for_fact_count(&harness.pool, &turn, 2).await?;
        ensure!(dedup_count(&harness.pool, &turn).await? == 2);
        ensure!(
            pii_fact_count(&harness.pool, &turn).await? >= 1,
            "expected sensitive degraded turn to retain non-none PII classification"
        );
        ensure!(dlq_count(&harness.pool, &turn).await? == 0);

        Ok(())
    }
    .await;

    harness.shutdown().await;
    result
}

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, and optional PII sidecar"]
async fn ingestion_turn_round_trip_through_restate_is_idempotent() -> Result<()> {
    let _guard = LIVE_E2E_LOCK.lock().await;
    let harness = LiveIngestionHarness::start().await?;
    let turn = same_fact_turn(TenantId::new(), SessionId::new(), 42);

    let result = async {
        let first = harness.ingest(&turn).await?;
        ensure!(first.inserted == 3, "unexpected first report: {first:?}");
        ensure!(first.failed == 0, "unexpected first report: {first:?}");

        wait_for_fact_count(&harness.pool, &turn, 3).await?;
        ensure!(dedup_count(&harness.pool, &turn).await? == 3);
        ensure!(
            pii_fact_count(&harness.pool, &turn).await? >= 1,
            "expected at least one non-none PII fact"
        );

        let second = harness.ingest(&turn).await?;
        ensure!(second.inserted == 0, "unexpected replay report: {second:?}");
        ensure!(fact_count(&harness.pool, &turn).await? == 3);
        ensure!(dedup_count(&harness.pool, &turn).await? == 3);

        Ok(())
    }
    .await;

    harness.shutdown().await;
    result
}
