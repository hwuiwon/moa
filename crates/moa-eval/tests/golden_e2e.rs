//! Golden end-to-end validation for the graph-primary memory stack.

use std::{
    collections::HashMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_brain::{
    planning::{NerExtractor, PlanningCtx, QueryPlanner, QueryRetrievalCtx, retrieve_for_query},
    retrieval::{CachedHybridRetriever, HybridRetriever, RetrievalHit},
};
use moa_core::{MemoryScope, ScopeContext, ScopedConn, SessionId, UserId, WorkspaceId};
use moa_eval::golden::comparator::{compare_top_k_within_window, dump_traces};
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, PiiClass, cypher};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact, FastPathCtx,
    FastRememberRequest, IngestCtx, IngestError, SessionTurn, fast_remember,
    ingest_turn_direct_with_ctx,
};
use moa_memory_pii::{PiiClassifier, PiiError, PiiResult, PiiSpan};
use moa_memory_vector::{Embedder, Error as VectorError, PgvectorStore, VECTOR_DIMENSION};
use moa_session::testing;
use serde::Deserialize;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const FIXTURE_SUBDIR: &str = "golden_100";
const QUERY_FILE: &str = "golden_queries.json";
const EXPECTED_FIXTURE_COUNT: usize = 100;
const RANK_WINDOW: usize = 2;
const SCORE_EPS: f64 = 0.02;
const SUPERSEDED_ALIASES: &[&str] = &[
    "fact-01", "fact-11", "fact-21", "fact-31", "fact-41", "fact-51", "fact-61", "fact-71",
    "fact-81", "fact-91",
];

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, Deserialize)]
struct GoldenFixture {
    uid_seed: String,
    label: NodeLabel,
    name: String,
    summary: String,
    entity_uids: Vec<String>,
    expected_embedding_seed: u32,
    valid_from: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
struct GoldenQueries {
    queries: Vec<GoldenQuery>,
    cross_queries: Vec<String>,
    historical_queries: Vec<HistoricalQuery>,
}

#[derive(Debug, Clone, Deserialize)]
struct GoldenQuery {
    query: String,
    expected_top_5_uids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct HistoricalQuery {
    query: String,
    as_of: DateTime<Utc>,
    expected_pre_supersession: String,
}

#[derive(Debug, Clone)]
struct GoldenEmbedder;

#[async_trait]
impl Embedder for GoldenEmbedder {
    fn model_name(&self) -> &'static str {
        "golden-mock-embedder"
    }

    fn model_version(&self) -> i32 {
        29
    }

