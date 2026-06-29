// Gold-resolution DB memory eval support.

use std::error::Error;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{RlsContext, SessionId, StoragePartitionId, UserId, traits::EmbeddingProvider};
use moa_db::ScopedConn;
use moa_eval::memory_eval::{
    BootstrapConfig, CorpusProfile, GoldPiiStatus, GoldResolutionStatus, LedgerFact,
    MemoryRetrievalEvalOptions, RETRIEVAL_EVAL_CANDIDATE_K, RETRIEVAL_EVAL_FINAL_K,
    SyntheticSession, SyntheticTurn, build_cached_embedding_fixtures, generate_memory_eval_corpus,
    read_gold_nodes_jsonl, resolve_gold_nodes, run_memory_retrieval_eval,
    tenant_id_from_storage_partition_id, write_embeddings_jsonl, write_gold_nodes_jsonl,
    write_memory_eval_corpus,
};
use moa_memory_graph::{PostgresGraphStore, NodeLabel, PiiClass};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact, IngestCtx, IngestError,
};
use moa_memory_pii::{PiiClassifier, PiiError, PiiResult, PiiSpan};
use moa_memory_types::ScopeTier;
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use moa_session::testing;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

static GOLD_RESOLUTION_TEST_LOCK: Mutex<()> = Mutex::const_new(());
const GOLD_RESOLUTION_EMBEDDER_MODEL: &str = "gold-resolution-mock-embedder";
const GOLD_RESOLUTION_EMBEDDER_VERSION: i32 = 31;

#[derive(Debug, Clone)]
struct GoldResolutionEmbedder;

#[async_trait]
impl EmbeddingProvider for GoldResolutionEmbedder {
    fn model_id(&self) -> &str {
        GOLD_RESOLUTION_EMBEDDER_MODEL
    }

    fn model_version(&self) -> i32 {
        GOLD_RESOLUTION_EMBEDDER_VERSION
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, texts: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| gold_resolution_vector(text))
            .collect())
    }
}

#[derive(Debug, Clone)]
struct GoldResolutionNoPiiClassifier;

#[async_trait]
impl PiiClassifier for GoldResolutionNoPiiClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, PiiError> {
        Ok(PiiResult {
            class: PiiClass::None,
            spans: Vec::<PiiSpan>::new(),
            model_version: "gold-resolution-no-pii".to_string(),
            abstained: false,
        })
    }
}

#[derive(Debug, Clone)]
struct GoldResolutionInsertOnlyDetector;

#[async_trait]
impl ContradictionDetector for GoldResolutionInsertOnlyDetector {
    async fn check_one_fast(
        &self,
        _fact_text: &str,
        _embedding: &[f32],
        _label: NodeLabel,
        _pii_class: PiiClass,
        _ctx: &ContradictionContext,
    ) -> Result<Conflict, IngestError> {
        Ok(Conflict::Insert)
    }

    async fn check_one_slow(
        &self,
        _fact: &EmbeddedFact,
        _ctx: &ContradictionContext,
    ) -> Result<Conflict, IngestError> {
        Ok(Conflict::Insert)
    }
}

struct GoldResolutionStack {
    pool: PgPool,
    database_url: String,
    schema_name: String,
}

impl GoldResolutionStack {
    async fn up() -> TestResult<Self> {
        let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .map_err(Box::<dyn Error + Send + Sync>::from)?;
        Ok(Self {
            pool: session_store.pool().clone(),
            database_url,
            schema_name,
        })
    }

