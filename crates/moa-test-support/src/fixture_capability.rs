//! Parent-process MCP capability fixture with scripted, deterministic outcomes.
//!
//! One in-process streamable-HTTP MCP server whose declared tools return exactly
//! the scripted outcomes a test asked for, including malformed structured
//! content, terminal tool errors, and bounded HTTP failures. It exists so tests
//! that need a connector to *misbehave in one specific way* share one fixture
//! rather than each growing a bespoke fake whose fidelity drifts.
//!
//! Available on its own (feature `capability-fixture`) as well as through the
//! full orchestrator fixture, because connector-catalog staging has to be tested
//! without Docker, Restate, or Postgres. The map-item-key and execution-task-id
//! helpers need `moa-execution`, so they are gated on `orchestrator-fixture`:
//! that keeps the standalone fixture's dependency graph to an HTTP server and
//! stops an unrelated crate's build state from breaking connector-catalog tests.

#[cfg(feature = "orchestrator-fixture")]
use std::collections::BTreeSet;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header::RETRY_AFTER};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use tokio::task::JoinHandle;
#[cfg(feature = "orchestrator-fixture")]
use uuid::Uuid;

const FIXTURE_MCP_PROTOCOL_VERSION: &str = "2025-03-26";

/// Stable registered name of the deterministic reversible fixture effect.
pub const REVERSIBLE_FIXTURE_FORWARD_TOOL: &str = "fixture_effect_apply";
/// Stable registered name of the deterministic fixture effect compensator.
pub const REVERSIBLE_FIXTURE_COMPENSATOR_TOOL: &str = "fixture_effect_revert";

/// Returns a real source-declared exact rollback pair for catalog and workflow fixtures.
#[must_use]
pub fn reversible_fixture_tool_definitions() -> (
    moa_core::types::tools::ToolDefinition,
    moa_core::types::tools::ToolDefinition,
) {
    use moa_core::types::{
        action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
        tools::{
            IdempotencyClass, ToolDiffStrategy, ToolInputShape, ToolPolicySpec,
            ToolRollbackDefinition, ToolRollbackInputBinding, ToolRollbackInputMapping,
            ToolRollbackValueSource,
        },
    };

    let input_schema = json!({
        "type": "object",
        "properties": {"effect_id": {"type": "string"}},
        "required": ["effect_id"],
        "additionalProperties": false
    });
    let policy = ToolPolicySpec {
        risk_level: RiskLevel::High,
        default_effect: ActionPolicyEffect::Allow,
        action_class: ActionClass::ExternalWrite,
        input_shape: ToolInputShape::Json,
        diff_strategy: ToolDiffStrategy::None,
    };
    let forward = moa_core::types::tools::ToolDefinition {
        name: REVERSIBLE_FIXTURE_FORWARD_TOOL.to_string(),
        description: "Apply one deterministic fixture effect by stable effect id.".to_string(),
        schema: input_schema.clone(),
        policy: policy.clone(),
        idempotency_class: IdempotencyClass::NonIdempotent,
        rollback: Some(ToolRollbackDefinition {
            compensator_tool_name: REVERSIBLE_FIXTURE_COMPENSATOR_TOOL.to_string(),
            input_mapping: ToolRollbackInputMapping {
                bindings: vec![ToolRollbackInputBinding {
                    target_pointer: "/effect_id".to_string(),
                    source: ToolRollbackValueSource::OriginalInput {
                        pointer: "/effect_id".to_string(),
                    },
                }],
            },
        }),
        max_output_tokens: 128,
    };
    let compensator = moa_core::types::tools::ToolDefinition {
        name: REVERSIBLE_FIXTURE_COMPENSATOR_TOOL.to_string(),
        description: "Revert one applied deterministic fixture effect by stable effect id."
            .to_string(),
        schema: input_schema,
        policy,
        idempotency_class: IdempotencyClass::Idempotent,
        rollback: None,
        max_output_tokens: 128,
    };
    (forward, compensator)
}

/// One deterministic result returned by a fixture MCP capability.
#[derive(Clone, Debug, PartialEq)]
pub enum FixtureCapabilityOutcome {
    /// Return a successful MCP tool result with structured content.
    Success {
        /// Structured result exposed to the execution task.
        output: Value,
    },
    /// Return the input object with fixed fields merged over it.
    SuccessWithInput {
        /// Object fields merged over the received input object.
        output: Value,
    },
    /// Return one explicit non-success HTTP transport response.
    HttpFailure {
        /// HTTP status in the supported 400-599 fixture range.
        status: u16,
        /// Optional retry delay exposed as a rounded-up `Retry-After` seconds header.
        retry_after_ms: Option<u64>,
        /// Stable diagnostic body returned by the fixture server.
        message: String,
    },
    /// Return a terminal MCP tool result with `isError = true`.
    TerminalFailure {
        /// Stable tool error returned to the execution task.
        message: String,
    },
    /// Apply the logical upstream effect, then drop the HTTP request before a response exists.
    ///
    /// This models the irreducibly ambiguous non-idempotent boundary: the
    /// caller cannot infer from the disconnected transport whether the effect
    /// happened, while the fixture's exact effect counter proves that it did.
    ApplyThenDisconnect,
}

