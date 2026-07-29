//! Tenant-operations Model Context Protocol transport and tools.
//!
//! This module is an inbound operator surface. It is intentionally separate
//! from the outbound MCP clients used by agent hands.
#![allow(clippy::result_large_err)]

mod agents;
mod analytics_sessions;
mod artifacts_learning;
mod command;
mod contract;
mod evals;
mod execution_runs;
mod experiments;
mod http;
mod result;

use axum::http::HeaderMap;
use axum::http::Method;
use moa_core::traits::Identity;
use moa_core::types::identifiers::TenantId;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, schemars, tool_handler};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use self::command::{McpCommandClient, ServicePath};
use crate::routes::AppState;

pub use http::{McpHttpConfig, McpHttpConfigError};

/// Build the complete edge router with an explicit MCP HTTP security configuration.
pub fn router(
    state: AppState,
    config: McpHttpConfig,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> axum::Router {
    crate::routes::base_router(state.clone()).merge(http::router(state, config, cancellation_token))
}

/// Empty parameters for tools that accept no arguments.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct EmptyInput {}

/// Exact published agent revision executed as one simulation or experiment variant.
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
struct AgentRevisionVariantInput {
    /// Caller-chosen unique label used to identify this variant in trials and scores.
    variant_key: String,
    /// Exact published agent artifact revision UUID executed by this variant.
    revision_uid: Uuid,
}

/// Clamp an optional list limit to `1..=max`, matching the advertised schema bound.
fn clamp_limit<T>(limit: Option<T>, max: T) -> Option<T>
where
    T: Ord + From<u8>,
{
    limit.map(|limit| limit.clamp(T::from(1), max))
}

/// Stateless MCP server backed by the edge's existing stores and orchestrator proxy.
#[derive(Clone)]
pub struct Server {
    state: AppState,
    tool_router: ToolRouter<Self>,
}

impl Server {
    fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: all_tools(),
        }
    }

    async fn tenant_command<Input, Request, Response>(
        &self,
        context: RequestContext<RoleServer>,
        input: &Input,
        path: ServicePath,
        summary: &'static str,
    ) -> CallToolResult
    where
        Input: Serialize,
        Request: Serialize + DeserializeOwned,
        Response: Serialize + DeserializeOwned,
    {
        let (identity, headers) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let request: Request = match tenant_request(identity.tenant_id, input) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let command = McpCommandClient::new(self.state.proxy.as_ref(), &identity, &headers);
        result::command_result(
            summary,
            command.call::<Request, Response>(path, &request).await,
        )
    }

    async fn command<Input, Response>(
        &self,
        context: RequestContext<RoleServer>,
        input: &Input,
        path: ServicePath,
        summary: &'static str,
    ) -> CallToolResult
    where
        Input: Serialize,
        Response: Serialize + DeserializeOwned,
    {
        let (identity, headers) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let command = McpCommandClient::new(self.state.proxy.as_ref(), &identity, &headers);
        result::command_result(summary, command.call::<Input, Response>(path, input).await)
    }

    async fn tenant_run_command<Input, Request, Response>(
        &self,
        context: RequestContext<RoleServer>,
        input: &Input,
        path: ServicePath,
        summary: &'static str,
    ) -> CallToolResult
    where
        Input: Serialize,
        Request: Serialize + DeserializeOwned,
        Response: Serialize + DeserializeOwned,
    {
        let (identity, headers) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let mut value = match serde_json::to_value(input) {
            Ok(value) => value,
            Err(error) => return result::execution_error(format!("invalid tool input: {error}")),
        };
        let Some(run) = value
            .get_mut("run")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return result::execution_error("tool input must contain a run object");
        };
        run.insert(
            "tenant_id".to_string(),
            serde_json::json!(identity.tenant_id),
        );
        let request = match serde_json::from_value::<Request>(value) {
            Ok(request) => request,
            Err(error) => return result::execution_error(format!("invalid tool input: {error}")),
        };
        let command = McpCommandClient::new(self.state.proxy.as_ref(), &identity, &headers);
        result::command_result(
            summary,
            command.call::<Request, Response>(path, &request).await,
        )
    }

    async fn session_command<Input, Request, Response>(
        &self,
        context: RequestContext<RoleServer>,
        input: &Input,
        session_id: Uuid,
        handler: &'static str,
        summary: &'static str,
    ) -> CallToolResult
    where
        Input: Serialize,
        Request: Serialize + DeserializeOwned,
        Response: Serialize + DeserializeOwned,
    {
        let (identity, headers) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let request: Request = match tenant_request(identity.tenant_id, input) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let body = match serde_json::to_vec(&request) {
            Ok(body) => body,
            Err(error) => return result::execution_error(format!("invalid tool input: {error}")),
        };
        let service_path = format!("/Session/{session_id}/{handler}");
        let ingress_path =
            crate::ingress::call_path(&crate::ingress::IngressScope::Unscoped, &service_path);
        let response = match self
            .state
            .proxy
            .forward(
                &identity,
                reqwest::Method::POST,
                &ingress_path,
                body,
                &headers,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return result::execution_error(format!("orchestrator unavailable: {error}"));
            }
        };
        let status = response.status();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                return result::execution_error(format!("orchestrator unavailable: {error}"));
            }
        };
        if !status.is_success() {
            let bounded = &bytes[..bytes.len().min(4 * 1024)];
            let message = String::from_utf8_lossy(bounded).trim().to_string();
            return result::execution_error(if message.is_empty() {
                format!("service rejected command with status {}", status.as_u16())
            } else {
                message
            });
        }
        let response = match serde_json::from_slice::<Response>(&bytes) {
            Ok(response) => response,
            Err(error) => {
                return result::execution_error(format!(
                    "service returned an invalid response: {error}"
                ));
            }
        };
        result::success(summary, &response)
    }

    async fn command_empty<Response>(
        &self,
        context: RequestContext<RoleServer>,
        path: ServicePath,
        summary: &'static str,
    ) -> CallToolResult
    where
        Response: Serialize + DeserializeOwned,
    {
        let (identity, headers) = match request_identity_and_headers(&context) {
            Ok(request) => request,
            Err(result) => return result,
        };
        let command = McpCommandClient::new(self.state.proxy.as_ref(), &identity, &headers);
        result::command_result(summary, command.call_empty::<Response>(path).await)
    }
}

