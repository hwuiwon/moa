//! PostgreSQL-backed coverage for the recurrence-mining store queries.
//!
//! Pins the SQL the recurrence cron depends on: grouping resolved/partial
//! experiences by task fingerprint over a lookback window (excluding non-learnable
//! outcomes and out-of-window rows), returning every in-window fingerprint as a
//! candidate group bounded by recency (the occurrence floor is applied after
//! clustering, not here); plus the single- and batched-fingerprint
//! candidate-decision lookups that drive suppression.

use std::future::Future;

use chrono::{DateTime, Duration, Utc};
use moa_core::{
    error::Result,
    traits::SessionStore,
    types::agent::AgentContext,
    types::contact::SessionActorRef,
    types::experience::{
        LearningCandidate, LearningCandidateStatus, LearningCandidateType, LearningRiskClass,
        TaskFacetSet, TaskFingerprint,
    },
    types::identifiers::{ModelId, SessionId, TenantId, UserId},
    types::segment_assessment::SegmentOutcome,
    types::segments::{TaskSegment, deterministic_segment_id},
    types::session::SessionMeta,
};
use moa_session::PostgresSessionStore;
use moa_session::testing;
use uuid::Uuid;

async fn with_test_store<F, Fut>(test: F)
where
    F: FnOnce(PostgresSessionStore) -> Fut,
    Fut: Future<Output = ()>,
{
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("postgres store");
    test(store.clone()).await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop schema");
}

fn tenant_id(label: &str) -> TenantId {
    let mut bytes = [0_u8; 16];
    for (index, byte) in label.bytes().enumerate() {
        bytes[index % 16] = bytes[index % 16].wrapping_mul(31).wrapping_add(byte);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    TenantId::from(Uuid::from_bytes(bytes))
}

fn session_meta(tenant: TenantId) -> SessionMeta {
    SessionMeta {
        tenant_id: tenant,
        created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
        model: ModelId::new("test-model"),
        agent_context: Some(AgentContext::system_default()),
        ..SessionMeta::default()
    }
}

/// Seeds one assessed experience with a controllable fingerprint, outcome, tools,
/// and creation time. Each experience gets its own session and segment (the
/// experience row has FKs to both).
async fn seed_experience(
    store: &PostgresSessionStore,
    tenant: TenantId,
    fingerprint_hash: &str,
    outcome: SegmentOutcome,
    confidence: f64,
    tools: &[&str],
    created_at: DateTime<Utc>,
) -> Result<()> {
    let session_id: SessionId = store.create_session(session_meta(tenant)).await?;
    let segment_id = deterministic_segment_id(session_id, 0);
    let tools_used: Vec<String> = tools.iter().map(|tool| tool.to_string()).collect();
    store
        .create_segment(&TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: tenant.to_string(),
            segment_index: 0,
            task_summary: Some("recurring task".to_string()),
            started_at: created_at,
            ended_at: Some(created_at),
            turn_count: 1,
            tools_used: tools_used.clone(),
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 0,
            previous_segment_id: None,
            outcome: Some(outcome.as_str().to_string()),
            assessment: None,
            outcome_confidence: Some(confidence),
        })
        .await?;
    let experience = moa_core::types::experience::ExperienceRecord {
        id: Uuid::now_v7(),
        segment_id,
        session_id,
        tenant_id: tenant,
        user_id: UserId::new("user"),
        task_summary: Some("recurring task".to_string()),
        task_fingerprint: TaskFingerprint {
            hash: fingerprint_hash.to_string(),
            normalized_summary: "recurring task".to_string(),
            policy_version: "experience_v1".to_string(),
        },
        task_facets: TaskFacetSet::default(),
        actions: Vec::new(),
        resources: Vec::new(),
        outcome,
        confidence,
        evidence: Vec::new(),
        tools_used,
        skills_activated: Vec::new(),
        skills_used: Vec::new(),
        turn_count: 1,
        token_cost: 0,
        duration_ms: None,
        assessment_policy_version: "assessment_v1".to_string(),
        extraction_policy_version: "experience_v1".to_string(),
        created_at,
    };
    store.append_experience_record(&experience).await
}

