//! Pure helpers for expanding behavior-lab experiment plans.

use moa_artifacts::simulation::{
    ExperimentPlanDefinition, ExperimentTargetKind, ExperimentTargetVariant,
    SimulationDataBundleDefinition, SimulationPersonaDefinition, SimulationProfileDefinition,
    SimulationScenarioDefinition,
};
use moa_core::{types::agent::AgentSessionSelection, types::identifiers::ModelId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::model::{
    ExperimentScorecard, ExperimentSimulatorConfig, ExperimentTarget, ExperimentVariant,
    NewExperimentTrial,
};

/// Default target-agent turn cap for plan-expanded simulator trials.
pub const DEFAULT_PLAN_TRIAL_MAX_TURNS: u32 = 8;

/// Errors returned while projecting an experiment plan into executable inputs.
#[derive(Debug, Error)]
pub enum PlanExpansionError {
    /// A plan has no target variants.
    #[error("experiment plan must include at least one target variant")]
    MissingTargetVariant,
    /// A plan has no executable trial matrix.
    #[error("experiment plan must include at least one {dimension}")]
    MissingPlanDimension {
        /// Empty matrix dimension.
        dimension: &'static str,
    },
    /// Agent-loop variants require a target model.
    #[error("agent-loop experiment plans require target_model")]
    MissingTargetModel,
    /// A simulator temperature value cannot be represented safely.
    #[error("simulator_temperature must be a finite f32-compatible number")]
    InvalidSimulatorTemperature,
    /// Procedure variants require a procedure reference.
    #[error("procedure target variants require procedure_ref")]
    MissingProcedureRef,
    /// A target variant has an invalid agent selector.
    #[error("target variant agent selector is invalid: {message}")]
    InvalidAgentSelector {
        /// Validation error message.
        message: String,
    },
    /// A persisted trial row did not name the selected simulation block.
    #[error("experiment trial missing selected {field} id")]
    MissingSelectionId {
        /// Missing simulation selector field.
        field: &'static str,
    },
    /// A persisted trial row refers to an ID that is absent from the pinned plan.
    #[error("experiment trial selected {field} `{id}` that does not exist in the pinned plan")]
    UnknownSelectionId {
        /// Simulation selector field.
        field: &'static str,
        /// Missing ID.
        id: String,
    },
}

/// Run-level payloads derived from the first target variant in a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanRunProjection {
    /// Target payload used to admit the run-level experiment run.
    pub target: ExperimentTarget,
    /// Variant payload stored on the experiment run.
    pub variant: ExperimentVariant,
    /// Scorecard derived from the plan scorecard metadata.
    pub scorecard: ExperimentScorecard,
    /// Artifact revisions associated with the run.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Pinned plan revision used by the run.
    pub plan_revision_uid: Uuid,
}

/// One executable trial emitted by plan fanout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExpandedPlanTrial {
    /// Durable trial row input.
    pub trial: NewExperimentTrial,
    /// Target payload selected for this trial.
    pub target: ExperimentTarget,
    /// Variant payload selected for this trial.
    pub variant: ExperimentVariant,
}

/// Embedded simulation blocks selected for one trial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSimulationSelection {
    /// Scenario selected by the trial.
    pub scenario: SimulationScenarioDefinition,
    /// Persona selected by the trial.
    pub persona: SimulationPersonaDefinition,
    /// Profile selected by the trial.
    pub profile: SimulationProfileDefinition,
    /// Data bundles selected by the trial.
    #[serde(default)]
    pub data_bundles: Vec<SimulationDataBundleDefinition>,
}

/// Projects a published experiment plan into run-level inputs.
pub fn project_plan_run(
    definition: &ExperimentPlanDefinition,
    plan_revision_uid: Uuid,
    plan_name: &str,
    run_name: &str,
) -> Result<PlanRunProjection, PlanExpansionError> {
    let first_variant = definition
        .target_variants
        .first()
        .ok_or(PlanExpansionError::MissingTargetVariant)?;
    let target = target_for_plan_variant(definition, first_variant)?;
    let variant =
        variant_payload_for_plan(plan_revision_uid, definition, first_variant, |value| {
            json!({
                "plan_revision_uid": value,
                "plan_name": plan_name,
                "run_name": run_name,
                "parallelism": definition.parallelism,
            })
        })?;
    Ok(PlanRunProjection {
        target,
        variant,
        scorecard: ExperimentScorecard {
            score_names: Vec::new(),
            evaluator_metadata: definition.scorecard.clone(),
        },
        artifact_revision_uids: vec![plan_revision_uid],
        plan_revision_uid,
    })
}

