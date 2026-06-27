use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_brain::planning::parse_temporal;
use moa_brain::retrieval::{LegSources, RetrievalHit, SourceTier};
use moa_core::{MoaError, SessionId, StoragePartitionId, UserId, traits::EmbeddingProvider};
use moa_db::ScopedConn;
use moa_eval::kernel::{CostLedger, ProviderProvenance};
use moa_eval::memory_eval::runner::QueryRewriteClassMetrics;
use moa_eval::memory_eval::{
    BinaryProbeOutcome, BootstrapConfig, CORPUS_SCHEMA_VERSION, CachedEmbeddingProvider,
    CandidateLegs, CorpusManifest, CorpusProfile, EmbeddingInputKind, EntityFragmentationCounts,
    ExtractionPrecisionCounts, GeneratedMemoryEvalCorpus, GoldNodeRecord, GoldPiiStatus,
    GoldResolutionReport, GoldResolutionStatus, LedgerFact, MemoryRetrievalEvalOptions,
    MemoryRetrievalEvalReport, MetricSummary, Probe, ProbeResult, ProbeType, QueryRewritePolicy,
    RETRIEVAL_EVAL_CANDIDATE_K, RETRIEVAL_EVAL_FINAL_K, RetrievedCandidate, SyntheticSession,
    SyntheticTurn, TranscriptStyle, aggregate_retrieval_eval_from_counts,
    aggregate_retrieval_eval_from_diagnostic_counts, aggregate_retrieval_eval_with_diagnostics,
    aggregate_retrieval_eval_with_extraction_precision, benjamini_hochberg,
    build_cached_embedding_fixtures, candidates_from_retrieval_hits, embedding_text_hash,
    generate_memory_eval_corpus, generate_memory_eval_corpus_with_style, mcnemar_paired_test,
    read_embedding_inputs_jsonl, read_embeddings_jsonl, read_gold_nodes_jsonl, read_ledger_jsonl,
    read_manifest_json, read_probes_jsonl, read_sessions_jsonl, resolve_gold_nodes,
    run_memory_retrieval_eval, stable_uuid_from_label, tenant_id_from_storage_partition_id,
    validate_corpus, write_embeddings_jsonl, write_gold_nodes_jsonl, write_ledger_jsonl,
    write_manifest_json, write_memory_eval_corpus, write_probes_jsonl, write_sessions_jsonl,
};
use moa_eval_core::EvalError;
use moa_memory_graph::{AgeGraphStore, NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact, IngestCtx, IngestError,
};
use moa_memory_pii::{PiiClassifier, PiiError, PiiResult, PiiSpan};
use moa_core::RlsContext;
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
            AgeGraphStore::scoped_for_app_role(self.pool.clone(), scope)
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
        .bind(storage_partition_id.as_str())
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
        vec![
            gold_resolution_storage_partition_id("explicit", &self.schema_name)
                .as_str()
                .to_string(),
            gold_resolution_storage_partition_id("partial", &self.schema_name)
                .as_str()
                .to_string(),
        ]
    }
}