    fn dimension(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, VectorError> {
        Ok(texts.iter().map(|text| golden_vector(text)).collect())
    }
}

#[derive(Debug, Clone)]
struct NoPiiClassifier;

#[async_trait]
impl PiiClassifier for NoPiiClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, PiiError> {
        Ok(PiiResult {
            class: PiiClass::None,
            spans: Vec::<PiiSpan>::new(),
            model_version: "golden-no-pii".to_string(),
            abstained: false,
        })
    }
}

#[derive(Debug, Clone)]
struct InsertOnlyDetector;

#[async_trait]
impl ContradictionDetector for InsertOnlyDetector {
    async fn check_one_fast(
        &self,
        _fact_text: &str,
        _embedding: &[f32],
        _label: NodeLabel,
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

struct GoldenStack {
    pool: PgPool,
    database_url: String,
    schema_name: String,
    workspace_uuid: Uuid,
    user_uuid: Uuid,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    scope: ScopeContext,
    graph: Arc<dyn GraphStore>,
    vector: Arc<PgvectorStore>,
    embedder: Arc<GoldenEmbedder>,
    pii: Arc<NoPiiClassifier>,
    detector: Arc<InsertOnlyDetector>,
}

impl GoldenStack {
    async fn up() -> TestResult<Self> {
        let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .map_err(box_error)?;
        let pool = session_store.pool().clone();
        let workspace_uuid = Uuid::now_v7();
        let user_uuid = Uuid::now_v7();
        let workspace_id = WorkspaceId::new(workspace_uuid.to_string());
        let scope = ScopeContext::workspace(workspace_id.clone());
        let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
        let graph = Arc::new(
            AgeGraphStore::scoped_for_app_role(pool.clone(), scope.clone())
                .with_vector_store(vector.clone()),
        );

        Ok(Self {
            pool,
            database_url,
            schema_name,
            workspace_uuid,
            user_uuid,
            session_id: SessionId::new(),
            workspace_id,
            scope,
            graph,
            vector,
            embedder: Arc::new(GoldenEmbedder),
            pii: Arc::new(NoPiiClassifier),
            detector: Arc::new(InsertOnlyDetector),
        })
    }

    fn ingest_ctx(&self) -> IngestCtx {
        IngestCtx::new(
            self.pool.clone(),
            self.graph.clone(),
            self.vector.clone(),
            self.embedder.clone(),
            self.pii.clone(),
            self.detector.clone(),
        )
    }

    fn fast_ctx(&self) -> FastPathCtx {
        FastPathCtx::new(
            self.pool.clone(),
            self.scope.clone(),
            self.graph.clone(),
            self.vector.clone(),
            self.embedder.clone(),
            self.pii.clone(),
            self.detector.clone(),
        )
        .with_assume_app_role(true)
    }

    fn memory_scope(&self) -> MemoryScope {
        MemoryScope::Workspace {
            workspace_id: self.workspace_id.clone(),
        }
    }

    async fn cleanup(self) -> TestResult {
        testing::cleanup_test_schema(&self.database_url, &self.schema_name)
            .await
            .map_err(box_error)
    }
}

#[test]
fn golden_fixture_count() {
    let fixtures = load_fixtures().expect("load golden fixtures");
    assert_eq!(fixtures.len(), EXPECTED_FIXTURE_COUNT);
    for (index, fixture) in fixtures.iter().enumerate() {
        let expected_seed = u32::try_from(index + 1).expect("fixture index fits u32");
        assert_eq!(fixture.expected_embedding_seed, expected_seed);
        assert_eq!(fixture.label, NodeLabel::Fact);
        assert_eq!(fixture.name, fixture.summary);
        assert_eq!(fixture.entity_uids.len(), 2);
        assert!(
            fixture
                .summary
                .starts_with(&format!("fact{:02}", expected_seed)),
            "{}",
            fixture.summary
        );
    }
}

#[tokio::test]
async fn golden_100_e2e() -> TestResult {
    let _guard = TEST_LOCK.lock().await;
    let stack = GoldenStack::up().await?;
    let result = run_golden_100_e2e(&stack).await;
    let cleanup = stack.cleanup().await;
    result?;
    cleanup
}

async fn run_golden_100_e2e(stack: &GoldenStack) -> TestResult {
    let fixtures = load_fixtures()?;
    let queries = load_queries()?;
    let mut uid_by_alias = HashMap::<String, Uuid>::new();
    let mut summary_by_alias = HashMap::<String, String>::new();
    let ctx = stack.ingest_ctx();

    for (index, fixture) in fixtures.iter().enumerate() {
        let turn_seq = u64::try_from(index + 1)?;
        let report =
            ingest_turn_direct_with_ctx(ctx.clone(), session_turn(stack, fixture, turn_seq))
                .await
                .map_err(|error| box_message(format!("{error:?}")))?;
        assert_eq!(report.inserted, 1, "fixture {}", fixture.uid_seed);
        assert_eq!(report.superseded, 0, "fixture {}", fixture.uid_seed);
        assert_eq!(report.skipped, 0, "fixture {}", fixture.uid_seed);
        assert_eq!(report.failed, 0, "fixture {}", fixture.uid_seed);

        let uid = uid_for_turn_seq(&stack.pool, stack.workspace_uuid, turn_seq).await?;
        let alias = fact_alias(&fixture.uid_seed);
        uid_by_alias.insert(alias.clone(), uid);
        summary_by_alias.insert(alias, fixture.summary.clone());
    }

    wait_for_dlq_empty(&stack.pool, stack.workspace_uuid, Duration::from_secs(60)).await?;
    assert_eq!(node_count(&stack.pool, stack.workspace_uuid).await?, 100);
    assert_eq!(
        embedding_count(&stack.pool, stack.workspace_uuid).await?,
        100
    );
    assert_eq!(
        changelog_count(&stack.pool, stack.workspace_uuid).await?,
        100
    );

    let fast_ctx = stack.fast_ctx();
    let mut supersede_pairs = Vec::new();
    for alias in SUPERSEDED_ALIASES {
        let old_uid = *uid_by_alias
            .get(*alias)
            .ok_or_else(|| box_message(format!("missing supersession alias {alias}")))?;
        let old_summary = summary_by_alias
            .get(*alias)
            .ok_or_else(|| box_message(format!("missing supersession summary {alias}")))?;
        uid_by_alias.insert(format!("{alias}-pre"), old_uid);
        let new_uid = fast_remember(
            FastRememberRequest {
                workspace_id: stack.workspace_uuid,
                user_id: None,
                scope: "workspace".to_string(),
                text: superseded_text(old_summary),
                label: NodeLabel::Fact,
                supersedes_specific: Some(old_uid),
                actor_id: stack.user_uuid,
                actor_kind: "user".to_string(),
            },
            &fast_ctx,
        )
        .await
        .map_err(box_error)?;
        uid_by_alias.insert((*alias).to_string(), new_uid);
        supersede_pairs.push((old_uid, new_uid));
    }

    assert_eq!(
        invalidated_count(&stack.pool, stack.workspace_uuid).await?,
        10
    );
    assert_eq!(
        supersedes_edge_count(&stack.pool, stack.workspace_uuid, &supersede_pairs).await?,
        10
    );

    let retrieval = RetrievalHarness::new(stack, stack.memory_scope());
    for query in &queries.queries {
        let hits = retrieval.retrieve(&query.query).await?;
        let expected = expected_uids(&uid_by_alias, &query.expected_top_5_uids)?;
        if let Err(error) = compare_top_k_within_window(&hits, &expected, RANK_WINDOW, SCORE_EPS) {
            panic!(
                "golden query failed: {}\nexpected aliases: {:?}\n{}",
                query.query, query.expected_top_5_uids, error
            );
        }
    }

    let other_scope = MemoryScope::Workspace {
        workspace_id: WorkspaceId::new(Uuid::now_v7().to_string()),
    };
    let other_retrieval = RetrievalHarness::new(stack, other_scope);
    for query in &queries.cross_queries {
        let hits = other_retrieval.retrieve(query).await?;
        assert!(
            hits.is_empty(),
            "RLS leak for `{query}`:\n{}",
            dump_traces(&hits)
        );
    }

    for historical in &queries.historical_queries {
        let hits = retrieve_historical(
            &stack.pool,
            stack.workspace_uuid,
            &historical.query,
            historical.as_of,
        )
        .await?;
        let expected = *uid_by_alias
            .get(&historical.expected_pre_supersession)
            .ok_or_else(|| {
                box_message(format!(
                    "missing historical alias {}",
                    historical.expected_pre_supersession
                ))
            })?;
        assert_eq!(
            hits.first().copied(),
            Some(expected),
            "historical query `{}` returned {:?}",
            historical.query,
            hits
        );
    }

    Ok(())
}

struct RetrievalHarness {
    planner: QueryPlanner,
    planning: PlanningCtx,
    hybrid: CachedHybridRetriever,
    embedder: Arc<GoldenEmbedder>,
}

impl RetrievalHarness {
    fn new(stack: &GoldenStack, scope: MemoryScope) -> Self {
        let scope_ctx = ScopeContext::from(scope.clone());
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            stack.pool.clone(),
            scope_ctx.clone(),
        ));
        let graph = Arc::new(
            AgeGraphStore::scoped_for_app_role(stack.pool.clone(), scope_ctx)
                .with_vector_store(vector.clone()),
        );
        let hybrid = HybridRetriever::new(stack.pool.clone(), graph.clone(), vector)
            .with_assume_app_role(true);
        let aliases = (1..=EXPECTED_FIXTURE_COUNT)
            .map(|index| format!("fact{index:02}"))
            .collect::<Vec<_>>();

        Self {
            planner: QueryPlanner::with_ner(NerExtractor::with_gazetteer(aliases)),
            planning: PlanningCtx::new(scope, graph),
            hybrid: CachedHybridRetriever::new_for_app_role(Arc::new(hybrid), stack.pool.clone()),
            embedder: stack.embedder.clone(),
        }
    }