/// Expands a plan into deterministic trial rows and target payloads.
pub fn expand_plan_trials(
    run_uid: Uuid,
    plan_revision_uid: Uuid,
    definition: &ExperimentPlanDefinition,
) -> Result<Vec<ExpandedPlanTrial>, PlanExpansionError> {
    require_plan_dimension("scenario", !definition.simulation.scenarios.is_empty())?;
    require_plan_dimension("persona", !definition.simulation.personas.is_empty())?;
    require_plan_dimension("profile", !definition.simulation.profiles.is_empty())?;
    require_plan_dimension("target variant", !definition.target_variants.is_empty())?;

    let mut trials = Vec::new();
    for (scenario_index, scenario) in definition.simulation.scenarios.iter().enumerate() {
        for (persona_index, persona) in definition.simulation.personas.iter().enumerate() {
            for (profile_index, profile) in definition.simulation.profiles.iter().enumerate() {
                for variant in &definition.target_variants {
                    let target = target_for_plan_variant(definition, variant)?;
                    let variant_payload = variant_payload_for_plan(
                        plan_revision_uid,
                        definition,
                        variant,
                        |value| {
                            json!({
                                "plan_revision_uid": value,
                                "variant_config": variant.config,
                            })
                        },
                    )?;
                    let data_bundle_ids = data_bundle_ids_for_scenario(definition, scenario);
                    for trial_index in 0..definition.trials_per_combination.max(1) {
                        let trial_key = stable_trial_key(
                            (scenario_index, &scenario.id),
                            (persona_index, &persona.id),
                            (profile_index, &profile.id),
                            &variant.key,
                            trial_index,
                        );
                        trials.push(ExpandedPlanTrial {
                            trial: NewExperimentTrial {
                                run_uid,
                                trial_key: trial_key.clone(),
                                target_kind: variant.kind,
                                variant_key: variant.key.clone(),
                                plan_revision_uid,
                                scenario_id: Some(scenario.id.clone()),
                                persona_id: Some(persona.id.clone()),
                                profile_id: Some(profile.id.clone()),
                                data_bundle_ids: data_bundle_ids.clone(),
                                artifact_revision_uids: Vec::new(),
                                simulator: ExperimentSimulatorConfig {
                                    model: ModelId::new(definition.simulator_model.clone()),
                                    temperature: simulator_temperature(variant)?,
                                    max_turns: DEFAULT_PLAN_TRIAL_MAX_TURNS,
                                    token_budget: definition.budget.max_trial_tokens,
                                    metadata: json!({}),
                                },
                                target_model: definition.target_model.as_ref().map(ModelId::new),
                                seed: Some(format!("{trial_key}:{plan_revision_uid}")),
                                score_run_id: deterministic_score_run_id(run_uid, &trial_key),
                            },
                            target: target.clone(),
                            variant: variant_payload.clone(),
                        });
                    }
                }
            }
        }
    }
    Ok(trials)
}

fn require_plan_dimension(
    dimension: &'static str,
    present: bool,
) -> Result<(), PlanExpansionError> {
    if present {
        Ok(())
    } else {
        Err(PlanExpansionError::MissingPlanDimension { dimension })
    }
}

fn simulator_temperature(
    variant: &ExperimentTargetVariant,
) -> Result<Option<f32>, PlanExpansionError> {
    let Some(value) = variant.config.get("simulator_temperature") else {
        return Ok(None);
    };
    let Some(value) = value.as_f64() else {
        return Ok(None);
    };
    if value.is_finite() && value <= f64::from(f32::MAX) && value >= f64::from(f32::MIN) {
        Ok(Some(value as f32))
    } else {
        Err(PlanExpansionError::InvalidSimulatorTemperature)
    }
}

