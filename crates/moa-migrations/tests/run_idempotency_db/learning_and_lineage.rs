//! Experiment provenance, learning, and lineage schema scenarios.

use super::support::*;

/// Typed Behavior Lab score provenance.
const EXPERIMENT_SCORE_PROVENANCE_SQL: &str =
    include_str!("../../migrations/postgres/V000041__experiment_score_provenance.sql");

#[test]
fn experiment_score_provenance_ownership_is_registered_offline() {
    // Pins: a tenant-scoped table with no ownership row is a table nothing is
    // accountable for, and the tenant-purge catalog scan would only notice it at
    // runtime against a live database.
    assert!(
        MIGRATION_OWNERSHIP.contains("name = \"experiment_score_provenance\""),
        "experiment-score provenance's table must be registered in migration-ownership.toml"
    );
    // The trial foreign key must not cascade: the tenant purge carries an
    // explicit delete for this table, and a cascade would make that step
    // unfalsifiable because the trial delete would remove the same rows anyway.
    assert!(
        !EXPERIMENT_SCORE_PROVENANCE_SQL.contains("ON DELETE CASCADE"),
        "no foreign key here may cascade over the explicit tenant-purge step"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn experiment_score_provenance_enforces_linkage_and_immutability_db() {
    // Pins the experiment-score provenance guarantees the database owns rather than the writer:
    // provenance cannot name a trial from another tenant, run, or pinned plan
    // revision; it cannot claim both targets or neither; and it cannot be
    // rewritten after the fact. An application that "checked first" would pass a
    // unit test and still admit a mislinked or mutated row on a concurrent path.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        let (_, second) = clean_apply_then_reapply(&target_url).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;

        let forced: Option<bool> = sqlx::query_scalar(
            "SELECT relrowsecurity AND relforcerowsecurity
               FROM pg_catalog.pg_class AS relation
               JOIN pg_catalog.pg_namespace AS namespace ON namespace.oid = relation.relnamespace
              WHERE namespace.nspname = 'moa'
                AND relation.relname = 'experiment_score_provenance'",
        )
        .fetch_optional(&target)
        .await?;

        seed_provenance_fixture(&target).await?;

        let correct = insert_provenance(&target, ProvenanceCell::default()).await;
        let replay = insert_provenance(&target, ProvenanceCell::default()).await;
        let wrong_run = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 20,
                experiment_run_uid: "22222222-2222-2222-2222-222222222222",
                ..ProvenanceCell::default()
            },
        )
        .await;
        let wrong_plan = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 21,
                plan_revision_uid: "33333333-3333-3333-3333-333333333333",
                ..ProvenanceCell::default()
            },
        )
        .await;
        let wrong_tenant = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 22,
                storage_partition_id: "99999999-9999-9999-9999-999999999999",
                ..ProvenanceCell::default()
            },
        )
        .await;
        let both_targets = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 23,
                target_execution_run_uid: Some("44444444-4444-4444-4444-444444444444"),
                ..ProvenanceCell::default()
            },
        )
        .await;
        let no_target = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 24,
                target_session_id: None,
                ..ProvenanceCell::default()
            },
        )
        .await;
        let short_hash = insert_provenance(
            &target,
            ProvenanceCell {
                score_id: 25,
                evidence_hash: "\\x00",
                ..ProvenanceCell::default()
            },
        )
        .await;

        let updated = sqlx::query(
            "UPDATE moa.experiment_score_provenance
                SET evidence_ref = 'rewritten'
              WHERE score_id = '00000000-0000-0000-0000-000000000010'",
        )
        .execute(&target)
        .await;

        let stored_ref: String = sqlx::query_scalar(
            "SELECT evidence_ref FROM moa.experiment_score_provenance
              WHERE score_id = '00000000-0000-0000-0000-000000000010'",
        )
        .fetch_one(&target)
        .await?;

        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(ProvenanceOutcome {
            second_apply_count: second.len(),
            forced,
            correct_accepted: correct.is_ok(),
            replay_refused: replay.is_err(),
            wrong_run_refused: wrong_run.is_err(),
            wrong_plan_refused: wrong_plan.is_err(),
            wrong_tenant_refused: wrong_tenant.is_err(),
            both_targets_refused: both_targets.is_err(),
            no_target_refused: no_target.is_err(),
            short_hash_refused: short_hash.is_err(),
            update_refused: updated.is_err(),
            stored_ref,
        })
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let outcome = outcome.expect("provenance assertions should complete on a fresh database");

    assert_eq!(
        outcome.second_apply_count, 0,
        "experiment-score provenance must be idempotent: a second run applied {} migrations",
        outcome.second_apply_count
    );
    assert_eq!(
        outcome.forced,
        Some(true),
        "experiment score provenance must force row-level security"
    );
    assert!(
        outcome.correct_accepted,
        "a correctly linked provenance row must be accepted"
    );
    assert!(
        outcome.replay_refused,
        "a second row for the same score id must be refused by the primary key"
    );
    assert!(
        outcome.wrong_run_refused,
        "provenance naming another experiment run must be refused"
    );
    assert!(
        outcome.wrong_plan_refused,
        "provenance naming another pinned plan revision must be refused"
    );
    assert!(
        outcome.wrong_tenant_refused,
        "provenance naming another tenant's partition must be refused"
    );
    assert!(
        outcome.both_targets_refused,
        "provenance claiming both a session and an execution run must be refused"
    );
    assert!(
        outcome.no_target_refused,
        "provenance claiming no target at all must be refused"
    );
    assert!(
        outcome.short_hash_refused,
        "an evidence hash that is not a 32-byte digest must be refused"
    );
    assert!(
        outcome.update_refused,
        "provenance must be immutable: the UPDATE trigger must refuse every rewrite"
    );
    assert_eq!(
        outcome.stored_ref, "session:00000000-0000-0000-0000-000000000005#seq=1",
        "the refused UPDATE must have left the stored evidence reference untouched"
    );
}

