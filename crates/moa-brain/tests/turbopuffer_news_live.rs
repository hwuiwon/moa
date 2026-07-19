// Live counterpart: see turbopuffer_news_offline.rs for the wiremock version that runs in PR CI.

//! Live end-to-end Turbopuffer promotion and retrieval test.

use std::sync::Arc;

use chrono::Utc;
use moa_brain::retrieval::{HybridRetriever, RetrievalRequest};
use moa_core::types::memory::RlsContext;
use moa_core::{
    config::MoaConfig, traits::EmbeddingProvider, types::contact::ContactId,
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
};
use moa_db::ScopedConn;
use moa_memory_graph::{PiiClass, PostgresGraphStore};
use moa_memory_ingest::{SessionTurn, ingest_turn_direct_with_pool};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{
    PgvectorStore, PromotionOptions, TurbopufferStore, VectorPartitionPromotion, finalize_promotion,
};
use moa_providers::{EmbedderConstructionRole, build_embedder_from_config};
use moa_session::testing;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Returns `true` when `name` is set to a common truthy value (`1`, `true`,
/// `yes`, or `on`, case-insensitively after trimming), matching how live-test
/// flags are written in a developer's `.env`.
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn require_live_turbopuffer() -> TestResult<()> {
    if !env_flag_enabled("MOA_RUN_LIVE_TURBOPUFFER_TESTS") {
        return Err("set MOA_RUN_LIVE_TURBOPUFFER_TESTS=1 to run live Turbopuffer tests".into());
    }
    required_env("TURBOPUFFER_API_KEY")?;
    Ok(())
}

fn required_env(name: &str) -> TestResult<String> {
    let value = std::env::var(name).map_err(|_| format!("{name} is required"))?;
    if value.trim().is_empty() {
        return Err(format!("{name} must not be empty").into());
    }
    Ok(value)
}

async fn news_transcript() -> TestResult<String> {
    if let Ok(path) = std::env::var("MOA_TURBOPUFFER_LIVE_NEWS_FACTS") {
        return Ok(tokio::fs::read_to_string(path).await?);
    }

    Ok(
        r#"
source: NASA Artemis news smoke
Fact: NASA Artemis II launched from Launch Pad 39B at Kennedy Space Center on April 1 2026.
Fact: NASA Artemis II splashed down off the California coast on April 10 2026 after a nearly ten day Moon mission.
Fact: NASA Artemis III core stage moved from Michoud Assembly Facility to the Pegasus barge for shipment to Kennedy Space Center.
Fact: NASA Artemis III core stage supports a 2027 crewed lunar mission using the Space Launch System.
"#
        .trim()
        .to_string(),
    )
}

#[tokio::test]
#[ignore = "live Turbopuffer plus configured embedding provider e2e; requires MOA_RUN_LIVE_TURBOPUFFER_TESTS=1, TURBOPUFFER_API_KEY, and the selected provider credential"]
async fn turbopuffer_live_news_ingest_promote_and_retrieve() -> TestResult {
    require_live_turbopuffer()?;
    let config = MoaConfig::load_from_env()?;
    let ingestion_embedder =
        build_embedder_from_config(&config, EmbedderConstructionRole::Ingestion)?;
    let retrieval_embedder =
        build_embedder_from_config(&config, EmbedderConstructionRole::Retrieval)?;
    assert_eq!(ingestion_embedder.model_id(), retrieval_embedder.model_id());
    assert_eq!(
        ingestion_embedder.model_version(),
        retrieval_embedder.model_version()
    );
    assert_eq!(
        ingestion_embedder.dimensions(),
        retrieval_embedder.dimensions()
    );

    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let pool = session_store.pool().clone();
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let workspace_text = storage_partition_id.to_string();
    let tenant_scope = RlsContext::tenant(tenant_id);
    let contact_scope = RlsContext::contact(tenant_id, contact_id);
    seed_workspace_embedder_state(
        &pool,
        &tenant_scope,
        &workspace_text,
        ingestion_embedder.as_ref(),
    )
    .await?;
    let transcript = news_transcript().await?;
    let turn = SessionTurn {
        tenant_id,
        contact_id: Some(contact_id),
        session_id: SessionId::new(),
        turn_seq: 1,
        transcript,
        dominant_pii_class: "none".to_string(),
        finalized_at: Utc::now(),
    };

    let ingest = ingest_turn_direct_with_pool(pool.clone(), turn)
        .await
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    assert!(
        ingest.inserted >= 3,
        "expected at least three news facts, got {ingest:?}"
    );

    let embedding_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.embeddings WHERE storage_partition_id = $1",
    )
    .bind(&workspace_text)
    .fetch_one(&pool)
    .await?;
    assert!(
        embedding_count >= 3,
        "expected configured-provider embeddings for ingested facts"
    );

    let promotion_pgvector = Arc::new(PgvectorStore::new_for_control_plane(
        pool.clone(),
        tenant_scope.clone(),
    ));
    let turbopuffer = Arc::new(TurbopufferStore::from_env()?);
    let scoped_turbopuffer =
        Arc::new(turbopuffer.with_storage_partition_id(workspace_text.clone()));
    let promotion =
        VectorPartitionPromotion::new(pool.clone(), promotion_pgvector, scoped_turbopuffer.clone());
    let report = promotion
        .promote(PromotionOptions {
            storage_partition_id: workspace_text.clone(),
            target_backend: "turbopuffer".to_string(),
            validate_percent: 100,
            dual_read_hours: 1,
        })
        .await?;
    assert_eq!(report.copied, embedding_count as usize);
    assert!(
        report.validation_overlap >= 0.95,
        "promotion validation overlap too low: {}",
        report.validation_overlap
    );

    let query_text = "Which Artemis mission core stage moved from Michoud to the Pegasus barge?";
    let query_embedding = retrieval_embedder
        .embed(&[query_text.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| std::io::Error::other("configured embedder returned no query embedding"))?;
    let retrieval_pgvector = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        contact_scope.clone(),
    ));
    let graph = Arc::new(PostgresGraphStore::scoped_for_app_role(
        pool.clone(),
        contact_scope,
    ));
    let retriever = HybridRetriever::new(pool.clone(), graph, retrieval_pgvector)
        .with_turbopuffer(Some(turbopuffer.clone()))
        .with_assume_app_role(true);
    let req = RetrievalRequest {
        seeds: Vec::new(),
        query_text: query_text.to_string(),
        query_embedding,
        scope: MemoryScope::Contact {
            tenant_id,
            contact_id,
        },
        label_filter: None,
        label_boost: None,
        max_pii_class: PiiClass::Restricted,
        k_final: 5,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: moa_brain::retrieval::EvidenceWindowPolicy::default(),
    };

    let dual_read_hits = retriever.retrieve(req.clone()).await?;
    assert_contains_artemis_core_stage(&dual_read_hits, "dual-read retrieval");

    finalize_promotion(&pool, &workspace_text).await?;
    let steady_hits = retriever.retrieve(req).await?;
    assert_contains_artemis_core_stage(&steady_hits, "steady Turbopuffer retrieval");

    let uids = sqlx::query_scalar::<_, Uuid>(
        "SELECT uid FROM moa.node_index WHERE storage_partition_id = $1",
    )
    .bind(&workspace_text)
    .fetch_all(&pool)
    .await?;
    scoped_turbopuffer
        .delete_in_storage_partition(&workspace_text, &uids)
        .await?;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name).await?;

    Ok(())
}

async fn seed_workspace_embedder_state(
    pool: &sqlx::PgPool,
    scope: &RlsContext,
    storage_partition_id: &str,
    embedder: &dyn EmbeddingProvider,
) -> TestResult {
    let mut conn = ScopedConn::begin(pool, scope).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(storage_partition_id)
    .bind(embedder.model_id())
    .bind(embedder.model_version())
    .bind(embedder.dimensions() as i32)
    .execute(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(())
}

fn assert_contains_artemis_core_stage(hits: &[moa_brain::retrieval::RetrievalHit], phase: &str) {
    let rendered = hits
        .iter()
        .map(|hit| {
            let props = hit
                .node
                .properties_summary
                .as_ref()
                .map(serde_json::Value::to_string)
                .unwrap_or_default();
            format!("{} {props}", hit.node.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    assert!(
        rendered.contains("artemis iii") && rendered.contains("core stage"),
        "{phase} did not return Artemis III core-stage fact; hits={hits:?}"
    );
}
