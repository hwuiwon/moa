//! SCIM Group resource handlers.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use moa_authz::enqueue_raw;
use moa_authz_schema::TupleOp;
use moa_ocsf::ActorInput;
use serde::Deserialize;
use uuid::Uuid;

use super::patch::{PatchOp, interpret_group};
use super::schema::{ListResponse, Meta, SCHEMA_GROUP, SCHEMA_LIST, ScimGroup, ScimGroupMember};
use super::{ScimResponseError, ScimState, authenticate_scim, map_db, map_outbox};

/// List SCIM groups.
pub async fn list_groups(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse<ScimGroup>>, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    let start = query.start_index.unwrap_or(1).max(1);
    let count = query.count.unwrap_or(50).clamp(1, 200);
    let filter = query
        .filter
        .as_deref()
        .map(parse_filter)
        .transpose()
        .map_err(|error| ScimResponseError::bad_request("invalidFilter", error))?;
    let (total, groups) =
        fetch_groups_page(&state, identity.tenant_id, filter.as_ref(), start, count)
            .await
            .map_err(map_db)?;

    Ok(Json(ListResponse {
        schemas: vec![SCHEMA_LIST.to_string()],
        total_results: total,
        items_per_page: groups.len() as i64,
        start_index: start,
        resources: groups,
    }))
}

/// Create a SCIM group.
pub async fn create_group(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Json(request): Json<ScimGroup>,
) -> Result<(StatusCode, Json<ScimGroup>), ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    validate_display_name(&request.display_name)?;
    let group_id = Uuid::new_v4();
    let mut tx = state.pool.begin().await.map_err(map_db)?;
    sqlx::query(
        r#"
        INSERT INTO scim_groups (id, tenant_id, display_name, external_id)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(group_id)
    .bind(identity.tenant_id)
    .bind(request.display_name.trim())
    .bind(request.external_id.as_deref())
    .execute(&mut *tx)
    .await
    .map_err(map_db)?;

    for member in &request.members {
        let user_id = parse_member_id(&member.value)?;
        add_group_member(
            &mut tx,
            identity.tenant_id,
            group_id,
            request.display_name.trim(),
            user_id,
            ActorInput::from_identity(&identity),
        )
        .await?;
    }

    moa_ocsf::emit_scim_group_created_tx(
        &mut tx,
        identity.tenant_id,
        ActorInput::from_identity(&identity),
        group_id,
    )
    .await
    .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)?;
    let body = fetch_group_by_id(&state, identity.tenant_id, group_id)
        .await?
        .ok_or_else(|| ScimResponseError::not_found("group not found after create"))?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// Read one SCIM group.
pub async fn get_group(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ScimGroup>, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    let group = fetch_group_by_id(&state, identity.tenant_id, id)
        .await?
        .ok_or_else(|| ScimResponseError::not_found("group not found"))?;
    Ok(Json(group))
}

/// Replace one SCIM group.
pub async fn put_group(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<ScimGroup>,
) -> Result<Json<ScimGroup>, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    validate_display_name(&request.display_name)?;
    let mut tx = state.pool.begin().await.map_err(map_db)?;
    let existing = fetch_group_row_for_update(&mut tx, identity.tenant_id, id).await?;
    let current_members = fetch_member_ids(&mut tx, id).await?;

    for user_id in &current_members {
        enqueue_group_mapping(
            &mut tx,
            TupleOp::Delete,
            identity.tenant_id,
            &existing.display_name,
            *user_id,
        )
        .await?;
        moa_ocsf::emit_group_membership_removed_tx(
            &mut tx,
            identity.tenant_id,
            ActorInput::from_identity(&identity),
            id,
            *user_id,
        )
        .await
        .map_err(map_audit)?;
    }
    sqlx::query("DELETE FROM scim_group_members WHERE group_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;

    sqlx::query(
        r#"
        UPDATE scim_groups
        SET display_name = $3,
            external_id = $4,
            updated_at = NOW(),
            version = version + 1
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(id)
    .bind(identity.tenant_id)
    .bind(request.display_name.trim())
    .bind(request.external_id.as_deref())
    .execute(&mut *tx)
    .await
    .map_err(map_db)?;

    for member in &request.members {
        let user_id = parse_member_id(&member.value)?;
        add_group_member(
            &mut tx,
            identity.tenant_id,
            id,
            request.display_name.trim(),
            user_id,
            ActorInput::from_identity(&identity),
        )
        .await?;
    }

    moa_ocsf::emit_scim_group_updated_tx(
        &mut tx,
        identity.tenant_id,
        ActorInput::from_identity(&identity),
        id,
    )
    .await
    .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)?;
    let body = fetch_group_by_id(&state, identity.tenant_id, id)
        .await?
        .ok_or_else(|| ScimResponseError::not_found("group not found"))?;
    Ok(Json(body))
}

