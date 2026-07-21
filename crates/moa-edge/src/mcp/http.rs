//! Streamable HTTP configuration and authenticated request boundary for MCP.

use std::str::FromStr;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::IdentityType;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::Server;
use crate::routes::{AppState, authenticate_edge_request, require_direct_authz};

/// Validated Host and Origin allowlists for the tenant-operations MCP endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpHttpConfig {
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
}

impl McpHttpConfig {
    /// Parse comma-delimited Host and Origin allowlists.
    pub fn parse(allowed_hosts: &str, allowed_origins: &str) -> Result<Self, McpHttpConfigError> {
        let allowed_hosts = parse_csv(allowed_hosts, "host")?;
        let allowed_origins = parse_csv(allowed_origins, "origin")?;
        for host in &allowed_hosts {
            validate_host(host)?;
        }
        for origin in &allowed_origins {
            validate_origin(origin)?;
        }
        Ok(Self {
            allowed_hosts,
            allowed_origins,
        })
    }

    /// Local development allowlists for the default edge port.
    pub fn local_default() -> Self {
        Self {
            allowed_hosts: vec![
                "localhost:10000".to_string(),
                "127.0.0.1:10000".to_string(),
                "[::1]:10000".to_string(),
            ],
            allowed_origins: vec![
                "http://localhost:10000".to_string(),
                "http://127.0.0.1:10000".to_string(),
                "http://[::1]:10000".to_string(),
            ],
        }
    }
}

impl Default for McpHttpConfig {
    fn default() -> Self {
        Self::local_default()
    }
}

/// Invalid MCP HTTP allowlist configuration.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum McpHttpConfigError {
    /// An allowlist is empty or contains an empty comma-delimited entry.
    #[error("MCP {kind} allowlist must contain only non-empty entries")]
    EmptyEntry {
        /// The rejected allowlist kind.
        kind: &'static str,
    },
    /// A Host entry is malformed or unsafe.
    #[error("invalid MCP allowed Host `{value}`")]
    InvalidHost {
        /// The rejected entry.
        value: String,
    },
    /// An Origin entry is malformed or unsafe.
    #[error("invalid MCP allowed Origin `{value}`")]
    InvalidOrigin {
        /// The rejected entry.
        value: String,
    },
}

/// Build the authenticated stateless Streamable HTTP MCP router.
pub(crate) fn router(
    state: AppState,
    config: McpHttpConfig,
    cancellation_token: CancellationToken,
) -> Router {
    // Build the tool router and contracts once; stateless mode invokes this
    // factory for every JSON-RPC request.
    let template = Server::new(state.clone());
    let service = StreamableHttpService::new(
        move || Ok(template.clone()),
        NeverSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(config.allowed_hosts)
            .with_allowed_origins(config.allowed_origins)
            .with_sse_keep_alive(None)
            .with_stateful_mode(false)
            .with_json_response(true)
            .with_cancellation_token(cancellation_token),
    );

    Router::new()
        .route_service("/mcp", service)
        .route_layer(middleware::from_fn_with_state(state, authenticate_mcp))
}

