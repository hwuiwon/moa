//! Frozen planner inputs and schema-guided provider request construction.

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{GeneratedAmendmentCandidate, GeneratedExecutionCandidate};
use moa_config::ExecutionConfig;
use moa_core::types::{
    completion::{CompletionRequest, NativeWebSearchPolicy},
    context::ContextMessage,
    execution_planning::{DurableUpgradeSignal, ExecutionTemplateInvocation},
    identifiers::ModelId,
};
use moa_execution::{
    compiler::CanonicalExecutionPlan,
    state::{ExecutionAmendmentProjection, ExecutionTaskId},
    wire::ExecutionPlanningContextSnapshot,
};
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable execution-planner prompt identifier.
pub const EXECUTION_PLANNER_PROMPT_VERSION: &str = "execution-planner-v8";
/// Fixed maximum collected planner output tokens.
pub const EXECUTION_PLANNER_MAX_OUTPUT_TOKENS: usize = 32_768;
const EXECUTION_PLANNER_PROMPT: &str = include_str!("../prompts/execution_planner.md");

/// Immutable inputs for initial plan generation or exact template instantiation.
#[derive(Clone, Debug)]
pub struct ExecutionPlanningRequest {
    /// Byte-identical persisted user-message text.
    pub objective: String,
    /// Immutable session-derived planning authority and capability snapshot.
    pub context: ExecutionPlanningContextSnapshot,
    /// Explicit exact template invocation, when present.
    pub execution_template: Option<ExecutionTemplateInvocation>,
    /// Bounded evidence from one Inline-to-Durable upgrade.
    pub durable_upgrade: Option<DurableUpgradeSignal>,
    /// Auxiliary provider model selected by server configuration.
    pub planner_model: ModelId,
    /// Tenant-independent compiler and estimate settings.
    pub config: ExecutionConfig,
    /// Journaled planning/compile time.
    pub now: DateTime<Utc>,
}

/// Frozen evidence supplied to one amendment planner operation.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentPlanningEvidence {
    /// Immutable original run goal.
    pub goal: moa_artifacts::execution_plan::ExecutionGoalContract,
    /// Active immutable plan snapshot.
    pub active_plan: CanonicalExecutionPlan,
    /// Compiler-bounded aggregate node state and exact replan origin.
    pub projection: ExecutionAmendmentProjection,
    /// Structured failure evidence that caused WaitingReplan.
    pub failure_evidence: Value,
    /// Exact originating waiting task.
    pub waiting_task: ExecutionTaskId,
}

/// Immutable inputs for one revision-fenced amendment generation operation.
#[derive(Clone, Debug)]
pub struct ExecutionAmendmentPlanningRequest {
    /// Run whose plan is waiting for amendment.
    pub run_uid: uuid::Uuid,
    /// Active base plan revision.
    pub base_plan_revision: u64,
    /// Persisted original planning context, optionally narrowed for live revocation.
    pub context: ExecutionPlanningContextSnapshot,
    /// Immutable goal and run evidence.
    pub evidence: AmendmentPlanningEvidence,
    /// Resource envelope remaining before amendment acceptance.
    pub remaining_budget: moa_artifacts::execution_plan::ExecutionBudgetLimit,
    /// Auxiliary provider model.
    pub planner_model: ModelId,
    /// Compiler settings.
    pub config: ExecutionConfig,
    /// Journaled planner time.
    pub now: DateTime<Utc>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct FrozenInitialPrompt<'a> {
    objective: &'a str,
    catalog: &'a moa_execution::ExecutionCapabilityCatalog,
    authorization: &'a moa_execution::ExecutionAuthorizationEnvelope,
    pinned_instruction_skills: &'a [moa_execution::wire::PinnedInstructionSkill],
    execution_templates: &'a [moa_execution::wire::PinnedExecutionTemplate],
    budget: &'a moa_artifacts::execution_plan::ExecutionBudgetLimit,
    durable_upgrade: Option<&'a DurableUpgradeSignal>,
}

/// Constructs the exact no-tools, no-native-search strict initial planner request.
pub fn initial_completion_request(
    request: &ExecutionPlanningRequest,
) -> Result<CompletionRequest, serde_json::Error> {
    build_initial_request(request, InitialRepair::None)
}