/// Patch one SCIM group.
pub async fn patch_group(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(patch): Json<PatchOp>,
) -> Result<Json<ScimGroup>, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    let mutation = interpret_group(&patch);
    let mut tx = state.pool.begin().await.map_err(map_db)?;
    let existing = fetch_group_row_for_update(&mut tx, identity.tenant_id, id).await?;
    let display_name = mutation
        .display_name
        .as_deref()
        .unwrap_or(&existing.display_name)
        .trim()
        .to_string();
    validate_display_name(&display_name)?;

    if mutation.display_name.is_some() && display_name != existing.display_name {
        let members = fetch_member_ids(&mut tx, id).await?;
        for user_id in &members {
            enqueue_group_mapping(
                &mut tx,
                TupleOp::Delete,
                identity.tenant_id,
                &existing.display_name,
                *user_id,
            )
            .await?;
            moa_ocsf::emit_group_membership_removed_tx(
                &mut tx,
                identity.tenant_id,
                ActorInput::from_identity(&identity),
                id,
                *user_id,
            )
            .await
            .map_err(map_audit)?;
            enqueue_group_mapping(
                &mut tx,
                TupleOp::Write,
                identity.tenant_id,
                &display_name,
                *user_id,
            )
            .await?;
            moa_ocsf::emit_group_membership_added_tx(
                &mut tx,
                identity.tenant_id,
                ActorInput::from_identity(&identity),
                id,
                *user_id,
            )
            .await
            .map_err(map_audit)?;
        }
        sqlx::query(
            "UPDATE scim_groups SET display_name = $3, updated_at = NOW(), version = version + 1 WHERE id = $1 AND tenant_id = $2",
        )
        .bind(id)
        .bind(identity.tenant_id)
        .bind(&display_name)
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
    }

    for member in mutation.add_members {
        let user_id = parse_member_id(&member)?;
        add_group_member(
            &mut tx,
            identity.tenant_id,
            id,
            &display_name,
            user_id,
            ActorInput::from_identity(&identity),
        )
        .await?;
    }
    for member in mutation.remove_members {
        let user_id = parse_member_id(&member)?;
        remove_group_member(
            &mut tx,
            identity.tenant_id,
            id,
            &display_name,
            user_id,
            ActorInput::from_identity(&identity),
        )
        .await?;
    }

    moa_ocsf::emit_scim_group_updated_tx(
        &mut tx,
        identity.tenant_id,
        ActorInput::from_identity(&identity),
        id,
    )
    .await
    .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)?;
    let body = fetch_group_by_id(&state, identity.tenant_id, id)
        .await?
        .ok_or_else(|| ScimResponseError::not_found("group not found"))?;
    Ok(Json(body))
}

/// Delete one SCIM group.
pub async fn delete_group(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    let mut tx = state.pool.begin().await.map_err(map_db)?;
    let existing = fetch_group_row_for_update(&mut tx, identity.tenant_id, id).await?;
    let members = fetch_member_ids(&mut tx, id).await?;
    for user_id in members {
        enqueue_group_mapping(
            &mut tx,
            TupleOp::Delete,
            identity.tenant_id,
            &existing.display_name,
            user_id,
        )
        .await?;
        moa_ocsf::emit_group_membership_removed_tx(
            &mut tx,
            identity.tenant_id,
            ActorInput::from_identity(&identity),
            id,
            user_id,
        )
        .await
        .map_err(map_audit)?;
    }
    sqlx::query("DELETE FROM scim_groups WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(identity.tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;

    moa_ocsf::emit_scim_group_deleted_tx(
        &mut tx,
        identity.tenant_id,
        ActorInput::from_identity(&identity),
        id,
    )
    .await
    .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)?;
    Ok(StatusCode::NO_CONTENT)
}

/// List query parameters.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// One-based start index.
    #[serde(rename = "startIndex")]
    pub start_index: Option<i64>,
    /// Page size.
    pub count: Option<i64>,
    /// Minimal SCIM filter expression.
    pub filter: Option<String>,
}

#[derive(Debug)]
enum GroupFilter {
    DisplayName(String),
    ExternalId(String),
}

#[derive(Debug, sqlx::FromRow)]
struct GroupRow {
    id: Uuid,
    external_id: Option<String>,
    display_name: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    version: i64,
}

