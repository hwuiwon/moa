//! Streamable HTTP configuration and authenticated request boundary for MCP.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::header::WWW_AUTHENTICATE;
use axum::http::{HeaderValue, StatusCode};
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
    tool_calls_per_minute: u32,
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
            tool_calls_per_minute: 60,
        })
    }

    /// Set the maximum tool calls admitted per authenticated principal each minute.
    pub fn with_tool_calls_per_minute(mut self, limit: u32) -> Result<Self, McpHttpConfigError> {
        if limit == 0 {
            return Err(McpHttpConfigError::ZeroToolRateLimit);
        }
        self.tool_calls_per_minute = limit;
        Ok(self)
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
            tool_calls_per_minute: 60,
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
    /// A zero tool-call limit would make the configured MCP server unusable.
    #[error("MCP tool calls per minute must be greater than zero")]
    ZeroToolRateLimit,
}

#[derive(Clone)]
struct McpAuthState {
    app: AppState,
    tool_limiter: Arc<ToolCallLimiter>,
}

struct ToolCallLimiter {
    limit: u32,
    windows: tokio::sync::Mutex<HashMap<(uuid::Uuid, uuid::Uuid), RateWindow>>,
}

struct RateWindow {
    started_at: Instant,
    calls: u32,
}

impl ToolCallLimiter {
    fn new(limit: u32) -> Self {
        Self {
            limit,
            windows: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn admit(&self, tenant_id: uuid::Uuid, principal_id: uuid::Uuid) -> Result<(), u64> {
        const WINDOW: Duration = Duration::from_secs(60);

        let now = Instant::now();
        let mut windows = self.windows.lock().await;
        windows.retain(|_, window| now.duration_since(window.started_at) < WINDOW);
        let window = windows
            .entry((tenant_id, principal_id))
            .or_insert(RateWindow {
                started_at: now,
                calls: 0,
            });
        if window.calls >= self.limit {
            let remaining = WINDOW.saturating_sub(now.duration_since(window.started_at));
            return Err(remaining.as_secs().max(1));
        }
        window.calls += 1;
        Ok(())
    }
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
    let tool_calls_per_minute = config.tool_calls_per_minute;
    let service = StreamableHttpService::new(
        move || Ok(template.clone()),
        NeverSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(config.allowed_hosts)
            .with_allowed_origins(config.allowed_origins)
            .with_sse_keep_alive(None)
            .with_legacy_session_mode(false)
            .with_stateless_protocol_metadata_required(true)
            .with_json_response(true)
            .with_cancellation_token(cancellation_token),
    );
    let auth_state = McpAuthState {
        app: state,
        tool_limiter: Arc::new(ToolCallLimiter::new(tool_calls_per_minute)),
    };

    Router::new()
        .route_service("/mcp", service)
        .route_layer(middleware::from_fn_with_state(auth_state, authenticate_mcp))
}

#[tracing::instrument(
    skip(mcp_state, request, next),
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
    State(mcp_state): State<McpAuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let state = &mcp_state.app;
    let span = tracing::Span::current();
    crate::routes::adopt_client_trace_parent(&span, request.headers());
    let principal = match authenticate_edge_request(state, request.headers(), &span).await {
        Ok(principal) => principal,
        Err(mut response) => {
            if response.status() == StatusCode::UNAUTHORIZED
                && let Some(challenge) = authorization_challenge(state, None)
            {
                response.headers_mut().insert(WWW_AUTHENTICATE, challenge);
            }
            return response;
        }
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
            let mut response = (StatusCode::FORBIDDEN, "insufficient OAuth scope").into_response();
            if let Some(challenge) = authorization_challenge(state, Some(required_scope)) {
                response.headers_mut().insert(WWW_AUTHENTICATE, challenge);
            }
            return response;
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
    if is_tool_call(request.headers())
        && let Err(retry_after) = mcp_state
            .tool_limiter
            .admit(identity.tenant_id.0, identity.id)
            .await
    {
        span.record("http.status_code", 429_i64);
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            "MCP tool-call rate limit exceeded",
        )
            .into_response();
        if let Ok(retry_after) = HeaderValue::from_str(&retry_after.to_string()) {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, retry_after);
        }
        return response;
    }
    if let Err(response) = require_direct_authz(
        state,
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

fn is_tool_call(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("Mcp-Method")
        .and_then(|value| value.to_str().ok())
        == Some("tools/call")
}

fn authorization_challenge(state: &AppState, scope: Option<&str>) -> Option<HeaderValue> {
    let metadata = protected_resource_metadata_url(state.oauth_server.resource())?;
    let challenge = match scope {
        Some(scope) => format!(
            "Bearer error=\"insufficient_scope\", scope=\"{scope}\", resource_metadata=\"{metadata}\""
        ),
        None => format!("Bearer resource_metadata=\"{metadata}\", scope=\"mcp:read mcp:write\""),
    };
    HeaderValue::from_str(&challenge).ok()
}

fn protected_resource_metadata_url(resource: &str) -> Option<Url> {
    let resource = Url::parse(resource).ok()?;
    let resource_path = resource.path().trim_end_matches('/');
    let metadata_path = if resource_path.is_empty() {
        "/.well-known/oauth-protected-resource".to_string()
    } else {
        format!("/.well-known/oauth-protected-resource{resource_path}")
    };
    let mut metadata = resource;
    metadata.set_path(&metadata_path);
    metadata.set_query(None);
    metadata.set_fragment(None);
    Some(metadata)
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
    use uuid::Uuid;

    use super::{
        McpHttpConfig, McpHttpConfigError, ToolCallLimiter, protected_resource_metadata_url,
    };

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

    #[test]
    fn mcp_http_config_rejects_a_disabled_tool_rate_limit_offline() {
        // Pins: the MCP tool-call limiter cannot be accidentally configured away.
        let config = McpHttpConfig::default();
        assert_eq!(
            config.with_tool_calls_per_minute(0),
            Err(McpHttpConfigError::ZeroToolRateLimit)
        );
    }

    #[test]
    fn protected_resource_metadata_url_preserves_the_mcp_resource_path_offline() {
        // Pins: the bearer challenge points at the RFC 9728 path-specific metadata document.
        assert_eq!(
            protected_resource_metadata_url("https://mcp.example.com/mcp")
                .map(|url| url.to_string()),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource/mcp".to_string())
        );
        assert_eq!(
            protected_resource_metadata_url("https://mcp.example.com").map(|url| url.to_string()),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource".to_string())
        );
    }

    #[tokio::test]
    async fn mcp_tool_call_limiter_is_scoped_per_principal_offline() {
        // Pins: one principal is throttled at the configured bound without consuming another's budget.
        let limiter = ToolCallLimiter::new(2);
        let tenant = Uuid::now_v7();
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();

        assert_eq!(limiter.admit(tenant, first).await, Ok(()));
        assert_eq!(limiter.admit(tenant, first).await, Ok(()));
        assert!(limiter.admit(tenant, first).await.is_err());
        assert_eq!(limiter.admit(tenant, second).await, Ok(()));
    }

    #[test]
    fn unsupported_mcp_method_is_not_an_oauth_scope_failure_offline() {
        // Pins: a valid OAuth principal reaches rmcp's 404/-32601 response for
        // an unsupported method instead of being rejected as a plain 403.
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "unsupported/method",
            "params": {}
        }))
        .expect("serialize unsupported request");

        assert!(
            crate::mcp::required_oauth_scope(&axum::http::Method::POST, &body).is_ok(),
            "unsupported method was misclassified as an OAuth scope failure"
        );
    }
}