/// Constructs the sole schema-regeneration request after an invalid initial response.
///
/// The malformed response is deliberately omitted so raw provider output never becomes
/// part of a second request or a persisted audit surface.
pub fn initial_schema_repair_completion_request(
    request: &ExecutionPlanningRequest,
) -> Result<CompletionRequest, serde_json::Error> {
    build_initial_request(request, InitialRepair::Schema)
}

/// Constructs the sole permitted initial-plan repair request from frozen first-call inputs.
pub fn initial_repair_completion_request(
    request: &ExecutionPlanningRequest,
    original_candidate_json: &str,
    immutable_goal_json: &str,
    compiler_report_json: &str,
) -> Result<CompletionRequest, serde_json::Error> {
    build_initial_request(
        request,
        InitialRepair::Compiler {
            original_candidate_json,
            immutable_goal_json,
            compiler_report_json,
        },
    )
}

enum InitialRepair<'a> {
    None,
    Schema,
    Compiler {
        original_candidate_json: &'a str,
        immutable_goal_json: &'a str,
        compiler_report_json: &'a str,
    },
}

/// Builds one initial or repair planner request sharing the same cacheable system prompt.
///
/// The static planner instructions stay in the leading `system` message so provider
/// prompt caching can reuse them; the per-turn frozen context and the optional repair
/// evidence travel together in one `user` message, keeping the request at exactly one
/// system and one user turn.
fn build_initial_request(
    request: &ExecutionPlanningRequest,
    repair: InitialRepair<'_>,
) -> Result<CompletionRequest, serde_json::Error> {
    let frozen = FrozenInitialPrompt {
        objective: &request.objective,
        catalog: &request.context.catalog,
        authorization: &request.context.authorization,
        pinned_instruction_skills: &request.context.pinned_instruction_skills,
        execution_templates: &request.context.execution_templates,
        budget: &request.context.budget,
        durable_upgrade: request.durable_upgrade.as_ref(),
    };
    let mut user_payload = format!(
        "<frozen_planning_context>{}</frozen_planning_context>",
        serde_json::to_string(&frozen)?
    );
    match repair {
        InitialRepair::None => {}
        InitialRepair::Schema => user_payload.push_str(
            "\nThe prior response failed strict schema validation. Regenerate the candidate exactly once from the frozen context and canonical response schema. Return only the replacement JSON object. Do not quote or reproduce the prior response.",
        ),
        InitialRepair::Compiler {
            original_candidate_json,
            immutable_goal_json,
            compiler_report_json,
        } => {
            user_payload.push_str(&format!(
                "\nRepair the candidate exactly once. Preserve immutable_goal byte-for-byte after canonicalization. Do not discover new authority or capabilities.\n<original_candidate>{original_candidate_json}</original_candidate>\n<immutable_goal>{immutable_goal_json}</immutable_goal>\n<compiler_report>{compiler_report_json}</compiler_report>"
            ));
        }
    }
    planner_request::<GeneratedExecutionCandidate>(
        request.planner_model.clone(),
        EXECUTION_PLANNER_PROMPT.to_string(),
        user_payload,
    )
}

/// Constructs a strict amendment request over persisted, revision-fenced run state.
pub fn amendment_completion_request(
    request: &ExecutionAmendmentPlanningRequest,
    repair: Option<(&str, &str)>,
) -> Result<CompletionRequest, serde_json::Error> {
    let repair = match repair {
        Some((candidate, report)) => AmendmentRepair::Compiler { candidate, report },
        None => AmendmentRepair::None,
    };
    build_amendment_request(request, repair)
}

/// Constructs the sole schema-regeneration request after an invalid amendment response.
///
/// The malformed response is deliberately omitted so raw provider output never becomes
/// part of a second request or a persisted audit surface.
pub fn amendment_schema_repair_completion_request(
    request: &ExecutionAmendmentPlanningRequest,
) -> Result<CompletionRequest, serde_json::Error> {
    build_amendment_request(request, AmendmentRepair::Schema)
}

enum AmendmentRepair<'a> {
    None,
    Schema,
    Compiler { candidate: &'a str, report: &'a str },
}