async fn fetch_groups_page(
    state: &ScimState,
    tenant_id: Uuid,
    filter: Option<&GroupFilter>,
    start: i64,
    count: i64,
) -> Result<(i64, Vec<ScimGroup>), sqlx::Error> {
    let offset = start.saturating_sub(1);
    let (total, rows): (i64, Vec<GroupRow>) = match filter {
        None => {
            let total = sqlx::query_scalar("SELECT COUNT(*) FROM scim_groups WHERE tenant_id = $1")
                .bind(tenant_id)
                .fetch_one(&state.pool)
                .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, display_name, created_at, updated_at, version
                FROM scim_groups
                WHERE tenant_id = $1
                ORDER BY created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(tenant_id)
            .bind(count)
            .bind(offset)
            .fetch_all(&state.pool)
            .await?;
            (total, rows)
        }
        Some(GroupFilter::DisplayName(display_name)) => {
            let total = sqlx::query_scalar(
                "SELECT COUNT(*) FROM scim_groups WHERE tenant_id = $1 AND display_name = $2",
            )
            .bind(tenant_id)
            .bind(display_name)
            .fetch_one(&state.pool)
            .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, display_name, created_at, updated_at, version
                FROM scim_groups
                WHERE tenant_id = $1 AND display_name = $2
                ORDER BY created_at DESC
                LIMIT $3 OFFSET $4
                "#,
            )
            .bind(tenant_id)
            .bind(display_name)
            .bind(count)
            .bind(offset)
            .fetch_all(&state.pool)
            .await?;
            (total, rows)
        }
        Some(GroupFilter::ExternalId(external_id)) => {
            let total = sqlx::query_scalar(
                "SELECT COUNT(*) FROM scim_groups WHERE tenant_id = $1 AND external_id = $2",
            )
            .bind(tenant_id)
            .bind(external_id)
            .fetch_one(&state.pool)
            .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, display_name, created_at, updated_at, version
                FROM scim_groups
                WHERE tenant_id = $1 AND external_id = $2
                ORDER BY created_at DESC
                LIMIT $3 OFFSET $4
                "#,
            )
            .bind(tenant_id)
            .bind(external_id)
            .bind(count)
            .bind(offset)
            .fetch_all(&state.pool)
            .await?;
            (total, rows)
        }
    };

    let mut groups = Vec::with_capacity(rows.len());
    for row in rows {
        let group_id = row.id;
        groups.push(scim_group_from_row(
            state,
            row,
            fetch_members(&state.pool, group_id).await?,
        ));
    }
    Ok((total, groups))
}

async fn fetch_group_by_id(
    state: &ScimState,
    tenant_id: Uuid,
    group_id: Uuid,
) -> Result<Option<ScimGroup>, ScimResponseError> {
    let row: Option<GroupRow> = sqlx::query_as(
        r#"
        SELECT id, external_id, display_name, created_at, updated_at, version
        FROM scim_groups
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(group_id)
    .bind(tenant_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(map_db)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let members = fetch_members(&state.pool, group_id).await.map_err(map_db)?;
    Ok(Some(scim_group_from_row(state, row, members)))
}

async fn fetch_group_row_for_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    group_id: Uuid,
) -> Result<GroupRow, ScimResponseError> {
    sqlx::query_as(
        r#"
        SELECT id, external_id, display_name, created_at, updated_at, version
        FROM scim_groups
        WHERE id = $1 AND tenant_id = $2
        FOR UPDATE
        "#,
    )
    .bind(group_id)
    .bind(tenant_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db)?
    .ok_or_else(|| ScimResponseError::not_found("group not found"))
}

async fn fetch_members(
    pool: &sqlx::PgPool,
    group_id: Uuid,
) -> Result<Vec<ScimGroupMember>, sqlx::Error> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT u.id, u.email
        FROM scim_group_members gm
        JOIN users u ON u.id = gm.user_id
        WHERE gm.group_id = $1
        ORDER BY u.email
        "#,
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, email)| ScimGroupMember {
            value: id.to_string(),
            display: Some(email),
        })
        .collect())
}

