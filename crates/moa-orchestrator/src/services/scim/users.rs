//! SCIM User resource handlers.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use moa_authz::enqueue_raw;
use moa_authz_schema::TupleOp;
use moa_ocsf::ActorInput;
use serde::Deserialize;
use uuid::Uuid;

use super::deactivation::{CascadeError, cascade_deactivate_user};
use super::patch::{PatchOp, UserMutation, interpret_user};
use super::schema::{ListResponse, Meta, Name, SCHEMA_LIST, SCHEMA_USER, ScimEmail, ScimUser};
use super::{ScimResponseError, ScimState, authenticate_scim, map_db, map_outbox};

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
    let (total, users) =
        fetch_users_page(&state, identity.tenant_id, filter.as_ref(), start, count)
            .await
            .map_err(map_db)?;

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
    let user_id = Uuid::new_v4();
    let mut tx = state.pool.begin().await.map_err(map_db)?;

    sqlx::query(
        r#"
        INSERT INTO users
            (id, tenant_id, email, external_id, given_name, family_name, display_name, active)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(user_id)
    .bind(identity.tenant_id)
    .bind(&email)
    .bind(request.external_id.as_deref())
    .bind(
        request
            .name
            .as_ref()
            .and_then(|name| name.given_name.as_deref()),
    )
    .bind(
        request
            .name
            .as_ref()
            .and_then(|name| name.family_name.as_deref()),
    )
    .bind(request.display_name.as_deref())
    .bind(request.active)
    .execute(&mut *tx)
    .await
    .map_err(map_db)?;

    if request.active {
        enqueue_tenant_member(&mut tx, identity.tenant_id, user_id, TupleOp::Write).await?;
    }

    moa_ocsf::emit_scim_user_created_tx(
        &mut tx,
        identity.tenant_id,
        ActorInput::from_identity(&identity),
        user_id,
    )
    .await
    .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)?;
    let body = fetch_user_by_id(&state, identity.tenant_id, user_id)
        .await?
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
    let user = fetch_user_by_id(&state, identity.tenant_id, id)
        .await?
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
    let mut tx = state.pool.begin().await.map_err(map_db)?;
    ensure_user_exists(&mut tx, identity.tenant_id, id).await?;

    if request.active {
        sqlx::query(
            r#"
            UPDATE users
            SET email = $3,
                external_id = $4,
                given_name = $5,
                family_name = $6,
                display_name = $7,
                active = true,
                deactivated_at = NULL,
                updated_at = NOW(),
                version = version + 1
            WHERE id = $1 AND tenant_id = $2
            "#,
        )
        .bind(id)
        .bind(identity.tenant_id)
        .bind(&email)
        .bind(request.external_id.as_deref())
        .bind(
            request
                .name
                .as_ref()
                .and_then(|name| name.given_name.as_deref()),
        )
        .bind(
            request
                .name
                .as_ref()
                .and_then(|name| name.family_name.as_deref()),
        )
        .bind(request.display_name.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        enqueue_tenant_member(&mut tx, identity.tenant_id, id, TupleOp::Write).await?;
    } else {
        cascade_deactivate_user(
            &mut tx,
            identity.tenant_id,
            id,
            ActorInput::from_identity(&identity),
        )
        .await
        .map_err(map_cascade)?;
        apply_user_mutation(
            &mut tx,
            identity.tenant_id,
            id,
            UserMutation {
                email: Some(email),
                display_name: request.display_name.clone(),
                given_name: request
                    .name
                    .as_ref()
                    .and_then(|name| name.given_name.clone()),
                family_name: request
                    .name
                    .as_ref()
                    .and_then(|name| name.family_name.clone()),
                active: None,
            },
        )
        .await?;
    }

    moa_ocsf::emit_scim_user_updated_tx(
        &mut tx,
        identity.tenant_id,
        ActorInput::from_identity(&identity),
        id,
    )
    .await
    .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)?;
    let body = fetch_user_by_id(&state, identity.tenant_id, id)
        .await?
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
    let mut tx = state.pool.begin().await.map_err(map_db)?;
    ensure_user_exists(&mut tx, identity.tenant_id, id).await?;

    match mutation.active {
        Some(false) => {
            cascade_deactivate_user(
                &mut tx,
                identity.tenant_id,
                id,
                ActorInput::from_identity(&identity),
            )
            .await
            .map_err(map_cascade)?;
        }
        Some(true) => {
            sqlx::query(
                r#"
                UPDATE users
                SET active = true,
                    deactivated_at = NULL,
                    updated_at = NOW(),
                    version = version + 1
                WHERE id = $1 AND tenant_id = $2
                "#,
            )
            .bind(id)
            .bind(identity.tenant_id)
            .execute(&mut *tx)
            .await
            .map_err(map_db)?;
            enqueue_tenant_member(&mut tx, identity.tenant_id, id, TupleOp::Write).await?;
        }
        None => {}
    }
    apply_user_mutation(&mut tx, identity.tenant_id, id, mutation).await?;

    moa_ocsf::emit_scim_user_updated_tx(
        &mut tx,
        identity.tenant_id,
        ActorInput::from_identity(&identity),
        id,
    )
    .await
    .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)?;
    let body = fetch_user_by_id(&state, identity.tenant_id, id)
        .await?
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
    let mut tx = state.pool.begin().await.map_err(map_db)?;
    ensure_user_exists(&mut tx, identity.tenant_id, id).await?;
    cascade_deactivate_user(
        &mut tx,
        identity.tenant_id,
        id,
        ActorInput::from_identity(&identity),
    )
    .await
    .map_err(map_cascade)?;
    sqlx::query("DELETE FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(identity.tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;

    moa_ocsf::emit_scim_user_deleted_tx(
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
enum UserFilter {
    Email(String),
    ExternalId(String),
}

#[derive(Debug, sqlx::FromRow)]
struct UserRow {
    id: Uuid,
    external_id: Option<String>,
    email: String,
    given_name: Option<String>,
    family_name: Option<String>,
    display_name: Option<String>,
    active: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    version: i64,
}

async fn fetch_users_page(
    state: &ScimState,
    tenant_id: Uuid,
    filter: Option<&UserFilter>,
    start: i64,
    count: i64,
) -> Result<(i64, Vec<ScimUser>), sqlx::Error> {
    let offset = start.saturating_sub(1);
    let (total, rows): (i64, Vec<UserRow>) = match filter {
        None => {
            let total = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE tenant_id = $1")
                .bind(tenant_id)
                .fetch_one(&state.pool)
                .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, email, given_name, family_name, display_name,
                       active, created_at, updated_at, version
                FROM users
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
        Some(UserFilter::Email(email)) => {
            let total = sqlx::query_scalar(
                "SELECT COUNT(*) FROM users WHERE tenant_id = $1 AND lower(email) = lower($2)",
            )
            .bind(tenant_id)
            .bind(email)
            .fetch_one(&state.pool)
            .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, email, given_name, family_name, display_name,
                       active, created_at, updated_at, version
                FROM users
                WHERE tenant_id = $1 AND lower(email) = lower($2)
                ORDER BY created_at DESC
                LIMIT $3 OFFSET $4
                "#,
            )
            .bind(tenant_id)
            .bind(email)
            .bind(count)
            .bind(offset)
            .fetch_all(&state.pool)
            .await?;
            (total, rows)
        }
        Some(UserFilter::ExternalId(external_id)) => {
            let total = sqlx::query_scalar(
                "SELECT COUNT(*) FROM users WHERE tenant_id = $1 AND external_id = $2",
            )
            .bind(tenant_id)
            .bind(external_id)
            .fetch_one(&state.pool)
            .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, email, given_name, family_name, display_name,
                       active, created_at, updated_at, version
                FROM users
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
    Ok((
        total,
        rows.into_iter()
            .map(|row| scim_user_from_row(state, row))
            .collect(),
    ))
}

async fn fetch_user_by_id(
    state: &ScimState,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<Option<ScimUser>, ScimResponseError> {
    let row: Option<UserRow> = sqlx::query_as(
        r#"
        SELECT id, external_id, email, given_name, family_name, display_name,
               active, created_at, updated_at, version
        FROM users
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(user_id)
    .bind(tenant_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(map_db)?;
    Ok(row.map(|row| scim_user_from_row(state, row)))
}

async fn ensure_user_exists(
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
        Err(ScimResponseError::not_found("user not found"))
    }
}

async fn apply_user_mutation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    mutation: UserMutation,
) -> Result<(), ScimResponseError> {
    if mutation.email.is_none()
        && mutation.display_name.is_none()
        && mutation.given_name.is_none()
        && mutation.family_name.is_none()
    {
        return Ok(());
    }
    sqlx::query(
        r#"
        UPDATE users
        SET email = COALESCE($3, email),
            display_name = COALESCE($4, display_name),
            given_name = COALESCE($5, given_name),
            family_name = COALESCE($6, family_name),
            updated_at = NOW(),
            version = version + 1
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(mutation.email)
    .bind(mutation.display_name)
    .bind(mutation.given_name)
    .bind(mutation.family_name)
    .execute(&mut **tx)
    .await
    .map_err(map_db)?;
    Ok(())
}

async fn enqueue_tenant_member(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
    op: TupleOp,
) -> Result<(), ScimResponseError> {
    enqueue_raw(
        &mut **tx,
        op,
        &format!("user:{user_id}"),
        "member",
        &format!("tenant:{tenant_id}"),
        Some(tenant_id),
    )
    .await
    .map_err(map_outbox)
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

fn map_cascade(error: CascadeError) -> ScimResponseError {
    tracing::error!(error = %error, "SCIM deactivation cascade failed");
    ScimResponseError::internal("deactivation cascade failed")
}

fn map_audit(error: moa_ocsf::EmitError) -> ScimResponseError {
    tracing::error!(error = %error, "SCIM security audit failed");
    ScimResponseError::internal("security audit failed")
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
