//! Integration coverage for consolidation, ingest, and branch reconciliation.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{Duration, TimeZone, Utc};
use moa_core::{ConfidenceLevel, MemoryScope, MemoryStore, PageType, WikiPage};
use moa_memory::FileMemoryStore;
use moa_session::testing;
use tempfile::tempdir;

fn sample_page(title: &str, page_type: PageType, content: &str) -> WikiPage {
    let timestamp = Utc.with_ymd_and_hms(2026, 4, 9, 16, 45, 0).unwrap();
    WikiPage {
        path: None,
        title: title.to_string(),
        page_type,
        content: content.to_string(),
        created: timestamp,
        updated: timestamp,
        confidence: ConfidenceLevel::High,
        related: Vec::new(),
        sources: Vec::new(),
        tags: vec!["rust".to_string()],
        auto_generated: false,
        last_referenced: timestamp,
        reference_count: 1,
        metadata: std::collections::HashMap::new(),
    }
}

fn workspace_scope(workspace_id: impl Into<moa_core::WorkspaceId>) -> MemoryScope {
    MemoryScope::Workspace {
        workspace_id: workspace_id.into(),
    }
}

async fn searchable_store() -> (tempfile::TempDir, FileMemoryStore) {
    let dir = tempdir().unwrap();
    let (session_store, _database_url, schema_name) =
        testing::create_isolated_test_store().await.unwrap();
    let store = FileMemoryStore::new_with_pool_and_schema(
        dir.path(),
        Arc::new(session_store.pool().clone()),
        Some(&schema_name),
    )
    .await
    .unwrap();
    (dir, store)
}

#[derive(Clone)]
struct SeededRng {
    state: u64,
}

impl SeededRng {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut state = self.state;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.state = state;
        state
    }

    fn next_usize(&mut self, upper_bound: usize) -> usize {
        (self.next_u64() as usize) % upper_bound.max(1)
    }
}