    async fn retrieve(&self, query: &str) -> TestResult<Vec<RetrievalHit>> {
        let ctx = QueryRetrievalCtx::new(
            &self.planner,
            &self.planning,
            self.embedder.as_ref(),
            &self.hybrid,
            PiiClass::Restricted,
        )
        .with_k_final(5)
        .with_reranker(false);
        retrieve_for_query(query, &ctx).await.map_err(box_error)
    }
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_fixtures() -> TestResult<Vec<GoldenFixture>> {
    let mut paths = fs::read_dir(fixture_root().join(FIXTURE_SUBDIR))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort_by_key(|path| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    paths
        .into_iter()
        .map(|path| {
            let contents = fs::read_to_string(&path)?;
            serde_json::from_str::<GoldenFixture>(&contents).map_err(Into::into)
        })
        .collect()
}

fn load_queries() -> TestResult<GoldenQueries> {
    let contents = fs::read_to_string(fixture_root().join(QUERY_FILE))?;
    serde_json::from_str(&contents).map_err(Into::into)
}

fn session_turn(stack: &GoldenStack, fixture: &GoldenFixture, turn_seq: u64) -> SessionTurn {
    SessionTurn {
        workspace_id: stack.workspace_id.clone(),
        user_id: UserId::new(stack.user_uuid.to_string()),
        session_id: stack.session_id,
        turn_seq,
        transcript: format!("Fact: {}", fixture.summary),
        dominant_pii_class: "none".to_string(),
        finalized_at: fixture.valid_from,
    }
}

fn fact_alias(seed: &str) -> String {
    format!("fact-{seed}")
}

fn superseded_text(summary: &str) -> String {
    format!(
        "{summary} superseded current replacement keeps golden retrieval active after fast path"
    )
}

fn expected_uids(aliases: &HashMap<String, Uuid>, expected: &[String]) -> TestResult<Vec<Uuid>> {
    expected
        .iter()
        .map(|alias| {
            aliases
                .get(alias)
                .copied()
                .ok_or_else(|| box_message(format!("unknown expected alias {alias}")))
        })
        .collect()
}

fn golden_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0_f32; VECTOR_DIMENSION];
    for token in tokens(text) {
        let weight = if token.starts_with("fact") && token.len() >= 6 {
            100.0
        } else {
            1.0
        };
        let index = stable_index(&token);
        vector[index] += weight;
    }

    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| !token.is_empty())
}