async fn fetch_member_ids(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_id: Uuid,
) -> Result<Vec<Uuid>, ScimResponseError> {
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT user_id FROM scim_group_members WHERE group_id = $1")
            .bind(group_id)
            .fetch_all(&mut **tx)
            .await
            .map_err(map_db)?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

async fn add_group_member(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    group_id: Uuid,
    display_name: &str,
    user_id: Uuid,
    actor: ActorInput,
) -> Result<(), ScimResponseError> {
    ensure_user_in_tenant(tx, tenant_id, user_id).await?;
    sqlx::query(
        "INSERT INTO scim_group_members (group_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(group_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db)?;
    enqueue_group_mapping(tx, TupleOp::Write, tenant_id, display_name, user_id).await?;
    moa_ocsf::emit_group_membership_added_tx(tx, tenant_id, actor, group_id, user_id)
        .await
        .map_err(map_audit)?;
    Ok(())
}

async fn remove_group_member(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    group_id: Uuid,
    display_name: &str,
    user_id: Uuid,
    actor: ActorInput,
) -> Result<(), ScimResponseError> {
    sqlx::query("DELETE FROM scim_group_members WHERE group_id = $1 AND user_id = $2")
        .bind(group_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map_err(map_db)?;
    enqueue_group_mapping(tx, TupleOp::Delete, tenant_id, display_name, user_id).await?;
    moa_ocsf::emit_group_membership_removed_tx(tx, tenant_id, actor, group_id, user_id)
        .await
        .map_err(map_audit)?;
    Ok(())
}

async fn ensure_user_in_tenant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<(), ScimResponseError> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND tenant_id = $2)")
            .bind(user_id)
            .bind(tenant_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(map_db)?;
    if exists {
        Ok(())
    } else {
        Err(ScimResponseError::bad_request(
            "invalidValue",
            "group member user does not exist in tenant",
        ))
    }
}

async fn enqueue_group_mapping(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    op: TupleOp,
    tenant_id: Uuid,
    display_name: &str,
    user_id: Uuid,
) -> Result<(), ScimResponseError> {
    let targets = group_targets(tenant_id, display_name);
    for target in targets {
        enqueue_raw(
            &mut **tx,
            op,
            &format!("user:{user_id}"),
            &target.relation,
            &target.object,
            Some(tenant_id),
        )
        .await
        .map_err(map_outbox)?;
    }
    Ok(())
}

struct GroupTarget {
    relation: String,
    object: String,
}

fn group_targets(tenant_id: Uuid, display_name: &str) -> Vec<GroupTarget> {
    let parts: Vec<&str> = display_name.split(':').collect();
    match parts.as_slice() {
        ["tenant", tenant, "workspace", workspace, relation]
            if Uuid::parse_str(tenant).ok() == Some(tenant_id)
                && Uuid::parse_str(workspace).is_ok() =>
        {
            vec![GroupTarget {
                relation: (*relation).to_string(),
                object: format!("workspace:{workspace}"),
            }]
        }
        ["tenant", tenant, relation] if Uuid::parse_str(tenant).ok() == Some(tenant_id) => {
            vec![GroupTarget {
                relation: (*relation).to_string(),
                object: format!("tenant:{tenant}"),
            }]
        }
        _ => vec![GroupTarget {
            relation: "member".to_string(),
            object: format!("tenant:{tenant_id}"),
        }],
    }
}

fn scim_group_from_row(
    state: &ScimState,
    row: GroupRow,
    members: Vec<ScimGroupMember>,
) -> ScimGroup {
    ScimGroup {
        schemas: vec![SCHEMA_GROUP.to_string()],
        id: Some(row.id.to_string()),
        external_id: row.external_id,
        display_name: row.display_name,
        members,
        meta: Some(Meta {
            resource_type: "Group".to_string(),
            created: row.created_at,
            last_modified: row.updated_at,
            version: format!("W/\"{}\"", row.version),
            location: format!("{}/Groups/{}", state.base_url, row.id),
        }),
    }
}

fn validate_display_name(display_name: &str) -> Result<(), ScimResponseError> {
    if display_name.trim().is_empty() {
        return Err(ScimResponseError::bad_request(
            "invalidValue",
            "displayName is required",
        ));
    }
    Ok(())
}

fn parse_member_id(value: &str) -> Result<Uuid, ScimResponseError> {
    Uuid::parse_str(value)
        .map_err(|_| ScimResponseError::bad_request("invalidValue", "member value must be UUID"))
}

fn parse_filter(filter: &str) -> Result<GroupFilter, &'static str> {
    let (attribute, value) = parse_eq_filter(filter)?;
    match attribute {
        "displayName" => Ok(GroupFilter::DisplayName(value.to_string())),
        "externalId" => Ok(GroupFilter::ExternalId(value.to_string())),
        _ => Err("only displayName and externalId equality filters are supported"),
    }
}

fn parse_eq_filter(filter: &str) -> Result<(&str, &str), &'static str> {
    let Some((attribute, raw_value)) = filter.split_once(" eq ") else {
        return Err("expected '<attribute> eq \"value\"'");
    };
    let attribute = attribute.trim();
    let value = raw_value.trim();
    if !(value.starts_with('"') && value.ends_with('"') && value.len() >= 2) {
        return Err("filter value must be quoted");
    }
    Ok((attribute, &value[1..value.len() - 1]))
}

fn map_audit(error: moa_ocsf::EmitError) -> ScimResponseError {
    tracing::error!(error = %error, "SCIM group security audit failed");
    ScimResponseError::internal("security audit failed")
}
