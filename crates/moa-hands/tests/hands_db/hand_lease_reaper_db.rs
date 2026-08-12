//! The durable hand-lease reaper and the tenant sandbox policy layer against
//! real Postgres.
//!
//! The offline lane proves the reaper's decision order with in-memory doubles.
//! This lane proves the parts only a database can: that two replicas issuing the
//! same `FOR UPDATE ... SKIP LOCKED` claim take disjoint generations rather than
//! both destroying one sandbox, that finalization is fenced to the claimed
//! generation, that a released generation stays `failed` behind its
//! backoff and never as `active`, and that the tenant policy layer round-trips
//! through the durable store.
//!
//! Shared resource: `moa.hand_leases` and `moa.tenant_sandbox_policy` are
//! schema-qualified tables in the long-running `moa` schema, so these tests
//! cannot be isolated behind a per-test schema. They isolate by seeding
//! per-test `session_id`/`tenant_id` UUIDs and asserting only about rows they
//! seeded. Reaper claims are additionally pinned to those sessions by asserting
//! on the returned generations for this test's sessions rather than on the
//! batch size.

use std::collections::HashSet;
use std::time::Duration;

use moa_core::types::action_policy::CallOrigin;
use moa_core::types::hands::{
    BuiltinPolicyRevision, CpuLimit, DiskLimit, EgressPolicy, LifetimeLimit, MemoryLimit,
    SandboxPolicySnapshot, SandboxProfile, SandboxTier,
};
use moa_core::types::identifiers::{
    ProviderAccountId, SandboxWorkspaceId, SessionId, TenantId, WorkspaceCheckpointId,
    WorkspaceOperationId,
};
use moa_hands::PostgresTenantSandboxPolicyStore;
use moa_hands::core::leases::{
    HAND_LEASE_SESSION_PAGE_SIZE, HandLeaseActivateRequest, HandLeasePolicy,
    HandLeaseProvisionRequest, HandLeaseRenewRequest, HandLeaseStatus, HandLeaseStore,
    HandLeaseWorkspaceAttachment, LeaseHandle, PostgresHandLeaseStore,
};
use moa_hands::core::reaper::{ExpiredHandLeaseClaims, PostgresExpiredHandLeaseClaims};
use moa_hands::{TenantSandboxPolicyStore, deployment_sandbox_policy};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use super::{database_url, seed_session};

async fn pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable")
}

