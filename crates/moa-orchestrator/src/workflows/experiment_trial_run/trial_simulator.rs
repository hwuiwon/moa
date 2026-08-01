//! Simulator prompt and model-call helpers for behavior-lab trial workflows.

use super::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct SimulatorContext {
    pub(super) prompt: String,
    pub(super) max_turns: u32,
}

pub(super) async fn load_simulator_context(
    ctx: &WorkflowContext<'_>,
    trial: ExperimentTrialRecord,
    pool: &sqlx::PgPool,
) -> Result<SimulatorContext, HandlerError> {
    let pool = pool.clone();
    let scope = trial.scope;
    Ok(ctx
        .run(|| async move {
            build_simulator_context(pool, scope, trial)
                .await
                .map(Json::from)
        })
        .name("experiment_trial_load_plan")
        .await?
        .into_inner())
}

async fn build_simulator_context(
    pool: sqlx::PgPool,
    scope: ActionRuleScope,
    trial: ExperimentTrialRecord,
) -> Result<SimulatorContext, HandlerError> {
    let registry = ArtifactRegistry::new(pool.clone());
    let plan_revision = registry
        .load_revision(&scope, trial.plan_revision_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| artifact_revision_not_found(trial.plan_revision_uid))?;
    if plan_revision.kind != ArtifactKind::ExperimentPlan {
        return Err(bad_request(format!(
            "artifact revision {} has kind {}, expected experiment_plan",
            trial.plan_revision_uid, plan_revision.kind
        )));
    }
    if plan_revision.status != ArtifactStatus::Published {
        return Err(bad_request(format!(
            "experiment plan revision {} must be published before trial execution",
            trial.plan_revision_uid
        )));
    }
    for revision_uid in &trial.artifact_revision_uids {
        let _ = registry
            .load_revision(&scope, *revision_uid)
            .await
            .map_err(moa_error_to_handler_error)?
            .ok_or_else(|| artifact_revision_not_found(*revision_uid))?;
    }
    let ArtifactDefinition::ExperimentPlan(definition) = &plan_revision.document.definition else {
        return Err(bad_request(
            "plan revision must contain an experiment_plan definition",
        ));
    };
    if definition.simulator_policy != trial.simulator.policy.reference() {
        return Err(bad_request(
            "persisted simulator policy does not match the pinned experiment plan",
        ));
    }
    let current_policy = SimulatorPolicyStore::new(pool)
        .resolve_policy(
            trial.scope.tenant_id(),
            definition.simulator_policy,
            Utc::now(),
        )
        .await
        .map_err(|error| bad_request(error.to_string()))?;
    if current_policy != trial.simulator.policy {
        return Err(bad_request(
            "persisted simulator policy snapshot no longer matches its certified registry row",
        ));
    }
    if trial.seed.is_some() != trial.simulator.policy.components.decoding.seeded {
        return Err(bad_request(
            "persisted simulator seed does not match the certified decoding policy",
        ));
    }
    let selection = select_simulation(
        definition,
        trial.scenario_id.as_deref(),
        trial.persona_id.as_deref(),
        trial.profile_id.as_deref(),
        &trial.data_bundle_ids,
    )
    .map_err(plan_expansion_error_to_handler_error)?;

    let max_turns = effective_max_turns(trial.simulator.max_turns, selection.scenario.max_turns);
    Ok(SimulatorContext {
        prompt: simulator_context_prompt(&selection, trial.seed.as_deref())?,
        max_turns,
    })
}

fn simulator_context_prompt(
    selection: &PlanSimulationSelection,
    seed: Option<&str>,
) -> Result<String, HandlerError> {
    let payload = serde_json::json!({
        "deterministic_seed": seed,
        "persona": selection.persona,
        "profile": selection.profile,
        "scenario": selection.scenario,
        "data_bundles": selection.data_bundles,
    });
    let bytes = moa_artifacts::canonical::canonical_json_bytes(&payload).map_err(|error| {
        TerminalError::new(format!(
            "serialize canonical simulator context failed: {error}"
        ))
    })?;
    String::from_utf8(bytes).map_err(|error| {
        TerminalError::new(format!("canonical simulator context is not UTF-8: {error}")).into()
    })
}

/// One simulator model call: the message it produced and what it actually cost.
///
/// The usage travels with the message because the run ledger reconciles against
/// it. Emitting these numbers as telemetry and dropping them, as this path used
/// to, leaves the simulator's spend unaccounted for in the envelope that is
/// supposed to bound it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SimulatorTurn {
    /// Typed decision emitted under the certified response protocol.
    pub(super) decision: SimulatorDecision,
    /// Next simulated user message, trimmed; empty for terminal decisions.
    pub(super) message: String,
    /// Bounded audit reason returned by the simulator.
    pub(super) reason: String,
    /// Actual tokens and cost the call consumed.
    pub(super) usage: ExperimentResourceUsage,
}

/// Builds the deterministic completion request for one simulator turn.
pub(super) fn simulator_completion_request(
    trial: &ExperimentTrialRecord,
    simulator_context: &SimulatorContext,
    transcript: &[ContextMessage],
    turn_index: u32,
) -> CompletionRequest {
    let instruction = format!(
        "Produce simulator decision number {} using the required response schema.",
        turn_index + 1
    );
    let mut request = CompletionRequest::new("");
    let components = &trial.simulator.policy.components;
    request.model = Some(components.model.clone());
    request.temperature = Some(components.decoding.temperature());
    request.max_output_tokens =
        Some(usize::try_from(components.decoding.max_output_tokens).unwrap_or(usize::MAX));
    request.response_format = Some(JsonResponseFormat::strict_json_schema(
        "behavior_lab_simulator_turn",
        "A typed simulated-user decision for one Behavior Lab turn.",
        simulator_response_schema(),
    ));
    request.tools = Vec::new();
    request.messages = vec![
        ContextMessage::system(components.system_prompt.clone()),
        ContextMessage::user(simulator_context.prompt.clone()),
    ];
    request.messages.extend(transcript.iter().cloned());
    request.messages.push(ContextMessage::user(instruction));
    request
}

/// Converts one durable gateway response into a metered simulator turn.
pub(super) fn simulator_turn_from_response(
    response: CompletionResponse,
) -> Result<SimulatorTurn, (ExperimentResourceUsage, HandlerError)> {
    let usage = response.token_usage();
    let input_tokens = usage.total_input_tokens() as u64;
    let output_tokens = usage.output_tokens as u64;
    let cost_cents = compute_cost_cents(response.model.as_str(), usage) as u64;
    record_simulation_tokens("simulator", input_tokens + output_tokens);
    record_simulation_cost_cents("simulator", cost_cents);
    let usage = ExperimentResourceUsage::model_call(
        input_tokens,
        output_tokens,
        cost_cents.saturating_mul(MICRO_USD_PER_CENT),
    );
    let parsed = parse_simulator_response(&response.text).map_err(|error| {
        (
            usage,
            TerminalError::new(format!(
                "simulator violated its certified response protocol: {error}"
            ))
            .into(),
        )
    })?;
    Ok(SimulatorTurn {
        decision: parsed.decision,
        message: parsed.message,
        reason: parsed.reason,
        usage,
    })
}

pub(super) fn effective_max_turns(simulator_max_turns: u32, scenario_max_turns: u32) -> u32 {
    let scenario_max_turns = (scenario_max_turns > 0).then_some(scenario_max_turns);
    scenario_max_turns
        .map(|max_turns| max_turns.min(simulator_max_turns.max(1)))
        .unwrap_or_else(|| simulator_max_turns.max(1))
}
