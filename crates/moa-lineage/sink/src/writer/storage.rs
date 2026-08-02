//! Postgres batch storage plus COPY rendering.

use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::Result;

use super::compliance::apply_compliance_hashes;
use super::rows::{ExperimentScoreProvenanceRow, LineageRow, PendingRow, ScoreRow};

/// Stores a claimed batch inside the caller's transaction.
///
/// The transaction belongs to the drain, which dequeues the same rows from the
/// acceptance queue before committing. That is what makes "stored" and "no
/// longer queued" a single fact rather than two facts a crash can separate.
///
pub(super) async fn write_pending_rows(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[PendingRow],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut lineage_rows = Vec::new();
    let mut score_rows = Vec::new();
    for row in rows {
        match row {
            PendingRow::Lineage(row) => lineage_rows.push(row.clone()),
            PendingRow::Score(row) => score_rows.push(row.clone()),
        }
    }

    lock_destruction_scopes(tx, rows).await?;
    write_rows(tx, &lineage_rows).await?;
    write_score_rows(tx, &score_rows).await?;
    Ok(())
}

/// Serializes writes with the canonical tenant/subject destruction fence.
///
/// If this transaction wins the lock, a later erasure waits and then deletes
/// these rows. If erasure wins, its permanent fence is visible to the INSERT
/// predicates below. Either ordering prevents an in-flight write from
/// resurrecting destroyed data.
async fn lock_destruction_scopes(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[PendingRow],
) -> Result<()> {
    let mut tenant_ids = Vec::<Uuid>::with_capacity(rows.len());
    let mut subject_ids = Vec::<Uuid>::with_capacity(rows.len());
    for row in rows {
        let Ok(tenant_id) = Uuid::parse_str(&row.storage_partition_id()) else {
            continue;
        };
        tenant_ids.push(tenant_id);
        if let Some(user_id) = row.user_id()
            && let Some(subject_id) = privacy_subject_uuid(&user_id)
        {
            subject_ids.push(subject_id);
        }
    }

    if !tenant_ids.is_empty() {
        sqlx::query(
            r#"
            SELECT pg_advisory_xact_lock_shared(
                hashtextextended('moa:destruction:tenant:' || scope_id::text, 0)
            )
            FROM (
                SELECT DISTINCT scope_id
                FROM unnest($1::uuid[]) AS input(scope_id)
                ORDER BY scope_id
            ) AS ordered_scopes
            ORDER BY scope_id
            "#,
        )
        .bind(tenant_ids)
        .execute(&mut **tx)
        .await?;
    }

    if !subject_ids.is_empty() {
        sqlx::query(
            r#"
            SELECT pg_advisory_xact_lock_shared(
                hashtextextended('moa:destruction:subject:' || scope_id::text, 0)
            )
            FROM (
                SELECT DISTINCT scope_id
                FROM unnest($1::uuid[]) AS input(scope_id)
                ORDER BY scope_id
            ) AS ordered_scopes
            ORDER BY scope_id
            "#,
        )
        .bind(subject_ids)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn privacy_subject_uuid(user_id: &str) -> Option<Uuid> {
    Uuid::parse_str(user_id.strip_prefix("contact:").unwrap_or(user_id)).ok()
}

async fn write_rows(tx: &mut Transaction<'_, Postgres>, rows: &[LineageRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut rows = rows.to_vec();
    apply_compliance_hashes(tx, &mut rows).await?;

    // Reuse a persistent per-connection staging table instead of dropping and recreating one
    // per drain: `ON COMMIT DELETE ROWS` empties it at each commit, so there is no catalog churn
    // and prepared-statement caching for the COPY/INSERT is preserved.
    sqlx::query(
        r#"
        CREATE TEMP TABLE IF NOT EXISTS lineage_copy (
            turn_id        UUID        NOT NULL,
            session_id     UUID        NOT NULL,
            user_id        TEXT        NOT NULL,
            storage_partition_id   TEXT        NOT NULL,
            ts             TIMESTAMPTZ NOT NULL,
            tier           SMALLINT    NOT NULL,
            record_kind    SMALLINT    NOT NULL,
            payload        JSONB       NOT NULL,
            integrity_hash BYTEA       NOT NULL,
            prev_hash      BYTEA
        ) ON COMMIT DELETE ROWS;
        "#,
    )
    .execute(&mut **tx)
    .await?;

    let copy_payload = render_copy_csv(&rows);
    let mut copy = (**tx)
        .copy_in_raw(
            r#"
            COPY lineage_copy (
                turn_id,
                session_id,
                user_id,
                storage_partition_id,
                ts,
                tier,
                record_kind,
                payload,
                integrity_hash,
                prev_hash
            )
            FROM STDIN WITH (FORMAT csv, NULL '\N')
            "#,
        )
        .await?;
    if let Err(error) = copy.send(copy_payload.as_bytes()).await {
        let _ = copy.abort("lineage copy failed").await;
        return Err(error.into());
    }
    copy.finish().await?;

    sqlx::query(
        r#"
        INSERT INTO analytics.turn_lineage (
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier,
            record_kind,
            payload,
            integrity_hash,
            prev_hash
        )
        SELECT
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier,
            record_kind,
            payload,
            integrity_hash,
            prev_hash
        FROM lineage_copy AS staged
        -- Permanent destruction fence, checked after taking the same advisory
        -- locks as erasure. A tenant fence suppresses every row; a subject fence
        -- suppresses only that UUID-backed user/contact.
        WHERE NOT EXISTS (
            SELECT 1
            FROM moa.destruction_operation_fence AS fence
            WHERE fence.tenant_id::TEXT = staged.storage_partition_id
              AND (
                    fence.subject_id IS NULL
                    OR staged.user_id = fence.subject_id::TEXT
                    OR staged.user_id = 'contact:' || fence.subject_id::TEXT
                  )
        )
        ON CONFLICT (turn_id, record_kind, ts) DO UPDATE
        SET payload = EXCLUDED.payload,
            integrity_hash = EXCLUDED.integrity_hash,
            prev_hash = COALESCE(EXCLUDED.prev_hash, analytics.turn_lineage.prev_hash)
        "#,
    )
    .execute(&mut **tx)
    .await?;

    Ok(())
}

fn render_copy_csv(rows: &[LineageRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let fields = [
            csv_field(&row.turn_id.to_string()),
            csv_field(&row.session_id.to_string()),
            csv_field(&row.user_id),
            csv_field(&row.storage_partition_id),
            csv_field(&row.ts.to_rfc3339()),
            csv_field(&row.tier.to_string()),
            csv_field(&row.record_kind.to_string()),
            csv_field(&row.payload.to_string()),
            csv_field(&bytea_hex(&row.integrity_hash)),
            row.prev_hash
                .as_ref()
                .map(|hash| csv_field(&bytea_hex(hash)))
                .unwrap_or_else(|| "\\N".to_string()),
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

async fn write_score_rows(tx: &mut Transaction<'_, Postgres>, rows: &[ScoreRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    // Reuse a persistent per-connection staging table (see `write_rows`): `ON COMMIT DELETE ROWS`
    // clears it at commit, avoiding per-drain catalog churn and preserving statement caching.
    sqlx::query(
        r#"
        CREATE TEMP TABLE IF NOT EXISTS lineage_scores_copy (
            score_id           UUID             NOT NULL,
            ts                 TIMESTAMPTZ      NOT NULL,
            storage_partition_id       TEXT             NOT NULL,
            user_id            TEXT,
            target_kind        TEXT             NOT NULL,
            turn_id            UUID,
            session_id         UUID,
            run_id             UUID,
            item_id            UUID,
            dataset_id         UUID,
            name               TEXT             NOT NULL,
            value_type         TEXT             NOT NULL,
            value_numeric      DOUBLE PRECISION,
            value_boolean      BOOLEAN,
            value_categorical  TEXT,
            source             TEXT             NOT NULL,
            model_or_evaluator TEXT             NOT NULL,
            comment            TEXT
        ) ON COMMIT DELETE ROWS;
        "#,
    )
    .execute(&mut **tx)
    .await?;

    let copy_payload = render_score_copy_csv(rows);
    let mut copy = (**tx)
        .copy_in_raw(
            r#"
            COPY lineage_scores_copy (
                score_id,
                ts,
                storage_partition_id,
                user_id,
                target_kind,
                turn_id,
                session_id,
                run_id,
                item_id,
                dataset_id,
                name,
                value_type,
                value_numeric,
                value_boolean,
                value_categorical,
                source,
                model_or_evaluator,
                comment
            )
            FROM STDIN WITH (FORMAT csv, NULL '\N')
            "#,
        )
        .await?;
    if let Err(error) = copy.send(copy_payload.as_bytes()).await {
        let _ = copy.abort("lineage score copy failed").await;
        return Err(error.into());
    }
    copy.finish().await?;

    sqlx::query(
        r#"
        INSERT INTO analytics.scores (
            score_id,
            ts,
            storage_partition_id,
            user_id,
            target_kind,
            turn_id,
            session_id,
            run_id,
            item_id,
            dataset_id,
            name,
            value_type,
            value_numeric,
            value_boolean,
            value_categorical,
            source,
            model_or_evaluator,
            comment
        )
        SELECT
            score_id,
            ts,
            storage_partition_id,
            user_id,
            target_kind,
            turn_id,
            session_id,
            run_id,
            item_id,
            dataset_id,
            name,
            value_type,
            value_numeric,
            value_boolean,
            value_categorical,
            source,
            model_or_evaluator,
            comment
        FROM lineage_scores_copy AS staged
        -- Purge fence, for the same reason as the lineage insert above: a score
        -- accepted before a purge must not be written after it. Without this the
        -- provenance insert below would also fail its foreign key against the
        -- already-purged trial, turning a purged tenant's in-flight scores into
        -- dead letters instead of correctly discarding them.
        WHERE NOT EXISTS (
            SELECT 1
            FROM moa.destruction_operation_fence AS fence
            WHERE fence.tenant_id::TEXT = staged.storage_partition_id
              AND (
                    fence.subject_id IS NULL
                    OR staged.user_id = fence.subject_id::TEXT
                    OR staged.user_id = 'contact:' || fence.subject_id::TEXT
                  )
        )
        -- Replay acceptance, not mutation. A score row is derived from the exact
        -- evidence its provenance names, so a replay that produced the same
        -- identity must have produced the same values; rewriting them here would
        -- let a second pass silently restate history. A replay that produced
        -- DIFFERENT values keeps its identity and is caught below, where the
        -- provenance comparison refuses it loudly instead of absorbing it.
        ON CONFLICT (score_id, ts) DO NOTHING
        "#,
    )
    .execute(&mut **tx)
    .await?;

    write_experiment_score_provenance(tx, rows).await?;

    Ok(())
}

/// Writes Behavior Lab provenance beside the score rows it explains.
///
/// Runs inside the score-write transaction, so a score row and its provenance
/// become visible together. A score whose provenance write fails is not
/// committed at all, which is what makes "only a provenance-backed score can
/// satisfy a requirement" true rather than aspirational.
async fn write_experiment_score_provenance(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rows: &[ScoreRow],
) -> Result<()> {
    let provenance = rows
        .iter()
        .filter_map(|row| row.provenance.as_ref())
        .collect::<Vec<_>>();
    if provenance.is_empty() {
        return Ok(());
    }

    sqlx::query(
        r#"
        CREATE TEMP TABLE IF NOT EXISTS experiment_score_provenance_copy (
            score_id                 UUID  NOT NULL,
            score_ts                 TIMESTAMPTZ NOT NULL,
            storage_partition_id     TEXT  NOT NULL,
            user_id                  TEXT,
            score_run_id             UUID  NOT NULL,
            experiment_run_uid       UUID  NOT NULL,
            plan_revision_uid        UUID  NOT NULL,
            trial_uid                UUID  NOT NULL,
            target_session_id        UUID,
            target_execution_run_uid UUID,
            evaluator_id             TEXT  NOT NULL,
            evaluator_version        TEXT  NOT NULL,
            score_name               TEXT  NOT NULL,
            value_type               TEXT  NOT NULL,
            evidence_ref             TEXT  NOT NULL,
            evidence_hash            BYTEA NOT NULL
        ) ON COMMIT DELETE ROWS;
        "#,
    )
    .execute(&mut **tx)
    .await?;

    let copy_payload = render_provenance_copy_csv(&provenance);
    let mut copy = (**tx)
        .copy_in_raw(
            r#"
            COPY experiment_score_provenance_copy (
                score_id,
                score_ts,
                storage_partition_id,
                user_id,
                score_run_id,
                experiment_run_uid,
                plan_revision_uid,
                trial_uid,
                target_session_id,
                target_execution_run_uid,
                evaluator_id,
                evaluator_version,
                score_name,
                value_type,
                evidence_ref,
                evidence_hash
            )
            FROM STDIN WITH (FORMAT csv, NULL '\N')
            "#,
        )
        .await?;
    if let Err(error) = copy.send(copy_payload.as_bytes()).await {
        let _ = copy.abort("experiment score provenance copy failed").await;
        return Err(error.into());
    }
    copy.finish().await?;

    sqlx::query(
        r#"
        INSERT INTO moa.experiment_score_provenance (
            score_id,
            score_ts,
            storage_partition_id,
            user_id,
            score_run_id,
            experiment_run_uid,
            plan_revision_uid,
            trial_uid,
            target_session_id,
            target_execution_run_uid,
            evaluator_id,
            evaluator_version,
            score_name,
            value_type,
            evidence_ref,
            evidence_hash
        )
        SELECT
            score_id,
            score_ts,
            storage_partition_id,
            user_id,
            score_run_id,
            experiment_run_uid,
            plan_revision_uid,
            trial_uid,
            target_session_id,
            target_execution_run_uid,
            evaluator_id,
            evaluator_version,
            score_name,
            value_type,
            evidence_ref,
            evidence_hash
        FROM experiment_score_provenance_copy AS staged
        -- Purge fence, matching the score insert this explains. Provenance for a
        -- score that was fenced out above would reference a trial the purge has
        -- already removed, so writing it could only ever be a foreign-key
        -- failure.
        WHERE NOT EXISTS (
            SELECT 1
            FROM moa.destruction_operation_fence AS fence
            WHERE fence.tenant_id::TEXT = staged.storage_partition_id
              AND (
                    fence.subject_id IS NULL
                    OR staged.user_id = fence.subject_id::TEXT
                    OR staged.user_id = 'contact:' || fence.subject_id::TEXT
                  )
        )
        -- Provenance is immutable by database trigger, so DO UPDATE is not even
        -- available here. DO NOTHING accepts an identical replay; the comparison
        -- below is what refuses a non-identical one.
        ON CONFLICT (score_id) DO NOTHING
        "#,
    )
    .execute(&mut **tx)
    .await?;

    let conflicts: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM experiment_score_provenance_copy AS staged
        JOIN moa.experiment_score_provenance AS stored
          ON stored.score_id = staged.score_id
        WHERE (
                  stored.storage_partition_id,
                  stored.score_ts,
                  stored.score_run_id,
                  stored.experiment_run_uid,
                  stored.plan_revision_uid,
                  stored.trial_uid,
                  stored.evaluator_id,
                  stored.evaluator_version,
                  stored.score_name,
                  stored.value_type,
                  stored.evidence_ref,
                  stored.evidence_hash
              ) IS DISTINCT FROM (
                  staged.storage_partition_id,
                  staged.score_ts,
                  staged.score_run_id,
                  staged.experiment_run_uid,
                  staged.plan_revision_uid,
                  staged.trial_uid,
                  staged.evaluator_id,
                  staged.evaluator_version,
                  staged.score_name,
                  staged.value_type,
                  staged.evidence_ref,
                  staged.evidence_hash
              )
           OR stored.target_session_id IS DISTINCT FROM staged.target_session_id
           OR stored.target_execution_run_uid IS DISTINCT FROM staged.target_execution_run_uid
           OR stored.user_id IS DISTINCT FROM staged.user_id
        "#,
    )
    .fetch_one(&mut **tx)
    .await?;
    if conflicts != 0 {
        return Err(crate::Error::ExperimentScoreProvenanceConflict { count: conflicts });
    }
    Ok(())
}

fn render_provenance_copy_csv(rows: &[&ExperimentScoreProvenanceRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let fields = [
            csv_field(&row.score_id.to_string()),
            csv_field(&row.score_ts.to_rfc3339()),
            csv_field(&row.storage_partition_id),
            nullable_csv(row.user_id.as_deref()),
            csv_field(&row.score_run_id.to_string()),
            csv_field(&row.experiment_run_uid.to_string()),
            csv_field(&row.plan_revision_uid.to_string()),
            csv_field(&row.trial_uid.to_string()),
            nullable_uuid_csv(row.target_session_id),
            nullable_uuid_csv(row.target_execution_run_uid),
            csv_field(&row.evaluator_id),
            csv_field(&row.evaluator_version),
            csv_field(&row.score_name),
            csv_field(&row.value_type),
            csv_field(&row.evidence_ref),
            csv_field(&bytea_hex(&row.evidence_hash)),
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

fn render_score_copy_csv(rows: &[ScoreRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let fields = [
            csv_field(&row.score_id.to_string()),
            csv_field(&row.ts.to_rfc3339()),
            csv_field(&row.storage_partition_id),
            nullable_csv(row.user_id.as_deref()),
            csv_field(&row.target_kind),
            nullable_uuid_csv(row.turn_id),
            nullable_uuid_csv(row.session_id),
            nullable_uuid_csv(row.run_id),
            nullable_uuid_csv(row.item_id),
            nullable_uuid_csv(row.dataset_id),
            csv_field(&row.name),
            csv_field(&row.value_type),
            nullable_csv(row.value_numeric.map(|value| value.to_string()).as_deref()),
            nullable_csv(row.value_boolean.map(|value| value.to_string()).as_deref()),
            nullable_csv(row.value_categorical.as_deref()),
            csv_field(&row.source),
            csv_field(&row.model_or_evaluator),
            nullable_csv(row.comment.as_deref()),
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

fn nullable_csv(value: Option<&str>) -> String {
    value.map(csv_field).unwrap_or_else(|| "\\N".to_string())
}

fn nullable_uuid_csv(value: Option<Uuid>) -> String {
    value
        .map(|value| csv_field(&value.to_string()))
        .unwrap_or_else(|| "\\N".to_string())
}

fn csv_field(value: &str) -> String {
    let escaped = value.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn bytea_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(2 + bytes.len().saturating_mul(2));
    out.push_str("\\x");
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