#[tokio::test]
async fn exact_owner_lookup_uses_index_under_large_session_history_db() {
    // Pins: compensation teardown must use the exact owner-key index rather
    // than fetching and filtering every lease accumulated by the session.
    let pool = pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    seed_session(&pool, session_id, tenant_id).await;
    sqlx::query(
        "INSERT INTO moa.hand_leases (\
             session_id, worker_id, tenant_id, provider, tier, handle, status, generation,\
             provisioning_operation_id, provisioning_deadline_at, created_at, updated_at\
         ) SELECT $1, 'unrelated-owner-' || series::TEXT, $2, 'local', 'local', NULL,\
                  'destroyed', 1, gen_random_uuid(), now(), now(), now()\
           FROM generate_series(1, 2000) AS series",
    )
    .bind(session_id)
    .bind(tenant_id)
    .execute(&pool)
    .await
    .expect("seed unrelated lease history");
    let target = format!(
        "execution_compensation:{}:{}",
        Uuid::now_v7(),
        Uuid::now_v7()
    );
    sqlx::query(
        "INSERT INTO moa.hand_leases (\
             session_id, worker_id, tenant_id, provider, tier, handle, status, generation,\
             provisioning_operation_id, provisioning_deadline_at, created_at, updated_at\
         ) VALUES ($1, $2, $3, 'local', 'local', NULL, 'stale', 7,\
                   gen_random_uuid(), now(), now(), now())",
    )
    .bind(session_id)
    .bind(&target)
    .bind(tenant_id)
    .execute(&pool)
    .await
    .expect("seed exact compensation lease");
    sqlx::query("ANALYZE moa.hand_leases")
        .execute(&pool)
        .await
        .expect("refresh hand lease planner statistics");

    let leases = PostgresHandLeaseStore::new(pool.clone())
        .list_live_owner_candidates(tenant_id, session_id, &target)
        .await
        .expect("load exact compensation owner");
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].worker_id, target);
    assert_eq!(leases[0].generation, 7);

    sqlx::query(
        "INSERT INTO moa.hand_leases (\
             session_id, worker_id, tenant_id, provider, tier, handle, status, generation,\
             provisioning_operation_id, provisioning_deadline_at, created_at, updated_at\
         ) SELECT $1, $2, $3, provider, 'local', NULL, 'stale', 8,\
                  gen_random_uuid(), now(), now(), now()\
           FROM (VALUES ('daytona'), ('e2b')) AS replacements(provider)",
    )
    .bind(session_id)
    .bind(&target)
    .bind(tenant_id)
    .execute(&pool)
    .await
    .expect("seed multiple invalid live replacements");
    let store = PostgresHandLeaseStore::new(pool.clone());
    let bounded = store
        .list_live_owner_candidates(tenant_id, session_id, &target)
        .await
        .expect("load bounded live candidates");
    assert_eq!(
        bounded.len(),
        2,
        "the release probe must never materialize more than two candidates"
    );
    assert!(
        store
            .has_live_owner(tenant_id, session_id, &target)
            .await
            .expect("check exact live replacement"),
        "the exact owner replacement probe must observe live ownership"
    );

    let candidate_plan: serde_json::Value = sqlx::query_scalar(
        "EXPLAIN (ANALYZE, FORMAT JSON)\
         SELECT session_id, worker_id, provider FROM moa.hand_leases\
          WHERE tenant_id = $1 AND session_id = $2 AND worker_id = $3\
            AND status <> 'destroyed'\
          ORDER BY provider LIMIT 2",
    )
    .bind(tenant_id)
    .bind(session_id)
    .bind(&target)
    .fetch_one(&pool)
    .await
    .expect("explain exact owner lookup");
    let candidate_plan = candidate_plan.to_string();
    assert!(
        candidate_plan.contains("hand_leases_tenant_live_owner_idx")
            || candidate_plan.contains("idx_hand_leases_tenant_owner")
            || candidate_plan.contains("hand_leases_pkey"),
        "exact owner lookup must use an owner-key index: {candidate_plan}"
    );
    assert!(
        !candidate_plan.contains("Seq Scan"),
        "exact owner lookup must not scan unrelated session history: {candidate_plan}"
    );
    let exists_plan: serde_json::Value = sqlx::query_scalar(
        "EXPLAIN (ANALYZE, FORMAT JSON)\
         SELECT EXISTS (SELECT 1 FROM moa.hand_leases\
          WHERE tenant_id = $1 AND session_id = $2 AND worker_id = $3\
            AND status <> 'destroyed')",
    )
    .bind(tenant_id)
    .bind(session_id)
    .bind(&target)
    .fetch_one(&pool)
    .await
    .expect("explain exact owner existence probe");
    let exists_plan = exists_plan.to_string();
    assert!(
        exists_plan.contains("hand_leases_tenant_live_owner_idx")
            || exists_plan.contains("idx_hand_leases_tenant_owner")
            || exists_plan.contains("hand_leases_pkey"),
        "exact replacement probe must use an owner-key index: {exists_plan}"
    );
    assert!(
        !exists_plan.contains("Seq Scan"),
        "exact replacement probe must not scan unrelated session history: {exists_plan}"
    );

    sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("delete lease history fixture");
    sqlx::query("DELETE FROM public.session_agent_context WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("delete session agent fixture");
    sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("delete session fixture");
}

