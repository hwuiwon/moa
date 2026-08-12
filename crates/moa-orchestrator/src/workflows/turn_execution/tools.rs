//! Root-turn governed tool selection, dispatch, persistence, and approval routing.

use std::{collections::HashMap, time::Instant};

use moa_core::{
    types::completion::ToolInvocation,
    types::completion::{CompletionRequest, ToolCallContent},
    types::execution_planning::{DurableUpgradeSignal, ExecutionPlanningEvidence},
    types::identifiers::{SessionId, ToolCallId},
    types::session::SessionMeta,
    types::tools::ToolOutput,
    types::tools::TrustedSandboxFileManifestRef,
};
use moa_wire::turn::TurnPhase;
use restate_sdk::prelude::*;
use serde::Deserialize;
use tracing::Instrument;

use moa_observability::{
    record_turn_tool_dispatch_duration, restate_observability::tool_dispatch_span,
};

use crate::tool_invocation::governed::{
    GovernedInvocationOrigin, GovernedInvocationOutcome, GovernedInvocationRequest,
    append_cached_tool_result, invoke_governed_tool,
    record_segment_tool_use as record_governed_segment_tool_use,
};
use crate::turn::util::{
    DURABLE_UPGRADE_CONTROL_TOOL_NAME, TurnEvidence, blocked_canary_message, stable_tool_call_id,
    tool_input_leaks_canary,
};
use crate::turn_driver::progress as driver_progress;
use crate::workflows::errors::moa_error_to_handler_error;
use crate::workflows::turn_events::{
    COORDINATOR_SECURITY_INPUT_TIMEOUT_MESSAGE, append_tool_call_event, append_tool_result_event,
    record_segment_tool_use,
};
use crate::workflows::turn_progress;
use crate::workflows::turn_responsiveness::{
    ToolBudgetDecision, ToolBudgetExhausted, ToolBudgetState,
};

use super::TurnExecutionImpl;

struct DelegationToolRequest<'a> {
    meta: &'a SessionMeta,
    identity: &'a moa_core::traits::Identity,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &'a ToolCallContent,
    trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
}

/// Executes one root-coordinator delegation tool call, returning whether it
/// successfully spawned a new worker (used by the model loop to escalate the turn cap).
async fn handle_delegation_tool(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    request: DelegationToolRequest<'_>,
    turn_evidence: &mut TurnEvidence,
) -> Result<bool, HandlerError> {
    let DelegationToolRequest {
        meta,
        identity,
        session_id,
        tool_id,
        tool_call,
        trusted_sandbox_manifest,
    } = request;
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(
        workflow.event_appender(),
        ctx,
        session_id,
        tool_id,
        tool_call,
    )
    .await?;
    let Some(tool) =
        moa_core::types::worker::tool_schema::DelegationTool::from_invocation(&invocation)
            .map_err(moa_error_to_handler_error)?
    else {
        return Err(
            TerminalError::new(format!("unsupported delegation tool {}", invocation.name)).into(),
        );
    };
    let is_spawn = matches!(
        tool,
        moa_core::types::worker::tool_schema::DelegationTool::Spawn(_)
    );

    let span = tool_dispatch_span(&invocation.name);
    turn_progress::maybe_emit(
        ctx,
        session_id,
        turn_progress::running_tool_summary(&invocation.name),
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    let dispatch_started = Instant::now();
    let output = crate::delegation::execute_delegation_tool(
        ctx,
        crate::delegation::DelegationParent::RootSession {
            session_id,
            meta,
            identity,
        },
        tool,
        trusted_sandbox_manifest,
    )
    .instrument(span)
    .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    // Delegation output is workflow-authored, but it renders worker-supplied
    // task text, so it is classified rather than trusted.
    let secured = moa_security::classify_tool_output(
        &output,
        moa_security::OutputClassification {
            capability: &moa_core::types::security::ToolCapabilityId::builtin(&invocation.name),
            active_canary: None,
        },
    );
    append_tool_result_event(
        workflow.event_appender(),
        ctx,
        session_id,
        tool_id,
        &invocation,
        &secured,
    )
    .await?;
    turn_evidence.record_tool_result(&invocation, &secured.safe_output);
    if !secured.is_error() {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(is_spawn && !secured.is_error())
}

#[derive(Clone, Debug)]
pub(super) enum ToolDispatchOutcome {
    Completed,
    DurableUpgrade(DurableUpgradeSignal),
    DurableUpgradeUnsupported(String),
    Cancelled,
    ToolBudgetExceeded(ToolBudgetExhausted),
    /// The prompt-injection circuit reached its halt threshold for this owner.
    SecurityHalt,
    /// The coordinator's bounded security-input wait expired without an answer.
    SecurityInputTimedOut,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableUpgradeToolInput {
    rationale: String,
    evidence: Vec<ExecutionPlanningEvidence>,
}

/// Returns the strict provider schema for the workflow-owned Durable-upgrade control tool.
pub(super) fn durable_upgrade_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": DURABLE_UPGRADE_CONTROL_TOOL_NAME,
        "description": "Request the one-way transition from bounded Inline work to a durable execution plan after the current turn has discovered concrete evidence that the remaining work needs durability, resumability, approval or signal handling, or broad fan-out. Call this control by itself.",
        "input_schema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["rationale", "evidence"],
            "properties": {
                "rationale": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 240,
                    "description": "One short sentence explaining why the discovered work now needs Durable execution."
                },
                "evidence": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 32,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["source", "summary", "value"],
                        "properties": {
                            "source": {"type": "string", "description": "Stable label for the already-observed source."},
                            "summary": {"type": "string", "description": "Concise summary of the observed fact."},
                            "value": {"description": "Structured evidence already gathered during this Inline turn."}
                        }
                    }
                }
            }
        }
    })
}