fn all_tools() -> ToolRouter<Server> {
    let mut router = analytics_sessions::router()
        + artifacts_learning::router()
        + agents::router()
        + execution_runs::router()
        + evals::router()
        + experiments::router();
    contract::enrich(&mut router);
    router
}

const MCP_READ_SCOPE: &str = "mcp:read";
const MCP_WRITE_SCOPE: &str = "mcp:write";

/// Derive the exact OAuth scope for one MCP HTTP message.
pub(crate) fn required_oauth_scope(method: &Method, body: &[u8]) -> Result<&'static str, ()> {
    if *method != Method::POST {
        return Ok(MCP_READ_SCOPE);
    }
    let value: Value = serde_json::from_slice(body).map_err(|_| ())?;
    match value.get("method").and_then(Value::as_str) {
        Some("tools/call") => {
            let name = value
                .pointer("/params/name")
                .and_then(Value::as_str)
                .ok_or(())?;
            let router = all_tools();
            let tool = router.map.get(name).ok_or(())?;
            let annotations = tool.attr.annotations.as_ref().ok_or(())?;
            if annotations.read_only_hint.is_none()
                || annotations.destructive_hint.is_none()
                || annotations.idempotent_hint.is_none()
                || annotations.open_world_hint.is_none()
                || (annotations.read_only_hint == Some(true)
                    && annotations.destructive_hint == Some(true))
            {
                return Err(());
            }
            Ok(if annotations.read_only_hint == Some(true) {
                MCP_READ_SCOPE
            } else {
                MCP_WRITE_SCOPE
            })
        }
        Some("initialize")
        | Some("ping")
        | Some("tools/list")
        | Some("notifications/initialized") => Ok(MCP_READ_SCOPE),
        _ => Err(()),
    }
}