/// One MCP tool exposed by the parent-process fixture capability server.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureCapabilityTool {
    /// Exact discovered MCP tool name.
    pub name: String,
    /// Human-readable tool description used in the generated capability catalog.
    pub description: String,
    /// Draft-compatible JSON schema accepted by the MCP tool.
    pub input_schema: Value,
    /// RFC 6901 pointer used by production map-key extraction, or `None` for ordinary tasks.
    pub item_key_pointer: Option<String>,
    /// Whether the upstream contract permits an identical retry after an ambiguous transport loss.
    pub idempotent: bool,
    /// Ordered unique-invocation outcomes; the final outcome repeats after the list is exhausted.
    pub outcomes: Vec<FixtureCapabilityOutcome>,
}

/// Configuration for an opt-in execution-run fixture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FixtureCapabilityOptions {
    /// Exact MCP tools exposed to the orchestrator.
    pub tools: Vec<FixtureCapabilityTool>,
    /// Additional exact environment passed to the dedicated orchestrator child.
    pub orchestrator_env: Vec<(String, String)>,
}

/// One unique logical fixture capability effect.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureCapabilityCall {
    /// Provider tool-use identifier used as the logical-effect idempotency key.
    pub invocation_id: String,
    /// Exact MCP capability name.
    pub capability: String,
    /// Production-canonical map item key, or the empty string for ordinary tasks.
    pub item_key: String,
    /// Complete MCP `arguments` value received from the orchestrator.
    pub input: Value,
    /// One-based arrival order among unique logical effects in this observation window.
    pub arrival_order: u64,
}

/// One HTTP arrival at the fixture capability server, including logical replays.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureCapabilityAttempt {
    /// Provider tool-use identifier carried by this transport arrival.
    pub invocation_id: String,
    /// Exact MCP capability name.
    pub capability: String,
    /// Production-canonical map item key, or the empty string for ordinary tasks.
    pub item_key: String,
    /// Complete MCP `arguments` value received from the orchestrator.
    pub input: Value,
    /// One-based order among every transport arrival in this observation window.
    pub arrival_order: u64,
    /// One-based order of the corresponding unique logical effect.
    pub logical_arrival_order: u64,
    /// Whether an earlier arrival already created this logical effect.
    pub is_replay: bool,
}

/// Controller for observing and releasing parent-process fixture capabilities.
#[derive(Clone)]
pub struct FixtureCapabilityController {
    state: Arc<FixtureCapabilityState>,
}

impl FixtureCapabilityController {
    /// Waits until at least `count` unique effects have arrived and returns the full ordered set.
    pub async fn wait_for_calls(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<FixtureCapabilityCall>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.state.arrival_notify.notified();
            let calls = self.calls();
            if calls.len() >= count {
                return Ok(calls);
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .with_context(|| {
                    format!(
                        "fixture capability received {} of {count} unique calls within {timeout:?}",
                        calls.len()
                    )
                })?;
        }
    }

    /// Releases exactly `count` currently pending logical effects in unique-arrival order.
    pub fn release(&self, count: usize) {
        if count == 0 {
            return;
        }
        let effects = {
            let observations = lock_unpoisoned(&self.state.observations);
            let effects = observations
                .calls
                .iter()
                .filter_map(|call| observations.effects.get(&call.invocation_id))
                .filter(|effect| effect.is_pending())
                .take(count)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                effects.len(),
                count,
                "release({count}) requires {count} pending fixture capability effects"
            );
            effects
        };
        for effect in effects {
            effect.resolve(EffectResolution::Outcome(effect.outcome.clone()));
        }
    }

    /// Returns unique logical effects in deterministic arrival order.
    #[must_use]
    pub fn calls(&self) -> Vec<FixtureCapabilityCall> {
        lock_unpoisoned(&self.state.observations).calls.clone()
    }

    /// Returns every HTTP arrival in deterministic arrival order.
    #[must_use]
    pub fn transport_attempts(&self) -> Vec<FixtureCapabilityAttempt> {
        lock_unpoisoned(&self.state.observations)
            .transport_attempts
            .clone()
    }

    /// Returns the exact number of HTTP `tools/call` arrivals, including replays.
    #[must_use]
    pub fn request_count(&self) -> usize {
        lock_unpoisoned(&self.state.observations)
            .transport_attempts
            .len()
    }

    /// Returns the exact number of unique logical upstream effects applied.
    #[must_use]
    pub fn effect_count(&self) -> usize {
        lock_unpoisoned(&self.state.observations).calls.len()
    }

    /// Returns the number of fixture `tools/call` handlers currently live.
    #[must_use]
    pub fn current_live_calls(&self) -> usize {
        lock_unpoisoned(&self.state.observations).current_live_calls
    }

    /// Returns the peak number of concurrently live fixture `tools/call` handlers.
    #[must_use]
    pub fn peak_live_calls(&self) -> usize {
        lock_unpoisoned(&self.state.observations).peak_live_calls
    }

    /// Derives stable task IDs for unique map item keys using production algorithms.
    #[cfg(feature = "orchestrator-fixture")]
    pub fn derived_task_ids(
        &self,
        run_uid: Uuid,
        node_id: &str,
    ) -> Result<Vec<moa_execution::state::ExecutionTaskId>> {
        let calls = self.calls();
        let mut seen = BTreeSet::new();
        let mut task_ids = Vec::new();
        for call in calls {
            let Some(tool) = self.state.tools.get(&call.capability) else {
                bail!(
                    "fixture capability `{}` disappeared while deriving task ids",
                    call.capability
                );
            };
            let Some(pointer) = tool.item_key_pointer.as_deref() else {
                continue;
            };
            let item_key = fixture_map_key(&call.input, pointer)
                .with_context(|| format!("derive map item key for `{}`", call.capability))?;
            if item_key != call.item_key {
                bail!(
                    "fixture call item key changed from `{}` to `{item_key}`",
                    call.item_key
                );
            }
            if seen.insert(item_key.clone()) {
                task_ids.push(moa_execution::state::ExecutionTaskId::derive(
                    run_uid, node_id, &item_key,
                )?);
            }
        }
        Ok(task_ids)
    }

    /// Cancels pending handlers and clears calls, attempts, script cursors, and arrival counters.
    pub fn reset(&self) {
        let effects = {
            let mut observations = lock_unpoisoned(&self.state.observations);
            let effects = observations.effects.values().cloned().collect::<Vec<_>>();
            let next_live_call_generation = observations.live_call_generation.wrapping_add(1);
            *observations = FixtureCapabilityObservations::default();
            observations.live_call_generation = next_live_call_generation;
            effects
        };
        for effect in effects {
            effect.resolve(EffectResolution::Reset);
        }
    }
}

