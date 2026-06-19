//! SCIM Group resource handlers.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use moa_ocsf::ActorInput;
use serde::Deserialize;
use uuid::Uuid;

use super::patch::{PatchOp, interpret_group};
use super::schema::{ListResponse, Meta, SCHEMA_GROUP, SCHEMA_LIST, ScimGroup, ScimGroupMember};
use super::{ScimResponseError, ScimState, authenticate_scim, map_db};
use crate::identity_admin::groups::{
    self as group_admin, GroupFilter, GroupMemberRow, GroupPatch, GroupRow, GroupWithMembers,
    GroupWrite,
};

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
    let (total, rows) = group_admin::fetch_groups_page(
        &state.pool,
        identity.tenant_id,
        filter.as_ref(),
        start,
        count,
    )
    .await
    .map_err(map_db)?;
    let groups = rows
        .into_iter()
        .map(|row| scim_group_from_record(&state, row))
        .collect::<Vec<_>>();

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
    let group_id = group_admin::create_group(
        &state.pool,
        identity.tenant_id,
        ActorInput::from_identity(&identity),
        GroupWrite {
            display_name: request.display_name.trim().to_string(),
            external_id: request.external_id.clone(),
            members: parse_member_ids(&request.members)?,
        },
    )
    .await?;
    let body = group_admin::fetch_group_by_id(&state.pool, identity.tenant_id, group_id)
        .await?
        .map(|row| scim_group_from_record(&state, row))
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
    let group = group_admin::fetch_group_by_id(&state.pool, identity.tenant_id, id)
        .await?
        .map(|row| scim_group_from_record(&state, row))
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
    group_admin::replace_group(
        &state.pool,
        identity.tenant_id,
        id,
        ActorInput::from_identity(&identity),
        GroupWrite {
            display_name: request.display_name.trim().to_string(),
            external_id: request.external_id.clone(),
            members: parse_member_ids(&request.members)?,
        },
    )
    .await?;
    let body = group_admin::fetch_group_by_id(&state.pool, identity.tenant_id, id)
        .await?
        .map(|row| scim_group_from_record(&state, row))
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
    if let Some(display_name) = mutation.display_name.as_deref() {
        validate_display_name(display_name)?;
    }
    group_admin::patch_group(
        &state.pool,
        identity.tenant_id,
        id,
        ActorInput::from_identity(&identity),
        GroupPatch {
            display_name: mutation.display_name.map(|value| value.trim().to_string()),
            add_members: mutation
                .add_members
                .iter()
                .map(|member| parse_member_id(member))
                .collect::<Result<Vec<_>, _>>()?,
            remove_members: mutation
                .remove_members
                .iter()
                .map(|member| parse_member_id(member))
                .collect::<Result<Vec<_>, _>>()?,
        },
    )
    .await?;
    let body = group_admin::fetch_group_by_id(&state.pool, identity.tenant_id, id)
        .await?
        .map(|row| scim_group_from_record(&state, row))
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
    group_admin::delete_group(
        &state.pool,
        identity.tenant_id,
        id,
        ActorInput::from_identity(&identity),
    )
    .await?;
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

fn parse_member_ids(members: &[ScimGroupMember]) -> Result<Vec<Uuid>, ScimResponseError> {
    members
        .iter()
        .map(|member| parse_member_id(&member.value))
        .collect()
}

fn scim_group_from_record(state: &ScimState, record: GroupWithMembers) -> ScimGroup {
    scim_group_from_row(
        state,
        record.group,
        record
            .members
            .into_iter()
            .map(scim_member_from_row)
            .collect(),
    )
}

fn scim_member_from_row(row: GroupMemberRow) -> ScimGroupMember {
    ScimGroupMember {
        value: row.user_id.to_string(),
        display: Some(row.email),
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