fn stable_index(token: &str) -> usize {
    if let Some(index) = fact_index(token) {
        return index;
    }

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in token.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    128 + ((hash as usize) % (VECTOR_DIMENSION - 128))
}

fn fact_index(token: &str) -> Option<usize> {
    let number = token.strip_prefix("fact")?.parse::<usize>().ok()?;
    (1..=EXPECTED_FIXTURE_COUNT)
        .contains(&number)
        .then_some(number - 1)
}

async fn uid_for_turn_seq(pool: &PgPool, workspace_id: Uuid, turn_seq: u64) -> TestResult<Uuid> {
    let mut conn = scoped_conn(pool, workspace_id).await?;
    let uid = sqlx::query_scalar::<_, Uuid>(
        "SELECT uid FROM moa.node_index \
         WHERE workspace_id = $1 \
           AND properties_summary->>'source_turn_seq' = $2 \
           AND valid_to IS NULL",
    )
    .bind(workspace_id.to_string())
    .bind(turn_seq.to_string())
    .fetch_one(conn.as_mut())
    .await?;
    conn.commit().await.map_err(box_error)?;
    Ok(uid)
}

async fn node_count(pool: &PgPool, workspace_id: Uuid) -> TestResult<i64> {
    scoped_count(
        pool,
        workspace_id,
        "SELECT count(*) FROM moa.node_index WHERE workspace_id = $1",
    )
    .await
}

