//! Lineage query, export, verify, erase, and render helpers.

use super::*;

pub(crate) async fn explain_report(config: &MoaConfig, id: &str) -> Result<String> {
    let id = Uuid::parse_str(id).with_context(|| format!("invalid session or turn id `{id}`"))?;
    let store = load_session_store(config).await?;
    let rows = sqlx::query(
        r#"
        SELECT turn_id, ts, record_kind, payload
        FROM analytics.turn_lineage
        WHERE session_id = $1 OR turn_id = $1
        ORDER BY ts ASC, record_kind ASC
        "#,
    )
    .bind(id)
    .fetch_all(store.pool())
    .await?;

    let mut report = String::new();
    if rows.is_empty() {
        report.push_str("no lineage records\n");
        return Ok(report);
    }

    let mut last_turn: Option<Uuid> = None;
    for row in rows {
        let turn_id: Uuid = row.try_get("turn_id")?;
        let ts: chrono::DateTime<Utc> = row.try_get("ts")?;
        let record_kind: i16 = row.try_get("record_kind")?;
        let payload: serde_json::Value = row.try_get("payload")?;
        if Some(turn_id) != last_turn {
            report.push_str(&format!("\n=== turn {turn_id}  {ts}\n"));
            last_turn = Some(turn_id);
        }
        render_lineage_record(record_kind, &payload, &mut report);
    }
    Ok(report)
}

pub(crate) async fn lineage_query_report(
    config: &MoaConfig,
    args: &LineageQueryArgs,
) -> Result<String> {
    if args.cold {
        anyhow::bail!("cold lineage query is not configured in this CLI build");
    }
    let prepared = prepare_lineage_sql(&args.sql)?;
    let store = load_session_store(config).await?;
    let mut tx = store.pool().begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout = '5s'")
        .execute(&mut *tx)
        .await?;
    let rows: serde_json::Value = sqlx::query_scalar(&format!(
        "SELECT COALESCE(jsonb_agg(row_to_json(lineage_query)), '[]'::jsonb) \
         FROM ({prepared}) lineage_query"
    ))
    .bind(&args.since)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    serde_json::to_string_pretty(&rows).map_err(Into::into)
}

