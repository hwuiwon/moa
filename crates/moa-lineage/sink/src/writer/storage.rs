//! Postgres and ClickHouse batch storage plus COPY rendering.

use uuid::Uuid;

use crate::Result;
use crate::store::LineageStore;

use super::compliance::apply_compliance_hashes;
use super::rows::{LineageRow, PendingRow, ScoreRow};

pub(super) async fn write_pending_rows(store: &LineageStore, rows: &[PendingRow]) -> Result<()> {
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

    match store {
        LineageStore::Postgres(pool) => write_rows(pool, &lineage_rows).await?,
        // Compliance tenants are refused at startup on the ClickHouse backend
        // (see `LineageStore::guard_compliance_backend`), so ClickHouse writes
        // never silently drop hash chaining here.
        LineageStore::ClickHouse { clickhouse, .. } => {
            clickhouse.insert_lineage_rows(&lineage_rows).await?;
        }
    }
    write_score_rows(store.postgres(), &score_rows).await?;
    Ok(())
}

async fn write_rows(pool: &sqlx::PgPool, rows: &[LineageRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut rows = rows.to_vec();
    let mut tx = pool.begin().await?;
    apply_compliance_hashes(&mut tx, &mut rows).await?;

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
    .execute(&mut *tx)
    .await?;

    let copy_payload = render_copy_csv(&rows);
    let mut copy = (*tx)
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
        FROM lineage_copy
        ON CONFLICT (turn_id, record_kind, ts) DO UPDATE
        SET payload = EXCLUDED.payload,
            integrity_hash = EXCLUDED.integrity_hash,
            prev_hash = COALESCE(EXCLUDED.prev_hash, analytics.turn_lineage.prev_hash)
        "#,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
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

async fn write_score_rows(pool: &sqlx::PgPool, rows: &[ScoreRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
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
    .execute(&mut *tx)
    .await?;

    let copy_payload = render_score_copy_csv(rows);
    let mut copy = (*tx)
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
        FROM lineage_scores_copy
        ON CONFLICT (score_id, ts) DO UPDATE
        SET storage_partition_id = EXCLUDED.storage_partition_id,
            user_id = EXCLUDED.user_id,
            target_kind = EXCLUDED.target_kind,
            turn_id = EXCLUDED.turn_id,
            session_id = EXCLUDED.session_id,
            run_id = EXCLUDED.run_id,
            item_id = EXCLUDED.item_id,
            dataset_id = EXCLUDED.dataset_id,
            name = EXCLUDED.name,
            value_type = EXCLUDED.value_type,
            value_numeric = EXCLUDED.value_numeric,
            value_boolean = EXCLUDED.value_boolean,
            value_categorical = EXCLUDED.value_categorical,
            source = EXCLUDED.source,
            model_or_evaluator = EXCLUDED.model_or_evaluator,
            comment = EXCLUDED.comment
        "#,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
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