    async fn ingest_ctx(&self, storage_partition_id: &StoragePartitionId) -> TestResult<IngestCtx> {
        let scope = RlsContext::tenant(tenant_id_from_storage_partition_id(storage_partition_id));
        self.seed_tenant_embedder_state(&scope, storage_partition_id)
            .await?;
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            self.pool.clone(),
            scope.clone(),
        ));
        let graph = Arc::new(
            PostgresGraphStore::scoped_for_app_role(self.pool.clone(), scope)
                .with_vector_store(vector.clone()),
        );
        Ok(IngestCtx::new(
            self.pool.clone(),
            graph,
            vector,
            Arc::new(GoldResolutionEmbedder),
            Arc::new(GoldResolutionNoPiiClassifier),
            Arc::new(GoldResolutionInsertOnlyDetector),
        ))
    }

    async fn seed_tenant_embedder_state(
        &self,
        scope: &RlsContext,
        storage_partition_id: &StoragePartitionId,
    ) -> TestResult {
        let runtime_storage_partition_id =
            tenant_id_from_storage_partition_id(storage_partition_id).to_string();
        let mut conn = ScopedConn::begin(&self.pool, scope).await?;
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
        .bind(&runtime_storage_partition_id)
        .bind(GOLD_RESOLUTION_EMBEDDER_MODEL)
        .bind(GOLD_RESOLUTION_EMBEDDER_VERSION)
        .bind(VECTOR_DIMENSION as i32)
        .execute(conn.as_mut())
        .await?;
        conn.commit().await?;
        Ok(())
    }

    async fn cleanup(self) -> TestResult {
        cleanup_gold_resolution_rows(&self.pool, &self.storage_partition_ids()).await?;
        testing::cleanup_test_schema(&self.database_url, &self.schema_name)
            .await
            .map_err(Box::<dyn Error + Send + Sync>::from)
    }

    fn storage_partition_ids(&self) -> Vec<String> {
        [
            gold_resolution_storage_partition_id("explicit", &self.schema_name),
            gold_resolution_storage_partition_id("partial", &self.schema_name),
        ]
        .iter()
        .map(|storage_partition_id| {
            tenant_id_from_storage_partition_id(storage_partition_id).to_string()
        })
        .collect()
    }
}

