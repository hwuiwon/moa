//! Live end-to-end Turbopuffer promotion and retrieval test.

use std::sync::Arc;

use chrono::Utc;
use moa_brain::retrieval::{HybridRetriever, RetrievalRequest};
use moa_core::{MemoryScope, ScopeContext, SessionId, UserId, WorkspaceId};
use moa_memory_graph::{AgeGraphStore, PiiClass};
use moa_memory_ingest::{SessionTurn, ingest_turn_direct_with_pool};
use moa_memory_vector::{
    CohereV4Embedder, Embedder, PgvectorStore, PromotionOptions, TurbopufferStore,
    WorkspacePromotion, finalize_promotion,
};
use moa_session::testing;
use secrecy::SecretString;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn require_live_turbopuffer() -> TestResult<()> {
    if std::env::var("MOA_RUN_LIVE_TURBOPUFFER_TESTS").as_deref() != Ok("1") {
        return Err("set MOA_RUN_LIVE_TURBOPUFFER_TESTS=1 to run live Turbopuffer tests".into());
    }
    required_env("TURBOPUFFER_API_KEY")?;
    cohere_api_key()?;
    Ok(())
}

fn required_env(name: &str) -> TestResult<String> {
    let value = std::env::var(name).map_err(|_| format!("{name} is required"))?;
    if value.trim().is_empty() {
        return Err(format!("{name} must not be empty").into());
    }
    Ok(value)
}

fn cohere_api_key() -> TestResult<String> {
    std::env::var("COHERE_API_KEY")
        .or_else(|_| std::env::var("MOA_COHERE_API_KEY"))
        .map_err(|_| "COHERE_API_KEY or MOA_COHERE_API_KEY is required".into())
        .and_then(|value| {
            if value.trim().is_empty() {
                Err("Cohere API key must not be empty".into())
            } else {
                Ok(value)
            }
        })
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
#[ignore = "live Turbopuffer plus Cohere e2e; requires MOA_RUN_LIVE_TURBOPUFFER_TESTS=1, TURBOPUFFER_API_KEY, and COHERE_API_KEY"]
async fn turbopuffer_live_news_ingest_promote_and_retrieve() -> TestResult {
    require_live_turbopuffer()?;

    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let pool = session_store.pool().clone();
    let workspace_id = WorkspaceId::new(format!("tp-news-e2e-{}", Uuid::now_v7().simple()));
    let workspace_text = workspace_id.to_string();
    let scope = ScopeContext::workspace(workspace_id.clone());
    let transcript = news_transcript().await?;
    let turn = SessionTurn {
        workspace_id: workspace_id.clone(),
        user_id: UserId::new("live-news-user"),
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

    let embedding_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.embeddings WHERE workspace_id = $1")
            .bind(&workspace_text)
            .fetch_one(&pool)
            .await?;
    assert!(
        embedding_count >= 3,
        "expected Cohere-backed embeddings for ingested facts"
    );

    let pgvector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    let turbopuffer = Arc::new(TurbopufferStore::from_env()?);
    let promotion = WorkspacePromotion::new(pool.clone(), pgvector.clone(), turbopuffer.clone());
    let report = promotion
        .promote(PromotionOptions {
            workspace_id: workspace_text.clone(),
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

    let embedder = CohereV4Embedder::new(SecretString::from(cohere_api_key()?));
    let query_text = "Which Artemis mission core stage moved from Michoud to the Pegasus barge?";
    let query_embedding = embedder
        .embed(&[query_text.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| std::io::Error::other("Cohere returned no query embedding"))?;
    let graph = Arc::new(AgeGraphStore::scoped_for_app_role(pool.clone(), scope));
    let retriever = HybridRetriever::new(pool.clone(), graph, pgvector)
        .with_turbopuffer(Some(turbopuffer.clone()))
        .with_assume_app_role(true);
    let req = RetrievalRequest {
        seeds: Vec::new(),
        query_text: query_text.to_string(),
        query_embedding,
        scope: MemoryScope::Workspace {
            workspace_id: workspace_id.clone(),
        },
        label_filter: None,
        max_pii_class: PiiClass::Restricted,
        k_final: 5,
        use_reranker: false,
        strategy: None,
    };

    let dual_read_hits = retriever.retrieve(req.clone()).await?;
    assert_contains_artemis_core_stage(&dual_read_hits, "dual-read retrieval");

    finalize_promotion(&pool, &workspace_text).await?;
    let steady_hits = retriever.retrieve(req).await?;
    assert_contains_artemis_core_stage(&steady_hits, "steady Turbopuffer retrieval");

    let uids =
        sqlx::query_scalar::<_, Uuid>("SELECT uid FROM moa.node_index WHERE workspace_id = $1")
            .bind(&workspace_text)
            .fetch_all(&pool)
            .await?;
    turbopuffer
        .delete_in_workspace(&workspace_text, &uids)
        .await?;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name).await?;

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
