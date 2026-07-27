//! Postgres row decoding and repository error helpers.

use super::*;

pub(super) fn storage_partition_id(tenant_id: TenantId) -> String {
    StoragePartitionId::for_tenant(tenant_id).to_string()
}

pub(super) fn ensure_rows_affected(rows: u64, operation: &str) -> Result<()> {
    if rows > 0 {
        return Ok(());
    }
    Err(Error::Repository(format!(
        "{operation} wrote no rows because its parent was not visible"
    )))
}

/// Confirms a batch insert wrote exactly the expected number of rows.
///
/// A multi-row `INSERT ... SELECT` joined to a parent row writes all `expected`
/// rows when the parent is visible and zero when it is not, so any other count
/// signals a lost or duplicated row and is reported as a repository error.
pub(super) fn ensure_all_rows_written(rows: u64, expected: u64, operation: &str) -> Result<()> {
    if rows == expected {
        return Ok(());
    }
    Err(Error::Repository(format!(
        "{operation} wrote {rows} rows but expected {expected}; parent may not be visible"
    )))
}

/// Encodes a string slice as a JSON array literal for transport as `TEXT`.
///
/// The literal is rebuilt into a Postgres `TEXT[]` with `jsonb_array_elements_text`,
/// which sidesteps binding nested `TEXT[][]` arrays through multi-column `UNNEST`.
pub(super) fn encode_text_array(values: &[String]) -> Result<String> {
    serde_json::to_string(values)
        .map_err(|error| Error::Repository(format!("failed to encode text array: {error}")))
}

/// Encodes JSON metadata as a string for transport as `TEXT` cast to `JSONB`.
pub(super) fn encode_jsonb(value: &serde_json::Value) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|error| Error::Repository(format!("failed to encode jsonb value: {error}")))
}

