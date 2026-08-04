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
    state::{ExecutionProjection, ExecutionTaskId},
    wire::ExecutionPlanningContextSnapshot,
};
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable execution-planner prompt identifier.
pub const EXECUTION_PLANNER_PROMPT_VERSION: &str = "execution-planner-v3";
/// Fixed maximum collected planner output tokens.
pub const EXECUTION_PLANNER_MAX_OUTPUT_TOKENS: usize = 32_768;
const EXECUTION_PLANNER_PROMPT: &str = include_str!("../prompts/execution_planner.txt");

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
    /// Current durable run projection and completed structured outputs.
    pub projection: ExecutionProjection,
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
    schema_version: u8,
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
    build_initial_request(request, None)
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
        Some((
            original_candidate_json,
            immutable_goal_json,
            compiler_report_json,
        )),
    )
}

/// Builds one initial or repair planner request sharing the same cacheable system prompt.
///
/// The static planner instructions stay in the leading `system` message so provider
/// prompt caching can reuse them; the per-turn frozen context and the optional repair
/// evidence travel together in one `user` message, keeping the request at exactly one
/// system and one user turn.
fn build_initial_request(
    request: &ExecutionPlanningRequest,
    repair: Option<(&str, &str, &str)>,
) -> Result<CompletionRequest, serde_json::Error> {
    let frozen = FrozenInitialPrompt {
        schema_version: 1,
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
    if let Some((original_candidate_json, immutable_goal_json, compiler_report_json)) = repair {
        user_payload.push_str(&format!(
            "\nRepair the candidate exactly once. Preserve immutable_goal byte-for-byte after canonicalization. Do not discover new authority or capabilities.\n<original_candidate>{original_candidate_json}</original_candidate>\n<immutable_goal>{immutable_goal_json}</immutable_goal>\n<compiler_report>{compiler_report_json}</compiler_report>"
        ));
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
    #[derive(Serialize)]
    struct FrozenAmendmentPrompt<'a> {
        schema_version: u8,
        run_uid: uuid::Uuid,
        base_plan_revision: u64,
        goal: &'a moa_artifacts::execution_plan::ExecutionGoalContract,
        evidence: &'a AmendmentPlanningEvidence,
        catalog: &'a moa_execution::ExecutionCapabilityCatalog,
        authorization: &'a moa_execution::ExecutionAuthorizationEnvelope,
        remaining_budget: &'a moa_artifacts::execution_plan::ExecutionBudgetLimit,
    }
    let frozen = FrozenAmendmentPrompt {
        schema_version: 1,
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
    if let Some((candidate, report)) = repair {
        user_payload.push_str(&format!(
            "\nRepair exactly once without new discovery.\n<original_amendment>{candidate}</original_amendment>\n<compiler_report>{report}</compiler_report>"
        ));
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
    fn execution_planner_prompt_v3_pins_compiler_invariants() {
        // Pins: the current emitted prompt version and its compiler-facing guidance
        // change together so live planner provenance identifies this exact contract.
        assert_eq!(EXECUTION_PLANNER_PROMPT_VERSION, "execution-planner-v3");
        assert_eq!(
            EXECUTION_PLANNER_PROMPT,
            concat!(
                "You are MOA's execution-plan compiler frontend. Return exactly the requested strict JSON schema.\n\n",
                "Preserve the user's objective, scope, definitions, time range, universe, output form, evidence expectations, exclusions, and constraints as individually identifiable goal entries. Use only capabilities, skills, node kinds, and authority in the frozen context. Never invent a capability or permission. Produce an acyclic plan with explicit requirement coverage and completion checks. If the frozen contract cannot support the request, encode the gap in the strict candidate so deterministic compilation rejects it; do not answer the user directly.\n\n",
                "Compiler invariants:\n",
                "- Set `goal.objective` to the frozen `objective` byte-for-byte.\n",
                "- Every goal-entry ID, completion-check ID, execution-node ID, and every ID referenced from those structures must match `[a-z][a-z0-9_-]{0,63}`.\n",
                "- Link every requirement and every constraint to at least one completion check via `requirement_ids` and `constraint_ids`.\n",
                "- Put every goal requirement ID in at least one completion check's `requirement_ids`. If the plan has only one completion check, it must list every requirement ID. For a simple `Agent`-to-`Output` plan, prefer one `OutputSchema` check listing all requirement IDs.\n",
                "- Use only whole-value execution references: exactly `$.input` or `$.nodes.<id>.output`; never append field paths.\n",
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