fn build_amendment_request(
    request: &ExecutionAmendmentPlanningRequest,
    repair: AmendmentRepair<'_>,
) -> Result<CompletionRequest, serde_json::Error> {
    #[derive(Serialize)]
    struct FrozenAmendmentPrompt<'a> {
        run_uid: uuid::Uuid,
        base_plan_revision: u64,
        goal: &'a moa_artifacts::execution_plan::ExecutionGoalContract,
        evidence: &'a AmendmentPlanningEvidence,
        catalog: &'a moa_execution::ExecutionCapabilityCatalog,
        authorization: &'a moa_execution::ExecutionAuthorizationEnvelope,
        remaining_budget: &'a moa_artifacts::execution_plan::ExecutionBudgetLimit,
    }
    let frozen = FrozenAmendmentPrompt {
        run_uid: request.run_uid,
        base_plan_revision: request.base_plan_revision,
        goal: &request.evidence.goal,
        evidence: &request.evidence,
        catalog: &request.context.catalog,
        authorization: &request.context.authorization,
        remaining_budget: &request.remaining_budget,
    };
    let system_prompt = format!(
        "{EXECUTION_PLANNER_PROMPT}\n\nGenerate only a restricted plan amendment. The goal is immutable."
    );
    let mut user_payload = format!(
        "<frozen_amendment_context>{}</frozen_amendment_context>",
        serde_json::to_string(&frozen)?
    );
    match repair {
        AmendmentRepair::None => {}
        AmendmentRepair::Schema => user_payload.push_str(
            "\nThe prior response failed strict schema validation. Regenerate the amendment exactly once from the frozen context and canonical response schema. Return only the replacement JSON object. Do not quote or reproduce the prior response.",
        ),
        AmendmentRepair::Compiler { candidate, report } => {
            user_payload.push_str(&format!(
                "\nRepair exactly once without new discovery.\n<original_amendment>{candidate}</original_amendment>\n<compiler_report>{report}</compiler_report>"
            ));
        }
    }
    planner_request::<GeneratedAmendmentCandidate>(
        request.planner_model.clone(),
        system_prompt,
        user_payload,
    )
}