async fn cleanup_gold_resolution_rows(
    pool: &PgPool,
    storage_partition_ids: &[String],
) -> TestResult {
    sqlx::query("DELETE FROM moa.edge_index WHERE storage_partition_id = ANY($1)")
        .bind(storage_partition_ids)
        .execute(pool)
        .await?;
    for table in [
        "embeddings",
        "ingest_dlq",
        "ingest_dedup",
        "memory_digests",
        "retrieval_lineage",
        "graph_changelog",
        "node_index",
        "storage_partition_state",
    ] {
        let sql = format!("DELETE FROM moa.{table} WHERE storage_partition_id = ANY($1)");
        sqlx::query(&sql)
            .bind(storage_partition_ids)
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn run_explicit_gold_resolution_case(stack: &GoldResolutionStack) -> TestResult {
    let (ledger, sessions) = explicit_gold_resolution_corpus(&stack.schema_name);
    let storage_partition_id = sessions
        .first()
        .expect("explicit gold corpus includes a session")
        .storage_partition_id
        .clone();
    let report = resolve_gold_nodes(
        stack.ingest_ctx(&storage_partition_id).await?,
        &ledger,
        &sessions,
    )
    .await?;

    assert_eq!(report.ingestion_coverage(), 1.0);
    assert!(report.unresolved_facts().is_empty());
    assert!(
        report.duplicate_resolutions().is_empty(),
        "explicit fact turns should resolve uniquely"
    );
    assert_eq!(report.records.len(), 2);
    for record in &report.records {
        assert_eq!(record.resolution_status, GoldResolutionStatus::Resolved);
        assert_eq!(record.node_uids.len(), 1);
        assert!(record.active);
        assert!(record.valid_from.is_some());
        assert_eq!(record.valid_to, None);
    }
    assert_eq!(report.scope_match_rate(), 1.0);

    let tenant_fact = report
        .records
        .iter()
        .find(|record| record.fact_id == "fact-explicit-runtime")
        .expect("tenant-scope ledger fact has a gold record");
    assert_eq!(tenant_fact.expected_scope, "tenant");
    assert_eq!(
        tenant_fact.scope.as_deref(),
        Some("tenant"),
        "gold_nodes should record actual stored node_index.scope"
    );
    assert_resolved_node_uses_runtime_storage_partition(stack, &storage_partition_id, tenant_fact)
        .await?;

    let contact_fact = report
        .records
        .iter()
        .find(|record| record.fact_id == "fact-explicit-user-preference")
        .expect("contact-scope ledger fact has a gold record");
    assert_eq!(contact_fact.expected_scope, "contact");
    assert_eq!(
        contact_fact.scope.as_deref(),
        Some("contact"),
        "unmarked contact-preference facts should stay contact scoped in slow-path ingest"
    );
    assert_eq!(contact_fact.pii_status, GoldPiiStatus::NotExpected);

    let temp = tempfile::tempdir().expect("create temp gold output directory");
    let gold_path = temp.path().join("gold_nodes.jsonl");
    write_gold_nodes_jsonl(&gold_path, &report.records).await?;
    let first_write = tokio::fs::read(&gold_path).await?;
    write_gold_nodes_jsonl(&gold_path, &report.records).await?;
    let second_write = tokio::fs::read(&gold_path).await?;
    assert_eq!(first_write, second_write, "gold_nodes.jsonl is byte-stable");

    let round_tripped = read_gold_nodes_jsonl(&gold_path).await?;
    assert_eq!(round_tripped, report.records);
    let first_line = first_write
        .split(|byte| *byte == b'\n')
        .find(|line| !line.is_empty())
        .expect("gold jsonl contains at least one record");
    let json: serde_json::Value = serde_json::from_slice(first_line)?;
    for required_field in [
        "fact_id",
        "node_uids",
        "scope",
        "active",
        "valid_from",
        "valid_to",
        "resolution_status",
    ] {
        assert!(
            json.get(required_field).is_some(),
            "gold_nodes.jsonl record should include {required_field}"
        );
    }

    Ok(())
}

async fn assert_resolved_node_uses_runtime_storage_partition(
    stack: &GoldResolutionStack,
    ledger_storage_partition_id: &StoragePartitionId,
    record: &moa_eval::memory_eval::GoldNodeRecord,
) -> TestResult {
    let runtime_storage_partition_id =
        tenant_id_from_storage_partition_id(ledger_storage_partition_id).to_string();
    assert_ne!(
        ledger_storage_partition_id.as_str(),
        runtime_storage_partition_id,
        "fixture must use a ledger label so gold resolution proves runtime partition mapping"
    );
    let node_uid = record
        .node_uids
        .first()
        .copied()
        .expect("resolved gold record includes one node uid");
    let stored_storage_partition_id: Option<String> =
        sqlx::query_scalar("SELECT storage_partition_id FROM moa.node_index WHERE uid = $1")
            .bind(node_uid)
            .fetch_optional(&stack.pool)
            .await?;

    assert_eq!(
        stored_storage_partition_id.as_deref(),
        Some(runtime_storage_partition_id.as_str()),
        "resolved gold node should be stored under the mapped runtime tenant UUID"
    );
    Ok(())
}

async fn run_partial_gold_resolution_case(stack: &GoldResolutionStack) -> TestResult {
    let (ledger, sessions) = partial_gold_resolution_corpus(&stack.schema_name);
    let storage_partition_id = sessions
        .first()
        .expect("partial gold corpus includes a session")
        .storage_partition_id
        .clone();
    let report = resolve_gold_nodes(
        stack.ingest_ctx(&storage_partition_id).await?,
        &ledger,
        &sessions,
    )
    .await?;

    assert!(
        report.ingestion_coverage() < 1.0,
        "coverage should drop when a ledger fact is deliberately unextractable"
    );
    assert_eq!(report.ingestion_coverage(), 0.5);
    assert_eq!(report.unresolved_facts(), vec!["fact-hidden-launch-code"]);

    let hidden = report
        .records
        .iter()
        .find(|record| record.fact_id == "fact-hidden-launch-code")
        .expect("hidden ledger fact has a gold record");
    assert_eq!(hidden.resolution_status, GoldResolutionStatus::Unresolved);
    assert!(hidden.node_uids.is_empty());
    assert_eq!(hidden.scope, None);
    assert!(!hidden.active);

    let explicit = report
        .records
        .iter()
        .find(|record| record.fact_id == "fact-visible-runtime")
        .expect("explicit ledger fact has a gold record");
    assert_eq!(explicit.resolution_status, GoldResolutionStatus::Resolved);
    assert_eq!(explicit.node_uids.len(), 1);

    Ok(())
}

fn explicit_gold_resolution_corpus(
    tenant_suffix: &str,
) -> (Vec<LedgerFact>, Vec<SyntheticSession>) {
    let session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08d01");
    let storage_partition_id = gold_resolution_storage_partition_id("explicit", tenant_suffix);
    let user_id = user("gold-resolution-explicit-user");
    let facts = vec![
        gold_fact(GoldFactSpec {
            storage_partition_id: storage_partition_id.clone(),
            user_id: user_id.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-explicit-runtime",
            valid_from: utc("2026-02-01T00:00:00Z"),
            subject: "runtime",
            predicate: "uses",
            object: "restate",
            answer: "Runtime uses Restate.",
            source_session_id: session,
            source_turn_seq: 1,
        }),
        gold_fact(GoldFactSpec {
            storage_partition_id: storage_partition_id.clone(),
            user_id: user_id.clone(),
            scope: ScopeTier::Contact,
            fact_id: "fact-explicit-user-preference",
            valid_from: utc("2026-02-02T00:00:00Z"),
            subject: "casey",
            predicate: "prefers",
            object: "terse updates",
            answer: "Casey prefers terse updates.",
            source_session_id: session,
            source_turn_seq: 2,
        }),
    ];
    let sessions = vec![SyntheticSession {
        session_id: session,
        storage_partition_id,
        user_id,
        turns: vec![
            SyntheticTurn {
                turn_seq: 1,
                transcript: "Fact: tenant shared runtime uses restate.".to_string(),
                fact_ids: vec!["fact-explicit-runtime".to_string()],
            },
            SyntheticTurn {
                turn_seq: 2,
                transcript: "Fact: casey prefers terse updates.".to_string(),
                fact_ids: vec!["fact-explicit-user-preference".to_string()],
            },
        ],
    }];
    (facts, sessions)
}

fn partial_gold_resolution_corpus(tenant_suffix: &str) -> (Vec<LedgerFact>, Vec<SyntheticSession>) {
    let session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08d02");
    let storage_partition_id = gold_resolution_storage_partition_id("partial", tenant_suffix);
    let user_id = user("gold-resolution-partial-user");
    let facts = vec![
        gold_fact(GoldFactSpec {
            storage_partition_id: storage_partition_id.clone(),
            user_id: user_id.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-visible-runtime",
            valid_from: utc("2026-02-03T00:00:00Z"),
            subject: "planner",
            predicate: "uses",
            object: "query rewrite",
            answer: "Planner uses query rewrite.",
            source_session_id: session,
            source_turn_seq: 1,
        }),
        gold_fact(GoldFactSpec {
            storage_partition_id: storage_partition_id.clone(),
            user_id: user_id.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-hidden-launch-code",
            valid_from: utc("2026-02-04T00:00:00Z"),
            subject: "launch",
            predicate: "code",
            object: "aurora",
            answer: "Launch code is aurora.",
            source_session_id: session,
            source_turn_seq: 2,
        }),
    ];
    let sessions = vec![SyntheticSession {
        session_id: session,
        storage_partition_id,
        user_id,
        turns: vec![
            SyntheticTurn {
                turn_seq: 1,
                transcript: "Fact: planner uses query rewrite.".to_string(),
                fact_ids: vec!["fact-visible-runtime".to_string()],
            },
            SyntheticTurn {
                turn_seq: 2,
                transcript: "Please remember launch code aurora".to_string(),
                fact_ids: vec!["fact-hidden-launch-code".to_string()],
            },
        ],
    }];
    (facts, sessions)
}

fn gold_resolution_storage_partition_id(kind: &str, tenant_suffix: &str) -> StoragePartitionId {
    storage_partition(&format!("gold-resolution-{kind}-tenant-{tenant_suffix}"))
}

struct GoldFactSpec {
    storage_partition_id: StoragePartitionId,
    user_id: UserId,
    scope: ScopeTier,
    fact_id: &'static str,
    valid_from: DateTime<Utc>,
    subject: &'static str,
    predicate: &'static str,
    object: &'static str,
    answer: &'static str,
    source_session_id: SessionId,
    source_turn_seq: u64,
}

fn gold_fact(spec: GoldFactSpec) -> LedgerFact {
    LedgerFact {
        storage_partition_id: spec.storage_partition_id,
        user_id: spec.user_id,
        scope: spec.scope,
        fact_id: spec.fact_id.to_string(),
        valid_from: spec.valid_from,
        valid_to: None,
        subject: spec.subject.to_string(),
        predicate: spec.predicate.to_string(),
        object: spec.object.to_string(),
        answer: spec.answer.to_string(),
        supersedes: Vec::new(),
        restates: None,
        prior_uses: None,
        prior_successes: None,
        source_session_id: spec.source_session_id,
        source_turn_seq: spec.source_turn_seq,
        pii_class: PiiClass::None,
        expected_redacted: false,
    }
}

fn gold_resolution_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0_f32; VECTOR_DIMENSION];
    for (index, byte) in text.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

fn session_id(value: &str) -> SessionId {
    SessionId(Uuid::parse_str(value).expect("stable fixture session UUID"))
}

fn user(value: &str) -> UserId {
    UserId::new(value)
}

fn storage_partition(value: &str) -> StoragePartitionId {
    StoragePartitionId::new(value)
}

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp parses")
        .with_timezone(&Utc)
}