fn request_identity_and_headers(
    context: &RequestContext<RoleServer>,
) -> Result<(Identity, HeaderMap), CallToolResult> {
    let Some(parts) = context.extensions.get::<axum::http::request::Parts>() else {
        return Err(result::execution_error(
            "missing authenticated HTTP request context",
        ));
    };
    let Some(identity) = parts.extensions.get::<Identity>() else {
        return Err(result::execution_error("missing authenticated identity"));
    };
    Ok((identity.clone(), parts.headers.clone()))
}

fn tenant_request<Request>(
    tenant_id: TenantId,
    input: &impl Serialize,
) -> Result<Request, CallToolResult>
where
    Request: DeserializeOwned,
{
    let mut value = serde_json::to_value(input)
        .map_err(|error| result::execution_error(format!("invalid tool input: {error}")))?;
    let Value::Object(object) = &mut value else {
        return Err(result::execution_error("tool input must be an object"));
    };
    object.insert("tenant_id".to_string(), serde_json::json!(tenant_id));
    serde_json::from_value(value)
        .map_err(|error| result::execution_error(format!("invalid tool input: {error}")))
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    /// Dispatch one tool call, normalizing every errored result into the
    /// documented `{error}` structured envelope. The macro's generated
    /// dispatch is bypassed so rmcp-internal argument-deserialization
    /// failures cannot reach clients without structured content.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let tool_call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router
            .call(tool_call)
            .await
            .map(result::normalize)
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("moa-edge", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "MOA tenant operations. Tenant scope is always the authenticated tenant; never invent or supply a tenant ID. Each tool description states when to use it, side effects, its structured result, and the recommended next tool. Successful structuredContent is always {summary, data}; execution failures set isError and return {error}. Inspect before mutating, validate drafts before publishing, and poll accepted eval, experiment, simulation, or execution runs by their returned ID.",
            )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::all_tools;
    use super::required_oauth_scope;

    fn collect_undocumented_properties(value: &Value, path: &str, failures: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                    for (property, schema) in properties {
                        let description = schema
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if description.is_empty() {
                            failures.push(format!("{path}.{property} needs a description"));
                        }
                    }
                }
                for (key, child) in object {
                    collect_undocumented_properties(child, &format!("{path}.{key}"), failures);
                }
            }
            Value::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    collect_undocumented_properties(child, &format!("{path}[{index}]"), failures);
                }
            }
            _ => {}
        }
    }

    fn find_schema_keyword<'a>(value: &'a Value, keyword: &str) -> Option<&'a Value> {
        match value {
            Value::Object(object) => object.get(keyword).or_else(|| {
                object
                    .values()
                    .find_map(|child| find_schema_keyword(child, keyword))
            }),
            Value::Array(values) => values
                .iter()
                .find_map(|child| find_schema_keyword(child, keyword)),
            _ => None,
        }
    }

    #[test]
    fn tenant_operations_tool_catalog_is_exact_and_has_no_internal_eval_handlers_offline() {
        // Pins: MCP discovery is an explicit allowlist, not a generic Restate dispatcher.
        let router = all_tools();
        let actual = router
            .map
            .keys()
            .map(|name| name.to_string())
            .collect::<BTreeSet<_>>();
        let expected = [
            "agent_definition_deploy",
            "agent_definition_install",
            "agent_definitions_list",
            "agent_deployments_list",
            "agent_installations_list",
            "agent_principal_deactivate",
            "agent_principal_get",
            "agent_principal_grant_act_as",
            "agent_principal_register",
            "agent_principal_revoke_act_as",
            "agent_principals_list",
            "agent_revision_compare",
            "agent_revision_simulate",
            "agent_revision_simulation_compare",
            "analytics_catalog",
            "analytics_query",
            "artifact_export",
            "artifact_import",
            "artifact_publish",
            "artifact_validate",
            "artifacts_list",
            "capabilities_list",
            "eval_compare",
            "eval_dataset_register",
            "eval_datasets_list",
            "eval_plan",
            "eval_run",
            "eval_run_status",
            "eval_scores",
            "eval_suites_summarize",
            "experiment_cancel",
            "experiment_compare",
            "experiment_plan_generate",
            "experiment_propose_improvements",
            "experiment_run",
            "experiment_scores",
            "experiment_status",
            "experiment_trial_status",
            "experiment_trials_list",
            "experiments_list",
            "learning_candidate_accept_rollback",
            "learning_candidate_accept_skill",
            "learning_candidate_dismiss",
            "learning_candidate_get",
            "learning_candidate_reject",
            "learning_candidates_list",
            "lineage_explain",
            "execution_review_decide",
            "execution_run_cancel",
            "execution_run_start",
            "execution_run_status",
            "execution_runs_list",
            "execution_signal",
            "session_events_list",
            "session_get",
            "sessions_list",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();

        assert_eq!(actual, expected, "MCP tool discovery allowlist drifted");
        assert_eq!(
            actual.len(),
            56,
            "expected exactly 56 tenant-operation tools"
        );
        assert!(!actual.contains("execute_run"));
        assert!(!actual.contains("replay"));
        for retired in [
            "index_rebuild_start",
            "index_rebuild_status",
            "index_rebuild_cancel",
            "index_rebuild_rollback",
            "index_rebuild_finalize",
        ] {
            assert!(
                !actual.contains(retired),
                "{retired} must not be advertised until model-aware retrieval exists"
            );
        }
    }

    #[test]
    fn oauth_scope_derives_from_complete_tool_annotations_offline() {
        // Pins: read/write OAuth authority follows the advertised tool contract.
        let read = serde_json::to_vec(&serde_json::json!({
            "method": "tools/call",
            "params": { "name": "sessions_list" }
        }))
        .expect("serialize read call");
        let write = serde_json::to_vec(&serde_json::json!({
            "method": "tools/call",
            "params": { "name": "artifact_publish" }
        }))
        .expect("serialize write call");
        assert_eq!(
            required_oauth_scope(&axum::http::Method::POST, &read),
            Ok("mcp:read")
        );
        assert_eq!(
            required_oauth_scope(&axum::http::Method::POST, &write),
            Ok("mcp:write")
        );
    }

    #[test]
    fn execution_run_mcp_contract() {
        // Pins: external MCP admission exposes only the pinned-template projection, while
        // legacy names and caller-supplied plan authority remain absent.
        let router = all_tools();
        let start = &router.map["execution_run_start"].attr.input_schema;
        let properties = start["properties"]
            .as_object()
            .expect("execution_run_start properties must be an object")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            properties,
            [
                "contact_id",
                "idempotency_key",
                "input",
                "objective",
                "session_id",
                "template",
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
        );
        let forbidden_names = [
            "tenant_id".to_string(),
            ["compiled", "plan", "id"].join("_"),
            ["raw", "plan"].join("_"),
            "plan".to_string(),
            ["run", "procedure"].join("_"),
            ["procedure", "status"].join("_"),
        ];
        for forbidden in forbidden_names {
            assert!(
                !properties.contains(forbidden.as_str()),
                "execution_run_start unexpectedly accepts {forbidden}"
            );
            assert!(
                !router.map.contains_key(forbidden.as_str()),
                "legacy or internal lifecycle tool {forbidden} must not be advertised"
            );
        }
    }

    #[test]
    fn tenant_operations_tool_schemas_never_accept_tenant_or_reviewer_overrides_offline() {
        // Pins: authenticated tenant/reviewer scope cannot be spoofed in tool arguments.
        for route in all_tools().map.into_values() {
            let properties = route
                .attr
                .input_schema
                .get("properties")
                .and_then(serde_json::Value::as_object);
            if let Some(properties) = properties {
                assert!(
                    !properties.contains_key("tenant_id"),
                    "{} unexpectedly accepts tenant_id",
                    route.attr.name
                );
                assert!(
                    !properties.contains_key("reviewer_subject"),
                    "{} unexpectedly accepts reviewer_subject",
                    route.attr.name
                );
                assert!(
                    !properties.contains_key("dispatch_token"),
                    "{} unexpectedly accepts dispatch_token",
                    route.attr.name
                );
            }
        }
    }

    #[test]
    fn tenant_operations_tool_contracts_are_model_decision_ready_offline() {
        // Pins: every advertised tool explains selection and sequencing and exposes
        // machine-readable input and output contracts to the calling model.
        let mut failures = Vec::new();

        for route in all_tools().map.into_values() {
            let name = route.attr.name.as_ref();
            let description = route.attr.description.as_deref().unwrap_or_default();
            for heading in ["Use when:", "Returns:", "Next:"] {
                if !description.contains(heading) {
                    failures.push(format!("{name} description is missing `{heading}`"));
                }
            }

            if route.attr.annotations.is_none() {
                failures.push(format!("{name} is missing MCP behavior annotations"));
            }

            if let Some(output_schema) = route.attr.output_schema.as_deref() {
                if output_schema.get("type") != Some(&serde_json::json!("object")) {
                    failures.push(format!(
                        "{name} outputSchema must describe an object envelope"
                    ));
                }
                if output_schema.get("required") != Some(&serde_json::json!(["summary", "data"])) {
                    failures.push(format!(
                        "{name} outputSchema must require exactly summary and data"
                    ));
                }
                if output_schema.get("additionalProperties") != Some(&serde_json::json!(false)) {
                    failures.push(format!(
                        "{name} outputSchema must reject unspecified envelope fields"
                    ));
                }
                for property in ["summary", "data"] {
                    let property_description = output_schema
                        .get("properties")
                        .and_then(serde_json::Value::as_object)
                        .and_then(|properties| properties.get(property))
                        .and_then(|schema| schema.get("description"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if property_description.is_empty() {
                        failures.push(format!(
                            "{name} outputSchema property `{property}` needs a description"
                        ));
                    }
                }
            } else {
                failures.push(format!("{name} is missing outputSchema"));
            }

            collect_undocumented_properties(
                &Value::Object(route.attr.input_schema.as_ref().clone()),
                &format!("{name}.inputSchema"),
                &mut failures,
            );
        }

        assert!(
            failures.is_empty(),
            "MCP model-facing contract failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn tenant_operations_input_schemas_pin_choices_and_runtime_bounds_offline() {
        // Pins: models see exact choices and the same bounds enforced by handlers.
        let router = all_tools();
        let analytics = &router.map["analytics_query"].attr.input_schema;
        assert_eq!(
            analytics["$defs"]["AnalyticsAggregationInput"]["enum"],
            serde_json::json!([
                "count",
                "count_distinct",
                "sum",
                "avg",
                "min",
                "max",
                "p50",
                "p95",
                "p99"
            ])
        );
        assert_eq!(
            analytics["$defs"]["AnalyticsFilterOperatorInput"]["enum"],
            serde_json::json!([
                "eq",
                "not_eq",
                "in",
                "not_in",
                "lt",
                "lte",
                "gt",
                "gte",
                "contains",
                "starts_with",
                "ends_with",
                "is_null",
                "is_not_null",
                "between"
            ])
        );
        assert_eq!(
            analytics["$defs"]["AnalyticsSortDirectionInput"]["enum"],
            serde_json::json!(["asc", "desc"])
        );
        assert_eq!(
            find_schema_keyword(&analytics["properties"]["limit"], "minimum"),
            Some(&serde_json::json!(1))
        );
        assert_eq!(
            find_schema_keyword(&analytics["properties"]["limit"], "maximum"),
            Some(&serde_json::json!(1000))
        );

        let artifacts = &router.map["artifacts_list"].attr.input_schema;
        assert_eq!(
            artifacts["$defs"]["ArtifactKindInput"]["enum"],
            serde_json::json!(["agent", "skill", "connector", "action", "experiment_plan"])
        );
        assert_eq!(
            artifacts["$defs"]["ArtifactStatusInput"]["enum"],
            serde_json::json!(["draft", "published", "archived"])
        );

        let review = &router.map["execution_review_decide"].attr.input_schema;
        assert_eq!(
            review["$defs"]["ExecutionReviewDecisionInput"]["enum"],
            serde_json::json!(["approved", "rejected"]),
            "execution review decision schema drifted: {review:#?}"
        );

        let eval_run = &router.map["eval_run"].attr.input_schema;
        assert_eq!(
            find_schema_keyword(&eval_run["properties"]["parallel"], "minimum"),
            Some(&serde_json::json!(1))
        );
    }
}
