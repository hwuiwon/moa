//! End-to-end slow-path ingestion coverage through a local Restate ingress.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use memory_ingest::{IngestApplyReport, SessionTurn, should_ingest_degraded};
use moa_core::{SessionId, UserId, WorkspaceId};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::time::sleep;
use uuid::Uuid;

use crate::support::restate_runtime::{OrchestratorPorts, reserve_orchestrator_ports};

mod support;

const DEFAULT_TEST_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:5432/moa";

static LIVE_E2E_LOCK: Mutex<()> = Mutex::const_new(());

struct LiveIngestionHarness {
    client: reqwest::Client,
    pool: PgPool,
    ingress: String,
    child: Child,
    _memory_dir: TempDir,
    _sandbox_dir: TempDir,
}

impl LiveIngestionHarness {
    async fn start() -> Result<Self> {
        let admin_url = restate_admin_url();
        let ingress = restate_ingress_url();
        let ports = reserve_orchestrator_ports()?;
        let endpoint_url = format!("http://127.0.0.1:{}", ports.restate);
        let memory_dir = tempfile::tempdir().context("create temporary memory root")?;
        let sandbox_dir = tempfile::tempdir().context("create temporary sandbox root")?;
        let pool = PgPool::connect(&test_database_url())
            .await
            .context("connect to test Postgres")?;
        let child = spawn_orchestrator(ports, &admin_url, &memory_dir, &sandbox_dir)?;

        wait_for_live(ports.health).await?;
        register_deployment(&admin_url, &endpoint_url).await?;

        Ok(Self {
            client: reqwest::Client::new(),
            pool,
            ingress,
            child,
            _memory_dir: memory_dir,
            _sandbox_dir: sandbox_dir,
        })
    }

    async fn ingest(&self, turn: &SessionTurn) -> Result<IngestApplyReport> {
        self.client
            .post(object_url(&self.ingress, turn))
            .json(turn)
            .send()
            .await
            .context("call IngestionVO/ingest_turn via restate ingress")?
            .error_for_status()
            .context("ingestion request should succeed")?
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

async fn register_deployment(admin_url: &str, endpoint_url: &str) -> Result<()> {
    for _attempt in 0..20 {
        let output = Command::new("restate")
            .args([
                "--connect-timeout",
                "10000",
                "--request-timeout",
                "30000",
                "deployments",
                "register",
                endpoint_url,
                "--yes",
            ])
            .env("RESTATE_ADMIN_URL", admin_url)
            .output()
            .context("register deployment with local restate-server")?;

        if output.status.success() {
            return Ok(());
        }

        sleep(Duration::from_secs(1)).await;
    }

    bail!("deployment registration did not succeed before retry budget was exhausted")
}

fn spawn_orchestrator(
    ports: OrchestratorPorts,
    admin_url: &str,
    memory_dir: &TempDir,
    sandbox_dir: &TempDir,
) -> Result<Child> {
    let postgres_url = test_database_url();

    let mut command = Command::new(env!("CARGO_BIN_EXE_moa-orchestrator"));
    command
        .arg("--port")
        .arg(ports.restate.to_string())
        .arg("--health-port")
        .arg(ports.health.to_string())
        .env("POSTGRES_URL", postgres_url)
        .env("RESTATE_ADMIN_URL", admin_url)
        .env("MOA_MEMORY_DIR", memory_dir.path())
        .env("MOA_SANDBOX_DIR", sandbox_dir.path())
        .env("MOA_DOCKER_ENABLED", "false")
        .env_remove("COHERE_API_KEY")
        .env_remove("MOA_COHERE_API_KEY")
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Ok(pii_url) =
        std::env::var("MOA_PII_SERVICE_URL").or_else(|_| std::env::var("MOA_PII_URL"))
    {
        command.env("MOA_PII_SERVICE_URL", pii_url);
    }

    command
        .spawn()
        .context("spawn moa-orchestrator binary for ingestion e2e")
}

fn test_database_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.to_string())
}

fn restate_admin_url() -> String {
    std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:9070".to_string())
}