async fn embedding_count(pool: &PgPool, workspace_id: Uuid) -> TestResult<i64> {
    scoped_count(
        pool,
        workspace_id,
        "SELECT count(*) FROM moa.embeddings WHERE workspace_id = $1",
    )
    .await
}

async fn changelog_count(pool: &PgPool, workspace_id: Uuid) -> TestResult<i64> {
    scoped_count(
        pool,
        workspace_id,
        "SELECT count(*) FROM moa.graph_changelog WHERE workspace_id = $1",
    )
    .await
}

async fn invalidated_count(pool: &PgPool, workspace_id: Uuid) -> TestResult<i64> {
    scoped_count(
        pool,
        workspace_id,
        "SELECT count(*) FROM moa.node_index WHERE workspace_id = $1 AND valid_to IS NOT NULL",
    )
    .await
}

async fn scoped_count(pool: &PgPool, workspace_id: Uuid, sql: &str) -> TestResult<i64> {
    let mut conn = scoped_conn(pool, workspace_id).await?;
    let count = sqlx::query_scalar::<_, i64>(sql)
        .bind(workspace_id.to_string())
        .fetch_one(conn.as_mut())
        .await?;
    conn.commit().await.map_err(box_error)?;
    Ok(count)
}

async fn supersedes_edge_count(
    pool: &PgPool,
    workspace_id: Uuid,
    pairs: &[(Uuid, Uuid)],
) -> TestResult<i64> {
    let mut count = 0_i64;
    for (old_uid, new_uid) in pairs {
        let mut conn = scoped_conn(pool, workspace_id).await?;
        let row = cypher::edge::SUPERSEDES_EXISTS
            .execute(&serde_json::json!({
                "old_uid": old_uid.to_string(),
                "new_uid": new_uid.to_string(),
            }))
            .fetch_optional(conn.as_mut())
            .await?;
        conn.commit().await.map_err(box_error)?;
        if row.is_some() {
            count += 1;
        }
    }
    Ok(count)
}

async fn wait_for_dlq_empty(pool: &PgPool, workspace_id: Uuid, timeout: Duration) -> TestResult {
    let started = tokio::time::Instant::now();
    loop {
        let count = scoped_count(
            pool,
            workspace_id,
            "SELECT count(*) FROM moa.ingest_dlq WHERE workspace_id = $1",
        )
        .await?;
        if count == 0 {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(box_message(format!(
                "ingest DLQ still has {count} rows after {timeout:?}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn retrieve_historical(
    pool: &PgPool,
    workspace_id: Uuid,
    query: &str,
    as_of: DateTime<Utc>,
) -> TestResult<Vec<Uuid>> {
    let mut conn = scoped_conn(pool, workspace_id).await?;
    let rows = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT uid
        FROM moa.node_index
        WHERE workspace_id = $1
          AND valid_from <= $2
          AND (valid_to IS NULL OR valid_to > $2)
          AND name_tsv @@ plainto_tsquery('simple', $3)
        ORDER BY ts_rank(name_tsv, plainto_tsquery('simple', $3)) DESC,
                 valid_from DESC,
                 uid
        LIMIT 5
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(as_of)
    .bind(query)
    .fetch_all(conn.as_mut())
    .await?;
    conn.commit().await.map_err(box_error)?;
    Ok(rows)
}

async fn scoped_conn<'a>(pool: &'a PgPool, workspace_id: Uuid) -> TestResult<ScopedConn<'a>> {
    let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id.to_string()));
    let mut conn = ScopedConn::begin(pool, &scope).await.map_err(box_error)?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await?;
    Ok(conn)
}

fn box_error<E>(error: E) -> Box<dyn Error + Send + Sync>
where
    E: Error + Send + Sync + 'static,
{
    Box::new(error)
}

fn box_message(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(std::io::Error::other(message.into()))
}
