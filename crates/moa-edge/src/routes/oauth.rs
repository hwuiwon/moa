//! OAuth authorization, consent, token, revocation, and metadata endpoints.

use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use base64::Engine;
use moa_auth_providers::oauth_as::{
    AuthorizationDecision, AuthorizationRequest, AuthorizationSubject, CodeExchangeRequest,
    OAuthError,
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;
use url::Url;
use uuid::Uuid;

use super::{AppState, authenticate_direct_request};

/// GET authorization parameters.
#[derive(Debug, Deserialize)]
pub(super) struct AuthorizeQuery {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    scope: Option<String>,
    state: Option<String>,
    #[serde(default)]
    resource: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
}

/// POST consent form fields rendered by GET authorization.
#[derive(Debug, Deserialize)]
pub(super) struct ConsentForm {
    request_id: Uuid,
    csrf_token: String,
    decision: String,
}

/// Token endpoint form.
#[derive(Debug, Deserialize)]
pub(super) struct TokenForm {
    #[serde(default)]
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    resource: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

/// Introspection and revocation form.
#[derive(Debug, Deserialize)]
pub(super) struct TokenIntrospectForm {
    token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

#[tracing::instrument(skip(state, headers, query))]
// SAFETY: validates the authenticated owner and persists only a pending consent
// transaction; GET never issues a code or token.
pub(super) async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/oauth/authorize").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let request = AuthorizationRequest {
        response_type: &query.response_type,
        client_id: &query.client_id,
        redirect_uri: &query.redirect_uri,
        scopes: split_scopes(query.scope.as_deref()),
        resource: &query.resource,
        state: query.state.as_deref(),
        code_challenge: &query.code_challenge,
        code_challenge_method: &query.code_challenge_method,
    };
    let subject = authorization_subject(&identity);
    match state
        .oauth_server
        .begin_authorization(&request, &subject)
        .await
    {
        Ok(pending) => consent_page(&pending),
        Err(error) if error.must_not_redirect() => direct_error(&error),
        Err(error) => redirect_error(
            &query.redirect_uri,
            error.error_code(),
            query.state.as_deref(),
        ),
    }
}

#[tracing::instrument(skip(state, headers, form))]
// SAFETY: the durable request is bound to the authenticated owner and a hashed
// CSRF value; Postgres accepts exactly one approve or deny decision.
pub(super) async fn authorize_decision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ConsentForm>,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/oauth/authorize").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let decision = match form.decision.as_str() {
        "approve" => AuthorizationDecision::Approve,
        "deny" => AuthorizationDecision::Deny,
        _ => {
            return token_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid decision",
            );
        }
    };
    let subject = authorization_subject(&identity);
    match state
        .oauth_server
        .complete_authorization(
            form.request_id,
            &SecretString::from(form.csrf_token),
            &subject,
            decision,
        )
        .await
    {
        Ok(outcome) => match outcome.code {
            Some(code) => redirect_success(
                &outcome.redirect_uri,
                code.code.expose_secret(),
                outcome.state.as_deref(),
            ),
            None => redirect_error(
                &outcome.redirect_uri,
                "access_denied",
                outcome.state.as_deref(),
            ),
        },
        Err(error) => token_error(
            StatusCode::BAD_REQUEST,
            error.error_code(),
            error_description(&error),
        ),
    }
}

#[tracing::instrument(skip(state, headers, form))]
// SAFETY: pre-auth token endpoint; client auth and every grant binding are
// verified before the code and token insert commit together.
pub(super) async fn token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    let Some((client_id, client_secret)) = client_credentials(
        &headers,
        form.client_id.as_deref(),
        form.client_secret.as_deref(),
    ) else {
        return invalid_client("client authentication required");
    };
    let client = match state.oauth_server.client(&client_id).await {
        Ok(Some(client)) => client,
        Ok(None) => return invalid_client("unknown client"),
        Err(error) => return server_error("client lookup", &error),
    };

    match form.grant_type.as_str() {
        "authorization_code" => {
            let (Some(code), Some(redirect_uri), Some(resource), Some(verifier)) = (
                form.code.as_deref(),
                form.redirect_uri.as_deref(),
                form.resource.as_deref(),
                form.code_verifier.as_deref(),
            ) else {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "code, redirect_uri, resource, and code_verifier are required",
                );
            };
            let code = SecretString::from(code);
            let request = CodeExchangeRequest {
                code: &code,
                redirect_uri,
                resource,
                code_verifier: verifier,
            };
            match state
                .oauth_server
                .exchange_authorization_code(&client, client_secret.as_ref(), &request)
                .await
            {
                Ok(grant) => token_success(&grant),
                Err(error) => token_error_from(&client_id, "authorization_code", &error),
            }
        }
        "refresh_token" => {
            let Some(refresh_token) = form.refresh_token.as_deref() else {
                return token_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "refresh_token is required",
                );
            };
            match state
                .oauth_server
                .refresh_token_grant(
                    &client,
                    client_secret.as_ref(),
                    &SecretString::from(refresh_token),
                )
                .await
            {
                Ok(grant) => token_success(&grant),
                Err(error) => token_error_from(&client_id, "refresh_token", &error),
            }
        }
        _ => token_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "unsupported grant_type",
        ),
    }
}