fn restate_ingress_url() -> String {
    std::env::var("RESTATE_INGRESS_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

fn object_url(ingress: &str, turn: &SessionTurn) -> String {
    format!(
        "{ingress}/IngestionVO/{}:{}/ingest_turn",
        turn.workspace_id, turn.session_id
    )
}

fn realistic_turn() -> SessionTurn {
    SessionTurn {
        workspace_id: WorkspaceId::new(format!("ingestion-e2e-{}", Uuid::now_v7().simple())),
        user_id: UserId::new("ingestion-e2e-user"),
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

fn same_fact_turn(workspace_id: WorkspaceId, session_id: SessionId, turn_seq: u64) -> SessionTurn {
    SessionTurn {
        workspace_id,
        user_id: UserId::new("ingestion-e2e-user"),
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
    let workspace_id = WorkspaceId::new(format!("ingestion-degraded-{}", Uuid::now_v7().simple()));
    let session_id = SessionId::new();
    for turn_seq in 1..=512 {
        let turn = SessionTurn {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("ingestion-e2e-user"),
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
        workspace_id: WorkspaceId::new(format!("ingestion-sensitive-{}", Uuid::now_v7().simple())),
        user_id: UserId::new("ingestion-e2e-user"),
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
        WHERE workspace_id = $1
          AND label = 'Fact'
          AND properties_summary->>'source_session_id' = $2
        "#,
    )
    .bind(turn.workspace_id.to_string())
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
        WHERE workspace_id = $1
          AND label = 'Fact'
          AND pii_class <> 'none'
          AND properties_summary->>'source_session_id' = $2
        "#,
    )
    .bind(turn.workspace_id.to_string())
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
        WHERE workspace_id = $1
          AND session_id = $2
          AND turn_seq = $3
        "#,
    )
    .bind(turn.workspace_id.to_string())
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
        WHERE workspace_id = $1
          AND session_id = $2
          AND turn_seq = $3
        "#,
    )
    .bind(turn.workspace_id.to_string())
    .bind(turn.session_id.0)
    .bind(i64::try_from(turn.turn_seq).context("turn sequence fits i64")?)
    .fetch_one(pool)
    .await
    .context("count dlq rows")
}

async fn changelog_count(pool: &PgPool, turn: &SessionTurn) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.graph_changelog
        WHERE workspace_id = $1
          AND target_kind = 'node'
          AND op = 'create'
          AND payload->'after'->>'source_session_id' = $2
        "#,
    )
    .bind(turn.workspace_id.to_string())
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
        WHERE workspace_id = $1
          AND label = 'Fact'
          AND properties_summary->>'source_session_id' = $2
        ORDER BY properties_summary->>'summary'
        "#,
    )
    .bind(turn.workspace_id.to_string())
    .bind(turn.session_id.to_string())
    .fetch_all(pool)
    .await
    .context("load fact summaries")
}

async fn set_slow_path_degraded(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    degraded: bool,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.workspace_state (workspace_id, slow_path_degraded)
        VALUES ($1, $2)
        ON CONFLICT (workspace_id) DO UPDATE
            SET slow_path_degraded = EXCLUDED.slow_path_degraded,
                updated_at = now()
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(degraded)
    .execute(pool)
    .await
    .context("set workspace degraded flag")?;
    Ok(())
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
    let workspace_id = WorkspaceId::new(format!("ingestion-repeat-{}", Uuid::now_v7().simple()));
    let first_turn = same_fact_turn(workspace_id.clone(), SessionId::new(), 10);
    let second_turn = same_fact_turn(workspace_id, SessionId::new(), 10);

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
        set_slow_path_degraded(&harness.pool, &turn.workspace_id, true).await?;

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
        set_slow_path_degraded(&harness.pool, &turn.workspace_id, true).await?;

        let report = harness.ingest(&turn).await?;
        ensure!(
            report.inserted == 2,
            "unexpected sensitive report: {report:?}"
        );
        ensure!(
            report.failed == 0,
            "unexpected sensitive report: {report:?}"
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
    let turn = same_fact_turn(
        WorkspaceId::new(format!("ingestion-e2e-{}", Uuid::now_v7().simple())),
        SessionId::new(),
        42,
    );

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
