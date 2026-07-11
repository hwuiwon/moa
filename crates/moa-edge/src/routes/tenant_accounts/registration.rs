//! Public tenant registration route.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::tenant_accounts::{TenantSignupRequest, application};

use super::super::auth_accounts::{
    issue_login_session, normalize_email, validate_password_policy, validate_settings,
};
use super::super::{AppState, attach_set_cookie, parse_json_body};
use super::{application_error_response, looks_like_email};

/// Create a new tenant and its first tenant admin.
#[tracing::instrument(skip(state, body))]
// SAFETY: Public signup creates a new tenant boundary and grants only tenant-admin access for that new tenant.
pub(crate) async fn signup(State(state): State<AppState>, body: Bytes) -> Response {
    let request: TenantSignupRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let slug = match normalize_slug(&request.slug) {
        Ok(slug) => slug,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    let name = request.name.trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "tenant name is required").into_response();
    }
    if let Err(response) = validate_password_policy(&request.admin_password) {
        return response;
    }
    if let Err(response) = validate_settings(request.settings.as_ref()) {
        return response;
    }
    let email = normalize_email(&request.admin_email);
    if !looks_like_email(&email) {
        return (
            StatusCode::BAD_REQUEST,
            "admin_email must be an email address",
        )
            .into_response();
    }
    let registration = match application::register_tenant(&state, request, slug, name, email).await
    {
        Ok(registration) => registration,
        Err(error) => return application_error_response(error),
    };
    match issue_login_session(&state, registration.credential, true).await {
        Ok(session) => {
            let response = (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "tenant": registration.tenant,
                    "session": session.body,
                })),
            )
                .into_response();
            attach_set_cookie(response, session.set_cookie)
        }
        Err(response) => response,
    }
}

fn normalize_slug(slug: &str) -> Result<String, &'static str> {
    let slug = slug.trim().to_ascii_lowercase();
    let valid_len = (3..=63).contains(&slug.len());
    let valid_chars = slug
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
    let valid_edges = !slug.starts_with('-') && !slug.ends_with('-');
    if valid_len && valid_chars && valid_edges {
        Ok(slug)
    } else {
        Err("slug must be 3-63 lowercase letters, digits, or hyphens")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_slug_accepts_dashboard_safe_slugs() {
        // Pins: tenant signup canonicalizes slugs before persistence.
        assert_eq!(
            normalize_slug("Acme-Team").expect("slug should normalize"),
            "acme-team"
        );
    }

    #[test]
    fn normalize_slug_rejects_path_like_values() {
        // Pins: tenant slugs cannot contain path separators or leading/trailing hyphens.
        for slug in ["../acme", "-acme", "acme-", "a"] {
            assert!(normalize_slug(slug).is_err(), "{slug} should be rejected");
        }
    }
}
