//! Durable ingestion replay ledger migration scenarios.

use super::support::*;

#[test]
fn ingest_apply_outcome_is_the_next_contiguous_migration_offline() {
    // Pins: replay-stable ingestion reporting is a forward schema change after
    // durable execution compensation, without rewriting the baseline checksum.
    assert_eq!(
        migration_version("ingest_apply_outcome")
            .expect("the ingestion outcome migration must be embedded"),
        56
    );
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn ingest_dedup_requires_a_closed_apply_outcome_db() {
    // Pins: V56 upgrades a populated V55 ledger, classifies its historical row
    // with the established skipped behavior, and makes every future outcome a
    // required closed-vocabulary value.
    let database = FreshMigrationDatabase::create()
        .await
        .expect("create ingestion outcome migration database");

    let outcome = async {
        install_required_extensions(database.target_url()).await?;
        apply_through_migration(database.target_url(), "execution_plan_compensation").await?;
        let tenant_id = uuid::Uuid::new_v4();
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        sqlx::query(
            "INSERT INTO moa.ingest_dedup \
                (storage_partition_id, user_id, tenant_id, session_id, turn_seq, \
                 fact_hash, fact_uid) \
             VALUES ($1, $2, $3, $4, 1, $5, $6)",
        )
        .bind(tenant_id.to_string())
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(tenant_id)
        .bind(uuid::Uuid::new_v4())
        .bind(vec![7_u8; 32])
        .bind(uuid::Uuid::new_v4())
        .execute(&target)
        .await?;
        target.close().await;

        let first = run_reporting_applied_serialized(database.target_url()).await?;
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(database.target_url())
            .await?;
        let column: Option<(String, String)> = sqlx::query_as(
            "SELECT data_type, is_nullable FROM information_schema.columns \
             WHERE table_schema = 'moa' AND table_name = 'ingest_dedup' \
               AND column_name = 'apply_outcome'",
        )
        .fetch_optional(&target)
        .await?;
        let historical_outcome: Option<String> =
            sqlx::query_scalar("SELECT apply_outcome FROM moa.ingest_dedup")
                .fetch_optional(&target)
                .await?;
        let constraint: Option<String> = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'moa.ingest_dedup'::regclass \
               AND conname = 'ingest_dedup_apply_outcome_check'",
        )
        .fetch_optional(&target)
        .await?;
        target.close().await;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
            first,
            column,
            constraint,
            historical_outcome,
        ))
    }
    .await;

    let outcome = database.finish(outcome).await;
    let (first, column, constraint, historical_outcome) =
        outcome.expect("ingestion outcome migration should apply");
    assert_eq!(
        first,
        expected_migration_labels_from("ingest_apply_outcome")
    );
    assert_eq!(
        column,
        Some(("text".to_string(), "NO".to_string())),
        "V56 must install one required text apply_outcome column"
    );
    assert_eq!(historical_outcome.as_deref(), Some("skipped"));
    let constraint = constraint.expect("V56 must install the apply-outcome check constraint");
    for expected in ["inserted", "superseded", "reinforced", "skipped"] {
        assert!(
            constraint.contains(expected),
            "outcome constraint omitted {expected:?}: {constraint}"
        );
    }
}
