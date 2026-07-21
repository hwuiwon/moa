//! Curated analytics, dashboard session, lineage, and learning-summary MCP tools.

use moa_core::types::contact::ContactId;
use moa_core::types::identifiers::SessionId;
use moa_core::wire::analytics::AnalyticsQueryRequest;
use moa_core::wire::lineage::LineageExplainRequest;
use moa_session::store::{DashboardEventPageRequest, DashboardSessionListRequest};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars, tool, tool_router};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use uuid::Uuid;

use super::{Server, clamp_limit, request_identity_and_headers, result, tenant_request};
use crate::routes::dashboard::sessions::{decode_cursor, encode_cursor};
use crate::routes::{analytics, lineage};

/// Build the observation-domain tool router.
pub(super) fn router() -> rmcp::handler::server::router::tool::ToolRouter<Server> {
    Server::analytics_sessions_router()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AnalyticsDimensionInput {
    /// Dimension field ID from the selected dataset in `analytics_catalog`.
    field: String,
    /// Optional output-column alias used by `order_by` and returned column metadata.
    alias: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum AnalyticsAggregationInput {
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
    P50,
    P95,
    P99,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AnalyticsMeasureInput {
    /// Measure field ID from the selected dataset; omit only for `count`.
    field: Option<String>,
    /// Aggregation allowed by the selected field in `analytics_catalog`.
    aggregation: AnalyticsAggregationInput,
    /// Optional output-column alias used by `order_by` and returned column metadata.
    alias: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum AnalyticsFilterOperatorInput {
    Eq,
    NotEq,
    In,
    NotIn,
    Lt,
    Lte,
    Gt,
    Gte,
    Contains,
    StartsWith,
    EndsWith,
    IsNull,
    IsNotNull,
    Between,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AnalyticsFilterInput {
    /// Filterable field ID from the selected dataset in `analytics_catalog`.
    field: String,
    /// Operator allowed by the selected field in `analytics_catalog`.
    operator: AnalyticsFilterOperatorInput,
    /// JSON value for the operator; omit for `is_null` and `is_not_null`, use an array for `in`, `not_in`, and `between`.
    value: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum AnalyticsSortDirectionInput {
    Asc,
    Desc,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AnalyticsOrderInput {
    /// Field ID or dimension/measure alias to sort by.
    field: String,
    /// Sort direction.
    direction: AnalyticsSortDirectionInput,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AnalyticsQueryInput {
    /// Dataset identifier returned by `analytics_catalog`; SQL is never accepted.
    dataset: String,
    /// Grouping or table fields to return; each field must be a catalog dimension.
    #[serde(default)]
    dimensions: Vec<AnalyticsDimensionInput>,
    /// Aggregate values to calculate; aggregations must be allowed by the catalog field.
    #[serde(default)]
    measures: Vec<AnalyticsMeasureInput>,
    /// Predicates applied before grouping; values must match the catalog field kind.
    #[serde(default)]
    filters: Vec<AnalyticsFilterInput>,
    /// Result ordering using field IDs or aliases selected above.
    #[serde(default)]
    order_by: Vec<AnalyticsOrderInput>,
    /// Maximum rows to return; defaults in the service and is bounded to 1–1000.
    #[schemars(range(min = 1, max = 1000))]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionsListInput {
    /// Maximum summaries to return; defaults in the store and is bounded to 1–200.
    #[schemars(range(min = 1, max = 200))]
    limit: Option<usize>,
    /// Opaque cursor returned by a previous call.
    cursor: Option<String>,
    /// Optional exact session status: `created`, `running`, `paused`, `completed`, `failed`, or `cancelled`.
    status: Option<String>,
    /// Optional exact channel: `chat`, `slack`, `email`, or `sms`.
    channel: Option<String>,
    /// Optional contact UUID whose sessions should be returned.
    contact_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionIdInput {
    /// Tenant-owned session UUID returned by `sessions_list`.
    session_id: Uuid,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionEventsInput {
    /// Tenant-owned session UUID returned by `sessions_list`.
    session_id: Uuid,
    /// Maximum redacted events to return; defaults in the store and is bounded to 1–200.
    #[schemars(range(min = 1, max = 200))]
    limit: Option<usize>,
    /// Opaque cursor returned by a previous call.
    cursor: Option<String>,
    /// Optional exact event-type filters; use event type strings observed in prior session results.
    #[serde(default)]
    event_types: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LineageExplainInput {
    /// Session or turn UUID whose lineage should be explained.
    id: Uuid,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LearningCandidatesInput {
    /// Optional exact candidate status: `proposed`, `evaluating`, `promoted`, `rejected`, or `rolled_back`.
    status: Option<String>,
    /// Maximum candidate summaries to return; defaults to 50 and is bounded to 1–200.
    #[schemars(range(min = 1, max = 200))]
    limit: Option<u32>,
}

#[tool_router(router = analytics_sessions_router)]
impl Server {
    /// Return the curated analytics datasets, dimensions, measures, and operators.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn analytics_catalog(&self) -> CallToolResult {
        result::success("Loaded analytics catalog.", &analytics::catalog())
    }

    /// Run a bounded curated analytics query for the authenticated tenant; raw SQL is unsupported.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    // SAFETY: tenant Operator authz is enforced for every request by the
    // authenticate_mcp middleware before any tool dispatch.
    async fn analytics_query(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(mut input): Parameters<AnalyticsQueryInput>,
    ) -> CallToolResult {
        let (identity, _) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        input.limit = clamp_limit(input.limit, 1000);
        let request: AnalyticsQueryRequest = match tenant_request(identity.tenant_id, &input) {
            Ok(request) => request,
            Err(result) => return result,
        };
        match analytics::query(&self.state, request).await {
            Ok(response) => result::success("Completed analytics query.", &response),
            Err(moa_analytics::Error::Execution(error))
            | Err(moa_analytics::Error::ClickHouse(error)) => read_error("analytics query", error),
            Err(error) => result::execution_error(error.to_string()),
        }
    }

    /// List bounded dashboard-safe session summaries using an opaque keyset cursor.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    // SAFETY: tenant Operator authz is enforced for every request by the
    // authenticate_mcp middleware before any tool dispatch.
    async fn sessions_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<SessionsListInput>,
    ) -> CallToolResult {
        let (identity, _) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let cursor = match decode_cursor(input.cursor.as_deref()) {
            Ok(cursor) => cursor,
            Err(_) => {
                return result::execution_error(
                    "malformed cursor; pass the exact opaque next_cursor from the previous page or omit it",
                );
            }
        };
        let status = match parse_optional(input.status, "session status") {
            Ok(status) => status,
            Err(result) => return result,
        };
        let channel = match parse_optional(input.channel, "channel") {
            Ok(channel) => channel,
            Err(result) => return result,
        };
        let request = DashboardSessionListRequest {
            limit: clamp_limit(input.limit, 200),
            cursor,
            status,
            channel,
            contact_id: input.contact_id.map(ContactId),
        };
        match self
            .state
            .session_store
            .list_dashboard_sessions(identity.tenant_id, request)
            .await
        {
            Ok(page) => match encode_cursor(page.next_cursor.as_ref()) {
                Ok(next_cursor) => result::success(
                    "Listed sessions.",
                    &serde_json::json!({ "sessions": page.sessions, "next_cursor": next_cursor }),
                ),
                Err(_) => result::execution_error("failed to encode cursor"),
            },
            Err(error) => read_error("session list", error),
        }
    }

    /// Load dashboard-safe details and aggregate usage for one tenant-owned session.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    // SAFETY: tenant Operator authz is enforced for every request by the
    // authenticate_mcp middleware before any tool dispatch.
    async fn session_get(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<SessionIdInput>,
    ) -> CallToolResult {
        let (identity, _) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        match self
            .state
            .session_store
            .get_dashboard_session_detail(identity.tenant_id, SessionId(input.session_id))
            .await
        {
            Ok(Some(detail)) => result::success("Loaded session details.", &detail),
            Ok(None) => result::execution_error("session not found"),
            Err(error) => read_error("session detail", error),
        }
    }

    /// List only redacted dashboard event summaries for a tenant-owned session.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    // SAFETY: tenant Operator authz is enforced for every request by the
    // authenticate_mcp middleware before any tool dispatch.
    async fn session_events_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<SessionEventsInput>,
    ) -> CallToolResult {
        let (identity, _) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let cursor = match decode_cursor(input.cursor.as_deref()) {
            Ok(cursor) => cursor,
            Err(_) => {
                return result::execution_error(
                    "malformed cursor; pass the exact opaque next_cursor from the previous page or omit it",
                );
            }
        };
        let event_types = if input.event_types.is_empty() {
            None
        } else {
            let mut parsed = Vec::with_capacity(input.event_types.len());
            for event_type in input.event_types {
                match parse_value(event_type, "event type") {
                    Ok(event_type) => parsed.push(event_type),
                    Err(result) => return result,
                }
            }
            Some(parsed)
        };
        let request = DashboardEventPageRequest {
            limit: clamp_limit(input.limit, 200),
            cursor,
            event_types,
        };
        match self
            .state
            .session_store
            .list_dashboard_session_events(identity.tenant_id, SessionId(input.session_id), request)
            .await
        {
            Ok(page) => match encode_cursor(page.next_cursor.as_ref()) {
                Ok(next_cursor) => result::success(
                    "Listed redacted session events.",
                    &serde_json::json!({ "events": page.events, "next_cursor": next_cursor }),
                ),
                Err(_) => result::execution_error("failed to encode cursor"),
            },
            Err(error) => read_error("session event list", error),
        }
    }

    /// Explain lineage records for one session or turn UUID using the configured read backend.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    // SAFETY: tenant Operator authz is enforced for every request by the
    // authenticate_mcp middleware before any tool dispatch.
    async fn lineage_explain(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<LineageExplainInput>,
    ) -> CallToolResult {
        let (identity, _) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let request = LineageExplainRequest {
            tenant_id: identity.tenant_id,
            id: input.id,
        };
        match lineage::explain(&self.state, request).await {
            Ok(response) => result::success("Explained lineage.", &response),
            Err(response) => result::execution_error(format!(
                "lineage read failed with status {}; verify the ID is a session or turn UUID visible to this tenant",
                response.status()
            )),
        }
    }

    /// List fresh redacted learning-candidate summaries for review triage.
    #[tool(annotations(
        read_only_hint = true,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    // SAFETY: tenant Operator authz is enforced for every request by the
    // authenticate_mcp middleware before any tool dispatch.
    async fn learning_candidates_list(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(input): Parameters<LearningCandidatesInput>,
    ) -> CallToolResult {
        let (identity, _) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let status = match parse_optional(input.status, "learning candidate status") {
            Ok(status) => status,
            Err(result) => return result,
        };
        match self
            .state
            .session_store
            .list_learning_candidate_summaries(
                identity.tenant_id,
                status,
                clamp_limit(input.limit, 200).unwrap_or(50),
            )
            .await
        {
            Ok(candidates) => result::success("Listed learning candidates.", &candidates),
            Err(error) => read_error("learning candidate list", error),
        }
    }
}

fn parse_optional<T>(value: Option<String>, label: &str) -> Result<Option<T>, CallToolResult>
where
    T: DeserializeOwned,
{
    value.map(|value| parse_value(value, label)).transpose()
}

fn parse_value<T>(value: String, label: &str) -> Result<T, CallToolResult>
where
    T: DeserializeOwned,
{
    // The serde message names the rejected value and lists the accepted
    // variants, so the calling model can self-correct without another probe.
    serde_json::from_value(Value::String(value))
        .map_err(|error| result::execution_error(format!("invalid {label}: {error}")))
}

fn read_error(operation: &'static str, error: impl std::fmt::Display) -> CallToolResult {
    tracing::error!(%error, operation, "MCP direct read failed");
    result::execution_error(format!(
        "{operation} failed on the server; this is not a tool-input error. Retry, and report it if it persists."
    ))
}