#[tracing::instrument(skip(state, headers, form))]
// SAFETY: pre-auth introspection endpoint; only the authenticated issuing
// confidential client can observe an active response.
pub(super) async fn introspect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TokenIntrospectForm>,
) -> Response {
    let Some((client_id, client_secret)) = client_credentials(
        &headers,
        form.client_id.as_deref(),
        form.client_secret.as_deref(),
    ) else {
        return introspect_unauthorized();
    };
    let client = match state.oauth_server.client(&client_id).await {
        Ok(Some(client)) => client,
        _ => return introspect_unauthorized(),
    };
    let Some(token) = form.token else {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "token is required",
        );
    };
    match state
        .oauth_server
        .introspect(&client, client_secret.as_ref(), &SecretString::from(token))
        .await
    {
        Ok(response) => json_no_store(StatusCode::OK, json!(response)),
        Err(OAuthError::InvalidClientCredentials) => introspect_unauthorized(),
        Err(error) => server_error("introspection", &error),
    }
}

#[tracing::instrument(skip(state, headers, form))]
// SAFETY: pre-auth revocation endpoint; the authenticated client can revoke
// only its own grant and unknown tokens are an idempotent success.
pub(super) async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TokenIntrospectForm>,
) -> Response {
    let Some((client_id, client_secret)) = client_credentials(
        &headers,
        form.client_id.as_deref(),
        form.client_secret.as_deref(),
    ) else {
        return invalid_client("client authentication required");
    };
    let client = match state.oauth_server.client(&client_id).await {
        Ok(Some(client)) => client,
        Ok(None) => return invalid_client("unknown client"),
        Err(error) => return server_error("client lookup", &error),
    };
    let Some(token) = form.token else {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "token is required",
        );
    };
    match state
        .oauth_server
        .revoke(&client, client_secret.as_ref(), &SecretString::from(token))
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(OAuthError::InvalidClientCredentials) => invalid_client("client authentication failed"),
        Err(error) => server_error("revocation", &error),
    }
}

// SAFETY: public RFC 8414 metadata contains deployment URLs only.
pub(super) async fn authorization_server_metadata(State(state): State<AppState>) -> Response {
    let issuer = state.oauth_server.issuer();
    json_no_store(
        StatusCode::OK,
        json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/oauth/authorize"),
            "token_endpoint": format!("{issuer}/oauth/token"),
            "introspection_endpoint": format!("{issuer}/oauth/introspect"),
            "revocation_endpoint": format!("{issuer}/oauth/revoke"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["mcp:read", "mcp:write"],
        }),
    )
}

// SAFETY: public RFC 9728 metadata contains deployment URLs only.
pub(super) async fn protected_resource_metadata(State(state): State<AppState>) -> Response {
    json_no_store(
        StatusCode::OK,
        json!({
            "resource": state.oauth_server.resource(),
            "authorization_servers": [state.oauth_server.issuer()],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["mcp:read", "mcp:write"],
        }),
    )
}

fn authorization_subject(identity: &moa_core::traits::Identity) -> AuthorizationSubject {
    AuthorizationSubject {
        subject_id: identity.id,
        subject_type: identity.identity_type.as_str().to_string(),
        tenant_id: identity.tenant_id,
    }
}

fn consent_page(pending: &moa_auth_providers::oauth_as::PendingAuthorization) -> Response {
    let scopes = escape_html(&pending.scopes.join(" "));
    let html = format!(
        "<!doctype html><html><body><main><h1>Authorize {}</h1><p>Resource: {}</p><p>Scopes: {scopes}</p><form method=\"post\" action=\"/oauth/authorize\"><input type=\"hidden\" name=\"request_id\" value=\"{}\"><input type=\"hidden\" name=\"csrf_token\" value=\"{}\"><button name=\"decision\" value=\"approve\" type=\"submit\">Approve</button><button name=\"decision\" value=\"deny\" type=\"submit\">Deny</button></form></main></body></html>",
        escape_html(&pending.client_id),
        escape_html(&pending.resource),
        pending.request_id,
        escape_html(pending.csrf_token.expose_secret()),
    );
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; form-action 'self'",
            ),
        ],
        Html(html),
    )
        .into_response()
}

fn client_credentials(
    headers: &HeaderMap,
    form_client_id: Option<&str>,
    form_client_secret: Option<&str>,
) -> Option<(String, Option<SecretString>)> {
    if let Some((client_id, secret)) = basic_auth_credentials(headers) {
        return Some((client_id, Some(secret)));
    }
    let client_id = form_client_id?.trim();
    if client_id.is_empty() {
        return None;
    }
    Some((
        client_id.to_string(),
        form_client_secret.map(SecretString::from),
    ))
}