#[tracing::instrument(
    skip(state, request, next),
    fields(
        http.route = "/mcp",
        http.status_code = tracing::field::Empty,
        moa.edge.auth.provider = tracing::field::Empty,
        moa.edge.auth.result = tracing::field::Empty,
        moa.mcp.tenant_id = tracing::field::Empty,
        moa.mcp.principal_type = tracing::field::Empty,
        moa.mcp.principal_id = tracing::field::Empty,
    )
)]
async fn authenticate_mcp(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let span = tracing::Span::current();
    crate::routes::adopt_client_trace_parent(&span, request.headers());
    let principal = match authenticate_edge_request(&state, request.headers(), &span).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if let Some(oauth) = principal.oauth.as_ref() {
        if oauth.resource != state.oauth_server.resource() {
            span.record("http.status_code", 403_i64);
            return (StatusCode::FORBIDDEN, "OAuth resource mismatch").into_response();
        }
        let (parts, body) = request.into_parts();
        let body = match to_bytes(body, 1024 * 1024).await {
            Ok(body) => body,
            Err(_) => {
                span.record("http.status_code", 400_i64);
                return (StatusCode::BAD_REQUEST, "invalid MCP request body").into_response();
            }
        };
        let required_scope = match super::required_oauth_scope(&parts.method, &body) {
            Ok(scope) => scope,
            Err(()) => {
                span.record("http.status_code", 403_i64);
                return (StatusCode::FORBIDDEN, "OAuth scope cannot be derived").into_response();
            }
        };
        if !principal.has_oauth_scope(required_scope) {
            span.record("http.status_code", 403_i64);
            return (StatusCode::FORBIDDEN, "insufficient OAuth scope").into_response();
        }
        request = Request::from_parts(parts, Body::from(body));
    }
    let identity = principal.identity.clone();
    if matches!(
        identity.identity_type,
        IdentityType::Contact | IdentityType::Agent
    ) {
        span.record("http.status_code", 403_i64);
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }

    span.record(
        "moa.mcp.tenant_id",
        tracing::field::display(identity.tenant_id),
    );
    span.record(
        "moa.mcp.principal_type",
        tracing::field::debug(identity.identity_type),
    );
    span.record("moa.mcp.principal_id", tracing::field::display(identity.id));
    request.extensions_mut().insert(principal);
    request.extensions_mut().insert(identity);
    let response = next.run(request).await;
    span.record("http.status_code", response.status().as_u16() as i64);
    response
}

fn parse_csv(value: &str, kind: &'static str) -> Result<Vec<String>, McpHttpConfigError> {
    if value.trim().is_empty() {
        return Err(McpHttpConfigError::EmptyEntry { kind });
    }
    let entries = value
        .split(',')
        .map(str::trim)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if entries.iter().any(String::is_empty) {
        return Err(McpHttpConfigError::EmptyEntry { kind });
    }
    Ok(entries)
}

fn validate_host(value: &str) -> Result<(), McpHttpConfigError> {
    if value.contains('*') || axum::http::uri::Authority::from_str(value).is_err() {
        return Err(McpHttpConfigError::InvalidHost {
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_origin(value: &str) -> Result<(), McpHttpConfigError> {
    let parsed = Url::parse(value).map_err(|_| McpHttpConfigError::InvalidOrigin {
        value: value.to_string(),
    })?;
    let valid_scheme = matches!(parsed.scheme(), "http" | "https");
    let root_path = parsed.path() == "/";
    let exact_origin = parsed.origin().ascii_serialization() == value.trim_end_matches('/');
    if value.contains('*')
        || !valid_scheme
        || !root_path
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !exact_origin
    {
        return Err(McpHttpConfigError::InvalidOrigin {
            value: value.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{McpHttpConfig, McpHttpConfigError};

    #[test]
    fn mcp_allowlists_reject_empty_wildcard_and_non_origin_entries_offline() {
        // Pins: MCP startup fails closed instead of disabling Host or Origin validation.
        assert_eq!(
            McpHttpConfig::parse("", "https://dashboard.example.com"),
            Err(McpHttpConfigError::EmptyEntry { kind: "host" })
        );
        assert!(matches!(
            McpHttpConfig::parse("*.example.com", "https://dashboard.example.com"),
            Err(McpHttpConfigError::InvalidHost { .. })
        ));
        assert!(matches!(
            McpHttpConfig::parse("api.example.com", "https://dashboard.example.com/callback"),
            Err(McpHttpConfigError::InvalidOrigin { .. })
        ));
    }

    #[test]
    fn mcp_allowlists_accept_exact_hosts_and_origins_offline() {
        // Pins: production may configure several exact protected-resource hosts and dashboards.
        let config = McpHttpConfig::parse(
            "mcp.example.com,mcp.internal.example.com:8443",
            "https://dashboard.example.com,https://admin.example.com:8443",
        )
        .expect("exact Host and Origin allowlists should validate");
        assert_eq!(config.allowed_hosts.len(), 2);
        assert_eq!(config.allowed_origins.len(), 2);
    }
}