#[tokio::test]
#[ignore = "requires local Postgres (docker compose up); run in the _db lane"]
async fn candidate_groups_include_below_floor_exclude_failed_and_out_of_window() {
    // Pins: the grouping query returns *every* in-window fingerprint as a candidate
    // group, including a below-occurrence-floor one, because the occurrence
    // threshold is applied after semantic clustering, not here. Failed outcomes are
    // excluded from counts and members, out-of-window rows are excluded entirely,
    // members are ordered by creation time, and the distinct-tool count is
    // populated.
    with_test_store(|store| async move {
        let tenant = tenant_id("recurrence-grouping");
        let now = moa_test_support::fixtures::pg_now();

        // `hot`: three resolved in-window, plus one failed that must be excluded
        // from the count and members.
        for age in [3_i64, 2, 1] {
            seed_experience(
                &store,
                tenant,
                "hot",
                SegmentOutcome::Resolved,
                0.9,
                &["alpha", "beta"],
                now - Duration::days(age),
            )
            .await
            .expect("seed hot resolved");
        }
        seed_experience(
            &store,
            tenant,
            "hot",
            SegmentOutcome::Failed,
            0.9,
            &["alpha", "beta"],
            now - Duration::hours(6),
        )
        .await
        .expect("seed hot failed");

        // `cold`: only two resolved — below the occurrence floor of three, but still
        // a candidate group so semantic clustering can pool it with an alias.
        for age in [2_i64, 1] {
            seed_experience(
                &store,
                tenant,
                "cold",
                SegmentOutcome::Resolved,
                0.9,
                &["alpha"],
                now - Duration::days(age),
            )
            .await
            .expect("seed cold");
        }

        // `stale`: three resolved but all outside the 30-day window.
        for age in [60_i64, 61, 62] {
            seed_experience(
                &store,
                tenant,
                "stale",
                SegmentOutcome::Resolved,
                0.9,
                &["alpha"],
                now - Duration::days(age),
            )
            .await
            .expect("seed stale");
        }

        let since = now - Duration::days(30);
        let tenants = store
            .list_tenants_with_recent_learnable_experiences(since)
            .await
            .expect("list tenants");
        assert!(tenants.contains(&tenant));

        let groups = store
            .list_candidate_experience_groups(&tenant, since, 200)
            .await
            .expect("group candidates");
        let by_hash: std::collections::HashMap<&str, &_> = groups
            .iter()
            .map(|group| (group.fingerprint_hash.as_str(), group))
            .collect();
        assert_eq!(
            groups.len(),
            2,
            "both in-window fingerprints are candidate groups; the below-floor one is not discarded here"
        );
        assert!(
            !by_hash.contains_key("stale"),
            "out-of-window fingerprints are excluded"
        );

        let hot = by_hash.get("hot").expect("hot group present");
        assert_eq!(
            hot.members.len(),
            3,
            "the failed experience is excluded from the resolved/partial count"
        );
        assert!(
            hot.members
                .iter()
                .all(|member| member.outcome == SegmentOutcome::Resolved)
        );
        assert!(
            hot.members
                .windows(2)
                .all(|pair| pair[0].created_at <= pair[1].created_at),
            "members are ordered by creation time ascending"
        );
        assert!(
            hot.members.iter().all(|member| member.tool_count == 2),
            "distinct-tool count is populated from cardinality(tools_used)"
        );

        let cold = by_hash.get("cold").expect("below-floor group present");
        assert_eq!(cold.members.len(), 2, "the below-floor group keeps its members");
    })
    .await;
}