/// Installs the authoritative workflow control schema only for an eligible root Inline turn.
pub(super) fn configure_durable_upgrade_tool_schema(
    request: &mut CompletionRequest,
    allowed: bool,
) {
    request.tools.retain(|schema| {
        schema
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|name| name != DURABLE_UPGRADE_CONTROL_TOOL_NAME)
    });
    if allowed {
        request.tools.push(durable_upgrade_tool_schema());
    }
}

fn durable_upgrade_signal_from_control_call(
    objective: &str,
    durable_upgrade_allowed: bool,
    active_canary: Option<&str>,
    tool_calls: &[&ToolCallContent],
) -> Result<Option<DurableUpgradeSignal>, String> {
    let upgrade_calls = tool_calls
        .iter()
        .filter(|call| call.invocation.name == DURABLE_UPGRADE_CONTROL_TOOL_NAME)
        .collect::<Vec<_>>();
    let Some(tool_call) = upgrade_calls.first() else {
        return Ok(None);
    };
    if !durable_upgrade_allowed {
        return Err("Durable upgrade is not available for this turn".to_string());
    }
    if upgrade_calls.len() != 1 || tool_calls.len() != 1 {
        return Err(
            "the Durable-upgrade control must be the only tool call in its model response"
                .to_string(),
        );
    }
    if tool_input_leaks_canary(active_canary, &tool_call.invocation.input)
        .map_err(|_| "Durable-upgrade control security screening failed".to_string())?
    {
        return Err(blocked_canary_message(DURABLE_UPGRADE_CONTROL_TOOL_NAME));
    }
    let input =
        serde_json::from_value::<DurableUpgradeToolInput>(tool_call.invocation.input.clone())
            .map_err(|_| "invalid Durable-upgrade control input".to_string())?;
    Ok(Some(DurableUpgradeSignal {
        objective: objective.to_string(),
        rationale: input.rationale,
        evidence: input.evidence,
    }))
}

/// Number of cache serves of one file after which the notice escalates to a STOP.
const ESCALATED_SERVE_THRESHOLD: usize = 3;

/// Filesystem prefix of the immutable skill-package mount.
///
/// Root-turn `file_read` is served only from the content-addressed, SHA256-validated
/// skill-package manifest (`root_trusted_file_read` in `services::tool_executor`),
/// never the live sandbox filesystem, and every manifest path lives under this prefix
/// (`.moa/skills/<slug>/...`). Restricting the cache to these paths keeps a
/// load-bearing invariant local to the cache itself: it can only ever hold immutable
/// content, so a cache serve can never return a stale body. Even if a future root tool
/// gained `file_read` access to a mutable filesystem, a read of that path would not
/// match this prefix and so would never be cached.
const SKILL_PACKAGE_PATH_PREFIX: &str = ".moa/skills/";

/// One remembered skill-package `file_read`, tracking how many times it has been
/// re-served this turn. The body is intentionally not stored: the first read already
/// placed the content in context, so a repeat is served as a notice-only reference.
struct CachedRead {
    serves: usize,
}

/// Per-turn memory of successful skill-package `file_read` calls, keyed by canonical
/// input.
///
/// Created once per user turn and dropped when the turn ends, so entries never leak
/// across turns. A repeated identical successful read of an immutable skill-package
/// file is answered from memory with a corrective, notice-only reference (the file
/// body is not repeated, since it is already in context from the first read) instead
/// of re-dispatching the tool; the notice escalates to a STOP once the same file has
/// been re-served [`ESCALATED_SERVE_THRESHOLD`] times. Only successful `file_read`
/// calls whose path is under [`SKILL_PACKAGE_PATH_PREFIX`] are remembered; every other
/// tool, every non-skill path, and every error is ignored.
#[derive(Default)]
pub(super) struct FileReadTurnCache {
    seen: HashMap<String, CachedRead>,
}

impl FileReadTurnCache {
    /// Returns whether this exact `file_read` would be served from the cache, without
    /// mutating serve state. Used to pick the budget path before dispatch.
    fn will_serve(&self, invocation: &ToolInvocation) -> bool {
        is_cacheable_skill_read(invocation)
            && self
                .seen
                .contains_key(&canonical_input_key(&invocation.input))
    }

    /// Serves a repeated skill-package `file_read` from memory, counting the serve and
    /// returning the notice-only reference; `None` for the first read, any non-skill
    /// path, or any non-`file_read` call. The notice escalates once the file has been
    /// re-served enough times.
    fn serve_repeat(&mut self, invocation: &ToolInvocation) -> Option<ToolOutput> {
        let path = cacheable_skill_read_path(invocation)?;
        let entry = self.seen.get_mut(&canonical_input_key(&invocation.input))?;
        entry.serves = entry.serves.saturating_add(1);
        Some(annotate_cached_file_read(path, entry.serves))
    }

    /// Remembers a successful skill-package `file_read` so later identical reads are
    /// served from memory. Non-`file_read` calls, reads of paths outside the immutable
    /// skill-package mount, and error outputs are ignored; the first successful read
    /// for a given input wins (later identical reads reuse its serve counter).
    fn remember(&mut self, invocation: &ToolInvocation, output: &ToolOutput) {
        if output.is_error || !is_cacheable_skill_read(invocation) {
            return;
        }
        self.seen
            .entry(canonical_input_key(&invocation.input))
            .or_insert(CachedRead { serves: 0 });
    }
}

/// Returns the `path` of a `file_read` that targets an immutable skill-package file,
/// or `None` for any other tool or a path outside [`SKILL_PACKAGE_PATH_PREFIX`].
fn cacheable_skill_read_path(invocation: &ToolInvocation) -> Option<&str> {
    if invocation.name != "file_read" {
        return None;
    }
    invocation
        .input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| path.starts_with(SKILL_PACKAGE_PATH_PREFIX))
}

