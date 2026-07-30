//! The durable hand-lease reaper and the tenant sandbox policy layer against
//! real Postgres.
//!
//! The offline lane proves the reaper's decision order with in-memory doubles.
//! This lane proves the parts only a database can: that two replicas issuing the
//! same `FOR UPDATE ... SKIP LOCKED` claim take disjoint generations rather than
//! both destroying one sandbox, that finalization is fenced to the claimed
//! generation, that a released generation comes back as `stale` behind its
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
use moa_core::types::identifiers::{SessionId, TenantId};
use moa_hands::PostgresTenantSandboxPolicyStore;
use moa_hands::core::leases::{
    HandLeasePolicy, HandLeaseStatus, HandLeaseStore, LeaseHandle, PostgresHandLeaseStore,
};
use moa_hands::core::reaper::{ExpiredHandLeaseClaims, PostgresExpiredHandLeaseClaims};
use moa_hands::{TenantSandboxPolicyStore, deployment_sandbox_policy};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

fn database_url() -> String {
    std::env::var("MOA_DATABASE_URL")
        .expect("MOA_DATABASE_URL must point at the local compose Postgres for _db tests")
}

async fn pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url())
        .await
        .expect("test Postgres should be reachable")
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
async fn seed_expired_active_lease(pool: &PgPool, session_id: SessionId, tenant_id: TenantId) {
    let store = PostgresHandLeaseStore::new(pool.clone());
    let policy = lease_policy(seconds(60), seconds(120));
    let claim = store
        .claim_for_provisioning(
            session_id,
            "",
            tenant_id,
            "local",
            SandboxTier::Local,
            &policy,
        )
        .await
        .expect("claim provisioning")
        .expect("claim is owned");
    store
        .activate(
            session_id,
            "",
            "local",
            claim.generation,
            LeaseHandle::new(moa_core::types::hands::HandHandle::local(
                std::path::PathBuf::from("/tmp/moa-reaper-db"),
            )),
        )
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
    for session_id in sessions {
        seed_expired_active_lease(&pool, session_id, tenant_id).await;
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
    }

    for session_id in sessions {
        let _ = sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
            .bind(session_id)
            .execute(&pool)
            .await;
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires the local compose Postgres via MOA_DATABASE_URL"]
async fn a_failed_destroy_stays_fenced_and_never_returns_to_active_db() {
    // Pins: releasing a claimed generation for retry puts it back as `stale`
    // behind a future `reap_not_before`, so the sandbox is neither reusable nor
    // retried in a tight loop. Reactivating here would hand a caller a sandbox
    // the reaper already decided to destroy.
    let pool = pool().await;
    let session_id = SessionId::new();
    seed_expired_active_lease(&pool, session_id, TenantId::new()).await;

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
        status, "stale",
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

    let _ = sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await;
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
    seed_expired_active_lease(&pool, session_id, TenantId::new()).await;

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

    let _ = sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
        .await;
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
    let policy = lease_policy(seconds(60), seconds(120));
    let claim = store
        .claim_for_provisioning(
            session_id,
            "",
            TenantId::new(),
            "local",
            SandboxTier::Local,
            &policy,
        )
        .await
        .expect("claim provisioning")
        .expect("claim is owned");
    let hard_deadline = claim.hard_expires_at.expect("bounded hard deadline");
    store
        .activate(
            session_id,
            "",
            "local",
            claim.generation,
            LeaseHandle::new(moa_core::types::hands::HandHandle::local(
                std::path::PathBuf::from("/tmp/moa-renew-db"),
            )),
        )
        .await
        .expect("activate lease");

    let greedy = chrono::Utc::now() + chrono::Duration::hours(24);
    assert!(
        store
            .renew_active(session_id, "", "local", claim.generation, greedy)
            .await
            .expect("renewal inside the hard lifetime succeeds")
    );

    let renewed = store
        .get(session_id, "", "local")
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

    let _ = sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
        .bind(session_id)
        .execute(&pool)
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
    for session_id in sessions {
        seed_expired_active_lease(&pool, session_id, tenant_id).await;
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
    for session_id in sessions {
        let _ = sqlx::query("DELETE FROM moa.hand_leases WHERE session_id = $1")
            .bind(session_id)
            .execute(&pool)
            .await;
    }
    pool.close().await;
}