/// One running fixture MCP capability server and its graceful-shutdown handle.
pub struct FixtureCapabilityRuntime {
    controller: FixtureCapabilityController,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl FixtureCapabilityRuntime {
    /// Starts one fixture MCP server on an ephemeral loopback port.
    pub async fn start(options: FixtureCapabilityOptions) -> Result<Self> {
        let state = Arc::new(FixtureCapabilityState::new(options.tools)?);
        let controller = FixtureCapabilityController {
            state: Arc::clone(&state),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind fixture capability MCP listener")?;
        let address = listener
            .local_addr()
            .context("read fixture capability listener address")?;
        let endpoint = format!("http://{address}");
        let router = Router::new().route("/", post(handle_mcp)).with_state(state);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });
            if let Err(error) = server.await {
                tracing::warn!(%error, "fixture capability MCP server stopped unexpectedly");
            }
        });
        Ok(Self {
            controller,
            endpoint,
            shutdown: Some(shutdown_tx),
            task: Some(task),
        })
    }

    /// Returns the observation and release controller for this server.
    pub fn controller(&self) -> &FixtureCapabilityController {
        &self.controller
    }

    /// Returns the base URL an MCP client should be configured with.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Stops the server and aborts its accept task.
    pub fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for FixtureCapabilityRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

struct FixtureCapabilityState {
    tools: BTreeMap<String, FixtureCapabilityTool>,
    observations: StdMutex<FixtureCapabilityObservations>,
    arrival_notify: Notify,
}

impl FixtureCapabilityState {
    fn new(tools: Vec<FixtureCapabilityTool>) -> Result<Self> {
        let mut indexed = BTreeMap::new();
        for tool in tools {
            if tool.name.trim().is_empty() {
                bail!("fixture capability tool name must be non-empty");
            }
            if !tool.input_schema.is_object() {
                bail!(
                    "fixture capability `{}` input schema must be an object",
                    tool.name
                );
            }
            if tool.outcomes.is_empty() {
                bail!(
                    "fixture capability `{}` needs at least one outcome",
                    tool.name
                );
            }
            for outcome in &tool.outcomes {
                if matches!(
                    outcome,
                    FixtureCapabilityOutcome::SuccessWithInput { output }
                        if !output.is_object()
                ) {
                    bail!(
                        "fixture capability `{}` success-with-input output must be an object",
                        tool.name
                    );
                }
                if let FixtureCapabilityOutcome::HttpFailure {
                    status,
                    retry_after_ms: _,
                    message,
                } = outcome
                {
                    if !(400..=599).contains(status) {
                        bail!(
                            "fixture capability `{}` HTTP failure status must be in 400..=599",
                            tool.name
                        );
                    }
                    if message.trim().is_empty() {
                        bail!(
                            "fixture capability `{}` HTTP failure message must be non-empty",
                            tool.name
                        );
                    }
                }
            }
            let name = tool.name.clone();
            if indexed.insert(name.clone(), tool).is_some() {
                bail!("duplicate fixture capability tool `{name}`");
            }
        }
        Ok(Self {
            tools: indexed,
            observations: StdMutex::new(FixtureCapabilityObservations::default()),
            arrival_notify: Notify::new(),
        })
    }