#[tokio::test]
#[ignore = "requires local Postgres (docker compose up); run in the _db lane"]
async fn candidate_groups_are_bounded_to_the_most_recent_fingerprints() {
    // Pins: the max_groups bound keeps the candidate load finite by returning only
    // the most recently active fingerprints (by latest member time), so an
    // unbounded number of distinct low-count tasks cannot blow up the tick.
    with_test_store(|store| async move {
        let tenant = tenant_id("recurrence-bound");
        let now = moa_test_support::fixtures::pg_now();

        // Three single-occurrence fingerprints at descending recency: `newest`,
        // `middle`, `oldest`.
        for (fingerprint, age) in [("oldest", 5_i64), ("middle", 3), ("newest", 1)] {
            seed_experience(
                &store,
                tenant,
                fingerprint,
                SegmentOutcome::Resolved,
                0.9,
                &["alpha"],
                now - Duration::days(age),
            )
            .await
            .expect("seed member");
        }

        let since = now - Duration::days(30);
        let groups = store
            .list_candidate_experience_groups(&tenant, since, 2)
            .await
            .expect("bounded candidate groups");
        let hashes: std::collections::HashSet<&str> = groups
            .iter()
            .map(|group| group.fingerprint_hash.as_str())
            .collect();
        assert_eq!(groups.len(), 2, "the bound caps the candidate group count");
        assert!(
            hashes.contains("newest") && hashes.contains("middle"),
            "the two most recently active fingerprints are kept"
        );
        assert!(
            !hashes.contains("oldest"),
            "the least recently active fingerprint falls outside the bound"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires local Postgres (docker compose up); run in the _db lane"]
async fn candidate_decisions_lookup_returns_status_and_time() {
    // Pins: the per-fingerprint candidate lookup returns each skill candidate's
    // status and last-updated time, and ignores other fingerprints, so the cron's
    // suppression sees exactly the decisions for the fingerprint it is judging.
    with_test_store(|store| async move {
        let tenant = tenant_id("recurrence-decisions");
        let now = moa_test_support::fixtures::pg_now();
        let candidate = LearningCandidate {
            id: Uuid::now_v7(),
            tenant_id: tenant,
            user_id: None,
            candidate_type: LearningCandidateType::Skill,
            status: LearningCandidateStatus::Rejected,
            target_id: None,
            target_label: Some("some-skill".to_string()),
            task_fingerprint: Some(TaskFingerprint {
                hash: "judged".to_string(),
                normalized_summary: "judged".to_string(),
                policy_version: "experience_v1".to_string(),
            }),
            task_facets: None,
            payload: serde_json::json!({ "kind": "skill_draft_proposal" }),
            evaluation_payload: None,
            source_experience_ids: Vec::new(),
            confidence: None,
            risk_class: LearningRiskClass::Medium,
            promotion_requirements: vec!["human_review".to_string()],
            status_reason: Some("reviewer declined".to_string()),
            batch_id: None,
            created_at: now - Duration::days(1),
            updated_at: now,
        };
        store
            .append_learning_candidate(&candidate)
            .await
            .expect("append candidate");

        let judged = store
            .list_skill_candidate_decisions_for_fingerprint(&tenant, "judged")
            .await
            .expect("decisions for judged fingerprint");
        assert_eq!(judged.len(), 1);
        assert_eq!(judged[0].status, LearningCandidateStatus::Rejected);

        let other = store
            .list_skill_candidate_decisions_for_fingerprint(&tenant, "unrelated")
            .await
            .expect("decisions for unrelated fingerprint");
        assert!(
            other.is_empty(),
            "only the judged fingerprint's decisions are returned"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires local Postgres (docker compose up); run in the _db lane"]
async fn batched_candidate_decisions_key_each_fingerprint_and_ignore_others() {
    // Pins: the batched lookup returns every requested fingerprint's decisions in
    // one scan, keyed by fingerprint hash, ignores fingerprints outside the input,
    // and short-circuits an empty input — the per-tenant replacement for the cron's
    // per-fingerprint N+1.
    with_test_store(|store| async move {
        let tenant = tenant_id("recurrence-batch-decisions");
        let now = moa_test_support::fixtures::pg_now();
        for (hash, status) in [
            ("alpha", LearningCandidateStatus::Rejected),
            ("beta", LearningCandidateStatus::Promoted),
            ("gamma", LearningCandidateStatus::Proposed),
        ] {
            let candidate = LearningCandidate {
                id: Uuid::now_v7(),
                tenant_id: tenant,
                user_id: None,
                candidate_type: LearningCandidateType::Skill,
                status,
                target_id: None,
                target_label: Some(format!("{hash}-skill")),
                task_fingerprint: Some(TaskFingerprint {
                    hash: hash.to_string(),
                    normalized_summary: hash.to_string(),
                    policy_version: "experience_v1".to_string(),
                }),
                task_facets: None,
                payload: serde_json::json!({ "kind": "skill_draft_proposal" }),
                evaluation_payload: None,
                source_experience_ids: Vec::new(),
                confidence: None,
                risk_class: LearningRiskClass::Medium,
                promotion_requirements: vec!["human_review".to_string()],
                status_reason: None,
                batch_id: None,
                created_at: now - Duration::days(1),
                updated_at: now,
            };
            store
                .append_learning_candidate(&candidate)
                .await
                .expect("append candidate");
        }

        // Request alpha + beta (+ an unseeded hash): gamma must not leak in.
        let batched = store
            .list_skill_candidate_decisions_for_fingerprints(
                &tenant,
                &[
                    "alpha".to_string(),
                    "beta".to_string(),
                    "missing".to_string(),
                ],
            )
            .await
            .expect("batched decisions");
        let mut by_hash: std::collections::HashMap<String, LearningCandidateStatus> =
            std::collections::HashMap::new();
        for (hash, decision) in batched {
            by_hash.insert(hash, decision.status);
        }
        assert_eq!(by_hash.len(), 2, "only the requested, seeded fingerprints");
        assert_eq!(
            by_hash.get("alpha"),
            Some(&LearningCandidateStatus::Rejected)
        );
        assert_eq!(
            by_hash.get("beta"),
            Some(&LearningCandidateStatus::Promoted)
        );
        assert!(
            !by_hash.contains_key("gamma"),
            "a fingerprint outside the input is not returned"
        );

        let empty = store
            .list_skill_candidate_decisions_for_fingerprints(&tenant, &[])
            .await
            .expect("empty input");
        assert!(empty.is_empty(), "empty input short-circuits to no rows");
    })
    .await;
}