fn deterministic_score_run_id(run_uid: Uuid, trial_key: &str) -> Uuid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa:experiment-trial-score-run:v1");
    hasher.update(run_uid.as_bytes());
    hasher.update(trial_key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// Selects embedded simulation blocks from a pinned plan for one stored trial row.
pub fn select_simulation(
    definition: &ExperimentPlanDefinition,
    scenario_id: Option<&str>,
    persona_id: Option<&str>,
    profile_id: Option<&str>,
    data_bundle_ids: &[String],
) -> Result<PlanSimulationSelection, PlanExpansionError> {
    let scenario_id = required_id("scenario", scenario_id)?;
    let persona_id = required_id("persona", persona_id)?;
    let profile_id = required_id("profile", profile_id)?;

    let scenario = find_by_id(&definition.simulation.scenarios, scenario_id, |value| {
        &value.id
    })
    .ok_or_else(|| unknown_id("scenario", scenario_id))?;
    let persona = find_by_id(&definition.simulation.personas, persona_id, |value| {
        &value.id
    })
    .ok_or_else(|| unknown_id("persona", persona_id))?;
    let profile = find_by_id(&definition.simulation.profiles, profile_id, |value| {
        &value.id
    })
    .ok_or_else(|| unknown_id("profile", profile_id))?;
    let data_bundles = data_bundle_ids
        .iter()
        .map(|id| {
            find_by_id(&definition.simulation.data_bundles, id, |value| &value.id)
                .cloned()
                .ok_or_else(|| unknown_id("data_bundle", id))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(PlanSimulationSelection {
        scenario: scenario.clone(),
        persona: persona.clone(),
        profile: profile.clone(),
        data_bundles,
    })
}

/// Projects one plan target variant into an executable target payload.
pub fn target_for_plan_variant(
    definition: &ExperimentPlanDefinition,
    variant: &ExperimentTargetVariant,
) -> Result<ExperimentTarget, PlanExpansionError> {
    match variant.kind {
        ExperimentTargetKind::AgentLoop => Ok(ExperimentTarget::AgentLoop {
            prompt: variant
                .config
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("Start behavior-lab simulation.")
                .to_string(),
            session_id: None,
            agent: agent_selection_for_variant(variant)?,
            model: definition
                .target_model
                .as_ref()
                .map(ModelId::new)
                .ok_or(PlanExpansionError::MissingTargetModel)?,
            attachments: Vec::new(),
        }),
        ExperimentTargetKind::Procedure => Ok(ExperimentTarget::Procedure {
            procedure_ref: variant
                .config
                .get("procedure_ref")
                .and_then(Value::as_str)
                .ok_or(PlanExpansionError::MissingProcedureRef)?
                .to_string(),
            input: variant
                .config
                .get("input")
                .cloned()
                .unwrap_or_else(|| json!({})),
            session_id: None,
            idempotency_key: None,
        }),
    }
}

fn agent_selection_for_variant(
    variant: &ExperimentTargetVariant,
) -> Result<Option<AgentSessionSelection>, PlanExpansionError> {
    if let Some(agent) = variant.config.get("agent") {
        if agent.is_null() {
            return Ok(None);
        }
        return serde_json::from_value(agent.clone())
            .map(Some)
            .map_err(|error| PlanExpansionError::InvalidAgentSelector {
                message: error.to_string(),
            });
    }

    let installation_uid = optional_uuid_config(&variant.config, "agent_installation_uid")?;
    let revision_uid = optional_uuid_config(&variant.config, "agent_revision_uid")?;
    if installation_uid.is_none() && revision_uid.is_none() {
        return Ok(None);
    }
    Ok(Some(AgentSessionSelection {
        installation_uid,
        revision_uid,
    }))
}

fn optional_uuid_config(config: &Value, key: &str) -> Result<Option<Uuid>, PlanExpansionError> {
    let Some(value) = config.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| PlanExpansionError::InvalidAgentSelector {
            message: format!("{key}: {error}"),
        })
}

fn variant_payload_for_plan(
    plan_revision_uid: Uuid,
    definition: &ExperimentPlanDefinition,
    variant: &ExperimentTargetVariant,
    metadata: impl FnOnce(Uuid) -> Value,
) -> Result<ExperimentVariant, PlanExpansionError> {
    Ok(ExperimentVariant {
        name: variant.key.clone(),
        model: definition.target_model.as_ref().map(ModelId::new),
        artifact_revision_uids: vec![plan_revision_uid],
        skill_refs: Vec::new(),
        procedure_ref: variant
            .config
            .get("procedure_ref")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        metadata: metadata(plan_revision_uid),
    })
}

fn data_bundle_ids_for_scenario(
    definition: &ExperimentPlanDefinition,
    scenario: &SimulationScenarioDefinition,
) -> Vec<String> {
    if scenario.data_bundle_ids.is_empty() {
        return definition
            .simulation
            .data_bundles
            .iter()
            .map(|bundle| bundle.id.clone())
            .collect();
    }
    definition
        .simulation
        .data_bundles
        .iter()
        .filter(|bundle| scenario.data_bundle_ids.contains(&bundle.id))
        .map(|bundle| bundle.id.clone())
        .collect()
}

fn stable_trial_key(
    scenario: (usize, &str),
    persona: (usize, &str),
    profile: (usize, &str),
    variant_key: &str,
    trial_index: u32,
) -> String {
    format!(
        "s{:02}-{}/p{:02}-{}/u{:02}-{}/v-{}/t{:03}",
        scenario.0 + 1,
        key_part(scenario.1),
        persona.0 + 1,
        key_part(persona.1),
        profile.0 + 1,
        key_part(profile.1),
        key_part(variant_key),
        trial_index + 1
    )
}

fn key_part(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "item".to_string()
    } else {
        trimmed.to_string()
    }
}

fn required_id<'a>(
    field: &'static str,
    id: Option<&'a str>,
) -> Result<&'a str, PlanExpansionError> {
    id.filter(|value| !value.trim().is_empty())
        .ok_or(PlanExpansionError::MissingSelectionId { field })
}

