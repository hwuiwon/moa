//! Cross-tenant isolation probes for the retrieval perf gate.

use super::*;

#[derive(Debug, Clone, Default)]
pub(super) struct LeakReport {
    pub(super) count: usize,
    pub(super) attempts: usize,
    pub(super) failures: Vec<String>,
}

pub(super) fn spawn_cross_tenant_attacks(
    stack: Stack,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<Result<LeakReport>> {
    tokio::spawn(async move {
        let report = Arc::new(Mutex::new(LeakReport::default()));
        while !stop.load(Ordering::Relaxed) {
            for outcome in run_attack_round(&stack).await {
                let mut guard = report.lock().await;
                guard.attempts += 1;
                if let Err(error) = outcome {
                    guard.count += 1;
                    guard.failures.push(error);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(report.lock().await.clone())
    })
}

pub(super) async fn run_attack_round(stack: &Stack) -> Vec<Result<(), String>> {
    vec![
        attack_unset_guc(stack).await,
        attack_cte_leak(stack).await,
        attack_vector_oracle(stack).await,
        attack_changelog_leak(stack).await,
        attack_dlq_leak(stack).await,
    ]
}

pub(super) async fn attack_unset_guc(stack: &Stack) -> Result<(), String> {
    let mut tx = stack.pool.begin().await.map_err(display)?;
    sqlx::query("RESET ALL")
        .execute(&mut *tx)
        .await
        .map_err(display)?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *tx)
        .await
        .map_err(display)?;
    let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index")
        .fetch_one(&mut *tx)
        .await
        .map_err(display)?;
    tx.rollback().await.map_err(display)?;
    (count == 0)
        .then_some(())
        .ok_or_else(|| format!("unset GUC leaked {count} node_index rows"))
}

pub(super) async fn attack_cte_leak(stack: &Stack) -> Result<(), String> {
    let tenant_a = stack.tenants[0].tenant_id;
    let tenant_b = stack.tenants[1].tenant_id;
    let mut conn = app_scoped_conn(&stack.pool, tenant_a)
        .await
        .map_err(display)?;
    let leaked = sqlx::query_scalar::<_, i64>(
        "WITH cte AS (SELECT * FROM moa.node_index) SELECT count(*) FROM cte WHERE storage_partition_id = $1",
    )
    .bind(tenant_b.to_string())
    .fetch_one(conn.as_mut())
    .await
    .map_err(display)?;
    conn.commit().await.map_err(display)?;
    (leaked == 0)
        .then_some(())
        .ok_or_else(|| format!("CTE leaked {leaked} tenant B rows"))
}

pub(super) async fn attack_vector_oracle(stack: &Stack) -> Result<(), String> {
    let tenant_a = stack.tenants[0].tenant_id;
    let tenant_b = stack.tenants[1].tenant_id;
    let embedding = first_embedding(&stack.pool, tenant_a)
        .await
        .map_err(display)?;
    let vector =
        PgvectorStore::new_for_app_role(stack.pool.clone(), ScopeContext::tenant(tenant_b));
    let matches = moa_memory_vector::VectorStore::knn(
        &vector,
        &moa_memory_vector::VectorQuery {
            embedding,
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
            as_of: None,
        },
    )
    .await
    .map_err(display)?;
    matches
        .is_empty()
        .then_some(())
        .ok_or_else(|| format!("vector oracle leaked matches: {matches:?}"))
}

pub(super) async fn attack_changelog_leak(stack: &Stack) -> Result<(), String> {
    let a_uid = stack.tenants[0].first_uid;
    let tenant_b = stack.tenants[1].tenant_id;
    let mut conn = app_scoped_conn(&stack.pool, tenant_b)
        .await
        .map_err(display)?;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog WHERE target_uid = $1",
    )
    .bind(a_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(display)?;
    conn.commit().await.map_err(display)?;
    (count == 0)
        .then_some(())
        .ok_or_else(|| format!("graph_changelog leaked {count} tenant A rows"))
}

pub(super) async fn attack_dlq_leak(stack: &Stack) -> Result<(), String> {
    let tenant_a = stack.tenants[0].tenant_id;
    let tenant_b = stack.tenants[1].tenant_id;
    let a_dlq = first_dlq(&stack.pool, tenant_a).await.map_err(display)?;
    let mut conn = app_scoped_conn(&stack.pool, tenant_b)
        .await
        .map_err(display)?;
    let leaked =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.ingest_dlq WHERE dlq_id = $1")
            .bind(a_dlq)
            .fetch_one(conn.as_mut())
            .await
            .map_err(display)?;
    conn.commit().await.map_err(display)?;
    (leaked == 0)
        .then_some(())
        .ok_or_else(|| format!("ingest_dlq leaked tenant A row {a_dlq}"))
}

pub(super) async fn seed_attack_dlq(pool: &PgPool, tenant_id: TenantId) -> Result<()> {
    sqlx::query(
        "INSERT INTO moa.ingest_dlq (storage_partition_id, payload, error) VALUES ($1, $2, $3)",
    )
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(json!({ "source": "perf_gate" }))
    .bind("perf_gate_fixture")
    .execute(pool)
    .await
    .context("seed perf gate DLQ fixture")?;
    Ok(())
}

pub(super) async fn first_embedding(
    pool: &PgPool,
    tenant_id: TenantId,
) -> Result<Vec<f32>, sqlx::Error> {
    let mut conn = app_scoped_conn(pool, tenant_id)
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let row = sqlx::query(
        "SELECT embedding::vector::text AS embedding FROM moa.embeddings WHERE storage_partition_id = $1 LIMIT 1",
    )
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .fetch_one(conn.as_mut())
    .await?;
    conn.commit()
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    parse_vector_text(&row.try_get::<String, _>("embedding")?)
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))
}

pub(super) async fn first_dlq(pool: &PgPool, tenant_id: TenantId) -> Result<i64, sqlx::Error> {
    let row = sqlx::query_scalar::<_, i64>(
        "SELECT dlq_id FROM moa.ingest_dlq WHERE storage_partition_id = $1 ORDER BY dlq_id LIMIT 1",
    )
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub(super) async fn app_scoped_conn<'a>(
    pool: &'a PgPool,
    tenant_id: TenantId,
) -> moa_core::Result<ScopedConn<'a>> {
    let scope = ScopeContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    Ok(conn)
}

pub(super) fn parse_vector_text(value: &str) -> Result<Vec<f32>> {
    let trimmed = value.trim().trim_start_matches('[').trim_end_matches(']');
    let vector = trimmed
        .split(',')
        .map(|part| part.trim().parse::<f32>().map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()?;
    if vector.len() != VECTOR_DIMENSION {
        bail!(
            "expected {VECTOR_DIMENSION} dimensions from pgvector text, got {}",
            vector.len()
        );
    }
    Ok(vector)
}

pub(super) fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}
