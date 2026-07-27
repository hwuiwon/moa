//! End-to-end slow-path ingestion coverage through a local Restate ingress.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use moa_config::MoaConfig;
use moa_core::{
    types::contact::ContactId,
    types::identifiers::SessionId,
    types::identifiers::TenantId,
    types::memory::{InformationBarrierClearances, InformationBarrierId, RlsContext},
};
use moa_db::ScopedConn;
use moa_memory_ingest::{IngestApplyReport, IngestRuntime, SessionTurn, should_ingest_degraded};
use moa_test_support::{OrchestratorTestFixture, postgres::test_database_url};
use serde_json::json;
use sqlx::PgPool;
use tokio::time::sleep;

struct LiveIngestionHarness {
    client: reqwest::Client,
    pool: PgPool,
    ingress: String,
    _fixture: OrchestratorTestFixture,
}

impl LiveIngestionHarness {
    async fn start(database_url: &str) -> Result<Self> {
        let pool = PgPool::connect(database_url)
            .await
            .context("connect to test Postgres")?;
        let fixture = OrchestratorTestFixture::with_script_and_env(
            json!({
                "default": {
                    "content": "ok",
                    "stop_reason": "end_turn"
                }
            }),
            vec![("MOA_DATABASE_URL".to_string(), database_url.to_string())],
        )
        .await
        .context("start shared orchestrator fixture for ingestion e2e")?;
        let ingress = fixture.ingress_url.clone();

        Ok(Self {
            client: reqwest::Client::new(),
            pool,
            ingress,
            _fixture: fixture,
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
            bail!("ingestion request should succeed; status={status} body={body}");
        }
        response
            .json::<IngestApplyReport>()
            .await
            .context("decode ingestion report")
    }

    async fn shutdown(self) {
        self.pool.close().await;
    }
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
        finalized_at: moa_test_support::fixtures::pg_now(),
        barrier: None,
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
        finalized_at: moa_test_support::fixtures::pg_now(),
        barrier: None,
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
            finalized_at: moa_test_support::fixtures::pg_now(),
            barrier: None,
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
        finalized_at: moa_test_support::fixtures::pg_now(),
        barrier: None,
    }
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
        FROM moa.node_index AS node
        JOIN moa.ingest_dedup AS dedup
          ON dedup.fact_uid = node.uid
         AND dedup.storage_partition_id = node.storage_partition_id
        WHERE dedup.storage_partition_id = $1
          AND dedup.session_id = $2
          AND dedup.turn_seq = $3
          AND node.label = 'Fact'
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.0)
    .bind(i64::try_from(turn.turn_seq).context("turn sequence fits i64")?)
    .fetch_one(pool)
    .await
    .context("count ingested fact nodes")
}

async fn visible_barrier_fact_count(
    pool: &PgPool,
    turn: &SessionTurn,
    barrier: &InformationBarrierId,
    clearances: InformationBarrierClearances,
) -> Result<i64> {
    let contact_id = turn
        .contact_id
        .context("conversation turn should carry a contact")?;
    let scope = RlsContext::contact(turn.tenant_id, contact_id).with_cleared_barriers(clearances);
    let mut conn = ScopedConn::begin_as_app(pool, &scope, true)
        .await
        .context("begin app-role barrier fact count")?;
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.node_index
        WHERE tenant_id = $1
          AND label = 'Fact'
          AND barrier = $2
        "#,
    )
    .bind(turn.tenant_id.0)
    .bind(barrier.as_str())
    .fetch_one(conn.as_mut())
    .await
    .context("count visible barriered facts")?;
    conn.commit()
        .await
        .context("commit app-role barrier fact count")?;
    Ok(count)
}

