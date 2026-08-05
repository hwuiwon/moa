//! One-way execution-plan v2 and durable-compensation migration scenarios.

use moa_artifacts::execution_plan::PlanAmendment;
use moa_execution::{
    CanonicalExecutionPlan,
    capability::{amendment_hash, plan_hash},
};
use serde_json::{Value, json};

use super::support::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[test]
fn execution_plan_compensation_is_the_next_forward_migration_offline() {
    // Pins: plan v2 follows the one-way session lifecycle cutover with no gap
    // or compatibility migration between them.
    assert_eq!(
        migration_version("execution_plan_compensation")
            .expect("the execution-plan compensation migration must be embedded"),
        55
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn inactive_v1_run_rewrites_hashes_and_installs_v2_constraints_db() {
    // Pins: the production runner performs the Rust rewrite before V55, keeps
    // confirmation/provenance bound to the new canonical hash, restores the
    // run trigger, and applies no work on an exact replay.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create execution-plan v2 migration database");
    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        apply_through_migration(database.target_url(), "session_status_idle").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let seeded = seed_v1_run(&target, true).await?;

        let first = run_reporting_applied_serialized(database.target_url()).await?;
        let row: (
            Value,
            Value,
            String,
            String,
            Option<String>,
            Value,
            bool,
            bool,
            Value,
        ) = sqlx::query_as(
            "SELECT initial_plan, active_plan, initial_plan_hash, active_plan_hash, \
                        confirmed_plan_hash, source_provenance, \
                        moa.execution_plan_snapshot_is_v2(initial_plan), \
                        moa.execution_plan_snapshot_is_v2(active_plan), plan_history \
                 FROM moa.execution_run WHERE run_uid = $1",
        )
        .bind(seeded.run_uid)
        .fetch_one(&target)
        .await?;
        let initial: CanonicalExecutionPlan = serde_json::from_value(row.0.clone())?;
        let active: CanonicalExecutionPlan = serde_json::from_value(row.1.clone())?;
        let initial_hash = plan_hash(&initial.definition)?.to_string();
        let active_hash = plan_hash(&active.definition)?.to_string();
        let amendment: PlanAmendment = serde_json::from_value(row.8[0]["amendment"].clone())?;
        let rewritten_amendment_hash = amendment_hash(&amendment)?.to_string();
        let trigger_enabled: bool = sqlx::query_scalar(
            "SELECT tgenabled <> 'D' FROM pg_catalog.pg_trigger \
             WHERE tgrelid = 'moa.execution_run'::REGCLASS \
               AND tgname = 'execution_run_update_guard'",
        )
        .fetch_one(&target)
        .await?;
        let compensation_schema: bool = sqlx::query_scalar(
            "SELECT to_regclass('moa.execution_compensation') IS NOT NULL \
                AND EXISTS (SELECT 1 FROM information_schema.columns \
                            WHERE table_schema = 'moa' \
                              AND table_name = 'execution_compensation' \
                              AND column_name = 'started_at') \
                AND EXISTS (SELECT 1 FROM information_schema.columns \
                            WHERE table_schema = 'moa' \
                              AND table_name = 'execution_compensation' \
                              AND column_name = 'completed_at') \
                AND EXISTS (SELECT 1 FROM pg_catalog.pg_trigger \
                            WHERE tgrelid = 'moa.execution_compensation'::REGCLASS \
                              AND tgname = 'execution_compensation_update_guard')",
        )
        .fetch_one(&target)
        .await?;
        let hard_outbox_schema: bool = sqlx::query_scalar(
            "SELECT \
                EXISTS (SELECT 1 FROM information_schema.columns \
                        WHERE table_schema = 'moa' \
                          AND table_name = 'execution_action_review_outbox' \
                          AND column_name = 'operation_id') \
                AND EXISTS (SELECT 1 FROM information_schema.columns \
                            WHERE table_schema = 'moa' \
                              AND table_name = 'execution_action_review_outbox' \
                              AND column_name = 'owner_kind') \
                AND NOT EXISTS (SELECT 1 FROM information_schema.columns \
                                WHERE table_schema = 'moa' \
                                  AND table_name = 'execution_action_review_outbox' \
                                  AND column_name = 'task_id')",
        )
        .fetch_one(&target)
        .await?;
        let compensation_reason: Option<String> = sqlx::query_scalar(
            "SELECT moa.execution_terminal_reason_for( \
                'failed', $1::JSONB, 'generated_plan' \
             )",
        )
        .bind(json!({
            "kind": "compensation_failure",
            "original_status": "cancelled",
            "original_reason": "cancelled",
            "original_cause": {"kind": "cancellation"},
            "compensation_id": uuid::Uuid::new_v4(),
            "outcome": {
                "kind": "unknown_outcome",
                "message": "upstream committed before disconnect",
                "usage": {
                    "cost_microusd": 0,
                    "tokens": 0,
                    "tool_calls": 1,
                    "retrieved_bytes": 0
                }
            }
        }))
        .fetch_one(&target)
        .await?;
        let transition_evidence =
            exercise_compensation_transitions(&target, seeded.run_uid, seeded.tenant_id).await?;
        let unconfirmed_pending_terminal =
            accepts_unconfirmed_pending_terminal(&target, seeded.run_uid).await?;
        target.close().await;
        let second = run_reporting_applied_serialized(database.target_url()).await?;

        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            row,
            initial_hash,
            active_hash,
            trigger_enabled,
            compensation_schema,
            hard_outbox_schema,
            compensation_reason,
            transition_evidence,
            unconfirmed_pending_terminal,
            seeded.old_hash,
            rewritten_amendment_hash,
        ))
    }
    .await;

    let outcome = database.finish(outcome).await;
    let (
        first,
        second,
        row,
        initial_hash,
        active_hash,
        trigger_enabled,
        compensation_schema,
        hard_outbox_schema,
        compensation_reason,
        transition_evidence,
        unconfirmed_pending_terminal,
        old_hash,
        rewritten_amendment_hash,
    ) = outcome.expect("inactive v1 run should migrate to v2");
    assert_eq!(
        first,
        vec![
            expected_migration_labels()
                .last()
                .expect("V55 label must exist")
                .clone()
        ]
    );
    assert!(
        second.is_empty(),
        "exact migration replay must apply no SQL"
    );
    assert_eq!(row.0["definition"]["schema_version"], json!(2));
    assert_eq!(
        row.0["definition"]["cancel_policy"],
        json!("retain_effects")
    );
    assert_eq!(row.0["definition"]["nodes"][0]["compensation"], Value::Null);
    assert_eq!(
        row.1["definition"]["nodes"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(row.2, initial_hash);
    assert_eq!(row.3, active_hash);
    assert_eq!(row.4.as_deref(), Some(initial_hash.as_str()));
    assert_eq!(
        row.5["planner"]["final_plan_hash"],
        json!(initial_hash),
        "generated-plan provenance must remain bound to the rewritten initial plan"
    );
    assert!(row.6 && row.7);
    assert_eq!(row.8[0]["amendment"]["schema_version"], json!(2));
    assert_eq!(
        row.8[0]["amendment"]["operations"][0]["node"]["compensation"],
        Value::Null
    );
    assert_eq!(row.8[0]["amendment_hash"], json!(rewritten_amendment_hash));
    assert_eq!(row.8[0]["active_plan_hash"], json!(active_hash));
    assert_ne!(row.2, old_hash, "schema v2 must receive a new plan hash");
    assert!(trigger_enabled);
    assert!(compensation_schema);
    assert!(hard_outbox_schema);
    assert_eq!(compensation_reason.as_deref(), Some("compensation_failed"));
    assert_eq!(
        transition_evidence.retry_state,
        ("pending".to_string(), 2, 2)
    );
    assert!(transition_evidence.retry_kept_outcome_and_started_at);
    assert!(transition_evidence.reclaim_kept_outcome_and_started_at);
    assert_eq!(transition_evidence.terminal_audit_count, 2);
    assert!(transition_evidence.budget_rejection_timestamp_is_atomic);
    assert!(transition_evidence.mapping_failure_insert_is_terminal);
    assert!(transition_evidence.revoked_review_is_terminal);
    assert!(unconfirmed_pending_terminal);
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn active_v1_run_blocks_cutover_without_applying_v55_db() {
    // Pins: admission/drain is a real deployment gate; the migration refuses an
    // acknowledged active v1 run before V55 DDL and preserves the V54 history.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create active execution-run migration database");
    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        apply_through_migration(database.target_url(), "session_status_idle").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let seeded = seed_v1_run(&target, false).await?;
        target.close().await;

        let error = run_reporting_applied_serialized(database.target_url())
            .await
            .expect_err("active v1 run must block the cutover");
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let schema_version: String = sqlx::query_scalar(
            "SELECT initial_plan #>> '{definition,schema_version}' \
             FROM moa.execution_run WHERE run_uid = $1",
        )
        .bind(seeded.run_uid)
        .fetch_one(&target)
        .await?;
        let v55_applied: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM public.refinery_schema_history WHERE version = 55)",
        )
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            format!("{error:#}"),
            schema_version,
            v55_applied,
        ))
    }
    .await;

    let outcome = database.finish(outcome).await;
    let (error, schema_version, v55_applied) = outcome.expect("inspect active-run rejection");
    assert!(
        error.contains("requires all v1 runs to be inactive"),
        "{error}"
    );
    assert_eq!(schema_version, "1");
    assert!(!v55_applied);
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn mixed_plan_versions_fail_before_rewrite_or_v55_ddl_db() {
    // Pins: a partially rewritten snapshot is diagnosed instead of being
    // laundered into v2, and the rejection leaves both data and history intact.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create mixed-version migration database");
    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        apply_through_migration(database.target_url(), "session_status_idle").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let seeded = seed_v1_run(&target, false).await?;
        sqlx::raw_sql("ALTER TABLE moa.execution_run DISABLE TRIGGER execution_run_update_guard;")
            .execute(&target)
            .await?;
        sqlx::query(
            "UPDATE moa.execution_run \
             SET active_plan = jsonb_set( \
                 active_plan, '{definition,schema_version}', '2'::JSONB \
             ) WHERE run_uid = $1",
        )
        .bind(seeded.run_uid)
        .execute(&target)
        .await?;
        sqlx::raw_sql("ALTER TABLE moa.execution_run ENABLE TRIGGER execution_run_update_guard;")
            .execute(&target)
            .await?;
        target.close().await;

        let error = run_reporting_applied_serialized(database.target_url())
            .await
            .expect_err("mixed plan versions must fail closed");
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let versions: (String, String) = sqlx::query_as(
            "SELECT initial_plan #>> '{definition,schema_version}', \
                    active_plan #>> '{definition,schema_version}' \
             FROM moa.execution_run WHERE run_uid = $1",
        )
        .bind(seeded.run_uid)
        .fetch_one(&target)
        .await?;
        let v55_applied: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM public.refinery_schema_history WHERE version = 55)",
        )
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            format!("{error:#}"),
            versions,
            v55_applied,
        ))
    }
    .await;

    let outcome = database.finish(outcome).await;
    let (error, versions, v55_applied) = outcome.expect("inspect mixed-version rejection");
    assert!(
        error.contains("malformed or mixed plan versions"),
        "{error}"
    );
    assert_eq!(versions, ("1".to_string(), "2".to_string()));
    assert!(!v55_applied);
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn corrupt_v1_amendment_history_rolls_back_the_cutover_db() {
    // Pins: every legacy amendment hash is verified before it is upgraded and
    // replayed; corrupt history rolls back the trigger toggle, run rewrite, and
    // V55 DDL as one atomic failure.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create corrupt execution-history migration database");
    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        apply_through_migration(database.target_url(), "session_status_idle").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let seeded = seed_v1_run(&target, true).await?;
        sqlx::raw_sql("ALTER TABLE moa.execution_run DISABLE TRIGGER execution_run_update_guard;")
            .execute(&target)
            .await?;
        sqlx::query(
            "UPDATE moa.execution_run \
             SET plan_history = jsonb_set( \
                 plan_history, '{0,amendment_hash}', to_jsonb(repeat('f', 64)) \
             ) WHERE run_uid = $1",
        )
        .bind(seeded.run_uid)
        .execute(&target)
        .await?;
        sqlx::raw_sql("ALTER TABLE moa.execution_run ENABLE TRIGGER execution_run_update_guard;")
            .execute(&target)
            .await?;
        target.close().await;

        let error = run_reporting_applied_serialized(database.target_url())
            .await
            .expect_err("corrupt amendment evidence must reject the cutover");
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let evidence: (String, String, bool, bool) = sqlx::query_as(
            "SELECT initial_plan #>> '{definition,schema_version}', \
                    plan_history #>> '{0,amendment_hash}', \
                    EXISTS (SELECT 1 FROM public.refinery_schema_history WHERE version = 55), \
                    (SELECT tgenabled <> 'D' FROM pg_catalog.pg_trigger \
                     WHERE tgrelid = 'moa.execution_run'::REGCLASS \
                       AND tgname = 'execution_run_update_guard') \
             FROM moa.execution_run WHERE run_uid = $1",
        )
        .bind(seeded.run_uid)
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((format!("{error:#}"), evidence))
    }
    .await;

    let outcome = database.finish(outcome).await;
    let (error, evidence) = outcome.expect("inspect corrupt amendment rejection");
    assert!(error.contains("amendment hash is corrupt"), "{error}");
    assert_eq!(evidence.0, "1");
    assert_eq!(evidence.1, "f".repeat(64));
    assert!(!evidence.2);
    assert!(evidence.3, "failed cutover must restore the update trigger");
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn v1_skill_template_rewrites_definition_source_hash_and_validation_db() {
    // Pins: the one-way cutover updates the complete immutable revision evidence
    // together and leaves a hard-v2 document that replays without another rewrite.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create skill-template migration database");
    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        apply_through_migration(database.target_url(), "session_status_idle").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let tenant_id = uuid::Uuid::new_v4();
        let artifact_uid = uuid::Uuid::new_v4();
        let revision_uid = uuid::Uuid::new_v4();
        let definition = json!({
            "api_version": "moa.artifact/v1",
            "kind": "skill",
            "metadata": {"name": "legacy"},
            "definition": {
                "type": "skill",
                "spec": {
                    "execution_plan": {
                        "goal": {
                            "requirements": [],
                            "deliverables": [],
                            "coverage": [],
                            "constraints": [],
                            "completion_checks": []
                        },
                        "plan": v1_plan_definition()
                    }
                }
            }
        });
        let source_text = serde_yaml::to_string(&definition)?.into_bytes();
        sqlx::query(
            "INSERT INTO moa.artifact \
                (artifact_uid, storage_partition_id, tenant_id, kind, name) \
             VALUES ($1, $2, $3, 'skill', $4)",
        )
        .bind(artifact_uid)
        .bind(tenant_id.to_string())
        .bind(tenant_id)
        .bind(format!("legacy-template-{artifact_uid}"))
        .execute(&target)
        .await?;
        sqlx::query(
            "INSERT INTO moa.artifact_revision ( \
                revision_uid, artifact_uid, storage_partition_id, tenant_id, \
                definition, canonical_hash, source_format, source_text, status, version \
             ) VALUES ($1, $2, $3, $4, $5, decode(repeat('0', 64), 'hex'), \
                       'yaml', $6, 'draft', 1)",
        )
        .bind(revision_uid)
        .bind(artifact_uid)
        .bind(tenant_id.to_string())
        .bind(tenant_id)
        .bind(&definition)
        .bind(source_text)
        .execute(&target)
        .await?;
        let first = run_reporting_applied_serialized(database.target_url()).await?;
        let row: (Value, Vec<u8>, Vec<u8>, Value, bool) = sqlx::query_as(
            "SELECT definition, canonical_hash, source_text, validation_report, \
                    moa.skill_execution_template_is_v2(definition) \
             FROM moa.artifact_revision WHERE revision_uid = $1",
        )
        .bind(revision_uid)
        .fetch_one(&target)
        .await?;
        let document: moa_artifacts::document::ArtifactDocument =
            serde_json::from_value(row.0.clone())?;
        let expected_hash = moa_artifacts::canonical::canonical_hash(&document)?;
        let rendered_source = String::from_utf8(row.2.clone())?;
        let source_document =
            moa_artifacts::document::ArtifactDocument::from_yaml(&rendered_source)?;
        let second = run_reporting_applied_serialized(database.target_url()).await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            second,
            row,
            expected_hash,
            source_document,
            document,
        ))
    }
    .await;

    let outcome = database.finish(outcome).await;
    let (first, second, row, expected_hash, source_document, document) =
        outcome.expect("inspect skill-template rewrite");
    assert!(!first.is_empty(), "the first cutover must apply V55");
    assert!(second.is_empty(), "exact replay must apply no migrations");
    assert_eq!(
        row.0
            .pointer("/definition/spec/execution_plan/plan/schema_version"),
        Some(&json!(2))
    );
    assert_eq!(
        row.0
            .pointer("/definition/spec/execution_plan/plan/cancel_policy"),
        Some(&json!("retain_effects"))
    );
    assert_eq!(row.1, expected_hash);
    assert_eq!(source_document, document);
    assert!(
        row.3.get("errors").is_some(),
        "validation evidence must be regenerated"
    );
    assert!(
        row.4,
        "the installed V55 hard-v2 predicate must accept the rewrite"
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn corrupt_v1_skill_template_rolls_back_every_revision_rewrite_db() {
    // Pins: skill-template cutover is one transaction. A later corrupt revision
    // cannot leave an earlier immutable revision with partially rewritten source
    // or hash evidence while V55 remains unapplied.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create corrupt skill-template migration database");
    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        apply_through_migration(database.target_url(), "session_status_idle").await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let tenant_id = uuid::Uuid::new_v4();
        let valid_artifact_uid = uuid::Uuid::parse_str("10000000-0000-4000-8000-000000000001")?;
        let corrupt_artifact_uid = uuid::Uuid::parse_str("10000000-0000-4000-8000-000000000002")?;
        let valid_revision_uid = uuid::Uuid::parse_str("20000000-0000-4000-8000-000000000001")?;
        let corrupt_revision_uid = uuid::Uuid::parse_str("20000000-0000-4000-8000-000000000002")?;
        let skill_document = |name: &str, plan: Value| {
            json!({
                "api_version": "moa.artifact/v1",
                "kind": "skill",
                "metadata": {"name": name},
                "definition": {
                    "type": "skill",
                    "spec": {
                        "execution_plan": {
                            "goal": {
                                "requirements": [],
                                "deliverables": [],
                                "coverage": [],
                                "constraints": [],
                                "completion_checks": []
                            },
                            "plan": plan
                        }
                    }
                }
            })
        };
        let valid_definition = skill_document("valid-first", v1_plan_definition());
        let mut corrupt_plan = v1_plan_definition();
        corrupt_plan["nodes"] = json!("not-an-array");
        let corrupt_definition = skill_document("corrupt-second", corrupt_plan);
        let valid_source = serde_yaml::to_string(&valid_definition)?.into_bytes();
        let corrupt_source = serde_yaml::to_string(&corrupt_definition)?.into_bytes();

        for (artifact_uid, revision_uid, definition, source) in [
            (
                valid_artifact_uid,
                valid_revision_uid,
                &valid_definition,
                &valid_source,
            ),
            (
                corrupt_artifact_uid,
                corrupt_revision_uid,
                &corrupt_definition,
                &corrupt_source,
            ),
        ] {
            sqlx::query(
                "INSERT INTO moa.artifact \
                    (artifact_uid, storage_partition_id, tenant_id, kind, name) \
                 VALUES ($1, $2, $3, 'skill', $4)",
            )
            .bind(artifact_uid)
            .bind(tenant_id.to_string())
            .bind(tenant_id)
            .bind(format!("skill-{artifact_uid}"))
            .execute(&target)
            .await?;
            sqlx::query(
                "INSERT INTO moa.artifact_revision ( \
                    revision_uid, artifact_uid, storage_partition_id, tenant_id, \
                    definition, canonical_hash, source_format, source_text, status, version \
                 ) VALUES ($1, $2, $3, $4, $5, decode(repeat('0', 64), 'hex'), \
                           'yaml', $6, 'draft', 1)",
            )
            .bind(revision_uid)
            .bind(artifact_uid)
            .bind(tenant_id.to_string())
            .bind(tenant_id)
            .bind(definition)
            .bind(source)
            .execute(&target)
            .await?;
        }

        target.close().await;
        let error = run_reporting_applied_serialized(database.target_url())
            .await
            .expect_err("corrupt skill template must reject the cutover");
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let rows: Vec<(uuid::Uuid, Value, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT revision_uid, definition, canonical_hash, source_text \
             FROM moa.artifact_revision \
             WHERE revision_uid IN ($1, $2) ORDER BY revision_uid",
        )
        .bind(valid_revision_uid)
        .bind(corrupt_revision_uid)
        .fetch_all(&target)
        .await?;
        let v55_applied: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM public.refinery_schema_history WHERE version = 55)",
        )
        .fetch_one(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            format!("{error:#}"),
            rows,
            v55_applied,
            valid_definition,
            corrupt_definition,
            valid_source,
            corrupt_source,
        ))
    }
    .await;

    let outcome = database.finish(outcome).await;
    let (
        error,
        rows,
        v55_applied,
        valid_definition,
        corrupt_definition,
        valid_source,
        corrupt_source,
    ) = outcome.expect("inspect corrupt skill-template rollback");
    assert!(error.contains("has no nodes array"), "{error}");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, valid_definition);
    assert_eq!(rows[0].2, vec![0; 32]);
    assert_eq!(rows[0].3, valid_source);
    assert_eq!(rows[1].1, corrupt_definition);
    assert_eq!(rows[1].2, vec![0; 32]);
    assert_eq!(rows[1].3, corrupt_source);
    assert!(!v55_applied, "failed cutover must not record V55");
}