    fn record_call(
        self: &Arc<Self>,
        invocation_id: String,
        capability: String,
        input: Value,
    ) -> Result<(RecordCall, LiveCallGuard)> {
        let tool = self
            .tools
            .get(&capability)
            .with_context(|| format!("unknown fixture capability `{capability}`"))?;
        let item_key = match tool.item_key_pointer.as_deref() {
            Some(pointer) => fixture_map_key(&input, pointer)
                .with_context(|| format!("extract fixture map key for `{capability}`"))?,
            None => String::new(),
        };
        let mut observations = lock_unpoisoned(&self.observations);
        observations.next_transport_order += 1;
        let transport_order = observations.next_transport_order;
        let mut unique_effect_arrived = false;
        let record = if let Some(effect) = observations.effects.get(&invocation_id).cloned() {
            let logical_arrival_order = effect.call.arrival_order;
            let conflict = effect.call.capability != capability
                || effect.call.item_key != item_key
                || effect.call.input != input;
            observations
                .transport_attempts
                .push(FixtureCapabilityAttempt {
                    invocation_id,
                    capability,
                    item_key,
                    input,
                    arrival_order: transport_order,
                    logical_arrival_order,
                    is_replay: true,
                });
            if conflict {
                RecordCall::Conflict
            } else {
                RecordCall::Effect(effect)
            }
        } else {
            observations.next_unique_order += 1;
            let unique_order = observations.next_unique_order;
            let script_index = observations
                .tool_script_cursors
                .entry(capability.clone())
                .or_default();
            let outcome = tool
                .outcomes
                .get(*script_index)
                .or_else(|| tool.outcomes.last())
                .context("validated fixture outcome script became empty")?
                .clone();
            *script_index += 1;
            let call = FixtureCapabilityCall {
                invocation_id: invocation_id.clone(),
                capability: capability.clone(),
                item_key: item_key.clone(),
                input: input.clone(),
                arrival_order: unique_order,
            };
            let effect = Arc::new(LogicalEffect::new(call.clone(), outcome));
            observations.calls.push(call);
            observations
                .transport_attempts
                .push(FixtureCapabilityAttempt {
                    invocation_id: invocation_id.clone(),
                    capability,
                    item_key,
                    input,
                    arrival_order: transport_order,
                    logical_arrival_order: unique_order,
                    is_replay: false,
                });
            observations
                .effects
                .insert(invocation_id, Arc::clone(&effect));
            unique_effect_arrived = true;
            RecordCall::Effect(effect)
        };
        observations.current_live_calls += 1;
        observations.peak_live_calls = observations
            .peak_live_calls
            .max(observations.current_live_calls);
        let live_call_generation = observations.live_call_generation;
        drop(observations);
        if unique_effect_arrived {
            self.arrival_notify.notify_waiters();
        }
        Ok((
            record,
            LiveCallGuard {
                state: Arc::clone(self),
                live_call_generation,
            },
        ))
    }

    fn listed_tools(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                    "annotations": {
                        "readOnlyHint": true,
                        "destructiveHint": false,
                        "idempotentHint": tool.idempotent,
                        "openWorldHint": false
                    }
                })
            })
            .collect()
    }
}

#[derive(Default)]
struct FixtureCapabilityObservations {
    calls: Vec<FixtureCapabilityCall>,
    transport_attempts: Vec<FixtureCapabilityAttempt>,
    effects: HashMap<String, Arc<LogicalEffect>>,
    tool_script_cursors: HashMap<String, usize>,
    next_unique_order: u64,
    next_transport_order: u64,
    current_live_calls: usize,
    peak_live_calls: usize,
    live_call_generation: u64,
}

struct LiveCallGuard {
    state: Arc<FixtureCapabilityState>,
    live_call_generation: u64,
}

impl Drop for LiveCallGuard {
    fn drop(&mut self) {
        let mut observations = lock_unpoisoned(&self.state.observations);
        if observations.live_call_generation == self.live_call_generation {
            observations.current_live_calls = observations.current_live_calls.saturating_sub(1);
        }
    }
}

struct LogicalEffect {
    call: FixtureCapabilityCall,
    outcome: FixtureCapabilityOutcome,
    resolution: StdMutex<Option<EffectResolution>>,
    resolved: Notify,
}

impl LogicalEffect {
    fn new(call: FixtureCapabilityCall, outcome: FixtureCapabilityOutcome) -> Self {
        Self {
            call,
            outcome,
            resolution: StdMutex::new(None),
            resolved: Notify::new(),
        }
    }

    fn is_pending(&self) -> bool {
        lock_unpoisoned(&self.resolution).is_none()
    }

    fn resolve(&self, resolution: EffectResolution) {
        let mut current = lock_unpoisoned(&self.resolution);
        if current.is_none() {
            *current = Some(resolution);
            drop(current);
            self.resolved.notify_waiters();
        }
    }

    async fn wait(&self) -> EffectResolution {
        loop {
            let notified = self.resolved.notified();
            if let Some(resolution) = lock_unpoisoned(&self.resolution).clone() {
                return resolution;
            }
            notified.await;
        }
    }
}

#[derive(Clone)]
enum EffectResolution {
    Outcome(FixtureCapabilityOutcome),
    Reset,
}

enum RecordCall {
    Effect(Arc<LogicalEffect>),
    Conflict,
}