#[tokio::test]
async fn consolidation_normalizes_dates_and_resolves_conflicts() {
    let dir = tempdir().unwrap();
    let store = FileMemoryStore::new(dir.path()).await.unwrap();
    let scope = workspace_scope("ws1");

    let mut architecture = sample_page(
        "Architecture",
        PageType::Topic,
        "# Architecture\n\nAuth service runs on port 3000 today.\n",
    );
    architecture.updated -= Duration::days(40);
    architecture.reference_count = 0;
    store
        .write_page(&scope, &"topics/architecture.md".into(), architecture)
        .await
        .unwrap();

    let mut deployment = sample_page(
        "Deployment",
        PageType::Topic,
        "# Deployment\n\nAuth service runs on port 4000.\n",
    );
    deployment.updated -= Duration::days(10);
    store
        .write_page(&scope, &"topics/deployment.md".into(), deployment)
        .await
        .unwrap();

    let mut removed = sample_page("Retired", PageType::Entity, "# Retired\n\nDeprecated.\n");
    removed
        .metadata
        .insert("entity_exists".to_string(), serde_json::Value::Bool(false));
    store
        .write_page(&scope, &"entities/retired.md".into(), removed)
        .await
        .unwrap();

    let report = store.run_consolidation(&scope).await.unwrap();
    assert!(report.relative_dates_normalized >= 1);
    assert!(report.contradictions_resolved >= 1);
    assert_eq!(report.pages_deleted, 1);

    let deployment = store
        .read_page(&scope, &"topics/deployment.md".into())
        .await
        .unwrap();
    assert!(deployment.content.contains("port 4000"));
    let architecture = store
        .read_page(&scope, &"topics/architecture.md".into())
        .await
        .unwrap();
    assert!(architecture.content.contains("2026-"));
    assert!(
        store
            .read_page(&scope, &"entities/retired.md".into())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn ingest_source_creates_summary_and_updates_related_pages() {
    let dir = tempdir().unwrap();
    let store = FileMemoryStore::new(dir.path()).await.unwrap();
    let scope = workspace_scope("ws1");

    let report = store
        .ingest_source(
            &scope,
            "RFC 0042 Auth Redesign",
            r"
## Entities
- Auth Service

## Topics
- Token Rotation

## Decisions
- Adopt single-use refresh tokens

## Contradictions
- Existing deployment docs still mention multi-use refresh tokens.
",
        )
        .await
        .unwrap();

    assert_eq!(
        report.source_path.as_str(),
        "sources/rfc-0042-auth-redesign.md"
    );
    assert_eq!(report.contradictions.len(), 1);
    assert!(
        report
            .affected_pages
            .iter()
            .any(|path| path.as_str() == "entities/auth-service.md")
    );
    assert!(
        store
            .read_page(&scope, &"topics/token-rotation.md".into())
            .await
            .unwrap()
            .content
            .contains("Source update")
    );
}

#[tokio::test]
async fn ingest_source_truncates_large_content() {
    let dir = tempdir().unwrap();
    let store = FileMemoryStore::new(dir.path()).await.unwrap();
    let scope = workspace_scope("ws1");
    let source = format!("# Large Source\n\n{}", "A".repeat(200_000));

    let report = store
        .ingest_source(&scope, "Large Source", &source)
        .await
        .unwrap();

    let page = store.read_page(&scope, &report.source_path).await.unwrap();
    assert!(page.content.contains("[Document truncated at 100KB."));
}

#[tokio::test]
async fn branch_reconciliation_merges_conflicting_writes() {
    let dir = tempdir().unwrap();
    let store = FileMemoryStore::new(dir.path()).await.unwrap();
    let scope = workspace_scope("ws1");

    let mut main_page = sample_page(
        "Architecture",
        PageType::Topic,
        "# Architecture\n\nKeep the original deployment command.\n",
    );
    main_page.updated = Utc.with_ymd_and_hms(2026, 4, 9, 17, 0, 0).unwrap();
    store
        .write_page(&scope, &"topics/architecture.md".into(), main_page)
        .await
        .unwrap();

    let mut branch_page = sample_page(
        "Architecture",
        PageType::Topic,
        "# Architecture\n\nAdd the canary rollout checklist.\n",
    );
    branch_page.updated = Utc.with_ymd_and_hms(2026, 4, 9, 16, 30, 0).unwrap();
    store
        .write_page_branched(
            &scope,
            &moa_core::BrainId::new(),
            &"topics/architecture.md".into(),
            branch_page,
        )
        .await
        .unwrap();

    let report = store.reconcile_branches(&scope).await.unwrap();
    assert_eq!(report.conflicts_resolved, 1);

    let merged = store
        .read_page(&scope, &"topics/architecture.md".into())
        .await
        .unwrap();
    assert!(merged.content.contains("original deployment command"));
    assert!(merged.content.contains("canary rollout checklist"));
}

#[tokio::test]
async fn consolidation_decays_confidence_once_and_is_stable_on_repeat_runs() {
    let dir = tempdir().unwrap();
    let store = FileMemoryStore::new(dir.path()).await.unwrap();
    let scope = workspace_scope("ws1");

    let mut page = sample_page(
        "Lonely Topic",
        PageType::Topic,
        "# Lonely Topic\n\nThis page is never referenced.\n",
    );
    page.updated -= Duration::days(45);
    page.last_referenced -= Duration::days(45);
    page.reference_count = 0;
    page.confidence = ConfidenceLevel::High;
    store
        .write_page(&scope, &"topics/lonely-topic.md".into(), page)
        .await
        .unwrap();

    let first = store.run_consolidation(&scope).await.unwrap();
    assert_eq!(first.confidence_decayed, 1);
    let first_page = store
        .read_page(&scope, &"topics/lonely-topic.md".into())
        .await
        .unwrap();
    assert_eq!(first_page.confidence, ConfidenceLevel::Medium);

    let second = store.run_consolidation(&scope).await.unwrap();
    assert_eq!(second.confidence_decayed, 0);
    let second_page = store
        .read_page(&scope, &"topics/lonely-topic.md".into())
        .await
        .unwrap();
    assert_eq!(second_page.confidence, ConfidenceLevel::Medium);
}

#[tokio::test]
async fn repeated_ingest_updates_existing_pages_without_duplicate_links() {
    let dir = tempdir().unwrap();
    let store = FileMemoryStore::new(dir.path()).await.unwrap();
    let scope = workspace_scope("ws1");

    let source = r"
## Entities
- Auth Service

## Topics
- Token Rotation
";

    let first = store
        .ingest_source(&scope, "RFC 0042 Auth Redesign", source)
        .await
        .unwrap();
    let second = store
        .ingest_source(&scope, "RFC 0042 Auth Redesign", source)
        .await
        .unwrap();

    assert_eq!(first.source_path, second.source_path);
    let entity = store
        .read_page(&scope, &"entities/auth-service.md".into())
        .await
        .unwrap();
    assert_eq!(
        entity
            .sources
            .iter()
            .filter(|value| value.as_str() == "sources/rfc-0042-auth-redesign.md")
            .count(),
        1
    );
    assert_eq!(
        entity
            .related
            .iter()
            .filter(|value| value.as_str() == "sources/rfc-0042-auth-redesign.md")
            .count(),
        1
    );
}

#[tokio::test]
async fn reconciliation_merges_multiple_branches_and_cleans_branch_directory() {
    let dir = tempdir().unwrap();
    let store = FileMemoryStore::new(dir.path()).await.unwrap();
    let scope = workspace_scope("ws1");

    store
        .write_page(
            &scope,
            &"topics/architecture.md".into(),
            sample_page(
                "Architecture",
                PageType::Topic,
                "# Architecture\n\nBase deployment flow.\n",
            ),
        )
        .await
        .unwrap();

    store
        .write_page_branched(
            &scope,
            &moa_core::BrainId::new(),
            &"topics/architecture.md".into(),
            sample_page(
                "Architecture",
                PageType::Topic,
                "# Architecture\n\nAdd canary validation.\n",
            ),
        )
        .await
        .unwrap();
    store
        .write_page_branched(
            &scope,
            &moa_core::BrainId::new(),
            &"entities/auth-service.md".into(),
            sample_page(
                "Auth Service",
                PageType::Entity,
                "# Auth Service\n\nDocuments the refresh endpoint.\n",
            ),
        )
        .await
        .unwrap();

    let report = store.reconcile_branches(&scope).await.unwrap();

    assert_eq!(report.branches_reconciled, 2);
    assert_eq!(report.pages_created, 1);
    let merged = store
        .read_page(&scope, &"topics/architecture.md".into())
        .await
        .unwrap();
    assert!(merged.content.contains("Base deployment flow"));
    assert!(merged.content.contains("canary validation"));
    assert!(
        store
            .read_page(&scope, &"entities/auth-service.md".into())
            .await
            .is_ok()
    );

    let branches_root = dir
        .path()
        .join("workspaces")
        .join("ws1")
        .join("memory")
        .join(".branches");
    assert!(tokio::fs::try_exists(&branches_root).await.unwrap());
    let mut entries = tokio::fs::read_dir(branches_root).await.unwrap();
    assert!(entries.next_entry().await.unwrap().is_none());
}

#[tokio::test]
async fn maintenance_operations_append_log_and_keep_results_searchable() {
    let (_dir, store) = searchable_store().await;
    let scope = workspace_scope("ws1");

    store
        .ingest_source(
            &scope,
            "RFC 0042 Auth Redesign",
            r"
## Entities
- Auth Service

## Topics
- Token Rotation
",
        )
        .await
        .unwrap();
    store.run_consolidation(&scope).await.unwrap();
    store
        .write_page_branched(
            &scope,
            &moa_core::BrainId::new(),
            &"topics/token-rotation.md".into(),
            sample_page(
                "Token Rotation",
                PageType::Topic,
                "# Token Rotation\n\nAdds canary refresh verification.\n",
            ),
        )
        .await
        .unwrap();
    store.reconcile_branches(&scope).await.unwrap();

    let log = store.load_scope_log(&scope).await.unwrap();
    assert!(log.contains("ingest"));
    assert!(log.contains("consolidation"));
    assert!(log.contains("reconcile"));

    let results = moa_core::MemoryStore::search(&store, "canary verification", &scope, 5)
        .await
        .unwrap();
    assert!(!results.is_empty());
    assert!(
        results
            .iter()
            .any(|result| result.path.as_str() == "topics/token-rotation.md")
    );

    let index = moa_core::MemoryStore::get_index(&store, &scope)
        .await
        .unwrap();
    assert!(index.lines().count() <= 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual stress test"]
async fn manual_stress_ingest_reconcile_and_consolidate_preserves_invariants() {
    let (dir, store) = searchable_store().await;
    let scope = workspace_scope("stress-ws");

    for index in 0..20 {
        store
            .write_page(
                &scope,
                &format!("topics/base-{index}.md").into(),
                sample_page(
                    &format!("Base {index}"),
                    PageType::Topic,
                    &format!(
                        "# Base {index}\n\nService {index} runs on port {} today.\n",
                        3000 + (index % 3)
                    ),
                ),
            )
            .await
            .unwrap();
    }

    for round in 0..12 {
        let source = format!(
            "## Entities\n- Auth Service {round}\n- Token Store {round}\n\n## Topics\n- Rotation {round}\n- Observability {round}\n\n## Decisions\n- Adopt guardrail {round}\n"
        );
        store
            .ingest_source(&scope, &format!("RFC {round}"), &source)
            .await
            .unwrap();

        for branch_index in 0..4 {
            let page_index = (round + branch_index) % 10;
            store
                .write_page_branched(
                    &scope,
                    &moa_core::BrainId::new(),
                    &format!("topics/base-{page_index}.md").into(),
                    sample_page(
                        &format!("Base {page_index}"),
                        PageType::Topic,
                        &format!(
                            "# Base {page_index}\n\nRound {round} branch {branch_index} update.\n"
                        ),
                    ),
                )
                .await
                .unwrap();
        }

        store.reconcile_branches(&scope).await.unwrap();
        store.run_consolidation(&scope).await.unwrap();
        moa_core::MemoryStore::rebuild_search_index(&store, &scope)
            .await
            .unwrap();
    }

    let index = moa_core::MemoryStore::get_index(&store, &scope)
        .await
        .unwrap();
    assert!(index.lines().count() <= 200);

    let results = moa_core::MemoryStore::search(&store, "guardrail", &scope, 20)
        .await
        .unwrap();
    assert!(!results.is_empty());

    let log = store.load_scope_log(&scope).await.unwrap();
    assert!(log.matches("ingest").count() >= 12);
    assert!(log.matches("consolidation").count() >= 12);
    assert!(log.matches("reconcile").count() >= 12);

    let branches_root = dir
        .path()
        .join("workspaces")
        .join("stress-ws")
        .join("memory")
        .join(".branches");
    let mut entries = tokio::fs::read_dir(branches_root).await.unwrap();
    assert!(entries.next_entry().await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual seeded fuzz test"]
async fn manual_seeded_memory_fuzz_preserves_core_invariants() {
    for seed in [7_u64, 19, 41] {
        let (dir, store) = searchable_store().await;
        let scope = workspace_scope(format!("fuzz-ws-{seed}"));
        let mut rng = SeededRng::new(seed);
        let mut tracked_terms = Vec::new();

        for index in 0..10 {
            let term = format!("seed{seed}-base{index}");
            tracked_terms.push(term.clone());
            store
                .write_page(
                    &scope,
                    &format!("topics/base-{index}.md").into(),
                    sample_page(
                        &format!("Base {index}"),
                        PageType::Topic,
                        &format!("# Base {index}\n\nKeyword {term}.\n"),
                    ),
                )
                .await
                .unwrap();
        }

        for round in 0..30 {
            match rng.next_usize(5) {
                0 => {
                    let page_index = rng.next_usize(10);
                    let term = format!("seed{seed}-direct-{round}-{page_index}");
                    tracked_terms.push(term.clone());
                    let path = format!("topics/base-{page_index}.md");
                    let mut page = store.read_page(&scope, &path.clone().into()).await.unwrap();
                    page.content.push_str(&format!("\nDirect update {term}.\n"));
                    store.write_page(&scope, &path.into(), page).await.unwrap();
                }
                1 => {
                    let page_index = rng.next_usize(10);
                    let term = format!("seed{seed}-branch-{round}-{page_index}");
                    tracked_terms.push(term.clone());
                    store
                        .write_page_branched(
                            &scope,
                            &moa_core::BrainId::new(),
                            &format!("topics/base-{page_index}.md").into(),
                            sample_page(
                                &format!("Base {page_index}"),
                                PageType::Topic,
                                &format!("# Base {page_index}\n\nBranch update {term}.\n"),
                            ),
                        )
                        .await
                        .unwrap();
                }
                2 => {
                    let source_name = format!("Seeded Source {seed}-{round}");
                    let entity = format!("Entity {seed}-{round}");
                    let topic = format!("Topic {seed}-{round}");
                    let decision = format!("Decision {seed}-{round}");
                    tracked_terms.push(format!("seed{seed}-ingest-{round}"));
                    store
                        .ingest_source(
                            &scope,
                            &source_name,
                            &format!(
                                "## Entities\n- {entity}\n\n## Topics\n- {topic}\n\n## Decisions\n- {decision}\n"
                            ),
                        )
                        .await
                        .unwrap();
                }
                3 => {
                    let page_index = rng.next_usize(10);
                    let mut page = store
                        .read_page(&scope, &format!("topics/base-{page_index}.md").into())
                        .await
                        .unwrap();
                    page.content.push_str("\nThis was noted today.\n");
                    page.updated -= Duration::days(35);
                    page.last_referenced -= Duration::days(35);
                    page.reference_count = 0;
                    store
                        .write_page(&scope, &format!("topics/base-{page_index}.md").into(), page)
                        .await
                        .unwrap();
                    store.run_consolidation(&scope).await.unwrap();
                }
                _ => {
                    moa_core::MemoryStore::rebuild_search_index(&store, &scope)
                        .await
                        .unwrap();
                }
            }

            if round % 3 == 0 {
                store.reconcile_branches(&scope).await.unwrap();
            }
            if round % 4 == 0 {
                store.refresh_scope_index(&scope).await.unwrap();
            }

            validate_memory_invariants(
                &store,
                &scope,
                &tracked_terms[..tracked_terms.len().min(8)],
            )
            .await;
        }

        store.reconcile_branches(&scope).await.unwrap();
        store.run_consolidation(&scope).await.unwrap();
        validate_memory_invariants(&store, &scope, &tracked_terms).await;

        let branches_root = dir
            .path()
            .join("workspaces")
            .join(format!("fuzz-ws-{seed}"))
            .join("memory")
            .join(".branches");
        let mut entries = tokio::fs::read_dir(branches_root).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }
}

async fn validate_memory_invariants(
    store: &FileMemoryStore,
    scope: &MemoryScope,
    tracked_terms: &[String],
) {
    let pages = moa_core::MemoryStore::list_pages(store, scope, None)
        .await
        .unwrap();
    assert!(!pages.is_empty());

    for summary in &pages {
        let page = store.read_page(scope, &summary.path).await.unwrap();
        let related_len = page.related.len();
        let sources_len = page.sources.len();
        let related_set = page.related.iter().collect::<HashSet<_>>();
        let sources_set = page.sources.iter().collect::<HashSet<_>>();
        assert_eq!(
            related_len,
            related_set.len(),
            "duplicate related links in {}",
            summary.path
        );
        assert_eq!(
            sources_len,
            sources_set.len(),
            "duplicate sources in {}",
            summary.path
        );
    }

    let index = moa_core::MemoryStore::get_index(store, scope)
        .await
        .unwrap();
    assert!(index.lines().count() <= 200, "index exceeded line budget");

    for term in tracked_terms.iter().take(6) {
        let before = moa_core::MemoryStore::search(store, term, scope, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|result| result.path.as_str().to_string())
            .collect::<Vec<_>>();
        moa_core::MemoryStore::rebuild_search_index(store, scope)
            .await
            .unwrap();
        let after = moa_core::MemoryStore::search(store, term, scope, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|result| result.path.as_str().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            before, after,
            "search/rebuild parity failed for term {term}"
        );
    }
}