struct SeededV1Run {
    run_uid: uuid::Uuid,
    tenant_id: uuid::Uuid,
    old_hash: String,
}

async fn seed_v1_run(target: &PgPool, terminal: bool) -> TestResult<SeededV1Run> {
    let tenant_id = uuid::Uuid::new_v4();
    let session_id = uuid::Uuid::new_v4();
    let planning_context_uid = uuid::Uuid::new_v4();
    let run_uid = uuid::Uuid::new_v4();
    let definition = v1_plan_definition();
    let old_hash = legacy_hash("moa.execution.plan", &definition, true)?;
    let amendment_node = v1_node("amended-output", vec!["output"]);
    let amendment = json!({
        "schema_version": 1,
        "base_plan_revision": 1,
        "reason": "migration history fixture",
        "evidence": {},
        "operations": [{"kind": "add_node", "node": amendment_node}]
    });
    let old_amendment_hash = legacy_hash("moa.execution.amendment", &amendment, false)?;
    let mut active_definition = definition.clone();
    active_definition["nodes"]
        .as_array_mut()
        .expect("fixture nodes must be an array")
        .push(v1_node("amended-output", vec!["output"]));
    let old_active_hash = legacy_hash("moa.execution.plan", &active_definition, true)?;
    let initial_snapshot = json!({
        "definition": definition,
        "plan_hash": old_hash,
        "catalog_hash": "0".repeat(64),
        "estimate": {
            "cost_microusd": 0,
            "tokens": 0,
            "tool_calls": 0,
            "retrieved_bytes": 0,
            "tasks": 1
        },
        "report": {"issues": []}
    });
    let active_snapshot = json!({
        "definition": active_definition,
        "plan_hash": old_active_hash,
        "catalog_hash": "0".repeat(64),
        "estimate": {
            "cost_microusd": 0,
            "tokens": 0,
            "tool_calls": 0,
            "retrieved_bytes": 0,
            "tasks": 2
        },
        "report": {"issues": []}
    });
    let plan_history = json!([{
        "base_plan_revision": 1,
        "plan_revision": 2,
        "amendment": amendment,
        "amendment_hash": old_amendment_hash,
        "outcome": "applied",
        "task_ids_to_release": [],
        "active_plan_hash": old_active_hash,
        "reason": "migration history fixture",
        "requirement_mapping": {},
        "failure_fingerprint": "4".repeat(64),
        "failure_fingerprint_count": 1,
        "recorded_at": "2026-08-04T00:00:00Z"
    }]);
    sqlx::query(
        "INSERT INTO moa.execution_planning_context ( \
            planning_context_uid, tenant_id, session_id, \
            originating_user_sequence_num, originating_user_event_hash, \
            owner_user_id, planning_context_hash, snapshot \
         ) VALUES ($1, $2, $3, 0, $4, 'migration-test', $4, '{}'::JSONB)",
    )
    .bind(planning_context_uid)
    .bind(tenant_id)
    .bind(session_id)
    .bind("1".repeat(64))
    .execute(target)
    .await?;
    sqlx::query(
        "INSERT INTO moa.execution_run ( \
            run_uid, tenant_id, session_id, originating_user_sequence_num, \
            planning_context_uid, planning_context_hash, owner_user_id, goal_contract, \
            initial_plan, active_plan, initial_plan_hash, active_plan_hash, \
            capability_catalog, authorization_envelope, source_provenance, source_kind, \
            input, status \
         ) VALUES ( \
            $1, $2, $3, 0, $4, $5, 'migration-test', $6, $7, $7, $8, $8, \
            $9, $10, $11, 'generated_plan', '{}'::JSONB, 'awaiting_confirmation' \
         )",
    )
    .bind(run_uid)
    .bind(tenant_id)
    .bind(session_id)
    .bind(planning_context_uid)
    .bind("1".repeat(64))
    .bind(json!({
        "objective": "migration",
        "requirements": [],
        "deliverables": [],
        "coverage": [],
        "constraints": [],
        "completion_checks": []
    }))
    .bind(&initial_snapshot)
    .bind(&old_hash)
    .bind(json!({
        "schema_version": 1,
        "capabilities": [],
        "catalog_hash": "0".repeat(64)
    }))
    .bind(json!({"capability_refs": [], "skill_refs": []}))
    .bind(json!({
        "kind": "generated_plan",
        "planner": {
            "model": "migration-test",
            "prompt_version": "v1",
            "candidate_hash": "2".repeat(64),
            "compiler_report_hash": "3".repeat(64),
            "final_plan_hash": old_hash,
            "repair_attempts": 0
        }
    }))
    .execute(target)
    .await?;

    if terminal {
        sqlx::query(
            "UPDATE moa.execution_run \
             SET status = 'queued', confirmed_plan_hash = initial_plan_hash, \
                 confirmed_at = NOW(), queued_at = NOW() \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .execute(target)
        .await?;
        sqlx::query(
            "UPDATE moa.execution_run SET status = 'running', started_at = NOW() \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .execute(target)
        .await?;
        sqlx::query(
            "UPDATE moa.execution_run \
             SET active_plan = $2, active_plan_hash = $3, plan_revision = 2, \
                 plan_history = $4 \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .bind(&active_snapshot)
        .bind(&old_active_hash)
        .bind(&plan_history)
        .execute(target)
        .await?;
        sqlx::query(
            "UPDATE moa.execution_run \
             SET status = 'completed', output = '{}'::JSONB, \
                 terminal_cause = '{\"kind\":\"completion\",\"limit_stop\":null}'::JSONB, \
                 terminal_reason = 'completed', \
                 terminal_satisfied_requirement_count = 0, \
                 terminal_requirement_count = 0, completed_at = NOW() \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .execute(target)
        .await?;
    }
    Ok(SeededV1Run {
        run_uid,
        tenant_id,
        old_hash,
    })
}

struct CompensationTransitionEvidence {
    retry_state: (String, i64, i64),
    retry_kept_outcome_and_started_at: bool,
    reclaim_kept_outcome_and_started_at: bool,
    terminal_audit_count: i32,
    budget_rejection_timestamp_is_atomic: bool,
    mapping_failure_insert_is_terminal: bool,
    revoked_review_is_terminal: bool,
}

async fn accepts_unconfirmed_pending_terminal(
    target: &PgPool,
    run_uid: uuid::Uuid,
) -> TestResult<bool> {
    let mut tx = target.begin().await?;
    sqlx::raw_sql("ALTER TABLE moa.execution_run DISABLE TRIGGER execution_run_update_guard;")
        .execute(&mut *tx)
        .await?;
    let updated = sqlx::query(
        "UPDATE moa.execution_run \
         SET status = 'awaiting_confirmation', queued_at = NULL, \
             confirmed_plan_hash = NULL, confirmed_at = NULL, \
             terminal_reason = NULL, terminal_cause = NULL, \
             terminal_satisfied_requirement_count = NULL, \
             terminal_requirement_count = NULL, \
             pending_terminal_status = 'cancelled', \
             pending_terminal_reason = 'cancelled', \
             pending_terminal_cause = '{ \
                 \"terminal_evidence\": { \
                     \"cause\": {\"kind\":\"cancellation\"}, \
                     \"satisfied_requirement_count\": 0, \
                     \"requirement_count\": 0 \
                 }, \
                 \"completion_check_results\": [], \
                 \"terminal_gaps\": [] \
             }'::JSONB, \
             pending_terminal_output = NULL, \
             cancellation_reason = 'migration cancellation fixture' \
         WHERE run_uid = $1",
    )
    .bind(run_uid)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::raw_sql("ALTER TABLE moa.execution_run ENABLE TRIGGER execution_run_update_guard;")
        .execute(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(updated == 1)
}

async fn exercise_compensation_transitions(
    target: &PgPool,
    run_uid: uuid::Uuid,
    tenant_id: uuid::Uuid,
) -> TestResult<CompensationTransitionEvidence> {
    let forward_task_id = uuid::Uuid::new_v4();
    let compensation_id = uuid::Uuid::new_v4();
    insert_compensation_fixture(
        target,
        run_uid,
        tenant_id,
        forward_task_id,
        compensation_id,
        1,
    )
    .await?;
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'running', updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .execute(target)
    .await?;
    let retry_outcome = json!({
        "result": {
            "kind": "failed",
            "message": "temporary rollback failure",
            "retryable": true,
            "usage": usage(1)
        },
        "review_audit": []
    });
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'pending', attempt = 2, generation = 2, \
             outcome = $2, error = $3, updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(&retry_outcome)
    .bind(json!({"class":"retryable", "message":"temporary rollback failure"}))
    .execute(target)
    .await?;
    let retry: (String, i64, i64, bool) = sqlx::query_as(
        "SELECT status, attempt, generation, \
                outcome = $2::JSONB AND started_at IS NOT NULL AND completed_at IS NULL \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(&retry_outcome)
    .fetch_one(target)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'running', updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .execute(target)
    .await?;
    let reclaim_kept_outcome_and_started_at: bool = sqlx::query_scalar(
        "SELECT outcome = $2::JSONB AND started_at IS NOT NULL AND completed_at IS NULL \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(&retry_outcome)
    .fetch_one(target)
    .await?;
    let first_audit = review_audit_entry(2, true);
    let completed_outcome = json!({
        "result": {
            "kind": "completed",
            "output": {"undone": true},
            "usage": usage(2)
        },
        "review_audit": [first_audit]
    });
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'completed', outcome = $2, error = NULL, updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(&completed_outcome)
    .execute(target)
    .await?;
    let late_audit = review_audit_entry(2, false);
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET outcome = jsonb_set( \
                 outcome, '{review_audit}', \
                 (outcome -> 'review_audit') || jsonb_build_array($2::JSONB) \
             ), \
             updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .bind(&late_audit)
    .execute(target)
    .await?;
    let terminal_audit_count: i32 = sqlx::query_scalar(
        "SELECT jsonb_array_length(outcome -> 'review_audit') \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(compensation_id)
    .fetch_one(target)
    .await?;

    let budget_task_id = uuid::Uuid::new_v4();
    let budget_compensation_id = uuid::Uuid::new_v4();
    insert_compensation_fixture(
        target,
        run_uid,
        tenant_id,
        budget_task_id,
        budget_compensation_id,
        2,
    )
    .await?;
    sqlx::query(
        "UPDATE moa.execution_compensation \
         SET status = 'failed', outcome = $2, error = $3, \
             completed_at = NOW(), updated_at = NOW() \
         WHERE compensation_id = $1",
    )
    .bind(budget_compensation_id)
    .bind(json!({
        "result": {
            "kind": "failed",
            "message": "approved execution budget cannot reserve compensation",
            "retryable": false,
            "usage": usage(0)
        },
        "review_audit": []
    }))
    .bind(json!({
        "class": "budget_exceeded",
        "message": "approved execution budget cannot reserve compensation"
    }))
    .execute(target)
    .await?;
    let budget_rejection_timestamp_is_atomic: bool = sqlx::query_scalar(
        "SELECT started_at IS NOT NULL AND started_at = completed_at \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(budget_compensation_id)
    .fetch_one(target)
    .await?;

    let mapping_task_id = uuid::Uuid::new_v4();
    let mapping_compensation_id = uuid::Uuid::new_v4();
    insert_forward_task_fixture(target, run_uid, tenant_id, mapping_task_id, 3).await?;
    sqlx::query(
        "INSERT INTO moa.execution_compensation ( \
            compensation_id, run_uid, forward_task_id, tenant_id, \
            registered_sequence, forward_generation, compensator, mapped_input, \
            status, outcome, error, started_at, completed_at \
         ) VALUES ( \
            $1, $2, $3, $4, 3, 1, $5, 'null'::JSONB, 'failed', $6, $7, \
            statement_timestamp(), statement_timestamp() \
         )",
    )
    .bind(mapping_compensation_id)
    .bind(run_uid)
    .bind(mapping_task_id)
    .bind(tenant_id)
    .bind(compensator_fixture())
    .bind(json!({
        "result": {
            "kind": "failed",
            "message": "compensation input mapping failed",
            "retryable": false,
            "usage": usage(0)
        },
        "review_audit": []
    }))
    .bind(json!({
        "class": "mapping_input_invalid",
        "message": "compensation input mapping failed"
    }))
    .execute(target)
    .await?;
    let mapping_failure_insert_is_terminal: bool = sqlx::query_scalar(
        "SELECT status = 'failed' AND mapped_input = 'null'::JSONB \
                AND started_at IS NOT NULL AND started_at = completed_at \
         FROM moa.execution_compensation WHERE compensation_id = $1",
    )
    .bind(mapping_compensation_id)
    .fetch_one(target)
    .await?;
    let revoked_review_is_terminal =
        exercise_revoked_action_review_status(target, tenant_id).await?;

    Ok(CompensationTransitionEvidence {
        retry_state: (retry.0, retry.1, retry.2),
        retry_kept_outcome_and_started_at: retry.3,
        reclaim_kept_outcome_and_started_at,
        terminal_audit_count,
        budget_rejection_timestamp_is_atomic,
        mapping_failure_insert_is_terminal,
        revoked_review_is_terminal,
    })
}

async fn exercise_revoked_action_review_status(
    target: &PgPool,
    tenant_id: uuid::Uuid,
) -> TestResult<bool> {
    let review_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO public.tenant_action_reviews ( \
            id, tenant_id, storage_partition_id, tool_call_id, tool_name, \
            action_class, risk_level, input_summary, normalized_input, envelope, \
            preview, tool_request, requested_by \
         ) VALUES ( \
            $1, $2, $3, $4, 'migration.review', 'write', 'high', \
            'migration review', '{}', '{}', '{}', '{}', 'migration-test' \
         )",
    )
    .bind(review_id)
    .bind(tenant_id)
    .bind(tenant_id.to_string())
    .bind(uuid::Uuid::new_v4())
    .execute(target)
    .await?;
    sqlx::query(
        "UPDATE public.tenant_action_reviews \
         SET status = 'revoked', decided_at = NOW(), \
             deny_reason = 'execution terminal fence revoked pending review' \
         WHERE id = $1",
    )
    .bind(review_id)
    .execute(target)
    .await?;
    let late_clear =
        sqlx::query("UPDATE public.tenant_action_reviews SET status = 'cleared' WHERE id = $1")
            .bind(review_id)
            .execute(target)
            .await
            .expect_err("revoked action review must reject a late clear");
    let status: String =
        sqlx::query_scalar("SELECT status FROM public.tenant_action_reviews WHERE id = $1")
            .bind(review_id)
            .fetch_one(target)
            .await?;
    Ok(status == "revoked"
        && late_clear
            .to_string()
            .contains("invalid tenant action review"))
}

async fn insert_compensation_fixture(
    target: &PgPool,
    run_uid: uuid::Uuid,
    tenant_id: uuid::Uuid,
    forward_task_id: uuid::Uuid,
    compensation_id: uuid::Uuid,
    sequence: i64,
) -> TestResult {
    insert_forward_task_fixture(target, run_uid, tenant_id, forward_task_id, sequence).await?;
    sqlx::query(
        "INSERT INTO moa.execution_compensation ( \
            compensation_id, run_uid, forward_task_id, tenant_id, \
            registered_sequence, forward_generation, compensator, mapped_input \
         ) VALUES ($1, $2, $3, $4, $5, 1, $6, '{}'::JSONB)",
    )
    .bind(compensation_id)
    .bind(run_uid)
    .bind(forward_task_id)
    .bind(tenant_id)
    .bind(sequence)
    .bind(compensator_fixture())
    .execute(target)
    .await?;
    Ok(())
}

async fn insert_forward_task_fixture(
    target: &PgPool,
    run_uid: uuid::Uuid,
    tenant_id: uuid::Uuid,
    forward_task_id: uuid::Uuid,
    sequence: i64,
) -> TestResult {
    sqlx::query(
        "INSERT INTO moa.execution_task ( \
            task_id, run_uid, tenant_id, node_id, item_key, plan_revision, status, \
            input, task_kind, retry_policy, estimate_cost_microusd, estimate_tokens, \
            estimate_tasks, estimate_tool_calls, estimate_retrieved_bytes \
         ) VALUES ( \
            $1, $2, $3, $4, $4, 1, 'completed', '{}', \
            '{\"kind\":\"output\",\"value\":null}', \
            '{\"max_attempts\":2,\"initial_backoff_ms\":1,\"max_backoff_ms\":1}', \
            0, 0, 1, 0, 0 \
         )",
    )
    .bind(forward_task_id)
    .bind(run_uid)
    .bind(tenant_id)
    .bind(format!("compensation-{sequence}"))
    .execute(target)
    .await?;
    Ok(())
}

fn compensator_fixture() -> Value {
    json!({
        "compensator": {"name": "test.undo", "version": "v1"},
        "input_mapping": {"bindings": []}
    })
}

fn usage(tool_calls: u64) -> Value {
    json!({
        "cost_microusd": 0,
        "tokens": 0,
        "tool_calls": tool_calls,
        "retrieved_bytes": 0
    })
}

fn review_audit_entry(generation: u64, accepted: bool) -> Value {
    json!({
        "review_uid": uuid::Uuid::new_v4(),
        "generation": generation,
        "accepted": accepted,
        "resolution": {"kind": "approved"},
        "recorded_at": "2026-08-04T00:00:00Z"
    })
}

fn v1_plan_definition() -> Value {
    json!({
        "schema_version": 1,
        "input_schema": {},
        "output_schema": {},
        "nodes": [v1_node("output", Vec::new())]
    })
}

fn v1_node(id: &str, depends_on: Vec<&str>) -> Value {
    json!({
        "id": id,
        "requirement_ids": [],
        "depends_on": depends_on,
        "when": null,
        "input": {},
        "output_schema": {},
        "operation": {"kind": "output", "value": {}},
        "retry": {
            "max_attempts": 1,
            "initial_backoff_ms": 1,
            "max_backoff_ms": 1
        },
        "budget": null
    })
}

fn legacy_hash(domain: &str, value: &Value, sort_nodes: bool) -> TestResult<String> {
    let mut canonical = value.clone();
    if sort_nodes {
        canonical["nodes"]
            .as_array_mut()
            .expect("fixture nodes must be an array")
            .sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    }
    let bytes = moa_core::canonical_json::canonical_json_bytes(&canonical)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(&bytes);
    Ok(hasher.finalize().to_hex().to_string())
}
