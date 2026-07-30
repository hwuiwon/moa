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
    let registry = ArtifactRegistry::new(pool);
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

const SIMULATOR_SYSTEM_PROMPT: &str = "\
You are the simulated user in a behavior-lab trial.
Return only the next user-visible message to send to the target agent. Do not call tools. Return \
DONE when the simulated user should stop.";

fn simulator_context_prompt(
    selection: &PlanSimulationSelection,
    seed: Option<&str>,
) -> Result<String, HandlerError> {
    let mut sections = Vec::new();
    if let Some(seed) = seed {
        sections.push(format!("Deterministic seed: {seed}"));
    }
    push_simulation_section(&mut sections, "Persona", &selection.persona)?;
    push_simulation_section(&mut sections, "Profile", &selection.profile)?;
    push_simulation_section(&mut sections, "Scenario", &selection.scenario)?;
    for (index, bundle) in selection.data_bundles.iter().enumerate() {
        push_simulation_section(&mut sections, &format!("Data bundle {}", index + 1), bundle)?;
    }
    Ok(sections.join("\n\n"))
}

fn push_simulation_section<T>(
    sections: &mut Vec<String>,
    label: &str,
    definition: &T,
) -> Result<(), HandlerError>
where
    T: Serialize,
{
    let definition = serde_json::to_string_pretty(definition).map_err(|error| {
        TerminalError::new(format!("serialize {label} simulation failed: {error}"))
    })?;
    sections.push(format!("{label} simulation:\n{definition}"));
    Ok(())
}

/// One simulator model call: the message it produced and what it actually cost.
///
/// The usage travels with the message because the run ledger reconciles against
/// it. Emitting these numbers as telemetry and dropping them, as this path used
/// to, leaves the simulator's spend unaccounted for in the envelope that is
/// supposed to bound it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SimulatorTurn {
    /// Next simulated user message, trimmed.
    pub(super) message: String,
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
    let mut request = CompletionRequest::new(format!(
        "Generate simulator user message number {}.",
        turn_index + 1
    ));
    request.model = Some(trial.simulator.model.clone());
    request.temperature = trial.simulator.temperature;
    request.max_output_tokens = Some(512);
    request.tools = Vec::new();
    request
        .messages
        .insert(0, ContextMessage::user(simulator_context.prompt.clone()));
    request
        .messages
        .insert(0, ContextMessage::system(SIMULATOR_SYSTEM_PROMPT));
    request.messages.extend(transcript.iter().cloned());
    request
}

/// Converts one durable gateway response into a metered simulator turn.
pub(super) fn simulator_turn_from_response(response: CompletionResponse) -> SimulatorTurn {
    let usage = response.token_usage();
    let input_tokens = usage.total_input_tokens() as u64;
    let output_tokens = usage.output_tokens as u64;
    let cost_cents = compute_cost_cents(response.model.as_str(), usage) as u64;
    record_simulation_tokens("simulator", input_tokens + output_tokens);
    record_simulation_cost_cents("simulator", cost_cents);
    SimulatorTurn {
        message: response.text.trim().to_string(),
        usage: ExperimentResourceUsage::model_call(
            input_tokens,
            output_tokens,
            cost_cents.saturating_mul(MICRO_USD_PER_CENT),
        ),
    }
}

pub(super) fn effective_max_turns(simulator_max_turns: u32, scenario_max_turns: u32) -> u32 {
    let scenario_max_turns = (scenario_max_turns > 0).then_some(scenario_max_turns);
    scenario_max_turns
        .map(|max_turns| max_turns.min(simulator_max_turns.max(1)))
        .unwrap_or_else(|| simulator_max_turns.max(1))
}

pub(super) fn simulator_done(message: &str) -> bool {
    let normalized = message.trim();
    normalized.is_empty()
        || normalized.eq_ignore_ascii_case("done")
        || normalized.eq_ignore_ascii_case("[done]")
}