/// Returns whether this read targets an immutable skill-package file and is therefore
/// safe to serve from the per-turn cache without any staleness risk.
fn is_cacheable_skill_read(invocation: &ToolInvocation) -> bool {
    cacheable_skill_read_path(invocation).is_some()
}

/// Serializes a tool input with object keys sorted so two equivalent reads that
/// differ only in key order share a cache key.
fn canonical_input_key(input: &serde_json::Value) -> String {
    fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut sorted = serde_json::Map::new();
                let mut keys = map.keys().collect::<Vec<_>>();
                keys.sort();
                for key in keys {
                    sorted.insert(key.clone(), canonicalize(&map[key]));
                }
                serde_json::Value::Object(sorted)
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(canonicalize).collect())
            }
            other => other.clone(),
        }
    }
    canonicalize(input).to_string()
}

/// Builds the notice-only output for a repeated skill-package `file_read`.
///
/// The file body is deliberately omitted: the first read this turn already placed the
/// content in context, so repeating it would only re-grow the event log and prompt
/// tokens for content the model already has. The output is a pure function of the read
/// path and serve count (both deterministic from turn state), so it replays
/// identically. The notice escalates to a STOP once the same file has been re-served
/// [`ESCALATED_SERVE_THRESHOLD`] times.
fn annotate_cached_file_read(path: &str, serves: usize) -> ToolOutput {
    let notice = if serves >= ESCALATED_SERVE_THRESHOLD {
        format!(
            "STOP: you have requested this identical read of `{path}` {serves} times this turn. \
             The file is unchanged and its full content is already in your context from the \
             earlier read; it is not repeated here. Do not read it again — continue the task \
             with the content you already have."
        )
    } else {
        format!(
            "(cached: `{path}` is unchanged and identical to your earlier read this turn — its \
             content is already in your context from that read and is not repeated here; do not \
             request it again)"
        )
    };
    ToolOutput::text(notice, std::time::Duration::ZERO)
}

pub(super) struct RootToolContext<'a> {
    pub(super) meta: &'a SessionMeta,
    pub(super) identity: &'a moa_core::traits::Identity,
    pub(super) session_id: SessionId,
    /// Session-facing workflow turn key recorded as the action-review owner turn.
    pub(super) turn_id: &'a str,
    /// Session turn generation that admitted this turn.
    pub(super) generation: u64,
    pub(super) active_canary: Option<&'a str>,
    pub(super) tool_catalog_pin: &'a moa_hands::ToolCatalogPin,
    pub(super) trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    /// Skills injected into this turn's manifest, used to detect which the model engaged.
    pub(super) selected_skills: &'a [String],
    pub(super) objective: &'a str,
    pub(super) durable_upgrade_allowed: bool,
    pub(super) resource_budget: moa_core::types::resource::ResourceBudget,
    pub(super) turn_evidence: &'a mut TurnEvidence,
    /// Per-turn `file_read` result memory shared across this turn's model-loop iterations.
    pub(super) file_read_cache: &'a mut FileReadTurnCache,
    /// Tools the security circuit disabled for the rest of this turn.
    pub(super) disabled_tools: &'a mut std::collections::BTreeSet<String>,
    /// Latched on when this turn successfully spawns a worker, so the model loop can
    /// escalate its turn cap for the remaining delegation, wait, and synthesis work.
    pub(super) delegated_worker: &'a mut bool,
}

pub(super) async fn dispatch_response_tool_calls(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    mut tool_context: RootToolContext<'_>,
    allowed_tools: &std::collections::BTreeSet<String>,
    tool_budget: &mut ToolBudgetState,
    tool_calls: &[&ToolCallContent],
    last_summary: &mut Option<String>,
) -> Result<ToolDispatchOutcome, HandlerError> {
    match durable_upgrade_signal_from_control_call(
        tool_context.objective,
        tool_context.durable_upgrade_allowed,
        tool_context.active_canary,
        tool_calls,
    ) {
        Ok(Some(signal)) => {
            if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
                *last_summary = Some(reason);
                return Ok(ToolDispatchOutcome::Cancelled);
            }
            let invocation = &tool_calls[0].invocation;
            if let Some(exhaustion) =
                record_tool_budget(ctx, tool_budget, invocation, false).await?
            {
                return Ok(ToolDispatchOutcome::ToolBudgetExceeded(exhaustion));
            }
            return Ok(ToolDispatchOutcome::DurableUpgrade(signal));
        }
        Ok(None) => {}
        Err(message) => return Ok(ToolDispatchOutcome::DurableUpgradeUnsupported(message)),
    }

    for (index, tool_call) in tool_calls.iter().enumerate() {
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
            *last_summary = Some(reason);
            return Ok(ToolDispatchOutcome::Cancelled);
        }
        // A call the per-turn cache will serve is not a dispatch, so it does not advance
        // the consecutive-repeat loop counter (it still counts against max_tool_calls).
        let cache_will_serve = tool_context
            .file_read_cache
            .will_serve(&tool_call.invocation);
        if let Some(exhaustion) =
            record_tool_budget(ctx, tool_budget, &tool_call.invocation, cache_will_serve).await?
        {
            return Ok(ToolDispatchOutcome::ToolBudgetExceeded(exhaustion));
        }
        match handle_tool_call(
            workflow,
            ctx,
            &mut tool_context,
            allowed_tools,
            index,
            tool_call,
        )
        .await?
        {
            ToolCallDisposition::Continue => {}
            ToolCallDisposition::SecurityHalt => {
                return Ok(ToolDispatchOutcome::SecurityHalt);
            }
            ToolCallDisposition::Cancelled(reason) => {
                *last_summary = Some(reason);
                return Ok(ToolDispatchOutcome::Cancelled);
            }
            ToolCallDisposition::SecurityInputTimedOut => {
                *last_summary = Some(COORDINATOR_SECURITY_INPUT_TIMEOUT_MESSAGE.to_string());
                return Ok(ToolDispatchOutcome::SecurityInputTimedOut);
            }
            // `handle_tool_call` already parked on the user's reply before
            // returning, so by the time control reaches here the suspend has been
            // answered and the loop may continue with the capability disabled.
            ToolCallDisposition::SecurityNeedsInput => {}
        }
    }
    Ok(ToolDispatchOutcome::Completed)
}

