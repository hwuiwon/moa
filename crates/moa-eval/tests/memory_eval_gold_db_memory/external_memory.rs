use std::sync::Arc;

use chrono::{TimeZone, Utc};
use moa_core::traits::EmbeddingProvider;
use moa_eval::external_memory::dataset::{
    EvidenceLabels, ExternalMemoryCase, ExternalMemorySession, ExternalMemoryTurn, validate_case,
};
use moa_eval::external_memory::harness::{ExternalMemoryBackend, run_retrieval_case};
use moa_eval::external_memory::moa_backend::MoaMemoryBackend;
use moa_memory_lifecycle::ConsolidationOptions;
use moa_providers::MockEmbedding;
use moa_session::PostgresSessionStore;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn one_turn_case(isolation_key: &str, source_prefix: &str, fact: &str) -> ExternalMemoryCase {
    let occurred_at = Utc
        .with_ymd_and_hms(2026, 7, 9, 12, 0, 0)
        .single()
        .expect("fixed timestamp should parse");
    ExternalMemoryCase {
        schema_version: 1,
        isolation_key: isolation_key.to_string(),
        sessions: vec![ExternalMemorySession {
            source_id: format!("{source_prefix}-session"),
            occurred_at,
            turns: vec![ExternalMemoryTurn {
                source_id: format!("{source_prefix}-turn"),
                occurred_at,
                role: "user".to_string(),
                text: format!("Fact: {fact}"),
            }],
        }],
        question: fact.to_string(),
        options: Vec::new(),
        answer: fact.to_string(),
        category: "common".to_string(),
        evidence_labels: EvidenceLabels {
            session_source_ids: Some(vec![format!("{source_prefix}-session")]),
            turn_source_ids: Some(vec![format!("{source_prefix}-turn")]),
        },
    }
}

#[tokio::test]
async fn external_memory_moa_backend_uses_production_ingest_evidence_and_isolation() -> TestResult {
    // Pins: the real adapter ingests through the production slow path, settles, honors the
    // evidence budget, reverses occurrence IDs, and leaks nothing across resets/isolations.
    let database_url = std::env::var("MOA_DATABASE_URL").expect(
        "external_memory_moa_backend_uses_production_ingest_evidence_and_isolation requires \
         MOA_DATABASE_URL",
    );
    let (database_url, schema_name) =
        moa_session::testing::provision_cloned_database_from(&database_url).await?;
    let store =
        match PostgresSessionStore::new_in_existing_schema(&database_url, &schema_name).await {
            Ok(store) => store,
            Err(error) => {
                moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await?;
                return Err(error.into());
            }
        };
    let pool = store.pool().clone();
    let run_result: TestResult = async {
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbedding::new(1_024));
        let mut backend =
            MoaMemoryBackend::new(pool.clone(), embedder, ConsolidationOptions::default())?;

        let ada = validate_case(one_turn_case(
            "fixture/question-ada",
            "ada-source",
            "deployment owner uses Ada",
        ))?;
        let evidence = run_retrieval_case(&mut backend, &ada, 256, 50).await?;
        assert!(evidence.tokens_used <= 256);
        assert!(!evidence.rendered_evidence.is_empty());
        assert!(evidence.ranked_source_refs.len() <= 50);
        assert!(evidence.ranked_source_refs.iter().any(|source| {
            source.session_source_id == "ada-source-session"
                && source.turn_source_id == "ada-source-turn"
        }));
        assert!(evidence.rendered_source_refs.iter().any(|source| {
            source.session_source_id == "ada-source-session"
                && source.turn_source_id == "ada-source-turn"
                && !source.evidence.is_empty()
        }));

        backend.reset("fixture/question-lin").await?;
        let leaked = backend
            .retrieve("deployment owner uses Ada", 256, 50)
            .await?;
        assert!(
            leaked.ranked_source_refs.is_empty() && leaked.rendered_source_refs.is_empty(),
            "new isolation must start empty"
        );

        let lin = validate_case(one_turn_case(
            "fixture/question-lin",
            "lin-source",
            "deployment owner uses Lin",
        ))?;
        let lin_evidence = run_retrieval_case(&mut backend, &lin, 256, 50).await?;
        assert!(lin_evidence.ranked_source_refs.iter().all(|source| {
            source.session_source_id == "lin-source-session"
                && source.turn_source_id == "lin-source-turn"
        }));
        assert!(lin_evidence.rendered_source_refs.iter().all(|source| {
            source.session_source_id == "lin-source-session"
                && source.turn_source_id == "lin-source-turn"
        }));

        backend.reset("fixture/question-lin").await?;
        let after_reset = backend
            .retrieve("deployment owner uses Lin", 256, 50)
            .await?;
        assert!(
            after_reset.ranked_source_refs.is_empty()
                && after_reset.rendered_source_refs.is_empty(),
            "reset must clear prior state"
        );
        Ok(())
    }
    .await;

    drop(store);
    pool.close().await;
    let cleanup_result =
        moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await;
    run_result?;
    cleanup_result?;
    Ok(())
}