#[tokio::test]
async fn live_session_paging_is_indexed_bounded_and_replayable_db() {
    // Pins: terminal teardown pages every live hand through the partial owner
    // index while destroyed history remains outside both the result and scan.
    let pool = pool().await;
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    seed_session(&pool, session_id, tenant_id).await;
    sqlx::query(
        "INSERT INTO moa.hand_leases (\
             session_id, worker_id, tenant_id, provider, tier, handle, status, generation,\
             provisioning_operation_id, provisioning_deadline_at, created_at, updated_at\
         ) SELECT $1, 'destroyed-owner-' || lpad(series::TEXT, 4, '0'), $2,\
                  'local', 'local', NULL, 'destroyed', 1, gen_random_uuid(), now(), now(), now()\
           FROM generate_series(1, 2000) AS series",
    )
    .bind(session_id)
    .bind(tenant_id)
    .execute(&pool)
    .await
    .expect("seed destroyed lease history");
    sqlx::query(
        "INSERT INTO moa.hand_leases (\
             session_id, worker_id, tenant_id, provider, tier, handle, status, generation,\
             provisioning_operation_id, provisioning_deadline_at, created_at, updated_at\
         ) SELECT $1, 'live-owner-' || lpad(series::TEXT, 4, '0'), $2,\
                  'local', 'local', NULL, 'stale', 1, gen_random_uuid(), now(), now(), now()\
           FROM generate_series(1, $3) AS series",
    )
    .bind(session_id)
    .bind(tenant_id)
    .bind(i64::try_from(HAND_LEASE_SESSION_PAGE_SIZE + 7).expect("page fixture fits i64"))
    .execute(&pool)
    .await
    .expect("seed live lease pages");
    sqlx::query("ANALYZE moa.hand_leases")
        .execute(&pool)
        .await
        .expect("refresh hand lease planner statistics");

    let store = PostgresHandLeaseStore::new(pool.clone());
    let first = store
        .list_live_session_page(tenant_id, session_id, None)
        .await
        .expect("load first live session page");
    let replay = store
        .list_live_session_page(tenant_id, session_id, None)
        .await
        .expect("replay first live session page");
    assert_eq!(first, replay);
    assert_eq!(first.leases.len(), HAND_LEASE_SESSION_PAGE_SIZE);
    let second = store
        .list_live_session_page(tenant_id, session_id, first.next_cursor.as_ref())
        .await
        .expect("load final live session page");
    assert_eq!(second.leases.len(), 7);
    assert_eq!(second.next_cursor, None);

    let plan: serde_json::Value = sqlx::query_scalar(
        "EXPLAIN (ANALYZE, FORMAT JSON)\
         SELECT session_id, worker_id, provider FROM moa.hand_leases\
          WHERE tenant_id = $1 AND session_id = $2 AND status <> 'destroyed'\
          ORDER BY worker_id, provider LIMIT 65",
    )
    .bind(tenant_id)
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("explain bounded live-session lookup");
    let plan = plan.to_string();
    assert!(
        plan.contains("hand_leases_tenant_live_owner_idx"),
        "live session page must use the partial owner index: {plan}"
    );
    assert!(
        !plan.contains("Seq Scan"),
        "live session page must not scan destroyed history: {plan}"
    );

    sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("delete lease paging fixture");
    sqlx::query("DELETE FROM public.session_agent_context WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("delete session agent fixture");
    sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("delete session fixture");
}

async fn seed_workspace(
    pool: &PgPool,
    tenant_id: TenantId,
    session_id: SessionId,
) -> (HandLeaseWorkspaceAttachment, ProviderAccountId) {
    let provider_account_id = ProviderAccountId::new();
    let workspace_id = SandboxWorkspaceId::new();
    sqlx::query(
        "INSERT INTO moa.sandbox_provider_accounts (\
             provider_account_id, provider, isolation_cell, organization_fingerprint\
         ) VALUES ($1, 'local', $2, $3)",
    )
    .bind(provider_account_id)
    .bind(format!("lease-reaper-{workspace_id}"))
    .bind(format!("lease-reaper-org-{workspace_id}"))
    .execute(pool)
    .await
    .expect("seed provider account");
    sqlx::query(
        "INSERT INTO moa.sandbox_workspaces (\
             workspace_id, tenant_id, scope_kind, scope_session_id, scope_worker_id,\
             provider, provider_account_id, provider_account_generation, durability_class,\
             lifecycle_state, writer_epoch, instance_generation\
         ) VALUES ($1, $2, 'worker', $3, 'worker', 'local', $4, 1, 'portable_filesystem',\
                   'active', 1, 1)",
    )
    .bind(workspace_id)
    .bind(tenant_id)
    .bind(session_id)
    .bind(provider_account_id)
    .execute(pool)
    .await
    .expect("seed sandbox workspace");
    (
        HandLeaseWorkspaceAttachment::new(workspace_id, 1, 1, None)
            .expect("seeded attachment validates"),
        provider_account_id,
    )
}

async fn cleanup_session_fixture(
    pool: &PgPool,
    session_id: SessionId,
    workspace_id: SandboxWorkspaceId,
    provider_account_id: ProviderAccountId,
) {
    let _ = sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(pool)
        .await;
    let _ = sqlx::query(
        "UPDATE moa.sandbox_workspaces \
         SET current_checkpoint_id = NULL, current_checkpoint_generation = 0 \
         WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_workspace_checkpoints WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_workspace_operations WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_workspaces WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM moa.sandbox_provider_accounts WHERE provider_account_id = $1")
        .bind(provider_account_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM public.sessions WHERE id = $1")
        .bind(session_id)
        .execute(pool)
        .await;
}

fn seconds(value: u64) -> LifetimeLimit {
    LifetimeLimit::Bounded {
        seconds: std::num::NonZeroU64::new(value).expect("nonzero seconds"),
    }
}

/// Builds a lease policy through the production resolution path.
fn lease_policy(idle: LifetimeLimit, hard: LifetimeLimit) -> HandLeasePolicy {
    let profile = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        idle,
        hard,
    )
    .expect("test profile should validate");
    let effective = moa_core::types::hands::resolve_effective_sandbox_profile(
        &SandboxPolicySnapshot::new("db-deployment", profile).expect("deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "db-capabilities-v1",
    )
    .expect("test resolution should succeed");
    HandLeasePolicy::from_effective(&effective)
}

/// Seeds one active lease with a live handle and an already-expired hard deadline.
async fn seed_expired_active_lease(
    pool: &PgPool,
    session_id: SessionId,
    tenant_id: TenantId,
) -> (SandboxWorkspaceId, ProviderAccountId) {
    seed_session(pool, session_id, tenant_id).await;
    let (attachment, provider_account_id) = seed_workspace(pool, tenant_id, session_id).await;
    let store = PostgresHandLeaseStore::new(pool.clone());
    let policy = lease_policy(seconds(60), seconds(120));
    let claim = store
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id: "worker",
            tenant_id,
            provider: "local",
            tier: SandboxTier::Local,
            attachment: attachment.clone(),
            policy: &policy,
            caller_deadline: None,
        })
        .await
        .expect("claim provisioning")
        .expect("claim is owned");
    store
        .activate(HandLeaseActivateRequest {
            tenant_id,
            session_id,
            worker_id: "worker",
            provider: "local",
            generation: claim.generation,
            handle: LeaseHandle::new(
                claim.provisioning_operation_id,
                moa_core::types::hands::HandHandle::local(std::path::PathBuf::from(
                    "/tmp/moa-reaper-db",
                )),
            ),
            attachment: attachment.clone(),
        })
        .await
        .expect("activate lease");
    // Push both deadlines into the past so the sandbox is destroyable now,
    // without waiting out a real lifetime.
    sqlx::query(
        "UPDATE moa.hand_leases \
         SET idle_expires_at = now() - interval '1 minute', \
             hard_expires_at = now() - interval '1 minute' \
         WHERE session_id = $1",
    )
    .bind(session_id)
    .execute(pool)
    .await
    .expect("expire the seeded lease");
    (attachment.workspace_id, provider_account_id)
}