pub(super) async fn record_tool_budget(
    ctx: &WorkflowContext<'_>,
    tool_budget: &mut ToolBudgetState,
    invocation: &ToolInvocation,
    cache_will_serve: bool,
) -> Result<Option<ToolBudgetExhausted>, HandlerError> {
    let decision = if cache_will_serve {
        tool_budget.record_cached_serve(invocation)
    } else {
        tool_budget.before_tool_dispatch(invocation)
    };
    match decision {
        ToolBudgetDecision::Allow {
            attempted_tool_calls,
        } => {
            driver_progress::set_tool_calls(ctx, attempted_tool_calls);
            Ok(None)
        }
        ToolBudgetDecision::Stop(exhaustion) => {
            driver_progress::set_tool_calls(ctx, tool_budget.attempted_tool_calls());
            Ok(Some(exhaustion))
        }
    }
}

async fn handle_tool_call(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    tool_context: &mut RootToolContext<'_>,
    allowed_tools: &std::collections::BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
) -> Result<ToolCallDisposition, HandlerError> {
    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    let meta = tool_context.meta;
    let session_id = tool_context.session_id;
    let active_canary = tool_context.active_canary;
    let selected_skills = tool_context.selected_skills;
    let tool_id = stable_tool_call_id(session_id, index, tool_call);

    // A capability the circuit disabled cannot dispatch again under this owner.
    // Refusing here, before policy evaluation and before the executor round-trip,
    // is what makes "disabled" mean the tool does not run — not that it runs and
    // its output is discarded.
    if tool_context
        .disabled_tools
        .contains(&tool_call.invocation.name)
    {
        refuse_disabled_capability(workflow, ctx, tool_context, tool_id, tool_call).await?;
        return Ok(ToolCallDisposition::Continue);
    }

    // Serve a repeated identical successful file_read from the per-turn cache with a
    // (possibly escalated) notice, so the model learns the content is already in context
    // and stops re-reading. The tool is not re-dispatched; the call/result pair is still
    // persisted so the notice reaches the next context.
    if let Some(cached_output) = tool_context
        .file_read_cache
        .serve_repeat(&tool_call.invocation)
    {
        let request = GovernedInvocationRequest {
            session: meta,
            identity: tool_context.identity,
            session_id,
            tool_id,
            tool_call,
            allowed_tools,
            expected_tool_contract_revision: tool_context
                .tool_catalog_pin
                .contract_revision(&tool_call.invocation.name),
            active_canary,
            trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
            origin: GovernedInvocationOrigin::RootTurn {
                turn_id: tool_context.turn_id,
                generation: tool_context.generation,
            },
            capability_provenance: None,
            capability_policy_context: None,
            resource_budget: tool_context.resource_budget,
        };
        append_cached_tool_result(ctx, &request, &cached_output).await?;
        tool_context
            .turn_evidence
            .record_tool_result(&tool_call.invocation, &cached_output);
        return Ok(ToolCallDisposition::Continue);
    }

    let turn_evidence = &mut *tool_context.turn_evidence;
    let outcome = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session: meta,
            identity: tool_context.identity,
            session_id,
            tool_id,
            tool_call,
            allowed_tools,
            expected_tool_contract_revision: tool_context
                .tool_catalog_pin
                .contract_revision(&tool_call.invocation.name),
            active_canary,
            trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
            origin: GovernedInvocationOrigin::RootTurn {
                turn_id: tool_context.turn_id,
                generation: tool_context.generation,
            },
            capability_provenance: None,
            capability_policy_context: None,
            resource_budget: tool_context.resource_budget,
        },
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;

    let mut disposition = ToolCallDisposition::Continue;
    let suspend_context = (session_id, tool_id);
    match outcome {
        GovernedInvocationOutcome::Completed(result) => {
            disposition = apply_coordinator_security_assessment(
                workflow,
                ctx,
                session_id,
                meta.tenant_id,
                tool_context.turn_id,
                tool_context.generation,
                &result,
                tool_context.disabled_tools,
            )
            .await?;
            turn_evidence.record_tool_result(&result.invocation, &result.output.safe_output);
            tool_context
                .file_read_cache
                .remember(&result.invocation, &result.output.safe_output);
            if result.should_record_segment_tool_use() {
                record_governed_segment_tool_use(ctx, session_id, &result.invocation.name).await?;
            }
            crate::workflows::turn_events::record_segment_skill_use_for_tool_call(
                ctx,
                session_id,
                &tool_call.invocation.name,
                &tool_call.invocation.input,
                selected_skills,
            )
            .await?;
        }
        GovernedInvocationOutcome::Delegation { tool_id, .. } => {
            let spawned_worker = handle_delegation_tool(
                workflow,
                ctx,
                DelegationToolRequest {
                    meta,
                    identity: tool_context.identity,
                    session_id,
                    tool_id,
                    tool_call,
                    trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
                },
                turn_evidence,
            )
            .await?;
            if spawned_worker {
                *tool_context.delegated_worker = true;
            }
        }
        GovernedInvocationOutcome::ExternalJob { .. }
        | GovernedInvocationOutcome::UnknownOutcome { .. }
        | GovernedInvocationOutcome::NotDispatched { .. } => {
            return Err(TerminalError::new(
                "root-turn governed invocation returned an execution-only outcome",
            )
            .into());
        }
    }
    if disposition == ToolCallDisposition::SecurityNeedsInput {
        // Idle until the user answers. Until then this turn makes no further tool
        // calls, which is the point of a score-3 suspend.
        let (suspend_session_id, suspend_tool_id) = suspend_context;
        let input_outcome = await_coordinator_security_input(
            ctx,
            suspend_session_id,
            tool_context.turn_id,
            tool_context.generation,
            suspend_tool_id,
            workflow.session_limits().coordinator_input_timeout_ms,
        )
        .await?;
        disposition = match input_outcome {
            CoordinatorSecurityInputOutcome::Answered => disposition,
            CoordinatorSecurityInputOutcome::Cancelled(reason) => {
                ToolCallDisposition::Cancelled(reason)
            }
            CoordinatorSecurityInputOutcome::TimedOut => ToolCallDisposition::SecurityInputTimedOut,
        };
    }
    Ok(disposition)
}

