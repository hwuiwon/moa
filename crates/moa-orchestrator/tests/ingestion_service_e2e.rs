//! End-to-end slow-path ingestion coverage through a local Restate ingress.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

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
    fixture: OrchestratorTestFixture,
}

impl LiveIngestionHarness {
    async fn start(database_url: &str) -> Result<Self> {
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
        Self::from_fixture(fixture).await
    }

    async fn start_owned() -> Result<Self> {
        let fixture = OrchestratorTestFixture::with_script(json!({
            "default": {
                "content": "ok",
                "stop_reason": "end_turn"
            }
        }))
        .await
        .context("start hermetic orchestrator fixture for ingestion recovery e2e")?;
        Self::from_fixture(fixture).await
    }

    async fn from_fixture(fixture: OrchestratorTestFixture) -> Result<Self> {
        let pool = PgPool::connect(&fixture.postgres_url)
            .await
            .context("connect to fixture Postgres")?;
        let ingress = fixture.ingress_url.clone();

        Ok(Self {
            client: reqwest::Client::new(),
            pool,
            ingress,
            fixture,
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

fn ingestion_gate_key(tenant_id: TenantId) -> i64 {
    let bytes = tenant_id.0.as_bytes();
    i64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

async fn install_ingestion_apply_gate(
    pool: &PgPool,
    tenant_id: TenantId,
    gate_key: i64,
) -> Result<(String, String)> {
    let suffix = tenant_id.0.simple().to_string();
    let function_name = format!("test_ingestion_apply_gate_{}", &suffix[..16]);
    let trigger_name = format!("test_ingestion_apply_trigger_{}", &suffix[..16]);
    let ddl = format!(
        r#"
        CREATE FUNCTION {function_name}() RETURNS trigger
        LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.storage_partition_id = '{tenant_id}' THEN
                PERFORM pg_advisory_xact_lock({gate_key});
            END IF;
            RETURN NEW;
        END;
        $$;
        CREATE TRIGGER {trigger_name}
        BEFORE INSERT ON moa.node_index
        FOR EACH ROW EXECUTE FUNCTION {function_name}();
        "#
    );
    sqlx::raw_sql(&ddl)
        .execute(pool)
        .await
        .context("install exact ingestion apply gate")?;
    Ok((trigger_name, function_name))
}

async fn wait_for_ingestion_apply_gate(
    pool: &PgPool,
    gate_key: i64,
    previous_waiter: Option<&(i32, String)>,
    timeout: Duration,
) -> Result<(i32, String)> {
    let key_bits = gate_key as u64;
    let class_id = (key_bits >> 32) as i64;
    let object_id = (key_bits & u64::from(u32::MAX)) as i64;
    let deadline = Instant::now() + timeout;
    loop {
        let waiters: Vec<(i32, String)> = sqlx::query_as(
            "SELECT pid, waitstart::text FROM pg_locks \
             WHERE locktype = 'advisory' AND NOT granted \
               AND classid::bigint = $1 AND objid::bigint = $2 \
             ORDER BY pid, waitstart",
        )
        .bind(class_id)
        .bind(object_id)
        .fetch_all(pool)
        .await
        .context("inspect exact ingestion advisory waiter")?;
        let observed = waiters.clone();
        let current = waiters
            .into_iter()
            .filter(|waiter| Some(waiter) != previous_waiter)
            .collect::<Vec<_>>();
        if let [waiter] = current.as_slice() {
            return Ok(waiter.clone());
        }
        ensure!(
            current.len() <= 1,
            "expected at most one new ingestion gate waiter; previous={previous_waiter:?}, observed={observed:?}"
        );
        ensure!(
            Instant::now() < deadline,
            "ingestion apply did not reach its advisory gate within {timeout:?}; previous={previous_waiter:?}, observed={observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_ingestion_journal_action(
    fixture: &OrchestratorTestFixture,
    turn: &SessionTurn,
    action_name: &str,
    timeout: Duration,
) -> Result<()> {
    let object_key = ingestion_object_key(turn);
    let query = format!(
        "SELECT journal.id, journal.index, journal.entry_type, journal.name, \
                journal.version, journal.entry_json \
         FROM sys_journal AS journal \
         JOIN sys_invocation AS invocation ON journal.id = invocation.id \
         WHERE invocation.target_service_name = 'IngestionVO' \
           AND invocation.target_service_key = '{object_key}' \
           AND invocation.target_handler_name = 'ingest_turn' \
           AND invocation.status != 'completed' \
           AND journal.name = '{action_name}'"
    );
    let client = reqwest::Client::new();
    let url = format!("{}/query", fixture.admin_url.trim_end_matches('/'));
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let last_observation = match client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({ "query": query }))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                match response.text().await {
                    Ok(body) if status.is_success() && body.trim().is_empty() => {
                        "successful query returned an empty body".to_string()
                    }
                    Ok(body) if status.is_success() => {
                        match serde_json::from_str::<serde_json::Value>(&body) {
                            Ok(payload) => match payload
                                .get("rows")
                                .and_then(serde_json::Value::as_array)
                            {
                                Some(rows) => {
                                    ensure!(
                                        rows.len() <= 1,
                                        "exact ingestion invocation has multiple `{action_name}` journal rows: {rows:?}"
                                    );
                                    let action_is_journaled = rows.first().is_some_and(|row| {
                                        row.get("entry_type").and_then(serde_json::Value::as_str)
                                            == Some("Command: Run")
                                            && row.get("name").and_then(serde_json::Value::as_str)
                                                == Some(action_name)
                                            && row
                                                .get("version")
                                                .and_then(serde_json::Value::as_u64)
                                                == Some(2)
                                            && row
                                                .get("entry_json")
                                                .and_then(serde_json::Value::as_str)
                                                .and_then(|entry| {
                                                    serde_json::from_str::<serde_json::Value>(entry)
                                                        .ok()
                                                })
                                                .and_then(|entry| {
                                                    entry
                                                        .pointer("/Command/Run/name")
                                                        .and_then(serde_json::Value::as_str)
                                                        .map(str::to_string)
                                                })
                                                .as_deref()
                                                == Some(action_name)
                                    });
                                    if action_is_journaled {
                                        return Ok(());
                                    }
                                    format!("journal rows={rows:?}")
                                }
                                None => format!("response omitted rows: {payload}"),
                            },
                            Err(error) => {
                                format!("decode JSON response: {error}; body={body:?}")
                            }
                        }
                    }
                    Ok(body) => format!("status {status}; body={body:?}"),
                    Err(error) => format!("read response body: {error}"),
                }
            }
            Err(error) => format!("send query: {error}"),
        };

        ensure!(
            tokio::time::Instant::now() < deadline,
            "Restate did not expose journaled `{action_name}` for IngestionVO/{object_key}/ingest_turn within {timeout:?}: {last_observation}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn ingestion_object_key(turn: &SessionTurn) -> String {
    format!("{}:{}", turn.tenant_id, turn.session_id)
}

fn object_url(ingress: &str, turn: &SessionTurn) -> String {
    format!(
        "{ingress}/restate/call/IngestionVO/{}/ingest_turn",
        ingestion_object_key(turn)
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
#[ignore = "requires local Restate, Postgres, Docker, and the orchestrator recovery fixture"]
async fn recovery_matrix_degraded_decision_does_not_flip_after_restart() -> Result<()> {
    // Pins: the Postgres degraded-state read is one durable decision. A restart
    // after that action commits but before graph writes cannot re-read a flipped
    // flag and change the command from ingest to skip.
    let harness = LiveIngestionHarness::start_owned().await?;
    let turn = low_pii_degraded_skip_turn();
    let gate_key = ingestion_gate_key(turn.tenant_id);
    let (trigger_name, function_name) =
        install_ingestion_apply_gate(&harness.pool, turn.tenant_id, gate_key).await?;
    set_slow_path_degraded(&harness.pool, turn.tenant_id, false).await?;

    let mut gate_owner = harness.pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(gate_key)
        .execute(&mut *gate_owner)
        .await?;

    let request_client = harness.client.clone();
    let request_url = object_url(&harness.ingress, &turn);
    let request_turn = turn.clone();
    let request = tokio::spawn(async move {
        request_client
            .post(request_url)
            .json(&request_turn)
            .send()
            .await
            .context("send recovery ingestion request")?
            .error_for_status()
            .context("recovery ingestion request should succeed")?
            .json::<IngestApplyReport>()
            .await
            .context("decode recovery ingestion report")
    });

    let first_waiter =
        wait_for_ingestion_apply_gate(&harness.pool, gate_key, None, Duration::from_secs(30))
            .await?;
    // Protocol-v4 journals expose this durable step as a version-2 Run command;
    // `completed` is intentionally unpopulated. The downstream apply waiter
    // above proves the handler received the completion and advanced past it.
    wait_for_ingestion_journal_action(
        &harness.fixture,
        &turn,
        "should_skip_degraded",
        Duration::from_secs(30),
    )
    .await?;
    set_slow_path_degraded(&harness.pool, turn.tenant_id, true).await?;
    harness
        .fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("restart after journaled degraded decision")?;
    // A PostgreSQL backend sleeping in an advisory-lock wait does not observe
    // the dead SDK process promptly. Terminate that exact orphan so the test
    // models the database observing the crash before replay starts a new apply.
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(first_waiter.0)
        .fetch_one(&harness.pool)
        .await?;
    ensure!(terminated, "terminate orphaned ingestion apply backend");
    let replay_waiter = wait_for_ingestion_apply_gate(
        &harness.pool,
        gate_key,
        Some(&first_waiter),
        Duration::from_secs(30),
    )
    .await?;
    ensure!(
        replay_waiter != first_waiter,
        "replayed ingestion apply must use a distinct Postgres backend wait"
    );

    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(gate_key)
        .fetch_one(&mut *gate_owner)
        .await?;
    ensure!(unlocked, "fixture connection must own ingestion gate");
    drop(gate_owner);

    let report = request.await.context("join recovery ingestion request")??;
    ensure!(report.inserted == 2, "unexpected replay report: {report:?}");
    wait_for_fact_count(&harness.pool, &turn, 2).await?;

    sqlx::raw_sql(&format!(
        "DROP TRIGGER {trigger_name} ON moa.node_index; DROP FUNCTION {function_name}();"
    ))
    .execute(&harness.pool)
    .await
    .context("remove ingestion apply gate")?;
    harness.shutdown().await;
    Ok(())
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