fn find_by_id<'a, T>(values: &'a [T], id: &str, id_of: impl Fn(&T) -> &str) -> Option<&'a T> {
    values.iter().find(|value| id_of(value) == id)
}

fn unknown_id(field: &'static str, id: impl Into<String>) -> PlanExpansionError {
    PlanExpansionError::UnknownSelectionId {
        field,
        id: id.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_artifacts::simulation::{
        ExperimentBudget, ExperimentLearningProposalSettings, ExperimentSimulationDefinition,
        SimulationDataSource, SimulationDataSourceKind,
    };

    #[test]
    fn expand_plan_trials_uses_ids_without_copying_simulation_blocks_offline() {
        // Pins: plan fanout stores plan-local IDs and leaves simulator metadata semantic-free.
        let plan_revision_uid = fixture_uuid(1);
        let definition = fixture_plan();
        let trials = expand_plan_trials(fixture_uuid(2), plan_revision_uid, &definition)
            .expect("valid plan matrix expands");

        let keys = trials
            .iter()
            .map(|trial| trial.trial.trial_key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "s01-damaged-food/p01-careful-customer/u01-premium-account/v-baseline/t001",
                "s01-damaged-food/p01-careful-customer/u01-premium-account/v-baseline/t002",
                "s01-damaged-food/p01-careful-customer/u01-premium-account/v-candidate-v2/t001",
                "s01-damaged-food/p01-careful-customer/u01-premium-account/v-candidate-v2/t002",
                "s02-merchant-dispute/p01-careful-customer/u01-premium-account/v-baseline/t001",
                "s02-merchant-dispute/p01-careful-customer/u01-premium-account/v-baseline/t002",
                "s02-merchant-dispute/p01-careful-customer/u01-premium-account/v-candidate-v2/t001",
                "s02-merchant-dispute/p01-careful-customer/u01-premium-account/v-candidate-v2/t002",
            ]
        );
        assert_eq!(trials.len(), 8);
        assert!(trials.iter().all(|trial| {
            trial.trial.plan_revision_uid == plan_revision_uid
                && trial.trial.artifact_revision_uids.is_empty()
                && trial.trial.simulator.metadata == json!({})
                && trial.trial.simulator.token_budget == Some(1_000)
        }));
        assert_eq!(trials[0].trial.scenario_id.as_deref(), Some("damaged-food"));
        assert_eq!(trials[0].trial.data_bundle_ids, vec!["orders".to_string()]);
    }

    #[test]
    fn expand_plan_trials_derives_stable_score_run_ids_offline() {
        // Pins: plan re-expansion cannot mint different score-run IDs for the same trial key.
        let plan_revision_uid = fixture_uuid(1);
        let run_uid = fixture_uuid(2);
        let definition = fixture_plan();

        let first = expand_plan_trials(run_uid, plan_revision_uid, &definition)
            .expect("valid plan matrix expands");
        let second = expand_plan_trials(run_uid, plan_revision_uid, &definition)
            .expect("valid plan matrix expands again");

        let first_ids = first
            .iter()
            .map(|trial| (trial.trial.trial_key.clone(), trial.trial.score_run_id))
            .collect::<Vec<_>>();
        let second_ids = second
            .iter()
            .map(|trial| (trial.trial.trial_key.clone(), trial.trial.score_run_id))
            .collect::<Vec<_>>();
        assert_eq!(first_ids, second_ids);
    }

    #[test]
    fn expand_plan_trials_rejects_empty_matrix_dimensions_offline() {
        // Pins: empty plan dimensions fail before the run enters the polling loop.
        let mut definition = fixture_plan();
        definition.simulation.scenarios.clear();

        let error = expand_plan_trials(fixture_uuid(2), fixture_uuid(1), &definition)
            .expect_err("empty scenarios should fail expansion");

        assert!(matches!(
            error,
            PlanExpansionError::MissingPlanDimension {
                dimension: "scenario"
            }
        ));
    }

    #[test]
    fn select_simulation_loads_blocks_from_pinned_plan_ids_offline() {
        // Pins: trial execution reconstructs simulator context from IDs plus the plan revision.
        let definition = fixture_plan();
        let selection = select_simulation(
            &definition,
            Some("damaged-food"),
            Some("careful-customer"),
            Some("premium-account"),
            &["orders".to_string()],
        )
        .expect("selected IDs exist");

        assert_eq!(selection.scenario.id, "damaged-food");
        assert_eq!(selection.persona.id, "careful-customer");
        assert_eq!(selection.profile.id, "premium-account");
        assert_eq!(selection.data_bundles.len(), 1);
        assert_eq!(selection.data_bundles[0].id, "orders");
    }

    #[test]
    fn select_simulation_rejects_missing_plan_ids_offline() {
        // Pins: stale trial rows fail before simulator prompt construction.
        let definition = fixture_plan();
        let error = select_simulation(
            &definition,
            Some("missing"),
            Some("careful-customer"),
            Some("premium-account"),
            &[],
        )
        .expect_err("unknown scenario should fail");

        assert!(matches!(
            error,
            PlanExpansionError::UnknownSelectionId {
                field: "scenario",
                ..
            }
        ));
    }

    #[test]
    fn target_for_plan_variant_preserves_agent_revision_selector_offline() {
        // Pins: behavior-lab plans can run the same simulation matrix against exact agent revisions.
        let mut definition = fixture_plan();
        let revision_uid = fixture_uuid(99);
        definition.target_variants[0].config =
            json!({"prompt": "start", "agent_revision_uid": revision_uid});

        let target = target_for_plan_variant(&definition, &definition.target_variants[0])
            .expect("agent selector should parse");

        let ExperimentTarget::AgentLoop { agent, .. } = target else {
            panic!("fixture target should be an agent loop");
        };
        assert_eq!(
            agent.expect("agent selector should be set").revision_uid,
            Some(revision_uid)
        );
    }

    #[test]
    fn expand_plan_trials_rejects_agent_loop_without_target_model_offline() {
        // Pins: agent-loop plan fanout refuses to admit trials with no target model to drive.
        let mut definition = fixture_plan();
        definition.target_model = None;

        let error = expand_plan_trials(fixture_uuid(2), fixture_uuid(1), &definition)
            .expect_err("agent-loop plan without target_model should fail expansion");

        assert!(matches!(error, PlanExpansionError::MissingTargetModel));
    }

    #[test]
    fn expand_plan_trials_rejects_procedure_variant_without_procedure_ref_offline() {
        // Pins: procedure target variants cannot expand without a procedure reference to invoke.
        let mut definition = fixture_plan();
        definition.target_variants[0].kind = ExperimentTargetKind::Procedure;
        definition.target_variants[0].config = json!({});

        let error = expand_plan_trials(fixture_uuid(2), fixture_uuid(1), &definition)
            .expect_err("procedure variant without procedure_ref should fail expansion");

        assert!(matches!(error, PlanExpansionError::MissingProcedureRef));
    }

    #[test]
    fn expand_plan_trials_rejects_out_of_range_simulator_temperature_offline() {
        // Pins: a simulator_temperature that cannot be represented as a finite f32 is rejected
        // before any trial row is emitted (JSON cannot carry NaN/Inf, so an out-of-f32-range
        // finite value drives the same guard).
        let mut definition = fixture_plan();
        definition.target_variants[0].config =
            json!({"prompt": "start", "simulator_temperature": 1e40});

        let error = expand_plan_trials(fixture_uuid(2), fixture_uuid(1), &definition)
            .expect_err("out-of-f32-range simulator temperature should fail expansion");

        assert!(matches!(
            error,
            PlanExpansionError::InvalidSimulatorTemperature
        ));
    }

    #[test]
    fn expand_plan_trials_rejects_unparsable_agent_selector_offline() {
        // Pins: a malformed `agent` selector in variant config surfaces as a validation error,
        // not a panic or a silently dropped selector.
        let mut definition = fixture_plan();
        definition.target_variants[0].config =
            json!({"prompt": "start", "agent": "not-a-selector"});

        let error = expand_plan_trials(fixture_uuid(2), fixture_uuid(1), &definition)
            .expect_err("malformed agent selector should fail expansion");

        assert!(matches!(
            error,
            PlanExpansionError::InvalidAgentSelector { .. }
        ));
    }

    fn fixture_plan() -> ExperimentPlanDefinition {
        ExperimentPlanDefinition {
            simulation: ExperimentSimulationDefinition {
                scenarios: vec![
                    SimulationScenarioDefinition {
                        id: "damaged-food".to_string(),
                        data_bundle_ids: vec!["orders".to_string()],
                        max_turns: 2,
                        ..SimulationScenarioDefinition::default()
                    },
                    SimulationScenarioDefinition {
                        id: "merchant-dispute".to_string(),
                        max_turns: 3,
                        ..SimulationScenarioDefinition::default()
                    },
                ],
                personas: vec![SimulationPersonaDefinition {
                    id: "careful-customer".to_string(),
                    ..SimulationPersonaDefinition::default()
                }],
                profiles: vec![SimulationProfileDefinition {
                    id: "premium-account".to_string(),
                    facts: json!({"tier": "premium"}),
                    ..SimulationProfileDefinition::default()
                }],
                data_bundles: vec![SimulationDataBundleDefinition {
                    id: "orders".to_string(),
                    sources: vec![SimulationDataSource {
                        id: "order-fixture".to_string(),
                        kind: SimulationDataSourceKind::MockData,
                        connector_ref: None,
                        fixture: json!({"order_id": "FOOD-42"}),
                        scope: None,
                        notes: String::new(),
                    }],
                    ui: json!({}),
                }],
                ui: json!({}),
            },
            target_variants: vec![
                ExperimentTargetVariant {
                    key: "baseline".to_string(),
                    kind: ExperimentTargetKind::AgentLoop,
                    config: json!({"prompt": "start"}),
                    ui: json!({}),
                },
                ExperimentTargetVariant {
                    key: "candidate-v2".to_string(),
                    kind: ExperimentTargetKind::AgentLoop,
                    config: json!({"prompt": "start"}),
                    ui: json!({}),
                },
            ],
            simulator_model: "gpt-5.1-mini".to_string(),
            target_model: Some("gpt-5.1".to_string()),
            parallelism: 2,
            trials_per_combination: 2,
            budget: ExperimentBudget {
                max_total_cents: 100,
                max_trial_cents: Some(25),
                max_total_tokens: Some(10_000),
                max_trial_tokens: Some(1_000),
            },
            scorecard: json!({}),
            learning_proposals: ExperimentLearningProposalSettings::default(),
            ui: json!({}),
        }
    }

    fn fixture_uuid(last_byte: u8) -> Uuid {
        let mut bytes = [0_u8; 16];
        bytes[15] = last_byte;
        Uuid::from_bytes(bytes)
    }
}