/// Whether one dispatched tool call left the turn able to continue.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolCallDisposition {
    /// The turn may keep running.
    Continue,
    /// The circuit halted this owner; the turn must terminate.
    SecurityHalt,
    /// The circuit suspended this owner pending the user's answer.
    SecurityNeedsInput,
    /// Cancellation won while the coordinator was parked for user input.
    Cancelled(String),
    /// The bounded coordinator input wait expired.
    SecurityInputTimedOut,
}

/// Records the refusal of a tool whose capability the circuit already disabled.
///
/// The model still needs a tool result for the call it made, so this writes the
/// call/result pair with a fixed safe notice and never reaches the executor.
async fn refuse_disabled_capability(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    tool_context: &RootToolContext<'_>,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let notice = moa_core::types::tools::ToolOutput::error(
        format!(
            "Tool {} is disabled for this turn: its output tripped the prompt-injection \
             security circuit. Do not retry it; continue without this capability.",
            tool_call.invocation.name
        ),
        std::time::Duration::ZERO,
    );
    let secured = moa_security::classify_tool_output(
        &notice,
        moa_security::OutputClassification {
            capability: &moa_core::types::security::ToolCapabilityId::builtin(
                &tool_call.invocation.name,
            ),
            active_canary: None,
        },
    );
    crate::workflows::turn_events::append_tool_call_event(
        workflow.event_appender(),
        ctx,
        tool_context.session_id,
        tool_id,
        tool_call,
    )
    .await?;
    crate::workflows::turn_events::append_tool_result_event(
        workflow.event_appender(),
        ctx,
        tool_context.session_id,
        tool_id,
        &tool_call.invocation,
        &secured,
    )
    .await
}

/// Registers a coordinator input request and parks the turn until it is answered.
///
/// The awakeable is created here because only this invocation can park on it; the
/// Session VO stores the mapping and advertises the matching pending reply target
/// so an unaddressed plain reply routes here instead of queuing behind the turn.
/// The question is fixed — the output that triggered this is exactly what MOA has
/// decided it cannot trust, so quoting it to the user would forward the attack.
async fn await_coordinator_security_input(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: &str,
    generation: u64,
    tool_id: ToolCallId,
    timeout_ms: u64,
) -> Result<CoordinatorSecurityInputOutcome, HandlerError> {
    let input_request_id = format!("security:{turn_id}:{generation}:{tool_id}");
    let awakeable = ctx.awakeable::<String>();
    let (awakeable_id, reply) = awakeable;
    let waiting_workflow_id = turn_id.to_string();

    crate::restate_identity::replay_safe_request(
        ctx.object_client::<crate::objects::session::SessionClient>(session_id.to_string())
            .register_coordinator_input(Json::from(
                moa_wire::turn::RegisterCoordinatorInputRequest {
                    turn_id: turn_id.to_string(),
                    generation,
                    input_request_id: input_request_id.clone(),
                    awakeable_id,
                    waiting_workflow_id: waiting_workflow_id.clone(),
                    question: COORDINATOR_SECURITY_INPUT_QUESTION.to_string(),
                },
            )),
    )
    .call()
    .await?;

    let outcome = restate_sdk::select! {
        answer = reply => {
            answer?;
            CoordinatorSecurityInputOutcome::Answered
        },
        reason = ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE) => {
            CoordinatorSecurityInputOutcome::Cancelled(reason?)
        },
        _ = ctx.sleep(std::time::Duration::from_millis(timeout_ms)) => {
            CoordinatorSecurityInputOutcome::TimedOut
        }
    };

    if !matches!(outcome, CoordinatorSecurityInputOutcome::Answered) {
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<crate::objects::session::SessionClient>(session_id.to_string())
                .clear_coordinator_input(Json::from(
                    moa_wire::turn::ClearCoordinatorInputRequest {
                        turn_id: turn_id.to_string(),
                        generation,
                        input_request_id,
                        waiting_workflow_id,
                    },
                )),
        )
        .call()
        .await?;
    }

    Ok(outcome)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoordinatorSecurityInputOutcome {
    Answered,
    Cancelled(String),
    TimedOut,
}

/// Fixed question asked when the circuit suspends a coordinator turn.
const COORDINATOR_SECURITY_INPUT_QUESTION: &str = "A tool returned output that MOA classified as a possible prompt-injection attempt, \
     and that capability has been disabled for this turn. Reply to say how you would \
     like to proceed.";