async fn cleanup_gold_resolution_rows(
    pool: &PgPool,
    storage_partition_ids: &[String],
) -> TestResult {
    cleanup_gold_resolution_age_rows(pool, storage_partition_ids).await?;
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

async fn cleanup_gold_resolution_age_rows(
    pool: &PgPool,
    storage_partition_ids: &[String],
) -> TestResult {
    for label in GOLD_RESOLUTION_AGE_EDGE_LABELS {
        cleanup_gold_resolution_age_table(pool, label, storage_partition_ids).await?;
    }
    for label in GOLD_RESOLUTION_AGE_NODE_LABELS {
        cleanup_gold_resolution_age_table(pool, label, storage_partition_ids).await?;
    }
    Ok(())
}

async fn cleanup_gold_resolution_age_table(
    pool: &PgPool,
    label: &str,
    storage_partition_ids: &[String],
) -> TestResult {
    let sql = format!(
        r#"
        DELETE FROM moa_graph."{label}"
        WHERE trim(both '"' from moa.age_property(properties, 'storage_partition_id')::text) = $1
        "#
    );
    for storage_partition_id in storage_partition_ids {
        sqlx::query(&sql)
            .bind(storage_partition_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

const GOLD_RESOLUTION_AGE_NODE_LABELS: &[&str] = &[
    "Entity", "Concept", "Decision", "Incident", "Lesson", "Fact", "Source",
];

const GOLD_RESOLUTION_AGE_EDGE_LABELS: &[&str] = &[
    "RELATES_TO",
    "DEPENDS_ON",
    "OWNED_BY",
    "SUPERSEDES",
    "CONTRADICTS",
    "DERIVED_FROM",
    "MENTIONED_IN",
    "CAUSED",
    "LEARNED_FROM",
    "APPLIES_TO",
];

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
    storage_partition(
        &stable_uuid_from_label(&format!("gold-resolution-{kind}-tenant-{tenant_suffix}"))
            .to_string(),
    )
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


fn assert_profile_shape(
    corpus: &GeneratedMemoryEvalCorpus,
    expected_users: usize,
    expected_tenants: usize,
    probe_range: std::ops::RangeInclusive<usize>,
) {
    assert_eq!(corpus.manifest.seeds.len(), 3);
    assert_eq!(
        corpus
            .manifest
            .seeds
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len(),
        3,
        "generator seeds should be independent"
    );
    assert_eq!(distinct_users(corpus).len(), expected_users);
    assert_eq!(distinct_tenants(corpus).len(), expected_tenants);
    assert!(
        probe_range.contains(&corpus.probes.len()),
        "probe count {} should be in expected range",
        corpus.probes.len()
    );

    for probe_type in [
        ProbeType::PointRecall,
        ProbeType::LatestValueAfterUpdate,
        ProbeType::Abstention,
        ProbeType::CrossUserIsolation,
        ProbeType::TenantSharedFact,
        ProbeType::MultiHop,
        ProbeType::TemporalAsOf,
        ProbeType::PreferenceApplication,
        ProbeType::PiiRedaction,
    ] {
        assert!(
            corpus
                .probes
                .iter()
                .any(|probe| probe.probe_type == probe_type),
            "generated corpus should include {probe_type:?}"
        );
    }
}

fn assert_ledger_first_fact_classes(ledger: &[LedgerFact]) {
    assert!(
        ledger.iter().any(|fact| !fact.supersedes.is_empty()),
        "ledger should include supersession facts"
    );
    assert!(
        ledger
            .iter()
            .any(|fact| fact.scope == ScopeTier::Tenant && fact.predicate == "require_runbook"),
        "ledger should include tenant-shared facts"
    );
    assert!(
        ledger.iter().any(|fact| fact.scope == ScopeTier::Contact
            && fact.predicate == "private_repository"
            && fact.pii_class == PiiClass::None),
        "ledger should include user-private facts"
    );
    assert!(
        ledger
            .iter()
            .any(|fact| fact.predicate == "on_call_primary" && fact.valid_to.is_some()),
        "ledger should include temporal facts"
    );
    assert!(
        ledger.iter().any(|fact| fact.predicate == "response_style"),
        "ledger should include preference facts"
    );
    assert!(
        ledger
            .iter()
            .any(|fact| fact.pii_class == PiiClass::Pii && fact.expected_redacted),
        "ledger should include PII facts"
    );

    let mut contradiction_objects = BTreeMap::<(String, String, String), BTreeSet<String>>::new();
    for fact in ledger
        .iter()
        .filter(|fact| fact.predicate == "cache_backend_conflict")
    {
        contradiction_objects
            .entry((
                fact.storage_partition_id.as_str().to_string(),
                fact.subject.clone(),
                fact.predicate.clone(),
            ))
            .or_default()
            .insert(fact.object.clone());
    }
    assert!(
        contradiction_objects
            .values()
            .any(|objects| objects.len() >= 2),
        "ledger should include unresolved contradiction facts"
    );
}

fn distinct_users(corpus: &GeneratedMemoryEvalCorpus) -> BTreeSet<String> {
    let mut users = BTreeSet::new();
    for fact in &corpus.ledger {
        users.insert(fact.user_id.as_str().to_string());
    }
    for session in &corpus.sessions {
        users.insert(session.user_id.as_str().to_string());
    }
    for probe in &corpus.probes {
        users.insert(probe.user_id.as_str().to_string());
    }
    users
}

fn distinct_tenants(corpus: &GeneratedMemoryEvalCorpus) -> BTreeSet<String> {
    let mut tenants = BTreeSet::new();
    for fact in &corpus.ledger {
        tenants.insert(fact.storage_partition_id.as_str().to_string());
    }
    for session in &corpus.sessions {
        tenants.insert(session.storage_partition_id.as_str().to_string());
    }
    for probe in &corpus.probes {
        tenants.insert(probe.storage_partition_id.as_str().to_string());
    }
    tenants
}

fn sessions_per_user(sessions: &[SyntheticSession]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for session in sessions {
        *counts
            .entry(session.user_id.as_str().to_string())
            .or_insert(0) += 1;
    }
    counts
}

fn fixture_vector(
    fixtures: &[moa_eval::memory_eval::CachedEmbeddingFixture],
    text: &str,
) -> Vec<f32> {
    let text_hash = embedding_text_hash(text);
    fixtures
        .iter()
        .find(|fixture| fixture.text_hash == text_hash)
        .expect("fixture vector exists for text")
        .vector
        .clone()
}

fn retrieval_metric_probe_results() -> Vec<ProbeResult> {
    vec![
        ProbeResult {
            probe_id: "probe-runtime".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::PointRecall,
            expected_fact_ids: fact_ids(&["fact-runtime"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x100,
                &[CandidateSpec {
                    fact_id: Some("fact-runtime"),
                    legs: legs(true, true, false),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-rank-five".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::LatestValueAfterUpdate,
            expected_fact_ids: fact_ids(&["fact-rank-five"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x200,
                &[
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(true, false, false),
                    },
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(false, true, false),
                    },
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(false, false, true),
                    },
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(true, true, false),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-rank-five"),
                        legs: legs(false, true, false),
                    },
                ],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-multi-hop".to_string(),
            user_id: "user-bob".to_string(),
            probe_type: ProbeType::MultiHop,
            expected_fact_ids: fact_ids(&["fact-service-owner", "fact-runbook"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x300,
                &[
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(true, false, false),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-service-owner"),
                        legs: legs(false, false, true),
                    },
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(false, true, true),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-runbook"),
                        legs: legs(true, false, true),
                    },
                ],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-temporal-miss".to_string(),
            user_id: "user-bob".to_string(),
            probe_type: ProbeType::TemporalAsOf,
            expected_fact_ids: fact_ids(&["fact-temporal-old"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x400,
                &[CandidateSpec {
                    fact_id: Some("fact-temporal-new"),
                    legs: legs(true, true, true),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(false),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: Some(false),
            temporal_filter_parsed: Some(true),
            temporal_filter_matches_as_of: Some(true),
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-pii-redacted".to_string(),
            user_id: "user-casey".to_string(),
            probe_type: ProbeType::PiiRedaction,
            expected_fact_ids: fact_ids(&["fact-pii-phone"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x500,
                &[CandidateSpec {
                    fact_id: Some("fact-pii-phone"),
                    legs: legs(false, false, true),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: Some(true),
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-abstains".to_string(),
            user_id: "user-casey".to_string(),
            probe_type: ProbeType::Abstention,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: Vec::new(),
            candidates: Vec::new(),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: None,
            abstention_correct: Some(true),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-cross-user-leak".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::CrossUserIsolation,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: fact_ids(&["fact-secret"]),
            candidates: metric_candidates(
                0x700,
                &[CandidateSpec {
                    fact_id: Some("fact-secret"),
                    legs: legs(true, false, false),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: None,
            abstention_correct: Some(false),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
    ]
}

#[derive(Debug, Clone, Copy)]
struct CandidateSpec {
    fact_id: Option<&'static str>,
    legs: LegSources,
}

fn metric_candidates(base: u128, specs: &[CandidateSpec]) -> Vec<RetrievedCandidate> {
    let mut fact_ids_by_uid = HashMap::new();
    let hits = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let uid = Uuid::from_u128(base + index as u128 + 1);
            if let Some(fact_id) = spec.fact_id {
                fact_ids_by_uid.insert(uid, fact_id.to_string());
            }
            RetrievalHit {
                uid,
                score: 1.0 / (index + 1) as f64,
                legs: spec.legs,
                source_tier: SourceTier::UserMemory,
                knowledge_chunk: None,
                node: metric_node(uid),
            }
        })
        .collect::<Vec<_>>();
    candidates_from_retrieval_hits(&hits, &fact_ids_by_uid, &HashMap::new())
}

fn metric_node(uid: Uuid) -> NodeIndexRow {
    NodeIndexRow {
        uid,
        label: NodeLabel::Fact,
        storage_partition_id: Some("metrics-storage-partition".to_string()),
        contact_id: Some("metrics-contact".to_string()),
        scope: "tenant".to_string(),
        name: format!("metric-node-{uid}"),
        pii_class: PiiClass::None,
        valid_to: None,
        valid_from: utc("2026-05-01T00:00:00Z"),
        properties_summary: None,
        last_accessed_at: utc("2026-05-02T00:00:00Z"),
        quality_score: 0.5,
    }
}

fn parse_metric_probe(
    probe_id: &str,
    probe_type: ProbeType,
    temporal_filter_parsed: Option<bool>,
    temporal_filter_matches_as_of: Option<bool>,
) -> ProbeResult {
    ProbeResult {
        probe_id: probe_id.to_string(),
        user_id: "user-parser".to_string(),
        probe_type,
        expected_fact_ids: fact_ids(&["fact-parser"]),
        blocked_fact_ids: Vec::new(),
        candidates: metric_candidates(
            0x1_0000,
            &[CandidateSpec {
                fact_id: Some("fact-parser"),
                legs: legs(false, false, true),
            }],
        ),
        post_rerank_candidates: None,
        retrieval_latency_ms: 0,
        answer_faithful: Some(true),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: (probe_type == ProbeType::TemporalAsOf).then_some(true),
        temporal_filter_parsed,
        temporal_filter_matches_as_of,
        preference_context_hit: None,
    }
}

fn legs(graph: bool, vector: bool, lexical: bool) -> LegSources {
    LegSources {
        graph,
        vector,
        lexical,
    }
}

fn fact_ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn assert_metric(summary: MetricSummary, numerator: f64, denominator: usize, value: f64) {
    assert_close(summary.numerator, numerator);
    assert_eq!(summary.denominator, denominator);
    assert_close(summary.value, value);
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 1.0e-12,
        "expected {expected}, got {actual}"
    );
}

fn binary_outcomes(
    probe_suffixes: &str,
    success_for_index: impl Fn(usize) -> bool,
) -> Vec<BinaryProbeOutcome> {
    probe_suffixes
        .chars()
        .enumerate()
        .map(|(index, suffix)| BinaryProbeOutcome {
            probe_id: format!("probe-{suffix}"),
            success: success_for_index(index),
        })
        .collect()
}

fn memory_budget_report(probe_results: Vec<ProbeResult>) -> MemoryRetrievalEvalReport {
    memory_budget_report_with_reranker(probe_results, false)
}


fn memory_budget_report_with_reranker(
    probe_results: Vec<ProbeResult>,
    reranker_enabled: bool,
) -> MemoryRetrievalEvalReport {
    let retrieval = aggregate_retrieval_eval_from_counts(
        3,
        3,
        probe_results,
        BootstrapConfig {
            resamples: 25,
            seed: 23,
        },
    );
    MemoryRetrievalEvalReport {
        manifest: CorpusManifest {
            version: CORPUS_SCHEMA_VERSION,
            corpus_id: "memory-budget-fixture".to_string(),
            profile: CorpusProfile::Pr,
            description: "Hermetic budget gate fixture.".to_string(),
            seeds: vec![1, 2, 3],
            transcript_style: TranscriptStyle::Marked,
        },
        candidate_k: RETRIEVAL_EVAL_CANDIDATE_K,
        final_k: RETRIEVAL_EVAL_FINAL_K,
        reranker_enabled,
        query_rewrite_policy: QueryRewritePolicy::Gated,
        query_rewrite_call_count: 0,
        query_rewrite_skip_count: 0,
        query_rewrite_call_rate: 0.0,
        query_rewrite_p50_latency_ms: 0,
        query_rewrite_p95_latency_ms: 0,
        query_rewrite_input_tokens: 0,
        query_rewrite_output_tokens: 0,
        query_rewrite_est_usd: 0.0,
        retrieval_plus_rewrite_p95_latency_ms: retrieval.metrics.p95_retrieval_latency_ms,
        query_rewrite_by_class: BTreeMap::from([(
            "exact_identifier".to_string(),
            QueryRewriteClassMetrics {
                total_count: 1,
                call_count: 0,
                skip_count: 1,
                call_rate: 0.0,
            },
        )]),
        aborted_over_budget: false,
        cost: None,
        providers: None,
        metrics: retrieval.metrics,
        probe_results: retrieval.probe_results,
        bootstrap: retrieval.bootstrap,
        cross_user_leak_probe_ids: retrieval.cross_user_leak_probe_ids,
        gold_resolution: GoldResolutionReport {
            ingest_reports: Vec::new(),
            records: Vec::new(),
        },
        consolidation: None,
    }
}

fn memory_budget_probe_results(cross_user_leak: bool) -> Vec<ProbeResult> {
    let cross_user_candidates = if cross_user_leak {
        metric_candidates(
            0xb00,
            &[CandidateSpec {
                fact_id: Some("fact-bob-secret"),
                legs: legs(true, false, false),
            }],
        )
    } else {
        Vec::new()
    };

    vec![
        ProbeResult {
            probe_id: "probe-latest-ordinary-blocked-leak".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::LatestValueAfterUpdate,
            expected_fact_ids: fact_ids(&["fact-current"]),
            blocked_fact_ids: fact_ids(&["fact-old"]),
            candidates: metric_candidates(
                0xa00,
                &[
                    CandidateSpec {
                        fact_id: Some("fact-old"),
                        legs: legs(true, false, false),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-current"),
                        legs: legs(false, true, false),
                    },
                ],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-cross-user-leak".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::CrossUserIsolation,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: fact_ids(&["fact-bob-secret"]),
            candidates: cross_user_candidates,
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(!cross_user_leak),
            abstention_correct: Some(!cross_user_leak),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-pii-redacted".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::PiiRedaction,
            expected_fact_ids: fact_ids(&["fact-phone"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0xc00,
                &[CandidateSpec {
                    fact_id: Some("fact-phone"),
                    legs: legs(false, false, true),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: Some(true),
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
    ]
}

fn reranker_recall_regression_probe_results() -> Vec<ProbeResult> {
    vec![ProbeResult {
        probe_id: "probe-reranker-regresses-recall".to_string(),
        user_id: "user-alice".to_string(),
        probe_type: ProbeType::PointRecall,
        expected_fact_ids: fact_ids(&["fact-owner"]),
        blocked_fact_ids: Vec::new(),
        candidates: metric_candidates(
            0xe00,
            &[CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            }],
        ),
        post_rerank_candidates: Some(metric_candidates(
            0xe10,
            &[CandidateSpec {
                fact_id: None,
                legs: legs(false, true, false),
            }],
        )),
        retrieval_latency_ms: 100,
        answer_faithful: Some(false),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
    }]
}

fn reranker_latency_without_gain_probe_results() -> Vec<ProbeResult> {
    vec![ProbeResult {
        probe_id: "probe-reranker-slow-without-gain".to_string(),
        user_id: "user-alice".to_string(),
        probe_type: ProbeType::PointRecall,
        expected_fact_ids: fact_ids(&["fact-owner"]),
        blocked_fact_ids: Vec::new(),
        candidates: metric_candidates(
            0xe20,
            &[CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            }],
        ),
        post_rerank_candidates: Some(metric_candidates(
            0xe30,
            &[CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            }],
        )),
        retrieval_latency_ms: 2_501,
        answer_faithful: Some(true),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
    }]
}

fn memory_budget_regression_probe_results(full_recall: bool) -> Vec<ProbeResult> {
    let candidate_specs = if full_recall {
        vec![
            CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            },
            CandidateSpec {
                fact_id: Some("fact-runbook"),
                legs: legs(false, true, false),
            },
        ]
    } else {
        vec![
            CandidateSpec {
                fact_id: None,
                legs: legs(false, false, true),
            },
            CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            },
        ]
    };

    vec![
        ProbeResult {
            probe_id: "probe-regression-multi-hop".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::MultiHop,
            expected_fact_ids: fact_ids(&["fact-owner", "fact-runbook"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(0xd00, &candidate_specs),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(full_recall),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-regression-cross-user-clean".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::CrossUserIsolation,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: fact_ids(&["fact-bob-secret"]),
            candidates: Vec::new(),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: Some(true),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
    ]
}

fn write_memory_budget_report(path: &Path, report: &MemoryRetrievalEvalReport) -> TestResult {
    let json = serde_json::to_vec_pretty(report)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn run_memory_budget_gate(report_path: &Path, previous_path: Option<&Path>) -> TestResult<Output> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(cargo);
    command
        .current_dir(workspace_root())
        .args([
            "run",
            "-p",
            "xtask",
            "--quiet",
            "--",
            "check-eval-budgets",
            "--suite",
            "memory_retrieval",
            "--max-regression-pct",
            "5",
            "--memory-eval-report",
        ])
        .arg(report_path);
    if let Some(previous_path) = previous_path {
        command.env("MOA_EVAL_PREVIOUS_MEMORY_REPORT", previous_path);
    } else {
        command.env_remove("MOA_EVAL_PREVIOUS_MEMORY_REPORT");
    }
    Ok(command.output()?)
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("moa-eval manifest lives under crates/moa-eval")
        .to_path_buf()
}

fn command_output_text(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn realistic_corpus() -> (
    CorpusManifest,
    Vec<LedgerFact>,
    Vec<SyntheticSession>,
    Vec<Probe>,
) {
    let alice_session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08c01");
    let bob_session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08c02");
    let payments_tenant = storage_partition("tenant-payments");
    let alice = user("user-alice");
    let bob = user("user-bob");

    let manifest = CorpusManifest {
        version: CORPUS_SCHEMA_VERSION,
        corpus_id: "memory-eval-pr-realistic".to_string(),
        profile: CorpusProfile::Pr,
        description:
            "PR memory retrieval corpus with updates, isolation, temporal, and PII probes."
                .to_string(),
        seeds: vec![1, 2, 3],
        transcript_style: TranscriptStyle::Marked,
    };

    let facts = vec![
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-deploy-target-v1".to_string(),
            valid_from: utc("2026-01-01T00:00:00Z"),
            valid_to: Some(utc("2026-01-08T00:00:00Z")),
            subject: "payments-api".to_string(),
            predicate: "deploy_target".to_string(),
            object: "staging".to_string(),
            answer: "As of January 5, payments-api deployed to staging.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 1,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-deploy-target-v2".to_string(),
            valid_from: utc("2026-01-08T00:00:00Z"),
            valid_to: None,
            subject: "payments-api".to_string(),
            predicate: "deploy_target".to_string(),
            object: "production-canary".to_string(),
            answer: "The latest payments-api deploy target is production-canary.".to_string(),
            supersedes: vec!["fact-deploy-target-v1".to_string()],
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 2,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-runbook".to_string(),
            valid_from: utc("2026-01-02T00:00:00Z"),
            valid_to: None,
            subject: "payments-api deploys".to_string(),
            predicate: "require_runbook".to_string(),
            object: "runbook/payments-canary".to_string(),
            answer: "Payments deploys require runbook/payments-canary.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 3,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            scope: ScopeTier::Contact,
            fact_id: "fact-bob-editor".to_string(),
            valid_from: utc("2026-01-03T00:00:00Z"),
            valid_to: None,
            subject: "Bob".to_string(),
            predicate: "preferred_editor".to_string(),
            object: "nvim".to_string(),
            answer: "Bob prefers nvim for config edits.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: bob_session,
            source_turn_seq: 1,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Contact,
            fact_id: "fact-alice-phone".to_string(),
            valid_from: utc("2026-01-04T00:00:00Z"),
            valid_to: None,
            subject: "Alice".to_string(),
            predicate: "contact_phone".to_string(),
            object: "+1-555-0100".to_string(),
            answer: "Alice's contact phone is [PHONE].".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 4,
            pii_class: PiiClass::Pii,
            expected_redacted: true,
        },
    ];

    let sessions = vec![
        SyntheticSession {
            session_id: alice_session,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            turns: vec![
                SyntheticTurn {
                    turn_seq: 1,
                    transcript: "Fact: payments-api deploy target is staging.".to_string(),
                    fact_ids: vec!["fact-deploy-target-v1".to_string()],
                },
                SyntheticTurn {
                    turn_seq: 2,
                    transcript: "Fact: payments-api deploy target is now production-canary."
                        .to_string(),
                    fact_ids: vec!["fact-deploy-target-v2".to_string()],
                },
                SyntheticTurn {
                    turn_seq: 3,
                    transcript: "Fact: payments deploys require runbook/payments-canary."
                        .to_string(),
                    fact_ids: vec!["fact-runbook".to_string()],
                },
                SyntheticTurn {
                    turn_seq: 4,
                    transcript: "Fact: Alice contact phone is +1-555-0100.".to_string(),
                    fact_ids: vec!["fact-alice-phone".to_string()],
                },
            ],
        },
        SyntheticSession {
            session_id: bob_session,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            turns: vec![SyntheticTurn {
                turn_seq: 1,
                transcript: "Fact: Bob prefers nvim for config edits.".to_string(),
                fact_ids: vec!["fact-bob-editor".to_string()],
            }],
        },
    ];

    let probes = vec![
        probe(ProbeSpec {
            probe_id: "probe-point-recall",
            probe_type: ProbeType::PointRecall,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "What is the latest payments-api deploy target?",
            answer: "The latest payments-api deploy target is production-canary.",
            expected_fact_ids: &["fact-deploy-target-v2"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-latest-value",
            probe_type: ProbeType::LatestValueAfterUpdate,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "After the deploy target update, where should payments-api deploy?",
            answer: "The latest payments-api deploy target is production-canary.",
            expected_fact_ids: &["fact-deploy-target-v2"],
            blocked_fact_ids: &["fact-deploy-target-v1"],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-abstention",
            probe_type: ProbeType::Abstention,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "What is the database password for payments production?",
            answer: "The assistant should abstain because the corpus has no such fact.",
            expected_fact_ids: &[],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-cross-user-isolation",
            probe_type: ProbeType::CrossUserIsolation,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "What editor does Bob prefer?",
            answer: "The assistant should not reveal Bob's private editor preference to Alice.",
            expected_fact_ids: &[],
            blocked_fact_ids: &["fact-bob-editor"],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-tenant-shared",
            probe_type: ProbeType::TenantSharedFact,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "Which runbook is required for payments deploys?",
            answer: "Payments deploys require runbook/payments-canary.",
            expected_fact_ids: &["fact-runbook"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-multi-hop",
            probe_type: ProbeType::MultiHop,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "Where should payments-api deploy and which runbook applies?",
            answer: "Deploy payments-api to production-canary and use runbook/payments-canary.",
            expected_fact_ids: &["fact-deploy-target-v2", "fact-runbook"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-temporal-as-of",
            probe_type: ProbeType::TemporalAsOf,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "Where did payments-api deploy on January 5, 2026?",
            answer: "As of January 5, payments-api deployed to staging.",
            expected_fact_ids: &["fact-deploy-target-v1"],
            blocked_fact_ids: &["fact-deploy-target-v2"],
            as_of: Some(utc("2026-01-05T12:00:00Z")),
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-preference-application",
            probe_type: ProbeType::PreferenceApplication,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "I need to edit a config file; which editor should you use for me?",
            answer: "Use nvim for Bob's config edit.",
            expected_fact_ids: &["fact-bob-editor"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-pii-redaction",
            probe_type: ProbeType::PiiRedaction,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice,
            query: "What is Alice's contact phone?",
            answer: "Alice's contact phone is [PHONE].",
            expected_fact_ids: &["fact-alice-phone"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: true,
        }),
    ];

    (manifest, facts, sessions, probes)
}

struct ProbeSpec<'a> {
    probe_id: &'a str,
    probe_type: ProbeType,
    storage_partition_id: StoragePartitionId,
    user_id: UserId,
    query: &'a str,
    answer: &'a str,
    expected_fact_ids: &'a [&'a str],
    blocked_fact_ids: &'a [&'a str],
    as_of: Option<DateTime<Utc>>,
    expected_redacted: bool,
}

fn probe(spec: ProbeSpec<'_>) -> Probe {
    Probe {
        probe_id: spec.probe_id.to_string(),
        probe_type: spec.probe_type,
        storage_partition_id: spec.storage_partition_id,
        user_id: spec.user_id,
        query: spec.query.to_string(),
        rewrite_query: None,
        expected_rewrite: None,
        query_class: None,
        answer: spec.answer.to_string(),
        expected_fact_ids: spec
            .expected_fact_ids
            .iter()
            .map(|fact_id| (*fact_id).to_string())
            .collect(),
        blocked_fact_ids: spec
            .blocked_fact_ids
            .iter()
            .map(|fact_id| (*fact_id).to_string())
            .collect(),
        as_of: spec.as_of,
        expected_redacted: spec.expected_redacted,
    }
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
