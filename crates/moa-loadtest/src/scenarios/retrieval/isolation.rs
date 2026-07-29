//! Cross-tenant isolation probes for the retrieval perf gate.

use super::*;
use moa_memory_vector::VectorMatch;

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
    let vector = PgvectorStore::new_for_app_role(stack.pool.clone(), RlsContext::tenant(tenant_b));
    let matches = moa_memory_vector::VectorStore::knn(
        &vector,
        &moa_memory_vector::VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(embedding, "test-model".to_string())
                .expect("valid query embedding"),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
            as_of: None,
        },
    )
    .await
    .map_err(display)?;
    assert_vector_matches_scoped_to_tenant(&stack.pool, tenant_b, &matches).await
}

async fn assert_vector_matches_scoped_to_tenant(
    pool: &PgPool,
    tenant_id: TenantId,
    matches: &[VectorMatch],
) -> Result<(), String> {
    let uids = unique_match_uids(matches);
    if uids.is_empty() {
        return Ok(());
    }

    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let mut conn = app_scoped_conn(pool, tenant_id).await.map_err(display)?;
    let visible_count = sqlx::query_scalar::<_, i64>(
        r#"
        WITH expected AS (SELECT unnest($1::uuid[]) AS uid)
        SELECT count(*)
        FROM expected
        JOIN moa.node_index AS node
          ON node.uid = expected.uid
         AND node.storage_partition_id = $2
        JOIN moa.embeddings AS embedding
          ON embedding.uid = expected.uid
         AND embedding.storage_partition_id = node.storage_partition_id
        "#,
    )
    .bind(&uids)
    .bind(&storage_partition_id)
    .fetch_one(conn.as_mut())
    .await
    .map_err(display)?;
    conn.commit().await.map_err(display)?;

    (visible_count as usize == uids.len())
        .then_some(())
        .ok_or_else(|| {
            format!("vector oracle returned off-scope matches for tenant {tenant_id}: {matches:?}")
        })
}

fn unique_match_uids(matches: &[VectorMatch]) -> Vec<Uuid> {
    let mut uids = matches.iter().map(|hit| hit.uid).collect::<Vec<_>>();
    uids.sort_unstable();
    uids.dedup();
    uids
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
) -> moa_core::error::Result<ScopedConn<'a>> {
    let scope = RlsContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .map_err(|error| moa_core::error::MoaError::StorageError(error.to_string()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_match_uids_collapses_duplicate_vector_hits() {
        // Pins: RLS leak accounting compares distinct returned nodes, not duplicated ANN hits.
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();

        assert_eq!(
            unique_match_uids(&[
                VectorMatch {
                    uid: second,
                    score: 0.7
                },
                VectorMatch {
                    uid: first,
                    score: 0.9
                },
                VectorMatch {
                    uid: second,
                    score: 0.6
                },
            ]),
            vec![first, second]
        );
    }
}