/// Builds one no-tools planner request from a cacheable, schema-guided system
/// prompt and a per-turn user payload.
///
/// The static planner instructions live in the leading `system` message so provider
/// prompt caching can reuse them across turns. Planner candidates contain free-form
/// JSON values that provider-native strict schemas cannot represent faithfully, so the
/// canonical generated schema is prompt guidance rather than a provider response format.
/// The per-turn frozen context and objective travel in a `user` message. Every provider
/// adapter rejects a system-only request, so at least one non-system message is mandatory.
fn planner_request<T: schemars::JsonSchema>(
    model: ModelId,
    system_prompt: String,
    user_payload: String,
) -> Result<CompletionRequest, serde_json::Error> {
    let schema = serde_json::to_string(&schema_for!(T))?;
    let system_prompt = format!(
        "{system_prompt}\n\nReturn only one JSON object that conforms exactly to the canonical schema below. Do not include Markdown fences or explanatory text.\n<response_schema>{schema}</response_schema>"
    );
    Ok(CompletionRequest {
        model: Some(model),
        messages: vec![
            ContextMessage::system(system_prompt),
            ContextMessage::user(user_payload),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(EXECUTION_PLANNER_MAX_OUTPUT_TOKENS),
        temperature: None,
        response_format: None,
        native_web_search: NativeWebSearchPolicy::Disabled,
        metadata: std::collections::HashMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_planner_prompt_v8_pins_long_horizon_and_compiler_invariants() {
        // Pins: the current emitted prompt version and its compiler-facing guidance
        // change together so live planner provenance identifies this exact contract.
        assert_eq!(EXECUTION_PLANNER_PROMPT_VERSION, "execution-planner-v8");
        assert_eq!(
            EXECUTION_PLANNER_PROMPT,
            concat!(
                "You are MOA's execution-plan compiler frontend. Return exactly the requested strict JSON schema.\n\n",
                "Preserve the user's objective, scope, definitions, time range, universe, output form, evidence expectations, exclusions, and constraints as individually identifiable goal entries. Use only capabilities, skills, node kinds, and authority in the frozen context. Never invent a capability or permission. Produce an acyclic plan with explicit requirement coverage and completion checks. If the frozen contract cannot support the request, encode the gap in the strict candidate so deterministic compilation rejects it; do not answer the user directly.\n\n",
                "Compiler invariants:\n",
                "- Set `goal.objective` to the frozen `objective` byte-for-byte.\n",
                "- Choose exactly one explicit `plan.cancel_policy`: `retain_effects` or `compensate_committed`.\n",
                "- Treat the frozen `budget.deadline_at` as the absolute Durable-run deadline. Never emit a wait, retry window, or active task whose bound reaches or exceeds it.\n",
                "- Use `WaitUntil` for a calendar-time delay. Its `wake` is a tagged temporal target, either `{\"kind\":\"at\",\"at\":\"<RFC3339 UTC>\"}` for an exact instant or `{\"kind\":\"after\",\"delay_seconds\":<positive integer>}` for a delay measured from the moment the node starts waiting. Emit exactly those fields for the chosen shape and nothing else. The resolved wake time must land before the run deadline. Its declared `result` is the structured value made available to downstream nodes after the timer fires.\n",
                "- Give every `Review` and `WaitSignal` an explicit `wait_policy` of `{\"expiry\": <temporal target>, \"on_expiry\": <expiry action>}`, using the same two temporal-target shapes. An expiry action is either `{\"kind\":\"fail_task\"}` or `{\"kind\":\"continue_with\",\"output\":<value matching that node's output_schema>}`. Represent human and external waits only with these storage-backed wait operations; never keep an `Agent` or `Capability` active while waiting for a person, callback, schedule, or retry time.\n",
                "- Always set `plan.input_wait_policy`. It is required, and it governs every task that pauses for runtime input rather than one named node, so its `on_expiry` accepts only `{\"kind\":\"fail_task\"}` — never `continue_with`.\n",
                "- Decompose long work into bounded active tasks separated by durable nodes. Never plan a continuously running multi-hour or multi-day model call, tool call, shell process, network connection, or sandbox; use a registered asynchronous capability when the catalog explicitly provides one.\n",
                "- Set every node's `compensation` explicitly. Use `null` unless the node is a direct side-effecting `Capability` whose exact catalog entry advertises the same compensator and bounded input mapping, and that compensator has `requires_sandbox=false`. Never add compensation to reads, agents, maps, reduces, reviews, signals, or outputs, never use a sandbox-backed compensator, and never invent rollback authority.\n",
                "- An amendment must preserve compensation for work that is running or committed and must not weaken the run's cancellation policy.\n",
                "- Every goal-entry ID, completion-check ID, execution-node ID, and every ID referenced from those structures must match `[a-z][a-z0-9_-]{0,63}`.\n",
                "- Link every requirement and every constraint to at least one completion check via `requirement_ids` and `constraint_ids`.\n",
                "- Put every goal requirement ID in at least one completion check's `requirement_ids`. If the plan has only one completion check, it must list every requirement ID. For a simple `Agent`-to-`Output` plan, prefer one `OutputSchema` check listing all requirement IDs.\n",
                "- Use only whole-value binding objects of exactly `{\"$ref\":\"<path>\"}` with no sibling keys or string interpolation. A reference path may select the complete `$.input` or `$.nodes.<id>.output` value, or append dot-separated object fields such as `$.input.query` and `$.nodes.lookup.output.items`; node references may read only declared dependencies. Never use bracket/index syntax.\n",
                "- When an `output` operation forwards another node's output, set the output node's `input` to `{}` and put `{\"$ref\":\"$.nodes.<id>.output\"}` directly in `operation.value`.\n",
            )
        );
    }

    #[test]
    fn planner_requests_embed_production_schema_without_provider_format_or_tools() {
        // Pins: free-form planner values remain valid because provider-native schema
        // enforcement is disabled while the exact production schema stays in the
        // cacheable system prompt for both candidate envelopes.
        fn assert_request<T: schemars::JsonSchema>(system_prompt: &str) {
            let request = planner_request::<T>(
                ModelId::new("planner-model"),
                system_prompt.to_string(),
                "per-turn context".to_string(),
            )
            .expect("planner request should serialize the production schema");
            let schema =
                serde_json::to_string(&schema_for!(T)).expect("production schema should serialize");

            assert_eq!(request.messages.len(), 2);
            assert_eq!(
                request.messages[0],
                ContextMessage::system(format!(
                    "{system_prompt}\n\nReturn only one JSON object that conforms exactly to the canonical schema below. Do not include Markdown fences or explanatory text.\n<response_schema>{schema}</response_schema>"
                ))
            );
            assert_eq!(
                request.messages[1],
                ContextMessage::user("per-turn context")
            );
            assert!(request.response_format.is_none());
            assert!(request.tools.is_empty());
        }

        assert_request::<GeneratedExecutionCandidate>(EXECUTION_PLANNER_PROMPT);
        assert_request::<GeneratedAmendmentCandidate>(
            "amendment planner instructions remain cacheable",
        );
    }
}