pub(super) fn connection_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeConnection> {
    Ok(KnowledgeConnection {
        connection_uid: row.try_get("connection_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        provider: row.try_get("provider").map_err(map_sqlx_error)?,
        connector: row.try_get("connector").map_err(map_sqlx_error)?,
        provider_account_id: row
            .try_get("provider_connection_id")
            .map_err(map_sqlx_error)?,
        credential_ref: row.try_get("credential_ref").map_err(map_sqlx_error)?,
        status: connection_status(row.try_get("status").map_err(map_sqlx_error)?)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
        source_selection: row.try_get("source_selection").map_err(map_sqlx_error)?,
        information_barrier: row
            .try_get::<Option<String>, _>("information_barrier")
            .map_err(map_sqlx_error)?
            .map(InformationBarrierId::parse)
            .transpose()
            .map_err(map_moa_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
        last_synced_at: row.try_get("last_synced_at").map_err(map_sqlx_error)?,
    })
}

pub(super) fn connection_projection_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<KnowledgeConnectionProjection> {
    let last_sync_status = row
        .try_get::<Option<String>, _>("last_sync_status")
        .map_err(map_sqlx_error)?
        .map(sync_run_status)
        .transpose()?;
    Ok(KnowledgeConnectionProjection {
        connection: connection_from_row(row)?,
        last_sync_status,
    })
}

pub(super) fn provider_account_lookup_from_rows(
    rows: &[sqlx::postgres::PgRow],
) -> Result<ProviderAccountConnectionLookup> {
    match rows {
        [] => Ok(ProviderAccountConnectionLookup::NotFound),
        [row] => connection_from_row(row).map(ProviderAccountConnectionLookup::Unique),
        rows => Ok(ProviderAccountConnectionLookup::Ambiguous {
            matches: rows.len(),
        }),
    }
}

pub(super) fn sync_run_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeSyncRun> {
    let max_records: Option<i64> = row.try_get("max_records").map_err(map_sqlx_error)?;
    let records_seen: i64 = row.try_get("records_seen").map_err(map_sqlx_error)?;
    let records_changed: i64 = row.try_get("records_changed").map_err(map_sqlx_error)?;
    let records_deleted: i64 = row.try_get("records_deleted").map_err(map_sqlx_error)?;
    let records_ingested: i64 = row.try_get("records_ingested").map_err(map_sqlx_error)?;
    let records_failed: i64 = row.try_get("records_failed").map_err(map_sqlx_error)?;
    let objects_parsed: i64 = row.try_get("objects_parsed").map_err(map_sqlx_error)?;
    let chunks_embedded: i64 = row.try_get("chunks_embedded").map_err(map_sqlx_error)?;
    let graph_nodes_upserted: i64 = row
        .try_get("graph_nodes_upserted")
        .map_err(map_sqlx_error)?;
    let graph_edges_upserted: i64 = row
        .try_get("graph_edges_upserted")
        .map_err(map_sqlx_error)?;
    Ok(KnowledgeSyncRun {
        sync_run_uid: row.try_get("sync_run_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        connection_uid: row.try_get("connection_id").map_err(map_sqlx_error)?,
        parser: row.try_get("parser_provider").map_err(map_sqlx_error)?,
        max_records: max_records
            .map(u32::try_from)
            .transpose()
            .map_err(map_int_error)?,
        information_barrier: row
            .try_get::<Option<String>, _>("information_barrier")
            .map_err(map_sqlx_error)?
            .map(InformationBarrierId::parse)
            .transpose()
            .map_err(map_moa_error)?,
        status: sync_run_status(row.try_get("status").map_err(map_sqlx_error)?)?,
        records_seen: u64::try_from(records_seen).map_err(map_int_error)?,
        records_changed: u64::try_from(records_changed).map_err(map_int_error)?,
        records_deleted: u64::try_from(records_deleted).map_err(map_int_error)?,
        records_ingested: u64::try_from(records_ingested).map_err(map_int_error)?,
        records_failed: u64::try_from(records_failed).map_err(map_int_error)?,
        objects_parsed: u64::try_from(objects_parsed).map_err(map_int_error)?,
        chunks_embedded: u64::try_from(chunks_embedded).map_err(map_int_error)?,
        graph_nodes_upserted: u64::try_from(graph_nodes_upserted).map_err(map_int_error)?,
        graph_edges_upserted: u64::try_from(graph_edges_upserted).map_err(map_int_error)?,
        error_code: row.try_get("error_code").map_err(map_sqlx_error)?,
        started_at: row.try_get("started_at").map_err(map_sqlx_error)?,
        finished_at: row.try_get("finished_at").map_err(map_sqlx_error)?,
        provider_trigger_completed_at: row
            .try_get("provider_trigger_completed_at")
            .map_err(map_sqlx_error)?,
    })
}

pub(super) fn link_claim_from_row(row: &sqlx::postgres::PgRow) -> Result<LinkClaim> {
    let state: String = row.try_get("state").map_err(map_sqlx_error)?;
    Ok(LinkClaim {
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        operation_id: row.try_get("operation_id").map_err(map_sqlx_error)?,
        request_hash: row.try_get("request_hash").map_err(map_sqlx_error)?,
        owner_identity_id: row.try_get("owner_identity_id").map_err(map_sqlx_error)?,
        connection_uid: row.try_get("connection_uid").map_err(map_sqlx_error)?,
        previous_credential_ref: row
            .try_get("previous_credential_ref")
            .map_err(map_sqlx_error)?,
        candidate_credential_ref: row
            .try_get("candidate_credential_ref")
            .map_err(map_sqlx_error)?,
        state: LinkClaimState::from_str_exact(&state).ok_or_else(|| {
            Error::Repository(format!("unknown knowledge link claim state `{state}`"))
        })?,
        sync_run_uid: row.try_get("sync_run_uid").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
    })
}

pub(super) fn object_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeObject> {
    Ok(KnowledgeObject {
        object_uid: row.try_get("object_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        connection_uid: row.try_get("connection_id").map_err(map_sqlx_error)?,
        object_type: row.try_get("object_type").map_err(map_sqlx_error)?,
        source_id: row.try_get("external_object_id").map_err(map_sqlx_error)?,
        parent_source_id: row
            .try_get("parent_external_object_id")
            .map_err(map_sqlx_error)?,
        source_uri: row.try_get("source_uri").map_err(map_sqlx_error)?,
        title: row.try_get("title").map_err(map_sqlx_error)?,
        change_token: row.try_get("change_token").map_err(map_sqlx_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
        status: object_status(row.try_get("status").map_err(map_sqlx_error)?)?,
        source_updated_at: row.try_get("last_modified_at").map_err(map_sqlx_error)?,
        deleted_at: row.try_get("deleted_at").map_err(map_sqlx_error)?,
    })
}

pub(super) fn object_projection_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<KnowledgeObjectProjection> {
    let chunk_count: i64 = row.try_get("chunk_count").map_err(map_sqlx_error)?;
    Ok(KnowledgeObjectProjection {
        object: object_from_row(row)?,
        parser: row.try_get("parser_provider").map_err(map_sqlx_error)?,
        parser_status: row.try_get("parser_status").map_err(map_sqlx_error)?,
        chunk_count: u64::try_from(chunk_count).map_err(map_int_error)?,
    })
}

pub(super) fn document_version_from_row(row: &sqlx::postgres::PgRow) -> Result<DocumentVersion> {
    Ok(DocumentVersion {
        version_uid: row
            .try_get("document_version_uid")
            .map_err(map_sqlx_error)?,
        object_uid: row.try_get("object_id").map_err(map_sqlx_error)?,
        parser: row.try_get("parser_provider").map_err(map_sqlx_error)?,
        parser_job_id: row.try_get("parser_job_id").map_err(map_sqlx_error)?,
        content_hash: row.try_get("content_hash").map_err(map_sqlx_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
    })
}

pub(super) fn chunk_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeChunk> {
    let ordinal: i32 = row.try_get("ordinal").map_err(map_sqlx_error)?;
    let token_count: i32 = row.try_get("token_count").map_err(map_sqlx_error)?;
    Ok(KnowledgeChunk {
        chunk_uid: row.try_get("chunk_uid").map_err(map_sqlx_error)?,
        version_uid: row.try_get("document_version_id").map_err(map_sqlx_error)?,
        chunk_hash: row.try_get("chunk_hash").map_err(map_sqlx_error)?,
        block_hashes: row.try_get("block_hashes").map_err(map_sqlx_error)?,
        text: row.try_get("text").map_err(map_sqlx_error)?,
        heading_path: row.try_get("heading_path").map_err(map_sqlx_error)?,
        ordinal: u32::try_from(ordinal).map_err(map_int_error)?,
        token_count: usize::try_from(token_count).map_err(map_int_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
    })
}

pub(super) fn contact_group_from_row(row: &sqlx::postgres::PgRow) -> Result<ContactGroup> {
    Ok(ContactGroup {
        group_uid: row.try_get("group_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        group_key: row.try_get("normalized_name").map_err(map_sqlx_error)?,
        display_name: row.try_get("display_name").map_err(map_sqlx_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
    })
}

pub(super) fn source_connection_id(metadata: &serde_json::Value) -> Option<Uuid> {
    metadata
        .get("source_connection_uid")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
}

pub(super) fn contact_group_target_member_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ContactGroupTargetMember> {
    Ok(ContactGroupTargetMember {
        contact_id: ContactId(
            row.try_get::<Uuid, _>("contact_id")
                .map_err(map_sqlx_error)?,
        ),
        evidence: row.try_get("evidence_ids").map_err(map_sqlx_error)?,
        metadata: row.try_get("metadata").map_err(map_sqlx_error)?,
    })
}

pub(super) fn provider_event_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<KnowledgeProviderEventRecord> {
    Ok(KnowledgeProviderEventRecord {
        provider_event_uid: row.try_get("provider_event_uid").map_err(map_sqlx_error)?,
        tenant_id: TenantId::from(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        connection_uid: row.try_get("connection_id").map_err(map_sqlx_error)?,
        provider: row.try_get("provider").map_err(map_sqlx_error)?,
        provider_event_id: row.try_get("provider_event_id").map_err(map_sqlx_error)?,
        event_type: row.try_get("event_type").map_err(map_sqlx_error)?,
        status: row.try_get("status").map_err(map_sqlx_error)?,
        payload: row.try_get("payload").map_err(map_sqlx_error)?,
        duplicate: row.try_get("duplicate").map_err(map_sqlx_error)?,
    })
}

pub(super) fn step_from_row(row: &sqlx::postgres::PgRow) -> Result<KnowledgeIngestionStep> {
    let attempt: i32 = row.try_get("attempt").map_err(map_sqlx_error)?;
    let duration_ms: Option<i64> = row.try_get("duration_ms").map_err(map_sqlx_error)?;
    Ok(KnowledgeIngestionStep {
        step_uid: row.try_get("step_uid").map_err(map_sqlx_error)?,
        sync_run_uid: row.try_get("sync_run_id").map_err(map_sqlx_error)?,
        object_uid: row.try_get("object_id").map_err(map_sqlx_error)?,
        step: row.try_get("stage").map_err(map_sqlx_error)?,
        status: ingestion_step_status(row.try_get("status").map_err(map_sqlx_error)?)?,
        started_at: row.try_get("started_at").map_err(map_sqlx_error)?,
        ended_at: row.try_get("ended_at").map_err(map_sqlx_error)?,
        duration_ms: duration_ms.map(|value| value as u64),
        counters: row.try_get("counters").map_err(map_sqlx_error)?,
        summary: row.try_get("safe_summary").map_err(map_sqlx_error)?,
        retry_count: u32::try_from(attempt).map_err(map_int_error)?,
        error_code: row.try_get("error_code").map_err(map_sqlx_error)?,
    })
}

pub(super) fn connection_status(value: String) -> Result<ConnectionStatus> {
    match value.as_str() {
        "pending" => Ok(ConnectionStatus::Pending),
        "active" => Ok(ConnectionStatus::Active),
        "disabled" => Ok(ConnectionStatus::Disabled),
        "error" => Ok(ConnectionStatus::Error),
        _ => Err(Error::Repository(format!(
            "unknown knowledge connection status `{value}`"
        ))),
    }
}

pub(super) fn sync_run_status(value: String) -> Result<crate::domain::SyncRunStatus> {
    match value.as_str() {
        "queued" => Ok(crate::domain::SyncRunStatus::Queued),
        "provider_syncing" => Ok(crate::domain::SyncRunStatus::ProviderSyncing),
        "provider_synced" => Ok(crate::domain::SyncRunStatus::ProviderSynced),
        "parse_pending" => Ok(crate::domain::SyncRunStatus::ParsePending),
        "ingesting" => Ok(crate::domain::SyncRunStatus::Ingesting),
        "failed_retryable" => Ok(crate::domain::SyncRunStatus::FailedRetryable),
        "failed_terminal" => Ok(crate::domain::SyncRunStatus::FailedTerminal),
        "canceled" => Ok(crate::domain::SyncRunStatus::Canceled),
        "completed" => Ok(crate::domain::SyncRunStatus::Completed),
        _ => Err(Error::Repository(format!(
            "unknown knowledge sync-run status `{value}`"
        ))),
    }
}

pub(super) fn object_status(value: String) -> Result<ObjectStatus> {
    match value.as_str() {
        "pending" => Ok(ObjectStatus::Pending),
        "active" => Ok(ObjectStatus::Active),
        "deleted" => Ok(ObjectStatus::Deleted),
        "error" => Ok(ObjectStatus::Error),
        _ => Err(Error::Repository(format!(
            "unknown knowledge object status `{value}`"
        ))),
    }
}

pub(super) fn ingestion_step_status(value: String) -> Result<IngestionStepStatus> {
    match value.as_str() {
        "started" => Ok(IngestionStepStatus::Started),
        "completed" => Ok(IngestionStepStatus::Completed),
        "failed" => Ok(IngestionStepStatus::Failed),
        "skipped" => Ok(IngestionStepStatus::Skipped),
        _ => Err(Error::Repository(format!(
            "unknown knowledge ingestion step status `{value}`"
        ))),
    }
}

pub(super) fn map_sqlx_error(error: sqlx::Error) -> Error {
    match error {
        // Preserve the driver diagnostics: SQLSTATE, constraint, table, and
        // the DETAIL line are what distinguish a duplicate-key race (23505 on
        // a named constraint) from a deadlock (40P01) in test and log output.
        sqlx::Error::Database(database) => Error::Database {
            code: database.code().map(|code| code.into_owned()),
            constraint: database.constraint().map(str::to_owned),
            table: database.table().map(str::to_owned),
            message: database.message().to_owned(),
            detail: database
                .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
                .and_then(|postgres| postgres.detail().map(str::to_owned)),
        },
        other => Error::Repository(other.to_string()),
    }
}

pub(super) fn map_moa_error(error: moa_core::error::MoaError) -> Error {
    Error::Repository(error.to_string())
}

pub(super) fn map_int_error(error: std::num::TryFromIntError) -> Error {
    Error::Repository(format!("knowledge integer conversion failed: {error}"))
}