pub(crate) async fn lineage_export_report(
    config: &MoaConfig,
    args: &LineageExportArgs,
) -> Result<String> {
    let workspace_id = resolve_workspace_arg(&args.workspace);
    let store = load_session_store(config).await?;
    let pattern = format!("%{}%", args.subject);
    let records: Vec<serde_json::Value> = sqlx::query_scalar(
        r#"
        SELECT row_to_json(lineage_row)::jsonb
        FROM (
            SELECT turn_id, session_id, user_id, workspace_id, ts, record_kind, payload,
                   integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE workspace_id = $1 AND payload::text ILIKE $2
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            LIMIT 10000
        ) lineage_row
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(pattern)
    .fetch_all(store.pool())
    .await?;
    let signing = local_cli_signing_key("dsar-export");
    let exporter = DsarExporter::new(signing);
    let bundle = exporter
        .export_records(
            workspace_id.as_str(),
            args.subject.as_bytes().to_vec(),
            records,
            Vec::new(),
            &args.out,
        )
        .await
        .context("writing DSAR bundle")?;
    Ok(format!(
        "dsar_export: {}\nrecords: {}\nsubject_hash: {}\n",
        bundle.bundle_uri,
        bundle.record_count,
        blake3::hash(&bundle.subject_pseudonym).to_hex()
    ))
}

pub(crate) async fn lineage_verify_report(
    config: &MoaConfig,
    args: &LineageVerifyArgs,
) -> Result<String> {
    let store = load_session_store(config).await?;
    if args.window == "hot" || args.window == "db" {
        let workspace_id = resolve_workspace_arg(&args.workspace);
        let rows =
            load_compliance_rows_for_interval(store.pool(), workspace_id.as_str(), &args.since)
                .await?;
        let report = verify_compliance_rows(rows, None)?;
        return Ok(format!(
            "lineage_verify: ok\nworkspace_id: {}\nrecords: {}\nroot_checked: false\n",
            workspace_id, report.records
        ));
    }

    let root = load_audit_root(store.pool(), &args.window).await?;
    let rows = load_compliance_rows_for_window(
        store.pool(),
        &root.workspace_id,
        root.window_start,
        root.window_end,
    )
    .await?;
    let report = verify_compliance_rows(rows, Some(root.merkle_root))?;
    Ok(format!(
        "lineage_verify: ok\nworkspace_id: {}\nrecords: {}\nroot_checked: true\n",
        root.workspace_id, report.records
    ))
}

pub(crate) async fn lineage_erase_report(
    config: &MoaConfig,
    args: &LineageEraseArgs,
) -> Result<String> {
    let workspace_id = resolve_workspace_arg(&args.workspace);
    let subject = hex::decode(&args.subject)
        .with_context(|| "subject must be a hex-encoded pseudonym for erase")?;
    let store = load_session_store(config).await?;
    let rows = sqlx::query(
        r#"
        UPDATE pii_vault.subject_keys
        SET erased_at = now()
        WHERE workspace_id = $1 AND subject_pseudonym = $2
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(subject)
    .execute(store.pool())
    .await?
    .rows_affected();
    Ok(format!(
        "lineage_erase: scheduled\nworkspace_id: {}\nsubjects: {}\n",
        workspace_id, rows
    ))
}

pub(crate) struct AuditRootRow {
    workspace_id: String,
    window_start: chrono::DateTime<Utc>,
    window_end: chrono::DateTime<Utc>,
    merkle_root: Vec<u8>,
}

pub(crate) struct ComplianceRow {
    turn_id: Uuid,
    record_kind: i16,
    ts: chrono::DateTime<Utc>,
    payload: serde_json::Value,
    integrity_hash: Vec<u8>,
    prev_hash: Option<Vec<u8>>,
}

pub(crate) struct VerificationReport {
    records: usize,
}

pub(crate) async fn load_audit_root(pool: &sqlx::PgPool, id_or_uri: &str) -> Result<AuditRootRow> {
    let row = if let Ok(root_id) = Uuid::parse_str(id_or_uri) {
        sqlx::query(
            r#"
            SELECT workspace_id, window_start, window_end, merkle_root
            FROM analytics.audit_roots
            WHERE root_id = $1
            "#,
        )
        .bind(root_id)
        .fetch_one(pool)
        .await?
    } else {
        sqlx::query(
            r#"
            SELECT workspace_id, window_start, window_end, merkle_root
            FROM analytics.audit_roots
            WHERE s3_object_uri = $1
            "#,
        )
        .bind(id_or_uri)
        .fetch_one(pool)
        .await?
    };
    Ok(AuditRootRow {
        workspace_id: row.try_get("workspace_id")?,
        window_start: row.try_get("window_start")?,
        window_end: row.try_get("window_end")?,
        merkle_root: row.try_get("merkle_root")?,
    })
}

pub(crate) async fn load_compliance_rows_for_interval(
    pool: &sqlx::PgPool,
    workspace_id: &str,
    since: &str,
) -> Result<Vec<ComplianceRow>> {
    load_compliance_rows(
        sqlx::query(
            r#"
            SELECT turn_id, record_kind, ts, payload, integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE workspace_id = $1
              AND prev_hash IS NOT NULL
              AND ts > now() - ($2::text)::interval
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            "#,
        )
        .bind(workspace_id)
        .bind(since)
        .fetch_all(pool)
        .await?,
    )
}

pub(crate) async fn load_compliance_rows_for_window(
    pool: &sqlx::PgPool,
    workspace_id: &str,
    window_start: chrono::DateTime<Utc>,
    window_end: chrono::DateTime<Utc>,
) -> Result<Vec<ComplianceRow>> {
    load_compliance_rows(
        sqlx::query(
            r#"
            SELECT turn_id, record_kind, ts, payload, integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE workspace_id = $1
              AND prev_hash IS NOT NULL
              AND ts >= $2
              AND ts <= $3
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            "#,
        )
        .bind(workspace_id)
        .bind(window_start)
        .bind(window_end)
        .fetch_all(pool)
        .await?,
    )
}

pub(crate) fn load_compliance_rows(rows: Vec<sqlx::postgres::PgRow>) -> Result<Vec<ComplianceRow>> {
    rows.into_iter()
        .map(|row| {
            Ok(ComplianceRow {
                turn_id: row.try_get("turn_id")?,
                record_kind: row.try_get("record_kind")?,
                ts: row.try_get("ts")?,
                payload: row.try_get("payload")?,
                integrity_hash: row.try_get("integrity_hash")?,
                prev_hash: row.try_get("prev_hash")?,
            })
        })
        .collect()
}

pub(crate) fn verify_compliance_rows(
    rows: Vec<ComplianceRow>,
    expected_root: Option<Vec<u8>>,
) -> Result<VerificationReport> {
    let mut leaves = Vec::with_capacity(rows.len());
    let mut previous_integrity: Option<&[u8]> = None;
    for row in &rows {
        if let (Some(previous), Some(prev_hash)) = (previous_integrity, row.prev_hash.as_deref())
            && prev_hash != previous
        {
            anyhow::bail!(
                "chain link mismatch at turn={} kind={} ts={}",
                row.turn_id,
                row.record_kind,
                row.ts
            );
        }
        let prev = row.prev_hash.as_deref().map(hash_from_slice).transpose()?;
        let (actual, _) = HashChain::link(prev, &row.payload)?;
        if actual.as_bytes() != row.integrity_hash.as_slice() {
            anyhow::bail!(
                "chain mismatch at turn={} kind={} ts={}",
                row.turn_id,
                row.record_kind,
                row.ts
            );
        }
        previous_integrity = Some(&row.integrity_hash);
        leaves.push(row.integrity_hash.clone());
    }
    if let Some(expected_root) = expected_root {
        let actual_root = blake3_merkle_root(&leaves)?;
        if actual_root.as_bytes() != expected_root.as_slice() {
            anyhow::bail!("merkle root mismatch for verified window");
        }
    }
    Ok(VerificationReport {
        records: rows.len(),
    })
}

pub(crate) fn local_cli_signing_key(label: &str) -> SigningKey {
    SigningKey::from_seed(
        label.to_string(),
        *blake3::hash(format!("moa-cli-local-signing:{label}").as_bytes()).as_bytes(),
    )
}

pub(crate) fn prepare_lineage_sql(sql: &str) -> Result<String> {
    let trimmed = sql.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("select ") || lower.starts_with("with ")) {
        anyhow::bail!("only SELECT or WITH queries are permitted");
    }
    if trimmed.contains(';') {
        anyhow::bail!("semicolon-separated statements are not permitted");
    }
    let Some(idx) = lower.find("from lineage") else {
        anyhow::bail!("query must use `FROM lineage` as the source table");
    };
    let replacement = "FROM (SELECT * FROM analytics.turn_lineage WHERE ts > now() - ($1::text)::interval) lineage";
    let mut prepared = String::with_capacity(trimmed.len() + replacement.len());
    prepared.push_str(&trimmed[..idx]);
    prepared.push_str(replacement);
    prepared.push_str(&trimmed[idx + "from lineage".len()..]);
    Ok(prepared)
}

pub(crate) fn render_lineage_record(kind: i16, payload: &serde_json::Value, out: &mut String) {
    let record = payload.get("record").unwrap_or(payload);
    match kind {
        1 => render_retrieval_record(record, out),
        2 => render_context_record(record, out),
        3 => render_generation_record(record, out),
        4 => render_citation_record(record, out),
        6 => render_decision_record(record, out),
        _ => {}
    }
}

pub(crate) fn render_retrieval_record(record: &serde_json::Value, out: &mut String) {
    let query = record
        .get("query_original")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let total_ms = record
        .pointer("/timings/total_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let top_k = record
        .get("top_k")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    out.push_str(&format!(
        "retrieval: query=\"{query}\" top_k={top_k} total_ms={total_ms}\n"
    ));
}

pub(crate) fn render_context_record(record: &serde_json::Value, out: &mut String) {
    let chunks = record
        .get("chunks_in_window")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let tokens = record
        .get("total_input_tokens_estimated")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    out.push_str(&format!(
        "context: chunks={chunks} estimated_input_tokens={tokens}\n"
    ));
}

pub(crate) fn render_generation_record(record: &serde_json::Value, out: &mut String) {
    let provider = record
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let model = record
        .get("response_model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let input = record
        .pointer("/usage/input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output = record
        .pointer("/usage/output_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    out.push_str(&format!(
        "generation: provider={provider} model={model} input_tokens={input} output_tokens={output}\n"
    ));
}

pub(crate) fn render_citation_record(record: &serde_json::Value, out: &mut String) {
    let vendor = record
        .get("vendor_used")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let verifier = record
        .get("verifier_used")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let citations = record
        .get("citations")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    out.push_str(&format!(
        "citation: vendor={vendor} verifier={verifier} citations={citations}\n"
    ));
}

pub(crate) fn render_decision_record(record: &serde_json::Value, out: &mut String) {
    let kind = record
        .get("kind")
        .and_then(serde_json::Value::as_object)
        .and_then(|object| object.get("kind"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("decision");
    let policy = record
        .get("policy_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    out.push_str(&format!("decision: kind={kind} policy={policy}\n"));
}

pub(crate) fn leg_trace(legs: moa_brain::retrieval::LegSources) -> String {
    let mut out = Vec::new();
    if legs.graph {
        out.push("graph");
    }
    if legs.vector {
        out.push("vector");
    }
    if legs.lexical {
        out.push("lexical");
    }
    if out.is_empty() {
        "none".to_string()
    } else {
        out.join("+")
    }
}

#[cfg(test)]
mod lineage_query_tests {
    use super::prepare_lineage_sql;

    #[test]
    fn prepare_lineage_sql_replaces_logical_lineage_source() {
        let sql = prepare_lineage_sql("SELECT count(*) FROM lineage WHERE record_kind = 4")
            .expect("lineage query should prepare");

        assert!(sql.contains("analytics.turn_lineage"));
        assert!(sql.contains("record_kind = 4"));
    }

    #[test]
    fn prepare_lineage_sql_rejects_mutating_statement() {
        let error = prepare_lineage_sql("DELETE FROM lineage")
            .expect_err("mutating lineage query should fail");

        assert!(error.to_string().contains("only SELECT"));
    }
}