async fn handle_mcp(
    State(state): State<Arc<FixtureCapabilityState>>,
    Json(request): Json<Value>,
) -> Response {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match method {
        "initialize" => json_rpc_result(
            id,
            json!({
                "protocolVersion": FIXTURE_MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "moa-fixture-capability", "version": "1" }
            }),
        ),
        "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
        "tools/list" => json_rpc_result(id, json!({ "tools": state.listed_tools() })),
        "tools/call" => handle_tool_call(state, id, request.get("params")).await,
        _ => json_rpc_error(id, -32601, format!("unknown fixture MCP method `{method}`")),
    }
}

async fn handle_tool_call(
    state: Arc<FixtureCapabilityState>,
    id: Value,
    params: Option<&Value>,
) -> Response {
    let Some(params) = params.and_then(Value::as_object) else {
        return json_rpc_error(id, -32602, "tools/call params must be an object");
    };
    let Some(capability) = params.get("name").and_then(Value::as_str) else {
        return json_rpc_error(id, -32602, "tools/call name must be a string");
    };
    let input = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let Some(invocation_id) = params
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("moa/toolInvocationId"))
        .and_then(Value::as_str)
    else {
        return json_rpc_error(
            id,
            -32602,
            "tools/call _meta.moa/toolInvocationId must be a string",
        );
    };
    let (record, _live_call) =
        match state.record_call(invocation_id.to_string(), capability.to_string(), input) {
            Ok(record) => record,
            Err(error) => return json_rpc_error(id, -32602, error.to_string()),
        };
    let RecordCall::Effect(effect) = record else {
        return json_rpc_error(
            id,
            -32602,
            "one tool invocation id cannot carry conflicting capability input",
        );
    };
    match effect.wait().await {
        EffectResolution::Outcome(FixtureCapabilityOutcome::Success { output }) => {
            successful_tool_response(id, output)
        }
        EffectResolution::Outcome(FixtureCapabilityOutcome::SuccessWithInput { output }) => {
            let Value::Object(mut merged) = effect.call.input.clone() else {
                return json_rpc_error(
                    id,
                    -32602,
                    "success-with-input requires object tool arguments",
                );
            };
            let Value::Object(output) = output else {
                return json_rpc_error(
                    id,
                    -32603,
                    "validated success-with-input output stopped being an object",
                );
            };
            merged.extend(output);
            successful_tool_response(id, Value::Object(merged))
        }
        EffectResolution::Outcome(FixtureCapabilityOutcome::HttpFailure {
            status,
            retry_after_ms,
            message,
        }) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut response = (status, message).into_response();
            if let Some(retry_after_ms) = retry_after_ms {
                let retry_after_seconds = retry_after_ms.div_ceil(1_000).max(1);
                if let Ok(value) = HeaderValue::from_str(&retry_after_seconds.to_string()) {
                    response.headers_mut().insert(RETRY_AFTER, value);
                }
            }
            response
        }
        EffectResolution::Outcome(FixtureCapabilityOutcome::TerminalFailure { message }) => {
            json_rpc_result(
                id,
                json!({
                    "content": [{ "type": "text", "text": message }],
                    "structuredContent": { "error": message },
                    "isError": true
                }),
            )
        }
        EffectResolution::Outcome(FixtureCapabilityOutcome::ApplyThenDisconnect) => {
            // Panicking the request task after `record_call` has committed the
            // fixture effect makes hyper tear down this HTTP exchange without
            // producing a response. The server and future requests stay alive.
            panic!("fixture applied logical effect, then disconnected transport")
        }
        EffectResolution::Reset => (
            StatusCode::SERVICE_UNAVAILABLE,
            "fixture capability observations reset",
        )
            .into_response(),
    }
}

fn successful_tool_response(id: Value, output: Value) -> Response {
    let text = match serde_json::to_string(&output) {
        Ok(text) => text,
        Err(error) => format!("fixture output serialization failed: {error}"),
    };
    json_rpc_result(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": output,
            "isError": false
        }),
    )
}

fn json_rpc_result(id: Value, result: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        })),
    )
        .into_response()
}

fn json_rpc_error(id: Value, code: i64, message: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message.into() }
        })),
    )
        .into_response()
}

/// Derives one canonical map item key with the production algorithm.
#[cfg(feature = "orchestrator-fixture")]
fn fixture_map_key(input: &Value, pointer: &str) -> Result<String> {
    Ok(moa_execution::bindings::extract_map_key(input, pointer)?)
}

/// Refuses map item keys when the production algorithm is not compiled in.
///
/// The standalone capability fixture deliberately does not depend on
/// `moa-execution`, so a test that declares `item_key_pointer` without the
/// `orchestrator-fixture` feature fails loudly rather than silently keying every
/// call the same.
#[cfg(not(feature = "orchestrator-fixture"))]
fn fixture_map_key(_input: &Value, pointer: &str) -> Result<String> {
    bail!(
        "fixture capability map item keys (pointer `{pointer}`) require the \
         `orchestrator-fixture` feature"
    )
}

fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// The fixture's own tests exercise the production map-key and task-id algorithms,
// so they compile only when those are available. A workspace-wide test run
// unifies `orchestrator-fixture` on and runs them.
#[cfg(all(test, feature = "orchestrator-fixture"))]
mod tests {
    use std::time::Duration;

    use moa_execution::bindings::extract_map_key;
    use moa_execution::state::ExecutionTaskId;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::{
        FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityRuntime,
        FixtureCapabilityTool,
    };

    fn tool() -> FixtureCapabilityTool {
        FixtureCapabilityTool {
            name: "fixture_map".to_string(),
            description: "Returns one deterministic company result.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "company": { "type": "string" } },
                "required": ["company"],
                "additionalProperties": false
            }),
            item_key_pointer: Some("/company".to_string()),
            idempotent: true,
            outcomes: vec![FixtureCapabilityOutcome::SuccessWithInput {
                output: json!({ "mentions": 7 }),
            }],
        }
    }

    fn request(id: u64, method: &str, params: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
    }

    #[tokio::test]
    async fn streamable_http_server_deduplicates_logical_effects_by_invocation_id() {
        // Pins: transport replays await one controller-owned logical effect while every arrival
        // remains observable and map task IDs use the production canonical-key algorithm.
        let runtime = FixtureCapabilityRuntime::start(FixtureCapabilityOptions {
            tools: vec![tool()],
            orchestrator_env: Vec::new(),
        })
        .await
        .expect("start fixture capability server");
        let client = reqwest::Client::new();

        let initialized = client
            .post(runtime.endpoint())
            .json(&request(
                1,
                "initialize",
                json!({ "protocolVersion": "2025-03-26" }),
            ))
            .send()
            .await
            .expect("initialize fixture MCP server")
            .json::<Value>()
            .await
            .expect("decode initialize response");
        assert_eq!(
            initialized.pointer("/result/protocolVersion"),
            Some(&json!("2025-03-26"))
        );

        let listed = client
            .post(runtime.endpoint())
            .json(&request(2, "tools/list", json!({})))
            .send()
            .await
            .expect("list fixture MCP tools")
            .json::<Value>()
            .await
            .expect("decode tool list response");
        assert_eq!(
            listed.pointer("/result/tools/0/name"),
            Some(&json!("fixture_map"))
        );
        assert_eq!(
            listed.pointer("/result/tools/0/annotations/idempotentHint"),
            Some(&json!(true))
        );

        let call = request(
            3,
            "tools/call",
            json!({
                "name": "fixture_map",
                "arguments": { "company": "AAPL" },
                "_meta": { "moa/toolInvocationId": "invocation-1" }
            }),
        );
        let first = tokio::spawn({
            let client = client.clone();
            let endpoint = runtime.endpoint().to_string();
            let call = call.clone();
            async move { client.post(endpoint).json(&call).send().await }
        });
        let second = tokio::spawn({
            let client = client.clone();
            let endpoint = runtime.endpoint().to_string();
            async move { client.post(endpoint).json(&call).send().await }
        });

        let calls = runtime
            .controller()
            .wait_for_calls(1, Duration::from_secs(2))
            .await
            .expect("wait for unique logical call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].item_key, "string:\"AAPL\"");
        tokio::time::timeout(Duration::from_secs(2), async {
            while runtime.controller().transport_attempts().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both transport attempts should arrive");
        assert_eq!(runtime.controller().current_live_calls(), 2);
        assert_eq!(runtime.controller().peak_live_calls(), 2);
        runtime.controller().release(1);

        let first = first
            .await
            .expect("join first transport")
            .expect("first transport response")
            .json::<Value>()
            .await
            .expect("decode first response");
        let second = second
            .await
            .expect("join replayed transport")
            .expect("replayed transport response")
            .json::<Value>()
            .await
            .expect("decode replayed response");
        assert_eq!(
            first.pointer("/result/structuredContent"),
            Some(&json!({ "company": "AAPL", "mentions": 7 }))
        );
        assert_eq!(second, first);
        assert_eq!(runtime.controller().calls().len(), 1);
        assert_eq!(runtime.controller().current_live_calls(), 0);
        assert_eq!(runtime.controller().peak_live_calls(), 2);
        let attempts = runtime.controller().transport_attempts();
        assert_eq!(attempts.len(), 2);
        assert_eq!(
            attempts.iter().filter(|attempt| attempt.is_replay).count(),
            1
        );
        let run_uid = Uuid::from_u128(17);
        let item_key =
            extract_map_key(&json!({ "company": "AAPL" }), "/company").expect("production map key");
        assert_eq!(
            runtime
                .controller()
                .derived_task_ids(run_uid, "screen")
                .expect("derive fixture task ids"),
            vec![
                ExecutionTaskId::derive(run_uid, "screen", &item_key)
                    .expect("derive expected production task id")
            ]
        );
    }

    #[tokio::test]
    async fn disconnected_first_handler_cannot_strand_a_replayed_logical_effect() {
        // Pins: the controller, not an individual HTTP future, owns pending invocation state.
        let runtime = FixtureCapabilityRuntime::start(FixtureCapabilityOptions {
            tools: vec![tool()],
            orchestrator_env: Vec::new(),
        })
        .await
        .expect("start fixture capability server");
        let client = reqwest::Client::new();
        let call = request(
            1,
            "tools/call",
            json!({
                "name": "fixture_map",
                "arguments": { "company": "NVDA" },
                "_meta": { "moa/toolInvocationId": "disconnected" }
            }),
        );
        let first = tokio::spawn({
            let client = client.clone();
            let endpoint = runtime.endpoint().to_string();
            let call = call.clone();
            async move { client.post(endpoint).json(&call).send().await }
        });
        runtime
            .controller()
            .wait_for_calls(1, Duration::from_secs(2))
            .await
            .expect("wait for first handler");
        first.abort();
        let _ = first.await;

        let replay = tokio::spawn({
            let endpoint = runtime.endpoint().to_string();
            async move { client.post(endpoint).json(&call).send().await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while runtime.controller().transport_attempts().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replayed transport should reach the fixture");
        runtime.controller().release(1);

        let response = replay
            .await
            .expect("join replayed transport")
            .expect("replayed transport response")
            .json::<Value>()
            .await
            .expect("decode replayed result");
        assert_eq!(
            response.pointer("/result/structuredContent"),
            Some(&json!({ "company": "NVDA", "mentions": 7 }))
        );
        assert_eq!(runtime.controller().calls().len(), 1);
        assert_eq!(runtime.controller().transport_attempts().len(), 2);
    }

    #[tokio::test]
    async fn non_idempotent_apply_then_disconnect_exposes_exact_ambiguous_counts() {
        // Pins: the upstream effect exists before the response disappears, and
        // the fixture distinguishes one applied effect from transport retries.
        let mut ambiguous_tool = tool();
        ambiguous_tool.idempotent = false;
        ambiguous_tool.outcomes = vec![FixtureCapabilityOutcome::ApplyThenDisconnect];
        let runtime = FixtureCapabilityRuntime::start(FixtureCapabilityOptions {
            tools: vec![ambiguous_tool],
            orchestrator_env: Vec::new(),
        })
        .await
        .expect("start ambiguous non-idempotent capability");
        let client = reqwest::Client::new();

        let listed = client
            .post(runtime.endpoint())
            .json(&request(1, "tools/list", json!({})))
            .send()
            .await
            .expect("list non-idempotent tool")
            .json::<Value>()
            .await
            .expect("decode non-idempotent tool list");
        assert_eq!(
            listed.pointer("/result/tools/0/annotations/idempotentHint"),
            Some(&json!(false))
        );

        let call = request(
            2,
            "tools/call",
            json!({
                "name": "fixture_map",
                "arguments": { "company": "AMBIGUOUS" },
                "_meta": { "moa/toolInvocationId": "ambiguous-effect" }
            }),
        );
        let request_task = tokio::spawn({
            let endpoint = runtime.endpoint().to_string();
            async move { client.post(endpoint).json(&call).send().await }
        });
        runtime
            .controller()
            .wait_for_calls(1, Duration::from_secs(2))
            .await
            .expect("wait for applied ambiguous effect");
        assert_eq!(runtime.controller().request_count(), 1);
        assert_eq!(runtime.controller().effect_count(), 1);
        runtime.controller().release(1);
        let response = request_task.await.expect("join disconnected request");
        assert!(
            response.is_err(),
            "apply-then-disconnect must not produce an HTTP response"
        );
        assert_eq!(runtime.controller().request_count(), 1);
        assert_eq!(runtime.controller().effect_count(), 1);
    }

    #[tokio::test]
    async fn scripted_outcomes_advance_only_for_new_logical_invocations() {
        // Pins: a transport replay receives the cached retryable failure and cannot consume the
        // next scripted success intended for a new execution-task generation.
        let mut retry_tool = tool();
        retry_tool.outcomes = vec![
            FixtureCapabilityOutcome::HttpFailure {
                status: 503,
                retry_after_ms: None,
                message: "try again".to_string(),
            },
            FixtureCapabilityOutcome::SuccessWithInput {
                output: json!({ "mentions": 9 }),
            },
        ];
        let runtime = FixtureCapabilityRuntime::start(FixtureCapabilityOptions {
            tools: vec![retry_tool],
            orchestrator_env: Vec::new(),
        })
        .await
        .expect("start retry fixture capability server");
        let client = reqwest::Client::new();
        let first_call = request(
            1,
            "tools/call",
            json!({
                "name": "fixture_map",
                "arguments": { "company": "AMD" },
                "_meta": { "moa/toolInvocationId": "generation-1" }
            }),
        );
        let first = tokio::spawn({
            let client = client.clone();
            let endpoint = runtime.endpoint().to_string();
            let first_call = first_call.clone();
            async move { client.post(endpoint).json(&first_call).send().await }
        });
        runtime
            .controller()
            .wait_for_calls(1, Duration::from_secs(2))
            .await
            .expect("wait for first generation");
        runtime.controller().release(1);
        let first = first
            .await
            .expect("join first generation")
            .expect("first generation response");
        assert_eq!(first.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);

        let replay = client
            .post(runtime.endpoint())
            .json(&first_call)
            .send()
            .await
            .expect("replay first generation");
        assert_eq!(replay.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(runtime.controller().calls().len(), 1);

        let second_call = request(
            2,
            "tools/call",
            json!({
                "name": "fixture_map",
                "arguments": { "company": "AMD" },
                "_meta": { "moa/toolInvocationId": "generation-2" }
            }),
        );
        let second = tokio::spawn({
            let endpoint = runtime.endpoint().to_string();
            async move { client.post(endpoint).json(&second_call).send().await }
        });
        runtime
            .controller()
            .wait_for_calls(2, Duration::from_secs(2))
            .await
            .expect("wait for second generation");
        runtime.controller().release(1);
        let second = second
            .await
            .expect("join second generation")
            .expect("second generation response")
            .json::<Value>()
            .await
            .expect("decode second generation success");
        assert_eq!(
            second.pointer("/result/structuredContent"),
            Some(&json!({ "company": "AMD", "mentions": 9 }))
        );
        assert_eq!(runtime.controller().calls().len(), 2);
        assert_eq!(runtime.controller().transport_attempts().len(), 3);
    }

    #[tokio::test]
    async fn http_failure_emits_exact_status_body_and_retry_after() {
        // Pins: adversarial service scenarios can model a bounded 429 response with an explicit
        // retry delay instead of collapsing every transport failure into one fixed fixture case.
        let mut rate_limited_tool = tool();
        rate_limited_tool.outcomes = vec![FixtureCapabilityOutcome::HttpFailure {
            status: 429,
            retry_after_ms: Some(1_500),
            message: "fixture rate limit".to_string(),
        }];
        let runtime = FixtureCapabilityRuntime::start(FixtureCapabilityOptions {
            tools: vec![rate_limited_tool],
            orchestrator_env: Vec::new(),
        })
        .await
        .expect("start rate-limit fixture capability server");
        let client = reqwest::Client::new();
        let call = request(
            1,
            "tools/call",
            json!({
                "name": "fixture_map",
                "arguments": { "company": "NVDA" },
                "_meta": { "moa/toolInvocationId": "rate-limited" }
            }),
        );
        let response = tokio::spawn({
            let endpoint = runtime.endpoint().to_string();
            async move { client.post(endpoint).json(&call).send().await }
        });
        runtime
            .controller()
            .wait_for_calls(1, Duration::from_secs(2))
            .await
            .expect("wait for rate-limited invocation");
        runtime.controller().release(1);

        let response = response
            .await
            .expect("join rate-limit transport")
            .expect("rate-limit response");
        assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("2")
        );
        assert_eq!(
            response.text().await.expect("read response body"),
            "fixture rate limit"
        );
    }

    #[tokio::test]
    async fn unsupported_http_failure_status_is_rejected_before_server_start() {
        // Pins: fixture scripts cannot accidentally model a successful HTTP response as a failure.
        let mut invalid_tool = tool();
        invalid_tool.outcomes = vec![FixtureCapabilityOutcome::HttpFailure {
            status: 200,
            retry_after_ms: None,
            message: "invalid fixture status".to_string(),
        }];

        let result = FixtureCapabilityRuntime::start(FixtureCapabilityOptions {
            tools: vec![invalid_tool],
            orchestrator_env: Vec::new(),
        })
        .await;
        let Err(error) = result else {
            panic!("successful HTTP status must be rejected");
        };
        assert!(
            error
                .to_string()
                .contains("HTTP failure status must be in 400..=599"),
            "unexpected validation error: {error:#}"
        );
    }

    #[tokio::test]
    async fn reset_cancels_pending_effects_and_starts_a_fresh_observation_window() {
        // Pins: resetting between scenarios cannot strand an HTTP handler or retain observations.
        let runtime = FixtureCapabilityRuntime::start(FixtureCapabilityOptions {
            tools: vec![tool()],
            orchestrator_env: Vec::new(),
        })
        .await
        .expect("start fixture capability server");
        let client = reqwest::Client::new();
        let pending = tokio::spawn({
            let endpoint = runtime.endpoint().to_string();
            async move {
                client
                    .post(endpoint)
                    .json(&request(
                        1,
                        "tools/call",
                        json!({
                            "name": "fixture_map",
                            "arguments": { "company": "MSFT" },
                            "_meta": { "moa/toolInvocationId": "pending" }
                        }),
                    ))
                    .send()
                    .await
            }
        });
        runtime
            .controller()
            .wait_for_calls(1, Duration::from_secs(2))
            .await
            .expect("wait for pending logical call");
        assert_eq!(runtime.controller().current_live_calls(), 1);
        assert_eq!(runtime.controller().peak_live_calls(), 1);

        runtime.controller().reset();

        let response = pending
            .await
            .expect("join reset transport")
            .expect("reset transport response");
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert!(runtime.controller().calls().is_empty());
        assert!(runtime.controller().transport_attempts().is_empty());
        assert_eq!(runtime.controller().current_live_calls(), 0);
        assert_eq!(runtime.controller().peak_live_calls(), 0);
    }
}
