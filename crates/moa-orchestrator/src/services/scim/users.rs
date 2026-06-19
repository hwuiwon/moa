//! SCIM User resource handlers.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use moa_ocsf::ActorInput;
use serde::Deserialize;
use uuid::Uuid;

use super::patch::{PatchOp, UserMutation, interpret_user};
use super::schema::{ListResponse, Meta, Name, SCHEMA_LIST, SCHEMA_USER, ScimEmail, ScimUser};
use super::{ScimResponseError, ScimState, authenticate_scim, map_db};
use crate::identity_admin::users::{self as user_admin, UserFilter, UserPatch, UserRow, UserWrite};

/// List SCIM users.
pub async fn list_users(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse<ScimUser>>, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    let start = query.start_index.unwrap_or(1).max(1);
    let count = query.count.unwrap_or(50).clamp(1, 200);
    let filter = query
        .filter
        .as_deref()
        .map(parse_filter)
        .transpose()
        .map_err(|error| ScimResponseError::bad_request("invalidFilter", error))?;
    let (total, rows) = user_admin::fetch_users_page(
        &state.pool,
        identity.tenant_id,
        filter.as_ref(),
        start,
        count,
    )
    .await
    .map_err(map_db)?;
    let users = rows
        .into_iter()
        .map(|row| scim_user_from_row(&state, row))
        .collect::<Vec<_>>();

    Ok(Json(ListResponse {
        schemas: vec![SCHEMA_LIST.to_string()],
        total_results: total,
        items_per_page: users.len() as i64,
        start_index: start,
        resources: users,
    }))
}

/// Create a SCIM user.
pub async fn create_user(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Json(request): Json<ScimUser>,
) -> Result<(StatusCode, Json<ScimUser>), ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    let email = primary_email(&request)?;
    let user_id = user_admin::create_user(
        &state.pool,
        identity.tenant_id,
        ActorInput::from_identity(&identity),
        UserWrite {
            email,
            external_id: request.external_id.clone(),
            given_name: request
                .name
                .as_ref()
                .and_then(|name| name.given_name.clone()),
            family_name: request
                .name
                .as_ref()
                .and_then(|name| name.family_name.clone()),
            display_name: request.display_name.clone(),
            active: request.active,
        },
    )
    .await?;
    let body = user_admin::fetch_user_by_id(&state.pool, identity.tenant_id, user_id)
        .await?
        .map(|row| scim_user_from_row(&state, row))
        .ok_or_else(|| ScimResponseError::not_found("user not found after create"))?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// Read one SCIM user.
pub async fn get_user(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ScimUser>, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    let user = user_admin::fetch_user_by_id(&state.pool, identity.tenant_id, id)
        .await?
        .map(|row| scim_user_from_row(&state, row))
        .ok_or_else(|| ScimResponseError::not_found("user not found"))?;
    Ok(Json(user))
}

/// Replace one SCIM user.
pub async fn put_user(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<ScimUser>,
) -> Result<Json<ScimUser>, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    let email = primary_email(&request)?;
    user_admin::replace_user(
        &state.pool,
        identity.tenant_id,
        id,
        ActorInput::from_identity(&identity),
        UserWrite {
            email,
            external_id: request.external_id.clone(),
            given_name: request
                .name
                .as_ref()
                .and_then(|name| name.given_name.clone()),
            family_name: request
                .name
                .as_ref()
                .and_then(|name| name.family_name.clone()),
            display_name: request.display_name.clone(),
            active: request.active,
        },
    )
    .await?;
    let body = user_admin::fetch_user_by_id(&state.pool, identity.tenant_id, id)
        .await?
        .map(|row| scim_user_from_row(&state, row))
        .ok_or_else(|| ScimResponseError::not_found("user not found"))?;
    Ok(Json(body))
}

/// Patch one SCIM user.
pub async fn patch_user(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(patch): Json<PatchOp>,
) -> Result<Json<ScimUser>, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    let mutation = interpret_user(&patch)
        .map_err(|error| ScimResponseError::bad_request("invalidSyntax", error))?;
    user_admin::patch_user(
        &state.pool,
        identity.tenant_id,
        id,
        ActorInput::from_identity(&identity),
        user_patch(mutation),
    )
    .await?;
    let body = user_admin::fetch_user_by_id(&state.pool, identity.tenant_id, id)
        .await?
        .map(|row| scim_user_from_row(&state, row))
        .ok_or_else(|| ScimResponseError::not_found("user not found"))?;
    Ok(Json(body))
}

/// Delete one SCIM user.
pub async fn delete_user(
    State(state): State<ScimState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ScimResponseError> {
    let identity = authenticate_scim(&state, &headers).await?;
    user_admin::delete_user(
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

fn user_patch(mutation: UserMutation) -> UserPatch {
    UserPatch {
        email: mutation.email,
        display_name: mutation.display_name,
        given_name: mutation.given_name,
        family_name: mutation.family_name,
        active: mutation.active,
    }
}

fn scim_user_from_row(state: &ScimState, row: UserRow) -> ScimUser {
    let formatted =
        row.display_name
            .clone()
            .or_else(|| match (&row.given_name, &row.family_name) {
                (Some(given), Some(family)) => Some(format!("{given} {family}")),
                (Some(given), None) => Some(given.clone()),
                (None, Some(family)) => Some(family.clone()),
                (None, None) => None,
            });
    ScimUser {
        schemas: vec![SCHEMA_USER.to_string()],
        id: Some(row.id.to_string()),
        external_id: row.external_id,
        user_name: row.email.clone(),
        name: Some(Name {
            given_name: row.given_name,
            family_name: row.family_name,
            formatted,
        }),
        display_name: row.display_name,
        emails: vec![ScimEmail {
            value: row.email,
            primary: Some(true),
            kind: Some("work".to_string()),
        }],
        active: row.active,
        meta: Some(Meta {
            resource_type: "User".to_string(),
            created: row.created_at,
            last_modified: row.updated_at,
            version: format!("W/\"{}\"", row.version),
            location: format!("{}/Users/{}", state.base_url, row.id),
        }),
    }
}

fn primary_email(request: &ScimUser) -> Result<String, ScimResponseError> {
    let email = request
        .emails
        .iter()
        .find(|email| email.primary == Some(true))
        .or_else(|| request.emails.first())
        .map(|email| email.value.as_str())
        .unwrap_or(&request.user_name)
        .trim();
    if email.is_empty() {
        return Err(ScimResponseError::bad_request(
            "invalidValue",
            "userName or emails[0].value is required",
        ));
    }
    Ok(email.to_string())
}

fn parse_filter(filter: &str) -> Result<UserFilter, &'static str> {
    let (attribute, value) = parse_eq_filter(filter)?;
    match attribute {
        "userName" | "emails.value" => Ok(UserFilter::Email(value.to_string())),
        "externalId" => Ok(UserFilter::ExternalId(value.to_string())),
        _ => Err("only userName, emails.value, and externalId equality filters are supported"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_filter_supports_external_id() {
        // Pins: SCIM list supports the externalId equality filter Okta sends.
        let parsed = parse_filter("externalId eq \"okta-123\"").expect("parse filter");
        match parsed {
            UserFilter::ExternalId(value) => assert_eq!(value, "okta-123"),
            UserFilter::Email(_) => panic!("expected externalId filter"),
        }
    }
}