struct ProvenanceOutcome {
    second_apply_count: usize,
    forced: Option<bool>,
    correct_accepted: bool,
    replay_refused: bool,
    wrong_run_refused: bool,
    wrong_plan_refused: bool,
    wrong_tenant_refused: bool,
    both_targets_refused: bool,
    no_target_refused: bool,
    short_hash_refused: bool,
    update_refused: bool,
    stored_ref: String,
}

struct ProvenanceCell {
    score_id: u8,
    storage_partition_id: &'static str,
    experiment_run_uid: &'static str,
    plan_revision_uid: &'static str,
    target_session_id: Option<&'static str>,
    target_execution_run_uid: Option<&'static str>,
    evidence_hash: &'static str,
}

impl Default for ProvenanceCell {
    fn default() -> Self {
        Self {
            score_id: 16,
            storage_partition_id: "11111111-1111-1111-1111-111111111111",
            experiment_run_uid: "00000000-0000-0000-0000-000000000003",
            plan_revision_uid: "00000000-0000-0000-0000-000000000004",
            target_session_id: Some("00000000-0000-0000-0000-000000000005"),
            target_execution_run_uid: None,
            evidence_hash: "\\x0000000000000000000000000000000000000000000000000000000000000001",
        }
    }
}

async fn insert_provenance(
    pool: &PgPool,
    cell: ProvenanceCell,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let score_id = format!("00000000-0000-0000-0000-0000000000{:02x}", cell.score_id);
    let score_ts = "2026-01-01T00:00:00Z";
    sqlx::query(
        "INSERT INTO analytics.scores (
             score_id, ts, storage_partition_id, target_kind, session_id, run_id, name,
             value_type, value_boolean, source, model_or_evaluator
         ) VALUES (
             $1::UUID, $2::TIMESTAMPTZ, '11111111-1111-1111-1111-111111111111',
             'session', '00000000-0000-0000-0000-000000000005'::UUID,
             '00000000-0000-0000-0000-000000000006'::UUID,
             'target_completed', 'boolean', TRUE, 'product_evaluator', 'target_completed@v1'
         ) ON CONFLICT (score_id, ts) DO NOTHING",
    )
    .bind(&score_id)
    .bind(score_ts)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.experiment_score_provenance (
             score_id, score_ts, storage_partition_id, user_id, score_run_id, experiment_run_uid,
             plan_revision_uid, trial_uid, target_session_id, target_execution_run_uid,
             evaluator_id, evaluator_version, score_name, value_type, evidence_ref, evidence_hash
         ) VALUES (
             $1::UUID, $2::TIMESTAMPTZ, $3, NULL, '00000000-0000-0000-0000-000000000006'::UUID, $4::UUID,
             $5::UUID, '00000000-0000-0000-0000-000000000002'::UUID, $6::UUID, $7::UUID,
             'target_completed', 'v1', 'target_completed', 'boolean',
             'session:00000000-0000-0000-0000-000000000005#seq=1', $8::BYTEA
         )",
    )
    .bind(&score_id)
    .bind(score_ts)
    .bind(cell.storage_partition_id)
    .bind(cell.experiment_run_uid)
    .bind(cell.plan_revision_uid)
    .bind(cell.target_session_id)
    .bind(cell.target_execution_run_uid)
    .bind(cell.evidence_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Seeds the exact score-run, experiment-run, and trial rows the linkage
/// constraints reference, for one tenant and one neighbour.
async fn seed_provenance_fixture(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for statement in [
        "INSERT INTO analytics.score_run (run_id, storage_partition_id, user_id, source)
         VALUES ('00000000-0000-0000-0000-000000000006', '11111111-1111-1111-1111-111111111111', NULL, 'experiment_trial')",
        "INSERT INTO moa.experiment_run (
             run_uid, storage_partition_id, user_id, name, target_kind, status, target, variant,
             scorecard, score_run_id, artifact_revision_uids, created_by_identity,
             plan_artifact_uid, resource_envelope, simulator_policy
         ) VALUES (
             '00000000-0000-0000-0000-000000000003', '11111111-1111-1111-1111-111111111111',
             NULL, 'fixture run', 'agent_loop', 'running', '{}'::jsonb, '{}'::jsonb, '{}'::jsonb,
             '00000000-0000-0000-0000-000000000006', '{}', '{}'::jsonb,
             '00000000-0000-4000-8000-0000000d74f0',
             '{\"version\": 1,
                     \"run_limits\": {\"cost_micro_usd\": 0, \"tokens\": 0, \"turns\": 0, \"model_calls\": 0, \"tool_calls\": 0},
                     \"trial_limits\": {\"cost_micro_usd\": 0, \"tokens\": 0, \"turns\": 0, \"model_calls\": 0, \"tool_calls\": 0},
                     \"deadline_at\": \"1970-01-01T00:00:00Z\"}'::jsonb,
             '{}'::jsonb
         )",
        "INSERT INTO moa.experiment_trial (
             trial_uid, run_uid, storage_partition_id, user_id, trial_key, status, target_kind,
             variant_key, plan_revision_uid, simulator, simulator_model, score_run_id,
             resource_envelope
         ) VALUES (
             '00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-000000000003',
             '11111111-1111-1111-1111-111111111111', NULL, 'fixture/0', 'running',
             'agent_loop', 'baseline', '00000000-0000-0000-0000-000000000004', '{}'::jsonb,
             'sim-model', '00000000-0000-0000-0000-000000000006',
             '{\"version\": 1,
                     \"limits\": {\"cost_micro_usd\": 0, \"tokens\": 0, \"turns\": 0, \"model_calls\": 0, \"tool_calls\": 0},
                     \"deadline\": \"1970-01-01T00:00:00Z\"}'::jsonb
         )",
    ] {
        pool.execute(statement).await?;
    }
    Ok(())
}

