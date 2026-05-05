# Step M29 — Validation: 100-fact ingestion + 10-supersession + retrieval golden test

_End-to-end golden test for the entire graph-primary memory stack. 100 facts ingested via M10 slow-path, 10 superseded via M11 fast-path, then a curated query set asserted against an exact expected ranking._

## 1 What this step is about

We need a single test that exercises every layer (extract → classify → embed → contradict → write → retrieve) on a fixed corpus so future PRs that perturb behavior get caught immediately. This is NOT performance (M30 owns that); it's correctness.

## 2 Files to read

- M10/M11/M12/M15/M16/M17 prompts.
- `crates/moa-eval/src/lib.rs`.

## 3 Goal

A `cargo test -p moa-eval --test golden_e2e` that:

1. Boots Postgres+AGE+pgvector+Restate via testcontainers.
2. Loads 100 fact fixtures from `moa-eval/tests/fixtures/golden_100/{01..100}.json` covering 5 entity-pair clusters.
3. Ingests via slow-path. Asserts `node_index` row count = 100, `embeddings` count = 100, `changelog` count = 100.
4. Applies 10 supersessions via fast-path. Asserts `valid_time_end IS NOT NULL` on 10 prior nodes; SUPERSEDES edges = 10.
5. Runs 20 curated queries with expected `(top_n_node_uids)` rankings; asserts ranking match (allow ties via score-windowed comparison).
6. Runs 5 RLS isolation queries (cross-workspace) — expect 0 hits.
7. Runs 3 valid_at-historical queries — assert pre-supersession answers.

## 4 Rules

- **Fixtures committed to repo**; deterministic embeddings via mock embedder for reproducibility (Cohere v4 only at integration tier).
- **Top-K window for fuzzy comparison**: ±2 ranks acceptable when scores within 0.02.
- **Test runs <5min** on CI.
- **Failures dump full retrieval traces** (legs, RRF scores, rerank order) for diff.

## 5 Tasks

### 5a Build fixture set

5 entity pairs × 20 facts each. Pair examples:

- (Service: auth-service, Concern: deployment) — 20 facts about how auth deploys
- (Service: payments, Concern: scaling) — 20 facts about scaling
- (Person: Alice, Decision: stack-choice) — 20 facts on Alice's decisions
- (Place: us-east-1, Incident: outage) — 20 facts on regional incidents
- (Concept: caching, Pattern: read-through) — 20 facts on caching patterns

Each fixture file:

```json
{
  "uid_seed": "01",
  "label": "Fact",
  "name": "Auth deploys via fly.io blue-green Mondays",
  "summary": "...",
  "entity_uids": ["auth-service", "deployment"],
  "expected_embedding_seed": 1,
  "valid_from": "2026-04-01T00:00:00Z"
}
```

### 5b Curate 20 queries

Cross-check by hand:

```json
{
  "query": "When does auth deploy?",
  "expected_top_5_uids": ["fact-01", "fact-04", "fact-12", "fact-08", "fact-19"]
}
```

### 5c Test scaffold

`crates/moa-eval/tests/golden_e2e.rs`:

```rust
#[tokio::test]
async fn golden_100_e2e() {
    let stack = TestStack::up().await;     // testcontainers: pg + restate
    let workspace = WorkspaceId::new();

    // 1. Ingest 100 facts via slow path
    for i in 1..=100u32 {
        let fixture = load_fixture(format!("golden_100/{:02}.json", i));
        ingest_via_slow_path(workspace, &fixture, &stack).await;
    }
    wait_for_dlq_empty(&stack, Duration::from_secs(60)).await;

    // 2. Assert counts
    assert_eq!(stack.node_count(workspace).await, 100);
    assert_eq!(stack.embedding_count(workspace).await, 100);
    assert_eq!(stack.changelog_count(workspace).await, 100);

    // 3. 10 supersessions
    for old_uid in pick_10_uids(&stack, workspace).await {
        fast_supersede_via_api(old_uid, &stack).await;
    }
    assert_eq!(stack.invalidated_count(workspace).await, 10);
    assert_eq!(stack.supersedes_edge_count(workspace).await, 10);

    // 4. 20 curated queries
    for q in load_queries() {
        let hits = retrieve(&q.query, workspace, &stack).await;
        assert_top_k_within_window(&hits, &q.expected_top_5_uids, /*window*/ 2, /*score_eps*/ 0.02);
    }

    // 5. Cross-tenant isolation
    let other = WorkspaceId::new();
    for q in &CROSS_QUERIES {
        let hits = retrieve(q, other, &stack).await;
        assert!(hits.is_empty(), "RLS leak: {}", q);
    }

    // 6. Bi-temporal historical reads
    for h in &HISTORICAL_QUERIES {
        let hits = retrieve_at(&h.query, workspace, h.as_of, &stack).await;
        assert_eq!(hits[0].uid, h.expected_pre_supersession);
    }
}
```

### 5d Ranking comparator

```rust
fn assert_top_k_within_window(hits: &[RetrievalHit], expected: &[Uuid], window: usize, score_eps: f64) {
    for (i, exp_uid) in expected.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(hits.len());
        let found = hits[lo..hi].iter().any(|h| h.uid == *exp_uid)
            || hits.iter().any(|h| h.uid == *exp_uid && (hits[i].score - h.score).abs() < score_eps);
        if !found {
            eprintln!("expected {} at rank ~{}, got: {:?}", exp_uid, i, hits.iter().take(10).collect::<Vec<_>>());
            panic!("ranking mismatch at rank {}", i);
        }
    }
}
```

### 5e Failure dump

```rust
fn dump_traces(hits: &[RetrievalHit]) {
    for h in hits {
        eprintln!("uid={} score={:.4} legs={:?} name={}", h.uid, h.score, h.legs, h.node.name);
    }
}
```

## 6 Deliverables

- `crates/moa-eval/tests/fixtures/golden_100/*.json` (100 files).
- `crates/moa-eval/tests/fixtures/golden_queries.json` (20 queries).
- `crates/moa-eval/tests/golden_e2e.rs` (~600 lines).
- `crates/moa-eval/src/golden/comparator.rs`.

## 7 Acceptance criteria

1. Test green on first run, deterministic across re-runs.
2. Failures emit human-readable diff (expected vs actual top-N).
3. Coverage: every M-step from M07 onward exercised.
4. Meta-test asserts fixture count == 100.

## 8 Tests

```sh
cargo test -p moa-eval --test golden_e2e
cargo test -p moa-eval golden_fixture_count
```

## 9 Cleanup

- Delete any prior small smoke tests that overlap with this golden suite.
- Remove obsolete fixtures from the wiki era.

## 10 What's next

**M30 — Performance gate.**