/// Scores one classified coordinator tool output against the session's circuit.
///
/// The Session virtual object owns the read-score-write step, so two tool results
/// landing in the same turn cannot interleave into a lost update. When the step
/// crosses a stage boundary the workflow journals exactly one neutral transition
/// fact; the fact carries no output, only closed vocabulary.
#[allow(clippy::too_many_arguments)]
async fn apply_coordinator_security_assessment(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tenant_id: moa_core::types::identifiers::TenantId,
    turn_id: &str,
    generation: u64,
    result: &crate::tool_invocation::governed::GovernedInvocationResult,
    disabled_tools: &mut std::collections::BTreeSet<String>,
) -> Result<ToolCallDisposition, HandlerError> {
    if result.output.assessment.is_safe() {
        return Ok(ToolCallDisposition::Continue);
    }
    let owner = moa_core::types::security::SecurityCircuitOwner::Coordinator {
        turn_id: turn_id.to_string(),
        generation,
    };
    // Journaled BEFORE the circuit moves, not after. Both the Session fact and
    // the signed finding are stamped from this one value, so a replay reproduces
    // byte-identical audit output. Taking it after the apply returned would also
    // be deterministic, but a crash in that window would record an occurrence
    // time later than the moment the circuit actually advanced.
    let occurred_at = ctx
        .run(|| async move { Ok(Json::from(chrono::Utc::now())) })
        .name("prompt_injection_transition_timestamp")
        .await?
        .into_inner();
    let applied = crate::restate_identity::replay_safe_request(
        ctx.object_client::<crate::objects::session::SessionClient>(session_id.to_string())
            .apply_security_assessment(Json::from(
                moa_wire::turn::ApplySecurityAssessmentRequest {
                    owner,
                    allow_superseded_owner_noop: false,
                    capability: result.output.capability.clone(),
                    tool_call_id: result.tool_id,
                    assessment: result.output.assessment.clone(),
                },
            )),
    )
    .call()
    .await?
    .into_inner();

    if !applied.stage.permits_dispatch() {
        disabled_tools.insert(result.invocation.name.clone());
    }
    // Halting is the owner outcome, decided from the stage the circuit reached
    // rather than from whether this particular assessment moved it. A capability
    // that was already halted must not let a later tool call continue the turn.
    let disposition = match applied.stage {
        moa_core::types::security::SecurityCircuitStage::Halted => {
            ToolCallDisposition::SecurityHalt
        }
        moa_core::types::security::SecurityCircuitStage::SuspendedForInput => {
            ToolCallDisposition::SecurityNeedsInput
        }
        _ => ToolCallDisposition::Continue,
    };

    let Some(transition) = applied.transition else {
        return Ok(disposition);
    };
    // The transition key IS the dedupe key. It is a digest over the logical
    // transition coordinates, so a replayed or retried owner collapses onto the
    // one Session fact instead of appending a second copy of the same transition.
    let dedupe_key = transition.key.clone();
    crate::workflows::turn_events::append_session_event_with_dedupe_key(
        workflow.event_appender(),
        ctx,
        session_id,
        moa_core::events::Event::PromptInjectionCircuitTransition {
            transition: transition.clone(),
            signals: result.output.assessment.signals.clone(),
            redacted_spans: result.output.assessment.redacted_spans,
            deduplicated_carriers: result.output.assessment.deduplicated_carriers,
        },
        dedupe_key,
    )
    .await?;

    // Synchronous, and before the owner outcome: a halt must never take effect
    // with no audit record explaining why it happened.
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<crate::services::security_events::SecurityEventsClient>()
            .record_circuit_transition(Json::from(
                crate::services::security_events::RecordCircuitTransitionRequest {
                    tenant_id,
                    session_id,
                    transition,
                    signals: result.output.assessment.signals.clone(),
                    occurred_at,
                },
            )),
    )
    .call()
    .await?;

    Ok(disposition)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::types::completion::{CompletionRequest, ToolCallContent, ToolInvocation};
    use moa_core::types::tools::ToolOutput;
    use serde_json::json;

    use super::{
        DURABLE_UPGRADE_CONTROL_TOOL_NAME, configure_durable_upgrade_tool_schema,
        durable_upgrade_signal_from_control_call, durable_upgrade_tool_schema,
    };

    #[test]
    fn durable_upgrade_tool_schema_compiles_to_openai_strict_compatible() {
        // Pins: the durable-upgrade control tool schema, after provider
        // compilation, satisfies OpenAI strict mode (its accept-any-value
        // `evidence.value` property must gain an explicit type). A violation
        // here 400s every live coordinator turn that offers the tool —
        // scripted-provider lanes cannot catch it.
        let compiled = moa_providers::compile_for_openai_strict(&durable_upgrade_tool_schema());
        let violations = moa_providers::openai_strict_violations(&compiled["input_schema"]);
        assert!(
            violations.is_empty(),
            "durable-upgrade tool schema violates OpenAI strict mode: {violations:?}"
        );
    }

    fn control_call(input: serde_json::Value) -> ToolCallContent {
        ToolCallContent {
            invocation: ToolInvocation {
                id: Some("durable-upgrade".to_string()),
                name: DURABLE_UPGRADE_CONTROL_TOOL_NAME.to_string(),
                input,
            },
            provider_metadata: None,
        }
    }

    #[test]
    fn root_inline_control_schema_replaces_conflicts_and_is_removed_when_ineligible() {
        // Pins: configured external schemas cannot shadow the workflow-owned control name,
        // and turns without root-Inline authority never expose the control to the model.
        let mut request = CompletionRequest::new("investigate");
        request.tools.push(json!({
            "name": DURABLE_UPGRADE_CONTROL_TOOL_NAME,
            "description": "untrusted conflicting schema",
            "input_schema": {}
        }));
        configure_durable_upgrade_tool_schema(&mut request, true);
        let controls = request
            .tools
            .iter()
            .filter(|schema| {
                schema.get("name").and_then(serde_json::Value::as_str)
                    == Some(DURABLE_UPGRADE_CONTROL_TOOL_NAME)
            })
            .collect::<Vec<_>>();
        assert_eq!(controls.len(), 1);
        assert_ne!(
            controls[0]
                .get("description")
                .and_then(serde_json::Value::as_str),
            Some("untrusted conflicting schema")
        );

        configure_durable_upgrade_tool_schema(&mut request, false);
        assert!(request.tools.iter().all(|schema| {
            schema.get("name").and_then(serde_json::Value::as_str)
                != Some(DURABLE_UPGRADE_CONTROL_TOOL_NAME)
        }));
    }

    #[test]
    fn workflow_control_builds_byte_exact_durable_upgrade_signal() {
        // Pins: only the workflow-owned control turns already-gathered structured evidence
        // into a signal, while the server supplies the byte-identical root objective.
        let objective = "Inspect the affected tenant accounts";
        let discovered = json!({"account_count": 420, "batch_key": "tenant:acme"});
        let call = control_call(json!({
            "rationale": "The discovered account workflow must continue durably.",
            "evidence": [{
                "source": "tool:tenant_inventory",
                "summary": "inventory contains 420 independently processable accounts",
                "value": discovered
            }]
        }));
        let signal = durable_upgrade_signal_from_control_call(objective, true, None, &[&call])
            .expect("valid workflow control should parse")
            .expect("workflow control should produce a signal");
        assert_eq!(signal.objective, objective);
        assert_eq!(
            signal.rationale,
            "The discovered account workflow must continue durably."
        );
        assert_eq!(signal.evidence.len(), 1);
        assert_eq!(signal.evidence[0].source, "tool:tenant_inventory");
        assert_eq!(
            signal.evidence[0].summary,
            "inventory contains 420 independently processable accounts"
        );
        assert_eq!(signal.evidence[0].value, discovered);
    }

    #[test]
    fn arbitrary_tool_calls_cannot_masquerade_as_durable_control() {
        // Pins: a built-in, MCP, cached, or fixture tool cannot trigger control flow merely by
        // returning or accepting an `execution_shape`-like object.
        let external = ToolCallContent {
            invocation: ToolInvocation {
                id: Some("external".to_string()),
                name: "tenant_inventory".to_string(),
                input: json!({"execution_shape": {"strategy": "durable"}}),
            },
            provider_metadata: None,
        };
        assert_eq!(
            durable_upgrade_signal_from_control_call("investigate", true, None, &[&external]),
            Ok(None)
        );
    }

    #[test]
    fn durable_control_rejects_malformed_mixed_and_unauthorized_calls() {
        // Pins: the control is strict, root-Inline-only, and cannot be mixed with side effects.
        let malformed = control_call(json!({"rationale": "Needs durability."}));
        assert!(
            durable_upgrade_signal_from_control_call("investigate", true, None, &[&malformed])
                .is_err()
        );
        let valid = control_call(json!({
            "rationale": "The remaining work needs durable execution.",
            "evidence": [{"source": "tool:probe", "summary": "500 items", "value": {"count": 500}}]
        }));
        assert!(
            durable_upgrade_signal_from_control_call("investigate", false, None, &[&valid])
                .is_err()
        );
        let external = ToolCallContent {
            invocation: ToolInvocation {
                id: Some("external".to_string()),
                name: "external_write".to_string(),
                input: json!({}),
            },
            provider_metadata: None,
        };
        assert!(
            durable_upgrade_signal_from_control_call(
                "investigate",
                true,
                None,
                &[&valid, &external],
            )
            .is_err()
        );
    }

    #[test]
    fn durable_control_blocks_canaries_and_sanitizes_invalid_input() {
        // Pins: the workflow-owned control receives the same canary protection as governed
        // tools, and malformed model input never persists attacker-controlled values in errors.
        let active_canary = "moa_canary_active_secret";
        for leaked in [active_canary, "moa_canary_unrelated_marker"] {
            let call = control_call(json!({
                "rationale": format!("Leaked marker: {leaked}"),
                "evidence": [{"source": "tool:probe", "summary": "500 items", "value": {"count": 500}}]
            }));
            let error = durable_upgrade_signal_from_control_call(
                "investigate",
                true,
                Some(active_canary),
                &[&call],
            )
            .expect_err("protected canary leakage must block the control");
            assert_eq!(
                error,
                "Tool request_durable_execution blocked because it leaked a protected canary token."
            );
            assert!(!error.contains(leaked));
        }

        let secret = "customer-secret-must-not-enter-history";
        let malformed = control_call(json!({
            "rationale": "The work needs durability.",
            "evidence": secret
        }));
        let error =
            durable_upgrade_signal_from_control_call("investigate", true, None, &[&malformed])
                .expect_err("malformed input must be rejected");
        assert_eq!(error, "invalid Durable-upgrade control input");
        assert!(!error.contains(secret));
    }

    use super::{ESCALATED_SERVE_THRESHOLD, FileReadTurnCache};

    fn file_read(path: &str) -> ToolInvocation {
        ToolInvocation {
            id: Some("call-1".to_string()),
            name: "file_read".to_string(),
            input: json!({ "path": path }),
        }
    }

    #[test]
    fn second_identical_file_read_is_served_as_a_notice_only_reference() {
        // Pins: after one successful skill read this turn, an identical repeat is served
        // from memory as a corrective notice that names the file and does NOT repeat the
        // body (the content is already in context from the first read).
        let path = ".moa/skills/memory-privacy-check/SKILL.md";
        let mut cache = FileReadTurnCache::default();
        let call = file_read(path);

        assert!(
            !cache.will_serve(&call),
            "first read must not be cache-served"
        );
        assert!(cache.serve_repeat(&call).is_none());
        cache.remember(&call, &ToolOutput::text("SKILL BODY", Duration::ZERO));

        assert!(cache.will_serve(&call), "a remembered read is cache-served");
        let served = cache.serve_repeat(&call).expect("repeat served from cache");
        let text = served.to_text();
        assert!(
            text.starts_with("(cached:"),
            "served text must lead with the cached notice: {text}"
        );
        assert!(
            text.contains(path),
            "the notice names the file path: {text}"
        );
        assert!(
            !text.contains("SKILL BODY"),
            "the file body must not be repeated on a cache serve: {text}"
        );
        assert!(!served.is_error, "a cached read is not an error");
    }

    #[test]
    fn repeated_serves_escalate_the_notice_to_stop_without_repeating_the_body() {
        // Pins: once the same file has been re-served ESCALATED_SERVE_THRESHOLD times, the
        // notice hardens from the soft "(cached: ...)" hint to a STOP with the serve count;
        // neither form repeats the file body.
        let mut cache = FileReadTurnCache::default();
        let call = file_read(".moa/skills/x/SKILL.md");
        cache.remember(&call, &ToolOutput::text("BODY", Duration::ZERO));

        for serve in 1..ESCALATED_SERVE_THRESHOLD {
            let text = cache.serve_repeat(&call).expect("served").to_text();
            assert!(
                text.starts_with("(cached:"),
                "serve {serve} should use the soft notice: {text}"
            );
            assert!(
                !text.contains("BODY"),
                "serve {serve} must omit the body: {text}"
            );
        }
        let escalated = cache.serve_repeat(&call).expect("served").to_text();
        assert!(
            escalated.starts_with("STOP:"),
            "the escalated serve must lead with STOP: {escalated}"
        );
        assert!(
            escalated.contains(&format!("{ESCALATED_SERVE_THRESHOLD} times")),
            "the escalated notice names the serve count: {escalated}"
        );
        assert!(
            !escalated.contains("BODY"),
            "the escalated serve must omit the body: {escalated}"
        );
    }

    #[test]
    fn a_different_path_read_is_not_served_from_cache() {
        // Pins: caching is keyed on the exact input, so an unrelated read still executes.
        let mut cache = FileReadTurnCache::default();
        cache.remember(
            &file_read(".moa/skills/a/SKILL.md"),
            &ToolOutput::text("A", Duration::ZERO),
        );

        assert!(!cache.will_serve(&file_read(".moa/skills/b/SKILL.md")));
        assert!(
            cache
                .serve_repeat(&file_read(".moa/skills/b/SKILL.md"))
                .is_none()
        );
    }

    #[test]
    fn cache_key_ignores_object_key_order() {
        // Pins: equivalent reads that differ only in JSON key order share a cache entry.
        let mut cache = FileReadTurnCache::default();
        let stored = ToolInvocation {
            id: Some("1".to_string()),
            name: "file_read".to_string(),
            input: json!({ "path": ".moa/skills/x/SKILL.md", "start_line": 1 }),
        };
        let reordered = ToolInvocation {
            id: Some("2".to_string()),
            name: "file_read".to_string(),
            input: json!({ "start_line": 1, "path": ".moa/skills/x/SKILL.md" }),
        };
        cache.remember(&stored, &ToolOutput::text("BODY", Duration::ZERO));

        assert!(cache.will_serve(&reordered));
        assert!(cache.serve_repeat(&reordered).is_some());
    }

    #[test]
    fn non_file_read_tool_is_never_cached() {
        // Pins: only file_read is memoized; other tools always dispatch.
        let mut cache = FileReadTurnCache::default();
        let bash = ToolInvocation {
            id: Some("1".to_string()),
            name: "bash".to_string(),
            input: json!({ "cmd": "ls" }),
        };
        cache.remember(&bash, &ToolOutput::text("out", Duration::ZERO));

        assert!(!cache.will_serve(&bash));
        assert!(cache.serve_repeat(&bash).is_none());
    }

    #[test]
    fn a_non_skill_package_path_read_is_never_cached() {
        // Pins: the cache only ever holds immutable skill-package files. A successful read
        // of any path outside the `.moa/skills/` mount is not memoized, so if a future root
        // tool could read a mutable file, the cache could never serve its stale body.
        let mut cache = FileReadTurnCache::default();
        for path in [
            "src/lib.rs",
            "README.md",
            "notes/skills/plan.md",
            ".moa/skill.md",
        ] {
            let call = file_read(path);
            cache.remember(&call, &ToolOutput::text("MUTABLE BODY", Duration::ZERO));
            assert!(
                !cache.will_serve(&call),
                "non-skill path {path} must not be cache-served"
            );
            assert!(
                cache.serve_repeat(&call).is_none(),
                "non-skill path {path} must never be served from cache"
            );
        }
    }

    #[test]
    fn error_output_is_not_remembered() {
        // Pins: a failed read is never served from cache, so a miss-path retry still
        // dispatches (and can trip genuine loop detection, since only successes are cached).
        let mut cache = FileReadTurnCache::default();
        let call = file_read(".moa/skills/x/SKILL.md");
        cache.remember(&call, &ToolOutput::error("boom", Duration::ZERO));

        assert!(!cache.will_serve(&call));
        assert!(cache.serve_repeat(&call).is_none());
    }

    #[test]
    fn a_fresh_cache_serves_nothing() {
        // Pins: the cache is per-turn — each turn builds a fresh FileReadTurnCache, so an
        // identical read in a later turn is not served from a prior turn's memory.
        let mut cache = FileReadTurnCache::default();
        assert!(!cache.will_serve(&file_read(".moa/skills/x/SKILL.md")));
        assert!(
            cache
                .serve_repeat(&file_read(".moa/skills/x/SKILL.md"))
                .is_none()
        );
    }
}
