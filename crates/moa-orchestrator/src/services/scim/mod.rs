//! SCIM v2 HTTP endpoints for enterprise provisioning.

pub mod auth;
pub mod deactivation;
pub mod groups;
pub mod meta;
pub mod patch;
pub mod schema;
pub mod users;

use std::sync::Arc;

use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use moa_authz::FgaClient;
use moa_core::traits::{AuthError, AuthProvider, Credential, Identity};
use schema::{SCHEMA_ERROR, ScimError};
use sqlx::PgPool;

/// Shared state for SCIM HTTP handlers.
#[derive(Clone)]
pub struct ScimState {
    /// Postgres pool.
    pub pool: PgPool,
    /// API-key auth provider used for SCIM clients.
    pub auth: Arc<dyn AuthProvider>,
    /// Optional OpenFGA client. SCIM is unavailable when authorization is disabled.
    pub fga_client: Option<FgaClient>,
    /// Public base URL for SCIM resource locations.
    pub base_url: String,
}

impl ScimState {
    /// Build SCIM handler state.
    #[must_use]
    pub fn new(
        pool: PgPool,
        auth: Arc<dyn AuthProvider>,
        fga_client: Option<FgaClient>,
        base_url: String,
    ) -> Self {
        Self {
            pool,
            auth,
            fga_client,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

/// Build the `/scim/v2` router.
pub fn router(state: ScimState) -> Router {
    Router::new()
        .route("/ServiceProviderConfig", get(meta::service_provider_config))
        .route("/ResourceTypes", get(meta::resource_types))
        .route("/Schemas", get(meta::schemas))
        .route("/Users", get(users::list_users).post(users::create_user))
        .route(
            "/Users/{id}",
            get(users::get_user)
                .put(users::put_user)
                .patch(users::patch_user)
                .delete(users::delete_user),
        )
        .route(
            "/Groups",
            get(groups::list_groups).post(groups::create_group),
        )
        .route(
            "/Groups/{id}",
            get(groups::get_group)
                .put(groups::put_group)
                .patch(groups::patch_group)
                .delete(groups::delete_group),
        )
        .with_state(state)
}

/// Error response returned by SCIM handlers.
#[derive(Debug)]
pub struct ScimResponseError {
    status: StatusCode,
    scim_type: Option<String>,
    detail: String,
}

impl ScimResponseError {
    /// Build a bad request SCIM error.
    pub fn bad_request(scim_type: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            scim_type: Some(scim_type.into()),
            detail: detail.into(),
        }
    }

    /// Build a conflict SCIM error.
    pub fn conflict(scim_type: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            scim_type: Some(scim_type.into()),
            detail: detail.into(),
        }
    }

    /// Build a not found SCIM error.
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            scim_type: None,
            detail: detail.into(),
        }
    }

    /// Build an unauthorized SCIM error.
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            scim_type: None,
            detail: detail.into(),
        }
    }

    /// Build a forbidden SCIM error.
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            scim_type: None,
            detail: detail.into(),
        }
    }

    /// Build a service unavailable SCIM error.
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            scim_type: None,
            detail: detail.into(),
        }
    }

    /// Build an internal SCIM error.
    pub fn internal(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            scim_type: None,
            detail: detail.into(),
        }
    }
}

impl IntoResponse for ScimResponseError {
    fn into_response(self) -> Response {
        let body = ScimError {
            schemas: vec![SCHEMA_ERROR.to_string()],
            status: self.status.as_u16().to_string(),
            detail: self.detail,
            scim_type: self.scim_type,
        };
        (self.status, Json(body)).into_response()
    }
}

/// Authenticate the bearer API key and verify SCIM admin authorization.
pub async fn authenticate_scim(
    state: &ScimState,
    headers: &HeaderMap,
) -> Result<Identity, ScimResponseError> {
    let authorization = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ScimResponseError::unauthorized("missing bearer token"))?;
    let token = authorization
        .strip_prefix("Bearer ")
        .ok_or_else(|| ScimResponseError::unauthorized("expected bearer token"))?;
    let credential = Credential::ApiKey(token.to_string());
    let identity = state
        .auth
        .authenticate(&credential)
        .await
        .map_err(map_auth_error)?;
    if identity.api_key_id.is_none() {
        return Err(ScimResponseError::forbidden(
            "SCIM endpoints require an API key principal",
        ));
    }
    auth::require_scim_admin(state, &identity).await?;
    Ok(identity)
}

/// Map a database error into a SCIM response.
pub fn map_db(error: sqlx::Error) -> ScimResponseError {
    if let Some(db_error) = error.as_database_error()
        && db_error.is_unique_violation()
    {
        return ScimResponseError::conflict("uniqueness", db_error.message().to_string());
    }
    tracing::error!(error = %error, "SCIM database error");
    ScimResponseError::internal("database error")
}

fn map_auth_error(error: AuthError) -> ScimResponseError {
    match error {
        AuthError::InvalidFormat | AuthError::Rejected | AuthError::Expired => {
            ScimResponseError::unauthorized("invalid bearer token")
        }
        AuthError::NotConfigured => {
            ScimResponseError::unavailable("local API-key authentication is not configured")
        }
        AuthError::Unavailable(detail) => ScimResponseError::unavailable(detail),
        AuthError::Internal(detail) => ScimResponseError::internal(detail),
    }
}