fn basic_auth_credentials(headers: &HeaderMap) -> Option<(String, SecretString)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (client_id, secret) = decoded.split_once(':')?;
    (!client_id.is_empty()).then(|| (client_id.to_string(), SecretString::from(secret)))
}

fn split_scopes(scope: Option<&str>) -> Vec<String> {
    scope
        .map(|raw| raw.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

fn redirect_success(redirect_uri: &str, code: &str, state: Option<&str>) -> Response {
    let mut pairs = vec![("code", code.to_string())];
    if let Some(state) = state {
        pairs.push(("state", state.to_string()));
    }
    redirect_with_params(redirect_uri, &pairs)
}

fn redirect_error(redirect_uri: &str, error: &str, state: Option<&str>) -> Response {
    let mut pairs = vec![("error", error.to_string())];
    if let Some(state) = state {
        pairs.push(("state", state.to_string()));
    }
    redirect_with_params(redirect_uri, &pairs)
}

fn redirect_with_params(redirect_uri: &str, pairs: &[(&str, String)]) -> Response {
    let Ok(mut url) = Url::parse(redirect_uri) else {
        return (StatusCode::BAD_REQUEST, "invalid redirect uri").into_response();
    };
    for (key, value) in pairs {
        url.query_pairs_mut().append_pair(key, value);
    }
    match header::HeaderValue::from_str(url.as_str()) {
        Ok(location) => (StatusCode::FOUND, [(header::LOCATION, location)]).into_response(),
        Err(_) => (StatusCode::BAD_REQUEST, "invalid redirect uri").into_response(),
    }
}

fn direct_error(error: &OAuthError) -> Response {
    token_error(
        StatusCode::BAD_REQUEST,
        error.error_code(),
        error_description(error),
    )
}

fn token_success(grant: &moa_auth_providers::oauth_as::TokenGrant) -> Response {
    json_no_store(
        StatusCode::OK,
        json!({
            "access_token": grant.access_token.expose_secret(),
            "token_type": grant.token_type,
            "expires_in": grant.expires_in,
            "refresh_token": grant.refresh_token.expose_secret(),
            "scope": grant.scopes.join(" "),
            "resource": grant.resource,
        }),
    )
}

fn token_error_from(client_id: &str, grant_type: &str, error: &OAuthError) -> Response {
    tracing::info!(
        client_id,
        grant_type,
        error_code = error.error_code(),
        "oauth grant rejected"
    );
    let status = match error {
        OAuthError::InvalidClient | OAuthError::InvalidClientCredentials => {
            StatusCode::UNAUTHORIZED
        }
        OAuthError::Storage(_)
        | OAuthError::ClientBootstrapConflict(_)
        | OAuthError::InvalidClientConfiguration(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    token_error(status, error.error_code(), error_description(error))
}

fn invalid_client(description: &str) -> Response {
    token_error(StatusCode::UNAUTHORIZED, "invalid_client", description)
}

fn introspect_unauthorized() -> Response {
    invalid_client("confidential client authentication required")
}

fn server_error(operation: &str, error: &OAuthError) -> Response {
    tracing::error!(operation, error = %error, "oauth storage operation failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "OAuth server unavailable",
    )
        .into_response()
}

fn token_error(status: StatusCode, error: &str, description: &str) -> Response {
    json_no_store(
        status,
        json!({ "error": error, "error_description": description }),
    )
}

fn error_description(error: &OAuthError) -> &'static str {
    match error {
        OAuthError::InvalidClient => "unknown client",
        OAuthError::InvalidClientCredentials => "client authentication failed",
        OAuthError::InvalidRedirectUri => "redirect_uri mismatch",
        OAuthError::InvalidScope => "requested scope is not permitted",
        OAuthError::InvalidRequest(message) => message,
        OAuthError::UnsupportedResponseType => "unsupported response_type",
        OAuthError::UnsupportedGrantType => "unsupported grant_type",
        OAuthError::InvalidGrant => "invalid or expired grant",
        OAuthError::AuthorizationAlreadyDecided => "authorization decision already recorded",
        OAuthError::ClientBootstrapConflict(_)
        | OAuthError::InvalidClientConfiguration(_)
        | OAuthError::Storage(_) => "internal error",
    }
}

fn json_no_store(status: StatusCode, body: serde_json::Value) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        axum::Json(body),
    )
        .into_response()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_credentials_do_not_override_basic_auth() {
        // Pins: HTTP Basic credentials take precedence over form credentials.
        let encoded = base64::engine::general_purpose::STANDARD.encode("header:secret");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {encoded}").parse().expect("valid header"),
        );
        let (client_id, secret) =
            client_credentials(&headers, Some("form"), Some("wrong")).expect("credentials resolve");
        assert_eq!(client_id, "header");
        assert_eq!(secret.expect("secret").expose_secret(), "secret");
    }

    #[test]
    fn consent_html_escapes_database_values() {
        // Pins: bootstrapped client metadata cannot inject consent-page markup.
        assert_eq!(
            escape_html("<client & 'owner'>"),
            "&lt;client &amp; &#39;owner&#39;&gt;"
        );
    }
}