async fn pii_fact_count(pool: &PgPool, turn: &SessionTurn) -> Result<i64> {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.node_index AS node
        JOIN moa.ingest_dedup AS dedup
          ON dedup.fact_uid = node.uid
         AND dedup.storage_partition_id = node.storage_partition_id
        WHERE dedup.storage_partition_id = $1
          AND dedup.session_id = $2
          AND dedup.turn_seq = $3
          AND node.label = 'Fact'
          AND node.pii_class <> 'none'
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.0)
    .bind(i64::try_from(turn.turn_seq).context("turn sequence fits i64")?)
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
        FROM moa.graph_changelog AS changelog
        JOIN moa.ingest_dedup AS dedup
          ON dedup.fact_uid = changelog.target_uid
         AND dedup.storage_partition_id = changelog.storage_partition_id
        WHERE dedup.storage_partition_id = $1
          AND dedup.session_id = $2
          AND dedup.turn_seq = $3
          AND changelog.target_kind = 'node'
          AND changelog.op = 'create'
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.0)
    .bind(i64::try_from(turn.turn_seq).context("turn sequence fits i64")?)
    .fetch_one(pool)
    .await
    .context("count graph changelog rows")
}

async fn fact_summaries(pool: &PgPool, turn: &SessionTurn) -> Result<Vec<String>> {
    sqlx::query_scalar::<_, String>(
        r#"
        SELECT node.properties_summary->>'summary'
        FROM moa.node_index AS node
        JOIN moa.ingest_dedup AS dedup
          ON dedup.fact_uid = node.uid
         AND dedup.storage_partition_id = node.storage_partition_id
        WHERE dedup.storage_partition_id = $1
          AND dedup.session_id = $2
          AND dedup.turn_seq = $3
          AND node.label = 'Fact'
          AND node.properties_summary ? 'summary'
        ORDER BY node.properties_summary->>'summary'
        "#,
    )
    .bind(turn.tenant_id.to_string())
    .bind(turn.session_id.0)
    .bind(i64::try_from(turn.turn_seq).context("turn sequence fits i64")?)
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
        IngestRuntime::from_config(pool, Arc::new(moa_crypto::LocalKmsProvider::new()), &config)
            .context("build live ingestion runtime")?;
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
    let harness = LiveIngestionHarness::start(&test_database_url()).await?;
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
    let harness = LiveIngestionHarness::start(&test_database_url()).await?;
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
    let harness = LiveIngestionHarness::start(&test_database_url()).await?;
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
    let harness = LiveIngestionHarness::start(&test_database_url()).await?;
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
    let harness = LiveIngestionHarness::start(&test_database_url()).await?;
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

#[tokio::test]
#[ignore = "requires local restate-server, Postgres, and optional PII sidecar"]
async fn conversation_ingest_uses_pinned_write_barrier_service_e2e() -> Result<()> {
    // Pins: the internal conversation turn produced from a pinned agent policy
    // keeps its typed write barrier through Restate ingestion and Postgres RLS.
    let (database_url, schema_name) = moa_session::testing::provision_cloned_database()
        .await
        .context("create isolated conversation ingestion database")?;
    let harness = LiveIngestionHarness::start(&database_url).await?;
    let barrier = InformationBarrierId::parse("deal-alpha")?;
    let mut turn = same_fact_turn(TenantId::new(), SessionId::new(), 84);
    turn.barrier = Some(barrier.clone());

    let result = async {
        let report = harness.ingest(&turn).await?;
        ensure!(
            report.inserted == 3,
            "unexpected barriered report: {report:?}"
        );
        ensure!(
            visible_barrier_fact_count(
                &harness.pool,
                &turn,
                &barrier,
                InformationBarrierClearances::new(),
            )
            .await?
                == 0,
            "conversation facts must fail closed without clearance"
        );
        ensure!(
            visible_barrier_fact_count(
                &harness.pool,
                &turn,
                &barrier,
                [barrier.clone()].into_iter().collect(),
            )
            .await?
                == 3,
            "conversation facts must be visible with the pinned clearance"
        );
        Ok(())
    }
    .await;

    harness.shutdown().await;
    let cleanup = moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await;
    result.and(cleanup.map_err(anyhow::Error::from))
}