async fn lease_status(pool: &PgPool, session_id: SessionId) -> String {
    sqlx::query_scalar::<_, String>("SELECT status FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .fetch_one(pool)
        .await
        .expect("read seeded lease status")
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn competing_replicas_claim_disjoint_generations_without_new_traffic_db() {
    // Pins: two replicas sweeping at once take disjoint generations, so a
    // hard-expired sandbox is destroyed exactly once and never twice. The
    // sweep is driven by the reaper alone — no tool call touches these
    // sessions after they are seeded, which is the whole point: the sandboxes
    // that most need destroying belong to sessions that never come back.
    let pool = pool().await;
    let tenant_id = TenantId::new();
    let sessions = [SessionId::new(), SessionId::new(), SessionId::new()];
    let mut fixtures = Vec::new();
    for session_id in sessions {
        fixtures.push(seed_expired_active_lease(&pool, session_id, tenant_id).await);
    }

    let left = PostgresExpiredHandLeaseClaims::new(pool.clone());
    let right = PostgresExpiredHandLeaseClaims::new(pool.clone());
    let claim_ttl = Duration::from_secs(300);
    let (left_claims, right_claims) = tokio::join!(
        left.claim_expired(64, claim_ttl),
        right.claim_expired(64, claim_ttl)
    );
    let left_claims = left_claims.expect("left replica claims");
    let right_claims = right_claims.expect("right replica claims");

    let mine = |claims: &[moa_hands::core::reaper::ClaimedHandLease]| {
        claims
            .iter()
            .filter(|claim| sessions.contains(&claim.session_id))
            .map(|claim| (claim.session_id, claim.generation))
            .collect::<HashSet<_>>()
    };
    let left_mine = mine(&left_claims);
    let right_mine = mine(&right_claims);

    assert!(
        left_mine.is_disjoint(&right_mine),
        "SKIP LOCKED must hand each generation to exactly one replica; \
         left={left_mine:?} right={right_mine:?}"
    );
    assert!(
        !left_mine.is_empty() || !right_mine.is_empty(),
        "the sweep must claim seeded expired leases without any new traffic"
    );

    // `claim_expired` deliberately claims across the whole fleet, so a sibling
    // test in this binary may own some of these rows. The pins that matter hold
    // either way: every seeded lease is claimed by exactly one owner (so none is
    // still active), and the generations this test's two replicas took are
    // disjoint.
    for session_id in sessions {
        assert_eq!(
            lease_status(&pool, session_id).await,
            "reaping",
            "a claimed generation is fenced as `reaping`, never left active"
        );
    }

    // Finalization is fenced to the claimed generation: rows this test claimed
    // become terminal, and rows another owner claimed are left to that owner.
    let mut finalized = Vec::new();
    for claim in left_claims.iter().chain(right_claims.iter()) {
        if !sessions.contains(&claim.session_id) {
            continue;
        }
        assert!(
            left.finalize_destroyed(claim)
                .await
                .expect("finalize the claimed generation"),
            "the current claim owner should win its finalization fence"
        );
        finalized.push(claim.session_id);
    }
    for session_id in finalized {
        assert_eq!(
            lease_status(&pool, session_id).await,
            "destroyed",
            "the generation this test claimed must finalize as destroyed"
        );
        let workspace_state = sqlx::query_scalar::<_, String>(
            "SELECT lifecycle_state FROM moa.sandbox_workspaces \
             WHERE tenant_id = $1 AND scope_session_id = $2",
        )
        .bind(tenant_id)
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .expect("read workspace state after compute finalization");
        assert_eq!(
            workspace_state, "ready",
            "safe compute destruction must leave the retained workspace ready, not falsely active"
        );
        let attachment_cleared = sqlx::query_scalar::<_, bool>(
            "SELECT workspace_id IS NULL \
                    AND workspace_writer_epoch IS NULL \
                    AND workspace_instance_generation IS NULL \
                    AND restored_checkpoint_id IS NULL \
             FROM moa.hand_leases WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .expect("read terminal hand attachment");
        assert!(
            attachment_cleared,
            "workspace transition and terminal attachment clearing must commit together"
        );
    }

    for (session_id, (workspace_id, provider_account_id)) in sessions.into_iter().zip(fixtures) {
        cleanup_session_fixture(&pool, session_id, workspace_id, provider_account_id).await;
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn a_failed_destroy_stays_fenced_and_never_returns_to_active_db() {
    // Pins: releasing a claimed generation for retry puts it back as `failed`
    // behind a future `reap_not_before`, so the sandbox is neither reusable nor
    // retried in a tight loop. Reactivating here would hand a caller a sandbox
    // the reaper already decided to destroy.
    let pool = pool().await;
    let session_id = SessionId::new();
    let tenant_id = TenantId::new();
    let (workspace_id, provider_account_id) =
        seed_expired_active_lease(&pool, session_id, tenant_id).await;

    let claims = PostgresExpiredHandLeaseClaims::new(pool.clone());
    let claimed = claims
        .claim_expired(64, Duration::from_secs(300))
        .await
        .expect("claim expired leases")
        .into_iter()
        .find(|claim| claim.session_id == session_id)
        .expect("the seeded lease is claimable");

    assert!(
        claims
            .release_for_retry(&claimed, Duration::from_secs(120))
            .await
            .expect("release the failed destroy for retry"),
        "the current claim owner should win its retry fence"
    );

    let (status, attempts, backoff_in_future) = sqlx::query_as::<_, (String, i32, Option<bool>)>(
        "SELECT status, reap_attempts, reap_not_before > now() \
             FROM moa.hand_leases WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("read released lease");

    assert_eq!(
        status, "failed",
        "a failed destroy must stay fenced off from reuse, never return to active"
    );
    assert_eq!(attempts, 1, "the failed attempt must be counted");
    assert_eq!(
        backoff_in_future,
        Some(true),
        "the retry must sit behind a backoff instead of spinning"
    );

    // A generation the reaper released is not claimable again until its backoff
    // elapses, which is what bounds the retry rate.
    let immediate = claims
        .claim_expired(64, Duration::from_secs(300))
        .await
        .expect("second claim pass")
        .into_iter()
        .any(|claim| claim.session_id == session_id);
    assert!(
        !immediate,
        "a released generation must not be re-claimed before its backoff elapses"
    );

    cleanup_session_fixture(&pool, session_id, workspace_id, provider_account_id).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn expired_reaper_claim_is_recovered_and_the_old_owner_is_fenced_db() {
    // Pins: a replica crash cannot strand a row in `reaping` forever. After the
    // claim TTL expires another replica owns a new token, and the crashed
    // owner's late finalization cannot delete that newer claim.
    let pool = pool().await;
    let session_id = SessionId::new();
    let tenant_id = TenantId::new();
    let (workspace_id, provider_account_id) =
        seed_expired_active_lease(&pool, session_id, tenant_id).await;

    let claims = PostgresExpiredHandLeaseClaims::new(pool.clone());
    let first = claims
        .claim_expired(64, Duration::from_secs(300))
        .await
        .expect("first replica claims")
        .into_iter()
        .find(|claim| claim.session_id == session_id)
        .expect("the seeded lease is claimable");
    sqlx::query(
        "UPDATE moa.hand_leases \
         SET reap_claim_expires_at = now() - interval '1 second' \
         WHERE session_id = $1",
    )
    .bind(session_id)
    .execute(&pool)
    .await
    .expect("simulate the first reaper crashing past its claim TTL");

    let recovered = claims
        .claim_expired(64, Duration::from_secs(300))
        .await
        .expect("second replica recovers expired claim")
        .into_iter()
        .find(|claim| claim.session_id == session_id)
        .expect("expired reaping claim must be reclaimable");
    assert_eq!(recovered.generation, first.generation);
    assert_ne!(
        recovered.claim_token, first.claim_token,
        "claim recovery must mint a new ownership token"
    );

    assert!(
        !claims
            .finalize_destroyed(&first)
            .await
            .expect("late stale owner finalization is a fenced no-op"),
        "the stale ownership token must lose its finalization fence"
    );
    let (status, live_token) = sqlx::query_as::<_, (String, Option<uuid::Uuid>)>(
        "SELECT status, reap_claim_token FROM moa.hand_leases WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("read recovered claim");
    assert_eq!(status, "reaping");
    assert_eq!(
        live_token,
        Some(recovered.claim_token),
        "the stale owner must not clear the recovered owner's claim"
    );

    assert!(
        claims
            .finalize_destroyed(&recovered)
            .await
            .expect("current owner finalizes"),
        "the recovered claim owner should finalize"
    );
    assert_eq!(lease_status(&pool, session_id).await, "destroyed");

    cleanup_session_fixture(&pool, session_id, workspace_id, provider_account_id).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn stale_reaper_claim_cannot_release_or_finalize_a_newer_attachment_db() {
    // Pins: all four workspace-attachment columns are part of the reaper CAS.
    // If a replacement attachment supersedes the captured epochs, the stale
    // owner cannot renew, release, or finalize it; the exact current owner can
    // finalize compute cleanup and clears the terminal lease attachment.
    let pool = pool().await;
    let session_id = SessionId::new();
    let tenant_id = TenantId::new();
    let (workspace_id, provider_account_id) =
        seed_expired_active_lease(&pool, session_id, tenant_id).await;

    let claims = PostgresExpiredHandLeaseClaims::new(pool.clone());
    let stale = claims
        .claim_expired(64, Duration::from_secs(300))
        .await
        .expect("claim expired leases")
        .into_iter()
        .find(|claim| claim.session_id == session_id)
        .expect("the seeded lease is claimable");
    let original = stale
        .attachment
        .as_ref()
        .expect("active lease claim carries its exact workspace attachment");
    assert_eq!(original.workspace_id, workspace_id);

    let newer_writer_epoch = original.workspace_writer_epoch + 1;
    let newer_instance_generation = original.workspace_instance_generation + 1;
    let commit_operation_id = WorkspaceOperationId::new();
    let checkpoint_id = WorkspaceCheckpointId::new();
    sqlx::query(
        "INSERT INTO moa.sandbox_workspace_operations (\
             operation_id, tenant_id, workspace_id, provider_account_id,\
             provider_account_generation, operation_kind, request_hash,\
             expected_writer_epoch, expected_instance_generation,\
             expected_checkpoint_generation, deadline_at, reconcile_not_before,\
             outcome_class, confirmed_disposition\
         ) VALUES (\
             $1, $2, $3, $4, 1, 'commit', $5, $6, $7, 0,\
             now() + interval '1 minute', now() + interval '2 minutes',\
             'confirmed', 'resource_present'\
         )",
    )
    .bind(commit_operation_id)
    .bind(tenant_id)
    .bind(workspace_id)
    .bind(provider_account_id)
    .bind(format!("reaper-new-head-{checkpoint_id}"))
    .bind(newer_writer_epoch)
    .bind(newer_instance_generation)
    .execute(&pool)
    .await
    .expect("seed the newer committed-head operation");
    sqlx::query(
        "INSERT INTO moa.sandbox_workspace_checkpoints (\
             checkpoint_id, tenant_id, workspace_id, generation,\
             source_writer_epoch, source_instance_generation,\
             source_checkpoint_generation, object_reference, manifest_digest,\
             logical_bytes, operation_id, lifecycle_state, verified_at\
         ) VALUES ($1, $2, $3, 1, $4, $5, 0, $6, $7, 1, $8, 'available', now())",
    )
    .bind(checkpoint_id)
    .bind(tenant_id)
    .bind(workspace_id)
    .bind(newer_writer_epoch)
    .bind(newer_instance_generation)
    .bind(format!("reaper/checkpoints/{checkpoint_id}"))
    .bind(format!("digest-{checkpoint_id}"))
    .bind(commit_operation_id)
    .execute(&pool)
    .await
    .expect("seed the newer verified checkpoint head");
    let newer = HandLeaseWorkspaceAttachment::new(
        workspace_id,
        newer_writer_epoch,
        newer_instance_generation,
        Some(checkpoint_id),
    )
    .expect("newer attachment validates");
    sqlx::query(
        "UPDATE moa.hand_leases \
         SET workspace_writer_epoch = $2, workspace_instance_generation = $3, \
             restored_checkpoint_id = $4 \
         WHERE session_id = $1 AND status = 'reaping'",
    )
    .bind(session_id)
    .bind(newer.workspace_writer_epoch)
    .bind(newer.workspace_instance_generation)
    .bind(newer.restored_checkpoint_id)
    .execute(&pool)
    .await
    .expect("simulate a newer attachment replacing the captured fences");
    sqlx::query(
        "UPDATE moa.sandbox_workspaces \
         SET writer_epoch = $2, instance_generation = $3, \
             current_checkpoint_id = $4, current_checkpoint_generation = 1 \
         WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .bind(newer.workspace_writer_epoch)
    .bind(newer.workspace_instance_generation)
    .bind(newer.restored_checkpoint_id)
    .execute(&pool)
    .await
    .expect("advance the durable workspace to the newer attachment and head");

    assert!(
        !claims
            .renew_claim(&stale, Duration::from_secs(300))
            .await
            .expect("stale renewal is a fenced no-op"),
        "a stale claim must not renew ownership over a newer attachment"
    );
    assert!(
        !claims
            .release_for_retry(&stale, Duration::from_secs(120))
            .await
            .expect("stale retry release is a fenced no-op"),
        "a stale claim must not release a newer attachment"
    );
    assert!(
        !claims
            .finalize_destroyed(&stale)
            .await
            .expect("stale finalization is a fenced no-op"),
        "a stale claim must not finalize a newer attachment"
    );

    let live = sqlx::query_as::<
        _,
        (
            String,
            Option<i64>,
            Option<i64>,
            Option<WorkspaceCheckpointId>,
        ),
    >(
        "SELECT status, workspace_writer_epoch, workspace_instance_generation, \
                restored_checkpoint_id \
         FROM moa.hand_leases WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("read the still-fenced newer attachment");
    assert_eq!(
        live,
        (
            "reaping".to_string(),
            Some(newer.workspace_writer_epoch),
            Some(newer.workspace_instance_generation),
            newer.restored_checkpoint_id,
        ),
        "all stale operations must leave the newer attachment untouched"
    );

    let ambiguous_operation_id = WorkspaceOperationId::new();
    sqlx::query(
        "INSERT INTO moa.sandbox_workspace_operations (\
             operation_id, tenant_id, workspace_id, provider_account_id,\
             provider_account_generation, operation_kind, request_hash,\
             expected_writer_epoch, expected_instance_generation,\
             expected_checkpoint_generation, deadline_at, reconcile_not_before,\
             outcome_class\
         ) VALUES (\
             $1, $2, $3, $4, 1, 'restore', $5, $6, $7, 1,\
             now() + interval '1 minute', now() + interval '2 minutes', 'unknown'\
         )",
    )
    .bind(ambiguous_operation_id)
    .bind(tenant_id)
    .bind(workspace_id)
    .bind(provider_account_id)
    .bind(format!("reaper-ambiguous-restore-{ambiguous_operation_id}"))
    .bind(newer.workspace_writer_epoch)
    .bind(newer.workspace_instance_generation)
    .execute(&pool)
    .await
    .expect("seed an ambiguous operation on the current attachment");

    let mut current = stale;
    current.attachment = Some(newer.clone());
    assert!(
        claims
            .finalize_destroyed(&current)
            .await
            .expect("current exact claim finalizes"),
        "the exact attachment owner should finalize compute cleanup"
    );
    let terminal = sqlx::query_as::<
        _,
        (
            String,
            Option<SandboxWorkspaceId>,
            Option<i64>,
            Option<i64>,
            Option<moa_core::types::identifiers::WorkspaceCheckpointId>,
        ),
    >(
        "SELECT status, workspace_id, workspace_writer_epoch, \
                workspace_instance_generation, restored_checkpoint_id \
         FROM moa.hand_leases WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("read finalized lease");
    assert_eq!(terminal, ("destroyed".to_string(), None, None, None, None));
    let workspace = sqlx::query_as::<_, (String, i64, i64, Option<WorkspaceCheckpointId>)>(
        "SELECT lifecycle_state, writer_epoch, instance_generation, current_checkpoint_id \
         FROM moa.sandbox_workspaces WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await
    .expect("read workspace after exact compute finalization");
    assert_eq!(
        workspace,
        (
            "reconciling".to_string(),
            newer.workspace_writer_epoch,
            newer.workspace_instance_generation,
            newer.restored_checkpoint_id,
        ),
        "ambiguous work must retain the exact newer head and move only lifecycle to reconciling"
    );

    cleanup_session_fixture(&pool, session_id, workspace_id, provider_account_id).await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn renewal_cannot_push_idle_past_the_hard_deadline_in_postgres_db() {
    // Pins: the store's `LEAST(...)` clamp is real SQL, not an in-memory
    // convenience. A renewal asking for a day gets the sandbox's remaining
    // lifetime, and the hard deadline is untouched.
    let pool = pool().await;
    let store = PostgresHandLeaseStore::new(pool.clone());
    let session_id = SessionId::new();
    let tenant_id = TenantId::new();
    seed_session(&pool, session_id, tenant_id).await;
    let (attachment, provider_account_id) = seed_workspace(&pool, tenant_id, session_id).await;
    let policy = lease_policy(seconds(60), seconds(120));
    let claim = store
        .claim_for_provisioning(HandLeaseProvisionRequest {
            session_id,
            worker_id: "worker",
            tenant_id,
            provider: "local",
            tier: SandboxTier::Local,
            attachment: attachment.clone(),
            policy: &policy,
            caller_deadline: None,
        })
        .await
        .expect("claim provisioning")
        .expect("claim is owned");
    let hard_deadline = claim.hard_expires_at.expect("bounded hard deadline");
    store
        .activate(HandLeaseActivateRequest {
            tenant_id,
            session_id,
            worker_id: "worker",
            provider: "local",
            generation: claim.generation,
            handle: LeaseHandle::new(
                claim.provisioning_operation_id,
                moa_core::types::hands::HandHandle::local(std::path::PathBuf::from(
                    "/tmp/moa-renew-db",
                )),
            ),
            attachment: attachment.clone(),
        })
        .await
        .expect("activate lease");

    let greedy = chrono::Utc::now() + chrono::Duration::hours(24);
    assert!(
        store
            .renew_active(HandLeaseRenewRequest {
                tenant_id,
                session_id,
                worker_id: "worker",
                provider: "local",
                generation: claim.generation,
                provisioning_operation_id: claim.provisioning_operation_id,
                attachment: attachment.clone(),
                idle_expires_at: greedy,
            })
            .await
            .expect("renewal inside the hard lifetime succeeds")
    );

    let renewed = store
        .get(tenant_id, session_id, "worker", "local")
        .await
        .expect("load renewed lease")
        .expect("lease exists");
    assert_eq!(
        renewed.idle_expires_at,
        Some(hard_deadline),
        "Postgres must cap the idle deadline at the hard deadline"
    );
    assert_eq!(
        renewed.hard_expires_at,
        Some(hard_deadline),
        "renewal must never move the hard deadline"
    );
    assert_eq!(renewed.status, HandLeaseStatus::Active);

    cleanup_session_fixture(
        &pool,
        session_id,
        attachment.workspace_id,
        provider_account_id,
    )
    .await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn tenant_sandbox_policy_round_trips_and_absence_is_the_named_identity_layer_db() {
    // Pins: an authored tenant layer is read back exactly, and a tenant that has
    // authored nothing yields `None` — which the router turns into the named
    // `tenant-sandbox-unset` identity layer rather than treating as an error or
    // inventing a permissive profile.
    let pool = pool().await;
    let store = PostgresTenantSandboxPolicyStore::new(pool.clone());
    let authored = TenantId::new();
    let silent = TenantId::new();

    let profile = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        seconds(180),
        seconds(900),
    )
    .expect("tenant profile should validate");
    sqlx::query(
        "INSERT INTO moa.tenant_sandbox_policy (tenant_id, revision, profile) \
         VALUES ($1, $2, $3)",
    )
    .bind(authored)
    .bind("tenant-policy-v1")
    .bind(sqlx::types::Json(&profile))
    .execute(&pool)
    .await
    .expect("seed the tenant policy layer");

    let loaded = store
        .current(authored)
        .await
        .expect("read authored tenant layer")
        .expect("authored tenant has a layer");
    assert_eq!(loaded.revision, "tenant-policy-v1");
    assert_eq!(loaded.profile, profile);

    assert!(
        store
            .current(silent)
            .await
            .expect("read silent tenant layer")
            .is_none(),
        "a tenant with no authored layer must report absence, not a fabricated policy"
    );
    assert_eq!(
        SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset).revision,
        "tenant-sandbox-unset"
    );

    // The deployment layer is unrelated to the tenant layer and still resolves.
    let deployment = deployment_sandbox_policy(&moa_config::MoaConfig::default())
        .expect("default deployment layer resolves");
    assert_eq!(deployment.revision, "local-development-unbounded");

    let _ = sqlx::query("DELETE FROM moa.tenant_sandbox_policy WHERE tenant_id = ANY($1)")
        .bind(vec![authored, silent])
        .execute(&pool)
        .await;
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn a_claim_in_flight_on_one_replica_does_not_block_another_replicas_sweep_db() {
    // Pins: `SKIP LOCKED` specifically, which the concurrent-claim test above
    // cannot isolate — two autocommit statements usually serialize fast enough
    // that the loser is excluded by the status filter instead, so that test
    // passes with or without the clause.
    //
    // Here a lock is held open across a real transaction boundary. With
    // `SKIP LOCKED` the second replica steps over the locked rows and returns
    // immediately; without it, the same statement waits on the lock until the
    // holder commits, which is how one slow destroy stalls the whole fleet's
    // sweep. The assertion is therefore on *promptness*, because blocking
    // versus skipping is the only difference the clause makes.
    let pool = pool().await;
    let tenant_id = TenantId::new();
    let sessions = [SessionId::new(), SessionId::new()];
    let mut fixtures = Vec::new();
    for session_id in sessions {
        fixtures.push(seed_expired_active_lease(&pool, session_id, tenant_id).await);
    }

    // Hold row locks on exactly this test's leases, standing in for a replica
    // whose own claim transaction has not committed yet.
    let mut holder = pool.begin().await.expect("begin holding transaction");
    sqlx::query("SELECT 1 FROM moa.hand_leases WHERE session_id = ANY($1) FOR UPDATE")
        .bind(sessions.to_vec())
        .fetch_all(&mut *holder)
        .await
        .expect("hold locks on the seeded leases");

    let claims = PostgresExpiredHandLeaseClaims::new(pool.clone());
    let swept = tokio::time::timeout(
        Duration::from_secs(5),
        claims.claim_expired(64, Duration::from_secs(300)),
    )
    .await
    .expect("a sweep must not block behind another replica's in-flight claim")
    .expect("claim expired leases");

    assert!(
        !swept
            .iter()
            .any(|claim| sessions.contains(&claim.session_id)),
        "locked rows must be skipped, not waited on"
    );

    holder.rollback().await.expect("release the held locks");
    for (session_id, (workspace_id, provider_account_id)) in sessions.into_iter().zip(fixtures) {
        cleanup_session_fixture(&pool, session_id, workspace_id, provider_account_id).await;
    }
    pool.close().await;
}