/// Seeds the minimum row set a learning candidate's foreign keys require.
///
/// A candidate now stands on real referents, so a constraint test cannot use
/// fabricated uuids: the insert would fail for the wrong reason and the test
/// would pass while proving nothing about the state machine. A contact is the
/// cheapest valid referent — a session would additionally drag in the
/// agent-context commit trigger, which has nothing to do with what this test
/// pins.
async fn seed_learning_candidate_fixture(
    pool: &PgPool,
    partition: &str,
    tenant: &str,
) -> Result<uuid::Uuid, Box<dyn std::error::Error + Send + Sync>> {
    let contact_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO contacts (id, contact_id, tenant_id, storage_partition_id, state) \
         VALUES ($1, $1, $2::UUID, $3, 'verified')",
    )
    .bind(contact_id)
    .bind(tenant)
    .bind(partition)
    .execute(pool)
    .await?;
    Ok(contact_id)
}

/// Inserts one candidate of `kind` at `status`, returning whether the write was accepted.
///
/// Candidate and source commit in ONE transaction, and that is not a convenience:
/// learning privacy provenance installs a DEFERRED constraint trigger that refuses to let a
/// transaction commit a candidate with no normalized source. Writing them as two
/// autocommitted statements fails at the first commit — which is the trigger
/// doing its job, and is why the production store writes them together too.
async fn try_insert_candidate(
    pool: &PgPool,
    partition: &str,
    tenant: &str,
    contact_id: uuid::Uuid,
    kind: &str,
    status: &str,
) -> Result<(bool, uuid::Uuid), Box<dyn std::error::Error + Send + Sync>> {
    let candidate_id = uuid::Uuid::now_v7();
    let mut tx = pool.begin().await?;
    let candidate_written = sqlx::query(
        "INSERT INTO learning_candidates \
         (id, tenant_id, storage_partition_id, candidate_type, proposal_kind, status, payload, risk_class) \
         VALUES ($1, $2, $3, 'skill', $4, $5, '{}'::JSONB, 'low')",
    )
    .bind(candidate_id)
    .bind(tenant)
    .bind(partition)
    .bind(kind)
    .bind(status)
    .execute(tx.as_mut())
    .await
    .is_ok();
    if !candidate_written {
        tx.rollback().await?;
        return Ok((false, candidate_id));
    }
    sqlx::query(
        "INSERT INTO learning_candidate_source \
         (id, candidate_id, tenant_id, storage_partition_id, source_kind, contact_id) \
         VALUES ($1, $2, $3, $4, 'contact', $5)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(candidate_id)
    .bind(tenant)
    .bind(partition)
    .bind(contact_id)
    .execute(tx.as_mut())
    .await?;
    let committed = tx.commit().await.is_ok();
    Ok((committed, candidate_id))
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn learning_privacy_provenance_rejects_forbidden_transitions_db() {
    // Pins the two database-level guarantees the review contract rests on, on a
    // fresh database carrying the whole migration set:
    //
    //  1. An informational proposal kind cannot hold a reviewable status. Before
    //     learning privacy provenance, memory/policy/prompt/eval suggestions were written as
    //     `Proposed` and sat on the review queue beside skill drafts that could
    //     actually be accepted, so a reviewer could press accept on something no
    //     code could apply.
    //  2. An advisory item cannot be walked to `Promoted` one legal-looking step
    //     at a time. A CHECK constraint sees one row version; only the transition
    //     trigger sees the pair, and the pair is where that escape lives.
    //
    // Repository-level compare-and-set is defense in depth on top of this, not a
    // substitute: it does not constrain a direct SQL writer.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;

        let tenant = uuid::Uuid::now_v7().to_string();
        let partition = tenant.clone();
        let contact_id = seed_learning_candidate_fixture(&pool, &partition, &tenant).await?;

        // Every reviewable status is refused for an advisory kind, and every
        // informational status is refused for a draft.
        let mut forbidden_accepted = Vec::new();
        for (kind, status) in [
            ("memory_advisory", "proposed"),
            ("memory_advisory", "evaluating"),
            ("memory_advisory", "promoted"),
            ("memory_advisory", "rejected"),
            ("memory_advisory", "rolled_back"),
            ("skill_authoring", "proposed"),
            ("skill_authoring", "promoted"),
            ("policy_authoring", "evaluating"),
            ("prompt_authoring", "rejected"),
            ("eval_authoring", "rolled_back"),
            ("skill_draft", "advisory"),
            ("skill_draft", "needs_authoring"),
            ("skill_draft", "dismissed"),
            ("skill_rollback", "rolled_back"),
            ("skill_rollback", "advisory"),
        ] {
            let (accepted, _) =
                try_insert_candidate(&pool, &partition, &tenant, contact_id, kind, status).await?;
            if accepted {
                forbidden_accepted.push(format!("{kind}/{status}"));
            }
        }

        // Every legal pair is still accepted, so the constraint is not simply
        // refusing everything.
        let mut legal_rejected = Vec::new();
        for (kind, status) in [
            ("skill_draft", "proposed"),
            ("skill_rollback", "proposed"),
            ("memory_advisory", "advisory"),
            ("memory_advisory", "dismissed"),
            ("skill_authoring", "needs_authoring"),
            ("eval_authoring", "dismissed"),
        ] {
            let (accepted, _) =
                try_insert_candidate(&pool, &partition, &tenant, contact_id, kind, status).await?;
            if !accepted {
                legal_rejected.push(format!("{kind}/{status}"));
            }
        }

        // An advisory item may only be dismissed, and its kind may not be
        // rewritten into a reviewable one to escape that.
        let (_, advisory_id) = try_insert_candidate(
            &pool,
            &partition,
            &tenant,
            contact_id,
            "memory_advisory",
            "advisory",
        )
        .await?;
        let promoted_directly =
            sqlx::query("UPDATE learning_candidates SET status = 'promoted' WHERE id = $1")
                .bind(advisory_id)
                .execute(&pool)
                .await
                .is_ok();
        let kind_rewritten = sqlx::query(
            "UPDATE learning_candidates SET proposal_kind = 'skill_draft', status = 'proposed' \
             WHERE id = $1",
        )
        .bind(advisory_id)
        .execute(&pool)
        .await
        .is_ok();
        let dismissed =
            sqlx::query("UPDATE learning_candidates SET status = 'dismissed' WHERE id = $1")
                .bind(advisory_id)
                .execute(&pool)
                .await
                .is_ok();

        // A candidate with no normalized source must not be committable at all.
        // Without this, a producer could file learning that no erasure could ever
        // reach and no export could ever explain — the original defect, reachable
        // again through a single forgotten insert.
        let sourceless_committed = {
            let mut tx = pool.begin().await?;
            sqlx::query(
                "INSERT INTO learning_candidates \
                 (id, tenant_id, storage_partition_id, candidate_type, proposal_kind, status, \
                  payload, risk_class) \
                 VALUES ($1, $2, $3, 'skill', 'skill_draft', 'proposed', '{}'::JSONB, 'low')",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(&tenant)
            .bind(&partition)
            .execute(tx.as_mut())
            .await?;
            tx.commit().await.is_ok()
        };

        // A skill draft may not skip the claim: `Proposed -> Promoted` directly
        // would let two reviewers both succeed at contradictory decisions.
        let (_, draft_id) = try_insert_candidate(
            &pool,
            &partition,
            &tenant,
            contact_id,
            "skill_draft",
            "proposed",
        )
        .await?;
        let skipped_claim =
            sqlx::query("UPDATE learning_candidates SET status = 'promoted' WHERE id = $1")
                .bind(draft_id)
                .execute(&pool)
                .await
                .is_ok();

        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            forbidden_accepted,
            legal_rejected,
            promoted_directly,
            kind_rewritten,
            dismissed,
            skipped_claim,
            sourceless_committed,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (
        forbidden_accepted,
        legal_rejected,
        promoted_directly,
        kind_rewritten,
        dismissed,
        skipped_claim,
        sourceless_committed,
    ) = outcome.expect("proposal-kind constraint probe should complete");

    assert!(
        forbidden_accepted.is_empty(),
        "these (proposal_kind, status) pairs must be rejected but were accepted: {forbidden_accepted:?}"
    );
    assert!(
        legal_rejected.is_empty(),
        "these legal (proposal_kind, status) pairs were rejected: {legal_rejected:?}"
    );
    assert!(
        !promoted_directly,
        "an advisory item must never reach `promoted`; no materializer exists for it"
    );
    assert!(
        !kind_rewritten,
        "rewriting proposal_kind must be refused, or an advisory item could be laundered into a reviewable draft"
    );
    assert!(
        dismissed,
        "dismissal is the one transition an advisory item admits and it must still work"
    );
    assert!(
        !skipped_claim,
        "a skill draft must pass through `evaluating`; skipping the claim would let two reviewers both succeed"
    );
    assert!(
        !sourceless_committed,
        "a candidate with no normalized source must not be committable: it could never be erased or explained"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn learning_log_final_schema_requires_normalized_source_db() {
    // Pins: every committed learning-log row has normalized provenance, while a
    // row and its source may still be committed atomically in one transaction.
    let admin_url = test_database_url();
    let db_name = unique_db_name();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database for learning-log provenance");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create learning-log provenance throwaway database");
    let target_url = with_database(&admin_url, &db_name);

    let outcome = async {
        clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;

        let tenant = uuid::Uuid::now_v7().to_string();
        let partition = tenant.clone();
        let contact_id = seed_learning_candidate_fixture(&pool, &partition, &tenant).await?;
        let (_, candidate_id) = try_insert_candidate(
            &pool,
            &partition,
            &tenant,
            contact_id,
            "skill_draft",
            "proposed",
        )
        .await?;

        let sourceless_committed = {
            let mut tx = pool.begin().await?;
            sqlx::query(
                "INSERT INTO learning_log \
                 (id, tenant_id, storage_partition_id, learning_type, target_id, payload, actor, \
                  valid_from, version) \
                 VALUES ($1, $2, $3, 'skill_created', 'target', '{}'::JSONB, 'test', now(), 1)",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(&tenant)
            .bind(&partition)
            .execute(tx.as_mut())
            .await?;
            tx.commit().await.is_ok()
        };

        let attributed_committed = {
            let learning_id = uuid::Uuid::now_v7();
            let mut tx = pool.begin().await?;
            sqlx::query(
                "INSERT INTO learning_log \
                 (id, tenant_id, storage_partition_id, learning_type, target_id, payload, actor, \
                  valid_from, version) \
                 VALUES ($1, $2, $3, 'skill_created', 'target', '{}'::JSONB, 'test', now(), 1)",
            )
            .bind(learning_id)
            .bind(&tenant)
            .bind(&partition)
            .execute(tx.as_mut())
            .await?;
            sqlx::query(
                "INSERT INTO learning_log_source \
                 (id, learning_id, tenant_id, storage_partition_id, source_kind, candidate_id) \
                 VALUES ($1, $2, $3, $4, 'candidate', $5)",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(learning_id)
            .bind(&tenant)
            .bind(&partition)
            .bind(candidate_id)
            .execute(tx.as_mut())
            .await?;
            tx.commit().await.is_ok()
        };

        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            sourceless_committed,
            attributed_committed,
        ))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (sourceless_committed, attributed_committed) =
        outcome.expect("learning-log completeness probe should complete");
    assert!(
        !sourceless_committed,
        "a learning-log entry with no normalized source must not commit"
    );
    assert!(
        attributed_committed,
        "a learning-log row and normalized source must commit atomically"
    );
}

/// Durable lineage acceptance queue.
const LINEAGE_JOURNAL_SQL: &str =
    include_str!("../../migrations/postgres/V000042__lineage_journal.sql");

#[test]
fn lineage_journal_ownership_is_registered_offline() {
    // Pins: the queue is tenant-scoped, so it needs an ownership row. Without one
    // the tenant-purge catalog scan only discovers it at runtime against a live
    // database, which is where the last six unregistered tables were found.
    assert!(
        MIGRATION_OWNERSHIP.contains("name = \"lineage_journal\""),
        "lineage journal's table must be registered in migration-ownership.toml"
    );
    // Row-level security admits the control plane only. A tenant-scoped request
    // connection has no legitimate reason to read pending lineage payloads, and
    // the queue is deliberately cross-tenant so one drain can batch across
    // partitions.
    assert!(
        LINEAGE_JOURNAL_SQL.contains("FORCE ROW LEVEL SECURITY")
            && LINEAGE_JOURNAL_SQL.contains("moa.current_control_plane()"),
        "the queue must be FORCE-RLS behind the control-plane predicate"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn lineage_journal_final_schema_is_durable_and_idempotent_db() {
    // Pins: lineage journal installs the durable acceptance queue on a pristine database
    // and re-applies as a no-op, and the database itself enforces the two
    // properties the writer's correctness rests on: claim eligibility is derived
    // from the lease pair (so it cannot drift into permanently unclaimable), and
    // a half-leased row cannot exist.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = async {
        let (first, second) = clean_apply_then_reapply(&target_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&target_url)
            .await?;
        let facts = lineage_journal_facts(&pool).await?;
        pool.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((first, second, facts))
    }
    .await;

    drop_database_with_zero_connections(&admin, &db_name).await;
    admin.close().await;

    let (first, second, facts) =
        outcome.expect("lineage journal migration should apply on a fresh database");

    assert!(
        first
            .iter()
            .any(|applied| applied.contains("lineage_journal")),
        "a pristine database must apply lineage journal, got {first:?}"
    );
    assert!(
        second.is_empty(),
        "re-applying must report no newly applied migrations, got {second:?}"
    );
    assert!(
        facts.claim_index_exists,
        "the drain reads through lineage_journal_claim_idx on every poll; without it every claim \
         is a sequential scan of the whole backlog"
    );
    assert!(
        facts.forces_row_level_security,
        "the queue holds cross-tenant lineage payloads; RLS must be FORCED, not merely enabled"
    );
    assert_eq!(
        facts.policy_names,
        vec!["lineage_journal_runtime_only".to_string()],
        "exactly one policy may exist, and it is the control-plane-only one"
    );
    assert_eq!(
        facts.unleased_claimable_at, facts.unleased_available_at,
        "an unleased row must be claimable at available_at"
    );
    assert_eq!(
        facts.leased_claimable_at, facts.leased_lease_expires_at,
        "stamping a lease in the future must push claimable_at to the lease expiry, with no \
         separate column for a claimant to forget to update"
    );
    assert_eq!(
        facts.half_lease_sqlstate.as_deref(),
        Some("23514"),
        "a half-leased row must fail with a check-constraint violation"
    );
    assert_eq!(
        facts.half_lease_constraint.as_deref(),
        Some("lineage_journal_lease_pair_check"),
        "the lease-pair constraint, not an unrelated tenant fence, must reject the row"
    );
}

/// Observable facts about the installed lineage acceptance queue.
struct LineageJournalFacts {
    claim_index_exists: bool,
    forces_row_level_security: bool,
    policy_names: Vec<String>,
    half_lease_sqlstate: Option<String>,
    half_lease_constraint: Option<String>,
    unleased_claimable_at: chrono::DateTime<chrono::Utc>,
    unleased_available_at: chrono::DateTime<chrono::Utc>,
    leased_claimable_at: chrono::DateTime<chrono::Utc>,
    leased_lease_expires_at: chrono::DateTime<chrono::Utc>,
}

async fn lineage_journal_facts(
    pool: &PgPool,
) -> Result<LineageJournalFacts, Box<dyn std::error::Error + Send + Sync>> {
    let claim_index_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE schemaname = 'analytics' \
         AND tablename = 'lineage_journal' AND indexname = 'lineage_journal_claim_idx')",
    )
    .fetch_one(pool)
    .await?;
    let forces_row_level_security: bool = sqlx::query_scalar(
        "SELECT relrowsecurity AND relforcerowsecurity FROM pg_class \
         WHERE oid = 'analytics.lineage_journal'::regclass",
    )
    .fetch_one(pool)
    .await?;
    let policy_names: Vec<String> = sqlx::query_scalar(
        "SELECT policyname FROM pg_policies WHERE schemaname = 'analytics' \
         AND tablename = 'lineage_journal' ORDER BY policyname",
    )
    .fetch_all(pool)
    .await?;

    let facts_partition = uuid::Uuid::now_v7().to_string();
    let unleased_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO analytics.lineage_journal \
         (journal_id, storage_partition_id, event_class, payload, available_at) \
         VALUES ($1, $2, 'lineage', '{}'::jsonb, now() + interval '30 seconds')",
    )
    .bind(unleased_id)
    .bind(&facts_partition)
    .execute(pool)
    .await?;
    let (unleased_claimable_at, unleased_available_at) = sqlx::query_as(
        "SELECT claimable_at, available_at FROM analytics.lineage_journal WHERE journal_id = $1",
    )
    .bind(unleased_id)
    .fetch_one(pool)
    .await?;

    sqlx::query(
        "UPDATE analytics.lineage_journal SET lease_owner = gen_random_uuid(), \
         lease_expires_at = now() + interval '10 minutes' WHERE journal_id = $1",
    )
    .bind(unleased_id)
    .execute(pool)
    .await?;
    let (leased_claimable_at, leased_lease_expires_at) = sqlx::query_as(
        "SELECT claimable_at, lease_expires_at FROM analytics.lineage_journal \
         WHERE journal_id = $1",
    )
    .bind(unleased_id)
    .fetch_one(pool)
    .await?;

    let half_lease_error = sqlx::query(
        "INSERT INTO analytics.lineage_journal \
         (journal_id, storage_partition_id, event_class, payload, lease_owner) \
         VALUES (gen_random_uuid(), $1, 'lineage', '{}'::jsonb, gen_random_uuid())",
    )
    .bind(&facts_partition)
    .execute(pool)
    .await
    .expect_err("a half-leased lineage row must violate the lease-pair constraint");
    let half_lease_sqlstate = half_lease_error
        .as_database_error()
        .and_then(|error| error.code().map(|code| code.into_owned()));
    let half_lease_constraint = half_lease_error
        .as_database_error()
        .and_then(|error| error.constraint().map(ToOwned::to_owned));

    Ok(LineageJournalFacts {
        claim_index_exists,
        forces_row_level_security,
        policy_names,
        half_lease_sqlstate,
        half_lease_constraint,
        unleased_claimable_at,
        unleased_available_at,
        leased_claimable_at,
        leased_lease_expires_at,
    })
}
