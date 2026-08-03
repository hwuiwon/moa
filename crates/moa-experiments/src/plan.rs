//! Pure helpers for expanding behavior-lab experiment plans.
//!
//! Expansion is the path that actually mints trials, so it is also the path
//! that has to bound them. A plan's per-dimension limits do not bound the
//! matrix: the trial count is the product of scenarios, personas, profiles,
//! target variants, and repetitions, and each factor can be individually legal
//! while the product is not. [`plan_matrix_shape`] resolves that product with
//! checked arithmetic before anything is allocated, and [`PlanTrialPager`]
//! hands the resulting trials back one page at a time.

use moa_artifacts::simulation::{
    ExperimentPlanDefinition, ExperimentTargetKind, ExperimentTargetVariant, MAX_PLAN_BLOCK_BYTES,
    MAX_PLAN_DATA_BUNDLES, MAX_PLAN_DEFINITION_BYTES, MAX_PLAN_FIELD_BYTES, MAX_PLAN_PARALLELISM,
    MAX_PLAN_PERSONAS, MAX_PLAN_PROFILES, MAX_PLAN_PROVIDER_CALL_QPS, MAX_PLAN_SCENARIOS,
    MAX_PLAN_TARGET_VARIANTS, MAX_PLAN_TOTAL_COST_CENTS, MAX_PLAN_TOTAL_TOKENS,
    MAX_PLAN_TOTAL_TRIALS, MAX_PLAN_TRIAL_COST_CENTS, MAX_PLAN_TRIAL_TOKENS,
    MAX_PLAN_TRIALS_PER_COMBINATION, MAX_SCENARIO_TURNS, PLAN_PROVIDER_CALLS_PER_TRIAL_TURN,
    SimulationDataBundleDefinition, SimulationPersonaDefinition, SimulationProfileDefinition,
    SimulationScenarioDefinition,
};
use moa_core::{
    types::agent::AgentSessionSelection, types::execution_planning::PinnedExecutionTemplateRef,
    types::experiments::ExperimentScorecard, types::identifiers::ModelId,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use thiserror::Error;
use uuid::Uuid;

use crate::evaluator::validate_scorecard;
use crate::model::{
    ExperimentSimulatorConfig, ExperimentTarget, ExperimentVariant, NewExperimentTrial,
};
use crate::simulator_policy::registry::ResolvedSimulatorPolicy;

pub mod admission;

/// Default target-agent turn cap for plan-expanded simulator trials.
pub(crate) const DEFAULT_PLAN_TRIAL_MAX_TURNS: u32 = 8;

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
    /// A plan matrix dimension declares more blocks than the platform accepts.
    #[error("experiment plan declares {actual} {dimension}s, over the limit of {limit}")]
    PlanDimensionTooLarge {
        /// Matrix dimension that is too wide.
        dimension: &'static str,
        /// Declared block count.
        actual: u64,
        /// Highest accepted block count.
        limit: u64,
    },
    /// A plan declared a scalar outside its accepted range.
    #[error("experiment plan {field} must be between {min} and {max}, got {actual}")]
    PlanValueOutOfRange {
        /// Plan field that is out of range.
        field: &'static str,
        /// Declared value.
        actual: u64,
        /// Lowest accepted value.
        min: u64,
        /// Highest accepted value.
        max: u64,
    },
    /// A serialized plan, block, or field is larger than the platform accepts.
    #[error("experiment plan {field} is {actual} bytes, over the limit of {limit} bytes")]
    PlanTooLarge {
        /// Path of the oversized plan element.
        field: String,
        /// Serialized size.
        actual: usize,
        /// Highest accepted serialized size.
        limit: usize,
    },
    /// A plan could not be serialized to measure its size.
    #[error("experiment plan {field} could not be measured: {message}")]
    UnmeasurablePlan {
        /// Path of the element that could not be measured.
        field: String,
        /// Serialization error message.
        message: String,
    },
    /// The declared trial matrix overflowed before it could be counted.
    #[error("experiment plan trial matrix size overflowed before it could be counted")]
    TrialMatrixOverflow,
    /// The declared trial matrix is larger than one run may mint.
    #[error("experiment plan expands to {actual} trials, over the limit of {limit}")]
    TrialMatrixTooLarge {
        /// Trials the declared matrix would mint.
        actual: u64,
        /// Highest accepted trial count for one run.
        limit: u64,
    },
    /// A plan declared no evidence requirements.
    #[error("experiment plan must declare a scorecard")]
    MissingScorecard,
    /// A plan declared a scorecard this build cannot run.
    #[error("experiment plan scorecard is not runnable: {message}")]
    UnrunnableScorecard {
        /// Registry validation error message.
        message: String,
    },
    /// Agent-loop variants require a target model.
    #[error("agent-loop experiment plans require target_model")]
    MissingTargetModel,
    /// Execution-template variants require an exact pinned template.
    #[error("execution-template target variants require template")]
    MissingExecutionTemplate,
    /// Execution-template variants require an explicit non-empty objective.
    #[error("execution-template target variants require a non-empty objective")]
    MissingExecutionObjective,
    /// An execution-template variant field is malformed.
    #[error("execution-template target variant is invalid: {message}")]
    InvalidExecutionTemplate {
        /// Validation error message.
        message: String,
    },
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
    /// An internal release case selection is empty, duplicated, or out of range.
    #[error("artifact release case selection is invalid: {message}")]
    InvalidReleaseCaseSelection {
        /// Exact reason the sparse selection cannot be expanded.
        message: String,
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
    plan_matrix_shape(definition)?;
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
    let artifact_revision_uids = variant.artifact_revision_uids.clone();
    Ok(PlanRunProjection {
        target,
        variant,
        scorecard: plan_scorecard(definition)?,
        artifact_revision_uids,
        plan_revision_uid,
    })
}

/// Returns the plan's scorecard after checking this build can run every requirement.
///
/// # Errors
///
/// Returns [`PlanExpansionError::MissingScorecard`] when the plan declared none
/// and [`PlanExpansionError::UnrunnableScorecard`] when it names an evaluator,
/// version, output, effect, or configuration this build cannot honour.
pub fn plan_scorecard(
    definition: &ExperimentPlanDefinition,
) -> Result<ExperimentScorecard, PlanExpansionError> {
    let scorecard = definition
        .scorecard
        .clone()
        .ok_or(PlanExpansionError::MissingScorecard)?;
    validate_scorecard(&scorecard).map_err(|error| PlanExpansionError::UnrunnableScorecard {
        message: error.to_string(),
    })?;
    Ok(scorecard)
}

/// Bounded shape of a plan's trial matrix, resolved before any trial exists.
///
/// Every field is the result of a checked computation over a plan that already
/// passed its per-dimension, per-byte, and per-scalar limits, so a value here
/// is safe to allocate against and safe to reserve against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanMatrixShape {
    /// Scenarios the plan declares.
    pub scenarios: u32,
    /// Personas the plan declares.
    pub personas: u32,
    /// Profiles the plan declares.
    pub profiles: u32,
    /// Target variants the plan declares.
    pub target_variants: u32,
    /// Repetitions of every matrix combination.
    pub trials_per_combination: u32,
    /// Total trials the matrix expands to.
    pub total_trials: u32,
    /// Trials the plan may execute concurrently.
    pub parallel_trials: u32,
    /// Provider calls per second the declared parallelism implies.
    pub provider_call_qps: u32,
    /// Cost in cents the plan declares it may spend.
    ///
    /// This is a *declared* number. It is checked at admission and is not, on
    /// its own, evidence of what a run actually spent; a runtime cost ledger
    /// reconciles actual spend against it.
    pub declared_total_cost_cents: u32,
}

/// One sparse scenario/persona/profile tuple selected by an artifact release pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCaseSelection {
    /// Scenario ID in the pinned plan.
    pub scenario_id: String,
    /// Persona ID in the pinned plan.
    pub persona_id: String,
    /// Profile ID in the pinned plan.
    pub profile_id: String,
    /// Paired repetitions emitted for every target variant.
    pub repetitions: u32,
}

/// Trials one [`PlanTrialPager`] page may contain.
pub const PLAN_TRIAL_PAGE_TRIALS: usize = 256;

/// Resolves and bounds a plan's trial matrix without expanding it.
///
/// Checks, in order: matrix dimensions are present and within their block
/// limits; the serialized plan, each block, and each free-form field are within
/// their byte limits; repetitions, parallelism, scenario turn caps, and budgets
/// are inside their accepted ranges; and the product of every matrix dimension
/// fits both `u64` arithmetic and the total-trial ceiling.
///
/// Over-limit plans are refused, never clamped, so a tenant never silently gets
/// a smaller matrix than the one it declared.
///
/// # Errors
///
/// Returns the first [`PlanExpansionError`] the plan violates.
pub fn plan_matrix_shape(
    definition: &ExperimentPlanDefinition,
) -> Result<PlanMatrixShape, PlanExpansionError> {
    let scenarios = plan_dimension(
        "scenario",
        definition.simulation.scenarios.len(),
        MAX_PLAN_SCENARIOS,
    )?;
    let personas = plan_dimension(
        "persona",
        definition.simulation.personas.len(),
        MAX_PLAN_PERSONAS,
    )?;
    let profiles = plan_dimension(
        "profile",
        definition.simulation.profiles.len(),
        MAX_PLAN_PROFILES,
    )?;
    let target_variants = plan_dimension(
        "target variant",
        definition.target_variants.len(),
        MAX_PLAN_TARGET_VARIANTS,
    )?;
    if definition.simulation.data_bundles.len() > MAX_PLAN_DATA_BUNDLES as usize {
        return Err(PlanExpansionError::PlanDimensionTooLarge {
            dimension: "data bundle",
            actual: definition.simulation.data_bundles.len() as u64,
            limit: u64::from(MAX_PLAN_DATA_BUNDLES),
        });
    }

    require_plan_bytes(definition)?;
    require_plan_scalars(definition)?;

    let total_trials = plan_trial_count(
        u64::from(scenarios),
        u64::from(personas),
        u64::from(profiles),
        u64::from(target_variants),
        u64::from(definition.trials_per_combination),
    )?;
    let total_trials =
        u32::try_from(total_trials).map_err(|_| PlanExpansionError::TrialMatrixTooLarge {
            actual: total_trials,
            limit: u64::from(MAX_PLAN_TOTAL_TRIALS),
        })?;

    let provider_call_qps = definition
        .parallelism
        .checked_mul(PLAN_PROVIDER_CALLS_PER_TRIAL_TURN)
        .ok_or(PlanExpansionError::TrialMatrixOverflow)?;
    if provider_call_qps > MAX_PLAN_PROVIDER_CALL_QPS {
        return Err(PlanExpansionError::PlanValueOutOfRange {
            field: "implied provider call qps",
            actual: u64::from(provider_call_qps),
            min: u64::from(PLAN_PROVIDER_CALLS_PER_TRIAL_TURN),
            max: u64::from(MAX_PLAN_PROVIDER_CALL_QPS),
        });
    }

    Ok(PlanMatrixShape {
        scenarios,
        personas,
        profiles,
        target_variants,
        trials_per_combination: definition.trials_per_combination,
        total_trials,
        parallel_trials: definition.parallelism.min(total_trials),
        provider_call_qps,
        declared_total_cost_cents: definition.budget.max_total_cents,
    })
}

/// Multiplies a plan's matrix dimensions with checked arithmetic.
///
/// Returns the trial count only when every multiplication fits in `u64` and the
/// product is within [`MAX_PLAN_TOTAL_TRIALS`]. Nothing is allocated on the way,
/// so an over-large or overflowing matrix is refused before a single trial
/// exists.
///
/// # Errors
///
/// Returns [`PlanExpansionError::TrialMatrixOverflow`] when the product does not
/// fit in `u64` and [`PlanExpansionError::TrialMatrixTooLarge`] when it exceeds
/// the total-trial ceiling.
pub fn plan_trial_count(
    scenarios: u64,
    personas: u64,
    profiles: u64,
    target_variants: u64,
    trials_per_combination: u64,
) -> Result<u64, PlanExpansionError> {
    let total = scenarios
        .checked_mul(personas)
        .and_then(|value| value.checked_mul(profiles))
        .and_then(|value| value.checked_mul(target_variants))
        .and_then(|value| value.checked_mul(trials_per_combination))
        .ok_or(PlanExpansionError::TrialMatrixOverflow)?;
    if total > u64::from(MAX_PLAN_TOTAL_TRIALS) {
        return Err(PlanExpansionError::TrialMatrixTooLarge {
            actual: total,
            limit: u64::from(MAX_PLAN_TOTAL_TRIALS),
        });
    }
    Ok(total)
}

/// Pages a bounded plan matrix into deterministic trial rows.
///
/// The pager resolves every fallible decision up front — matrix bounds, target
/// payloads, variant payloads, and per-scenario data bundles — so iteration
/// itself cannot fail and never holds more than one page of trials. Trials are
/// emitted in scenario, persona, profile, variant, repetition order.
pub struct PlanTrialPager<'a> {
    definition: &'a ExperimentPlanDefinition,
    run_uid: Uuid,
    plan_revision_uid: Uuid,
    simulator_policy: &'a ResolvedSimulatorPolicy,
    shape: PlanMatrixShape,
    variants: Vec<PreparedVariant>,
    cases: Vec<PreparedCase<'a>>,
    cursor: MatrixCursor,
    emitted: u32,
}

/// One target variant resolved once for the whole matrix.
struct PreparedVariant {
    key: String,
    kind: ExperimentTargetKind,
    target: ExperimentTarget,
    payload: ExperimentVariant,
}

/// One validated case tuple resolved to its immutable plan blocks.
struct PreparedCase<'a> {
    scenario_index: usize,
    persona_index: usize,
    profile_index: usize,
    scenario: &'a SimulationScenarioDefinition,
    persona: &'a SimulationPersonaDefinition,
    profile: &'a SimulationProfileDefinition,
    repetitions: u32,
    data_bundle_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct MatrixCursor {
    case: usize,
    variant: usize,
    trial: u32,
}

impl<'a> PlanTrialPager<'a> {
    /// Bounds a plan and prepares its per-variant and per-scenario payloads.
    ///
    /// # Errors
    ///
    /// Returns the first [`PlanExpansionError`] the plan violates, including
    /// every matrix bound checked by [`plan_matrix_shape`] and every malformed
    /// target variant. No trial is allocated when this fails.
    pub fn new(
        run_uid: Uuid,
        plan_revision_uid: Uuid,
        definition: &'a ExperimentPlanDefinition,
        simulator_policy: &'a ResolvedSimulatorPolicy,
    ) -> Result<Self, PlanExpansionError> {
        let shape = plan_matrix_shape(definition)?;
        let cases = prepare_matrix_cases(definition);
        Self::from_prepared(
            run_uid,
            plan_revision_uid,
            definition,
            simulator_policy,
            shape,
            cases,
        )
    }

    /// Builds a pager for the sparse case tuples selected by a release pack.
    ///
    /// Normal Behavior Lab runs expand the plan's Cartesian product. Release
    /// evaluation instead executes only the approved tuples, preserving each
    /// case's own repetition count and pairing every target variant on the same
    /// tuple and repetition index.
    ///
    /// # Errors
    ///
    /// Returns an error when the plan is unrunnable, a selected block ID is not
    /// present in the pinned plan, a tuple is duplicated, or the selected trial
    /// count exceeds the normal plan limits.
    pub fn new_selected(
        run_uid: Uuid,
        plan_revision_uid: Uuid,
        definition: &'a ExperimentPlanDefinition,
        simulator_policy: &'a ResolvedSimulatorPolicy,
        selections: &[PlanCaseSelection],
    ) -> Result<Self, PlanExpansionError> {
        let (shape, cases) = prepare_selected_cases(definition, selections)?;
        Self::from_prepared(
            run_uid,
            plan_revision_uid,
            definition,
            simulator_policy,
            shape,
            cases,
        )
    }

    fn from_prepared(
        run_uid: Uuid,
        plan_revision_uid: Uuid,
        definition: &'a ExperimentPlanDefinition,
        simulator_policy: &'a ResolvedSimulatorPolicy,
        shape: PlanMatrixShape,
        cases: Vec<PreparedCase<'a>>,
    ) -> Result<Self, PlanExpansionError> {
        // Expansion is independently fail-closed. The run-admission path also
        // projects this scorecard, but internal callers must not be able to mint
        // trials for a plan the finalizer cannot evaluate.
        let _ = plan_scorecard(definition)?;
        let variants = definition
            .target_variants
            .iter()
            .map(|variant| {
                Ok(PreparedVariant {
                    key: variant.key.clone(),
                    kind: variant.kind,
                    target: target_for_plan_variant(definition, variant)?,
                    payload: variant_payload_for_plan(
                        plan_revision_uid,
                        definition,
                        variant,
                        |value| {
                            json!({
                                "plan_revision_uid": value,
                                "variant_config": variant.config,
                            })
                        },
                    )?,
                })
            })
            .collect::<Result<Vec<_>, PlanExpansionError>>()?;
        Ok(Self {
            definition,
            run_uid,
            plan_revision_uid,
            simulator_policy,
            shape,
            variants,
            cases,
            cursor: MatrixCursor::default(),
            emitted: 0,
        })
    }

    /// Returns the bounded shape of the matrix being paged.
    #[must_use]
    pub const fn shape(&self) -> PlanMatrixShape {
        self.shape
    }

    /// Returns how many trials the pager has not emitted yet.
    #[must_use]
    pub const fn remaining(&self) -> u32 {
        self.shape.total_trials - self.emitted
    }

    /// Returns the next page of at most [`PLAN_TRIAL_PAGE_TRIALS`] trials.
    ///
    /// An empty page means the matrix is exhausted.
    pub fn next_page(&mut self) -> Vec<ExpandedPlanTrial> {
        let page = PLAN_TRIAL_PAGE_TRIALS.min(self.remaining() as usize);
        self.by_ref().take(page).collect()
    }

    fn advance(&mut self) {
        self.cursor.trial += 1;
        if self
            .cases
            .get(self.cursor.case)
            .is_some_and(|case| self.cursor.trial < case.repetitions)
        {
            return;
        }
        self.cursor.trial = 0;
        self.cursor.variant += 1;
        if self.cursor.variant < self.variants.len() {
            return;
        }
        self.cursor.variant = 0;
        self.cursor.case += 1;
    }
}

impl Iterator for PlanTrialPager<'_> {
    type Item = ExpandedPlanTrial;

    fn next(&mut self) -> Option<Self::Item> {
        if self.emitted >= self.shape.total_trials {
            return None;
        }
        let cursor = self.cursor;
        let case = self.cases.get(cursor.case)?;
        let scenario = case.scenario;
        let persona = case.persona;
        let profile = case.profile;
        let variant = self.variants.get(cursor.variant)?;
        let trial_key = stable_trial_key(
            (case.scenario_index, &scenario.id),
            (case.persona_index, &persona.id),
            (case.profile_index, &profile.id),
            &variant.key,
            cursor.trial,
        );
        let plan_revision_uid = self.plan_revision_uid;
        let paired_seed = stable_paired_seed(
            (case.scenario_index, &scenario.id),
            (case.persona_index, &persona.id),
            (case.profile_index, &profile.id),
            cursor.trial,
            plan_revision_uid,
        );
        let expanded = ExpandedPlanTrial {
            trial: NewExperimentTrial {
                run_uid: self.run_uid,
                trial_key: trial_key.clone(),
                target_kind: variant.kind,
                variant_key: variant.key.clone(),
                plan_revision_uid,
                scenario_id: Some(scenario.id.clone()),
                persona_id: Some(persona.id.clone()),
                profile_id: Some(profile.id.clone()),
                data_bundle_ids: case.data_bundle_ids.clone(),
                artifact_revision_uids: Vec::new(),
                simulator: ExperimentSimulatorConfig {
                    policy: self.simulator_policy.clone(),
                    max_turns: DEFAULT_PLAN_TRIAL_MAX_TURNS,
                    token_budget: self.definition.budget.max_trial_tokens,
                },
                target_model: self.definition.target_model.as_ref().map(ModelId::new),
                seed: self
                    .simulator_policy
                    .components
                    .decoding
                    .seeded
                    .then_some(paired_seed),
                score_run_id: deterministic_score_run_id(self.run_uid, &trial_key),
            },
            target: variant.target.clone(),
            variant: variant.payload.clone(),
        };
        self.emitted += 1;
        self.advance();
        Some(expanded)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining() as usize;
        (remaining, Some(remaining))
    }
}

fn prepare_matrix_cases(definition: &ExperimentPlanDefinition) -> Vec<PreparedCase<'_>> {
    let mut cases = Vec::new();
    for (scenario_index, scenario) in definition.simulation.scenarios.iter().enumerate() {
        let data_bundle_ids = data_bundle_ids_for_scenario(definition, scenario);
        for (persona_index, persona) in definition.simulation.personas.iter().enumerate() {
            for (profile_index, profile) in definition.simulation.profiles.iter().enumerate() {
                cases.push(PreparedCase {
                    scenario_index,
                    persona_index,
                    profile_index,
                    scenario,
                    persona,
                    profile,
                    repetitions: definition.trials_per_combination,
                    data_bundle_ids: data_bundle_ids.clone(),
                });
            }
        }
    }
    cases
}

fn prepare_selected_cases<'a>(
    definition: &'a ExperimentPlanDefinition,
    selections: &[PlanCaseSelection],
) -> Result<(PlanMatrixShape, Vec<PreparedCase<'a>>), PlanExpansionError> {
    let base = plan_matrix_shape(definition)?;
    if selections.is_empty() {
        return Err(PlanExpansionError::InvalidReleaseCaseSelection {
            message: "at least one approved case is required".to_string(),
        });
    }

    let mut tuples = BTreeSet::new();
    let mut scenario_indexes = BTreeSet::new();
    let mut persona_indexes = BTreeSet::new();
    let mut profile_indexes = BTreeSet::new();
    let mut cases = Vec::with_capacity(selections.len());
    let mut repetitions = 0_u64;
    let mut max_repetitions = 0_u32;
    for selection in selections {
        if !(1..=MAX_PLAN_TRIALS_PER_COMBINATION).contains(&selection.repetitions) {
            return Err(PlanExpansionError::InvalidReleaseCaseSelection {
                message: format!(
                    "case `{}` repetitions must be between 1 and {MAX_PLAN_TRIALS_PER_COMBINATION}",
                    selection.scenario_id
                ),
            });
        }
        let scenario_index = definition
            .simulation
            .scenarios
            .iter()
            .position(|value| value.id == selection.scenario_id)
            .ok_or_else(|| unknown_id("scenario", &selection.scenario_id))?;
        let persona_index = definition
            .simulation
            .personas
            .iter()
            .position(|value| value.id == selection.persona_id)
            .ok_or_else(|| unknown_id("persona", &selection.persona_id))?;
        let profile_index = definition
            .simulation
            .profiles
            .iter()
            .position(|value| value.id == selection.profile_id)
            .ok_or_else(|| unknown_id("profile", &selection.profile_id))?;
        if !tuples.insert((scenario_index, persona_index, profile_index)) {
            return Err(PlanExpansionError::InvalidReleaseCaseSelection {
                message: format!(
                    "duplicate tuple `{}/{}/{}`",
                    selection.scenario_id, selection.persona_id, selection.profile_id
                ),
            });
        }
        scenario_indexes.insert(scenario_index);
        persona_indexes.insert(persona_index);
        profile_indexes.insert(profile_index);
        repetitions = repetitions
            .checked_add(u64::from(selection.repetitions))
            .ok_or(PlanExpansionError::TrialMatrixOverflow)?;
        max_repetitions = max_repetitions.max(selection.repetitions);
        let scenario = &definition.simulation.scenarios[scenario_index];
        cases.push(PreparedCase {
            scenario_index,
            persona_index,
            profile_index,
            scenario,
            persona: &definition.simulation.personas[persona_index],
            profile: &definition.simulation.profiles[profile_index],
            repetitions: selection.repetitions,
            data_bundle_ids: data_bundle_ids_for_scenario(definition, scenario),
        });
    }
    let total_trials = repetitions
        .checked_mul(u64::from(base.target_variants))
        .ok_or(PlanExpansionError::TrialMatrixOverflow)?;
    if total_trials > u64::from(MAX_PLAN_TOTAL_TRIALS) {
        return Err(PlanExpansionError::TrialMatrixTooLarge {
            actual: total_trials,
            limit: u64::from(MAX_PLAN_TOTAL_TRIALS),
        });
    }
    let total_trials =
        u32::try_from(total_trials).map_err(|_| PlanExpansionError::TrialMatrixOverflow)?;
    Ok((
        PlanMatrixShape {
            scenarios: u32::try_from(scenario_indexes.len())
                .map_err(|_| PlanExpansionError::TrialMatrixOverflow)?,
            personas: u32::try_from(persona_indexes.len())
                .map_err(|_| PlanExpansionError::TrialMatrixOverflow)?,
            profiles: u32::try_from(profile_indexes.len())
                .map_err(|_| PlanExpansionError::TrialMatrixOverflow)?,
            target_variants: base.target_variants,
            trials_per_combination: max_repetitions,
            total_trials,
            parallel_trials: definition.parallelism.min(total_trials),
            provider_call_qps: base.provider_call_qps,
            declared_total_cost_cents: base.declared_total_cost_cents,
        },
        cases,
    ))
}

/// Resolves the bounded shape of an approved sparse release-case selection.
///
/// # Errors
///
/// Returns the same validation errors as [`PlanTrialPager::new_selected`].
pub fn selected_plan_matrix_shape(
    definition: &ExperimentPlanDefinition,
    selections: &[PlanCaseSelection],
) -> Result<PlanMatrixShape, PlanExpansionError> {
    prepare_selected_cases(definition, selections).map(|(shape, _)| shape)
}

#[cfg(test)]
fn collect_pager_trials(
    run_uid: Uuid,
    plan_revision_uid: Uuid,
    definition: &ExperimentPlanDefinition,
    simulator_policy: &ResolvedSimulatorPolicy,
) -> Result<Vec<ExpandedPlanTrial>, PlanExpansionError> {
    let mut pager = PlanTrialPager::new(run_uid, plan_revision_uid, definition, simulator_policy)?;
    let mut trials = Vec::with_capacity(pager.remaining() as usize);
    loop {
        let page = pager.next_page();
        if page.is_empty() {
            return Ok(trials);
        }
        trials.extend(page);
    }
}

fn plan_dimension(
    dimension: &'static str,
    declared: usize,
    limit: u32,
) -> Result<u32, PlanExpansionError> {
    if declared == 0 {
        return Err(PlanExpansionError::MissingPlanDimension { dimension });
    }
    u32::try_from(declared)
        .ok()
        .filter(|declared| *declared <= limit)
        .ok_or(PlanExpansionError::PlanDimensionTooLarge {
            dimension,
            actual: declared as u64,
            limit: u64::from(limit),
        })
}

fn require_plan_bytes(definition: &ExperimentPlanDefinition) -> Result<(), PlanExpansionError> {
    require_bytes("definition", definition, MAX_PLAN_DEFINITION_BYTES)?;
    for (index, scenario) in definition.simulation.scenarios.iter().enumerate() {
        require_bytes(
            &format!("simulation.scenarios[{index}]"),
            scenario,
            MAX_PLAN_BLOCK_BYTES,
        )?;
    }
    for (index, persona) in definition.simulation.personas.iter().enumerate() {
        require_bytes(
            &format!("simulation.personas[{index}]"),
            persona,
            MAX_PLAN_BLOCK_BYTES,
        )?;
    }
    for (index, profile) in definition.simulation.profiles.iter().enumerate() {
        require_bytes(
            &format!("simulation.profiles[{index}]"),
            profile,
            MAX_PLAN_BLOCK_BYTES,
        )?;
        require_bytes(
            &format!("simulation.profiles[{index}].facts"),
            &profile.facts,
            MAX_PLAN_FIELD_BYTES,
        )?;
    }
    for (index, data_bundle) in definition.simulation.data_bundles.iter().enumerate() {
        require_bytes(
            &format!("simulation.data_bundles[{index}]"),
            data_bundle,
            MAX_PLAN_BLOCK_BYTES,
        )?;
        for (source_index, source) in data_bundle.sources.iter().enumerate() {
            require_bytes(
                &format!("simulation.data_bundles[{index}].sources[{source_index}].fixture"),
                &source.fixture,
                MAX_PLAN_FIELD_BYTES,
            )?;
        }
    }
    for (index, variant) in definition.target_variants.iter().enumerate() {
        require_bytes(
            &format!("target_variants[{index}]"),
            variant,
            MAX_PLAN_BLOCK_BYTES,
        )?;
        require_bytes(
            &format!("target_variants[{index}].config"),
            &variant.config,
            MAX_PLAN_FIELD_BYTES,
        )?;
    }
    Ok(())
}

fn require_bytes<T: Serialize>(
    field: &str,
    value: &T,
    limit: usize,
) -> Result<(), PlanExpansionError> {
    let actual = serde_json::to_vec(value)
        .map_err(|error| PlanExpansionError::UnmeasurablePlan {
            field: field.to_string(),
            message: error.to_string(),
        })?
        .len();
    if actual > limit {
        return Err(PlanExpansionError::PlanTooLarge {
            field: field.to_string(),
            actual,
            limit,
        });
    }
    Ok(())
}

fn require_plan_scalars(definition: &ExperimentPlanDefinition) -> Result<(), PlanExpansionError> {
    require_range(
        "trials_per_combination",
        definition.trials_per_combination,
        1,
        MAX_PLAN_TRIALS_PER_COMBINATION,
    )?;
    require_range(
        "parallelism",
        definition.parallelism,
        1,
        MAX_PLAN_PARALLELISM,
    )?;
    require_range(
        "budget.max_total_cents",
        definition.budget.max_total_cents,
        1,
        MAX_PLAN_TOTAL_COST_CENTS,
    )?;
    if let Some(max_trial_cents) = definition.budget.max_trial_cents {
        require_range(
            "budget.max_trial_cents",
            max_trial_cents,
            1,
            MAX_PLAN_TRIAL_COST_CENTS.min(definition.budget.max_total_cents),
        )?;
    }
    if let Some(max_total_tokens) = definition.budget.max_total_tokens {
        require_range(
            "budget.max_total_tokens",
            max_total_tokens,
            1,
            MAX_PLAN_TOTAL_TOKENS,
        )?;
    }
    if let Some(max_trial_tokens) = definition.budget.max_trial_tokens {
        require_range(
            "budget.max_trial_tokens",
            max_trial_tokens,
            1,
            MAX_PLAN_TRIAL_TOKENS.min(
                definition
                    .budget
                    .max_total_tokens
                    .unwrap_or(MAX_PLAN_TRIAL_TOKENS),
            ),
        )?;
    }
    for scenario in &definition.simulation.scenarios {
        require_range(
            "scenario max_turns",
            scenario.max_turns,
            1,
            MAX_SCENARIO_TURNS,
        )?;
    }
    Ok(())
}

fn require_range(
    field: &'static str,
    actual: u32,
    min: u32,
    max: u32,
) -> Result<(), PlanExpansionError> {
    if actual < min || actual > max {
        return Err(PlanExpansionError::PlanValueOutOfRange {
            field,
            actual: u64::from(actual),
            min: u64::from(min),
            max: u64::from(max),
        });
    }
    Ok(())
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
pub(crate) fn target_for_plan_variant(
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
            agent: agent_selection_for_variant(variant)?,
            model: definition
                .target_model
                .as_ref()
                .map(ModelId::new)
                .ok_or(PlanExpansionError::MissingTargetModel)?,
            attachments: Vec::new(),
        }),
        ExperimentTargetKind::ExecutionTemplate => {
            let template = execution_template_for_variant(variant)?;
            let objective = variant
                .config
                .get("objective")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or(PlanExpansionError::MissingExecutionObjective)?
                .to_string();
            Ok(ExperimentTarget::ExecutionTemplate {
                template,
                objective,
                input: variant
                    .config
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                session_id: optional_config(variant, "session_id")?,
                idempotency_key: optional_config(variant, "idempotency_key")?,
            })
        }
    }
}

fn execution_template_for_variant(
    variant: &ExperimentTargetVariant,
) -> Result<PinnedExecutionTemplateRef, PlanExpansionError> {
    let value = variant
        .config
        .get("template")
        .ok_or(PlanExpansionError::MissingExecutionTemplate)?;
    serde_json::from_value(value.clone()).map_err(|error| {
        PlanExpansionError::InvalidExecutionTemplate {
            message: format!("template: {error}"),
        }
    })
}

fn optional_config<T>(
    variant: &ExperimentTargetVariant,
    key: &'static str,
) -> Result<Option<T>, PlanExpansionError>
where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = variant.config.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| PlanExpansionError::InvalidExecutionTemplate {
            message: format!("{key}: {error}"),
        })
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
    let execution_template = if variant.kind == ExperimentTargetKind::ExecutionTemplate {
        Some(execution_template_for_variant(variant)?)
    } else {
        None
    };
    let mut artifact_revision_uids = vec![plan_revision_uid];
    if let Some(template) = &execution_template {
        artifact_revision_uids.push(template.revision_uid);
    }
    Ok(ExperimentVariant {
        name: variant.key.clone(),
        model: definition.target_model.as_ref().map(ModelId::new),
        artifact_revision_uids,
        skill_refs: Vec::new(),
        execution_template,
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

/// Builds the canonical durable key for one expanded plan trial.
///
/// Release-environment provisioning uses the same coordinate before the run is
/// dispatched, so its per-trial overlay cannot drift from the pager's trial.
#[must_use]
pub fn stable_trial_key(
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

fn stable_paired_seed(
    scenario: (usize, &str),
    persona: (usize, &str),
    profile: (usize, &str),
    trial_index: u32,
    plan_revision_uid: Uuid,
) -> String {
    format!(
        "s{:02}-{}/p{:02}-{}/u{:02}-{}/t{:03}:{plan_revision_uid}",
        scenario.0 + 1,
        key_part(scenario.1),
        persona.0 + 1,
        key_part(persona.1),
        profile.0 + 1,
        key_part(profile.1),
        trial_index + 1,
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
    use moa_core::types::experiments::{ScorecardEffect, ScorecardRequirement};

    fn fixture_policy() -> ResolvedSimulatorPolicy {
        crate::simulator_policy::test_support::resolved_policy()
    }

    #[test]
    fn plan_expansion_refuses_a_plan_that_declares_no_scorecard_offline() {
        // Pins the second half of the `Option<ExperimentScorecard>` closure.
        // Validation reports a missing scorecard, but a report is advisory —
        // expansion is the path that actually mints trials, so it must refuse
        // independently. A plan that reached this point without a scorecard
        // would otherwise expand into trials that can never prove anything.
        let mut definition = fixture_plan();
        definition.scorecard = None;

        let error = project_plan_run(&definition, fixture_uuid(1), "plan", "run")
            .expect_err("a plan with no scorecard must not project a run");

        assert!(
            matches!(error, PlanExpansionError::MissingScorecard),
            "unexpected expansion error: {error:?}"
        );
    }

    #[test]
    fn plan_expansion_refuses_a_scorecard_this_build_cannot_run_offline() {
        // Pins that a syntactically valid scorecard naming an evaluator this build
        // does not have is refused at expansion rather than producing trials that
        // wait forever for a row nothing will ever write.
        let mut definition = fixture_plan();
        definition.scorecard = Some(
            ExperimentScorecard::new(vec![ScorecardRequirement {
                evaluator_id: "evaluator_from_the_future".to_string(),
                evaluator_version: "v1".to_string(),
                config: json!({}),
                effect: ScorecardEffect::Blocking,
            }])
            .expect("structurally valid"),
        );

        let error = project_plan_run(&definition, fixture_uuid(1), "plan", "run")
            .expect_err("an unrunnable scorecard must not project a run");

        assert!(
            matches!(error, PlanExpansionError::UnrunnableScorecard { .. }),
            "unexpected expansion error: {error:?}"
        );
    }

    #[test]
    fn plan_admission_accepts_the_registered_objective_scenario_gate_offline() {
        // Pins: the production trial runtime now persists typed release-case
        // evidence and the registered evaluator fails closed when that evidence
        // is absent. Plan admission therefore treats scenario_outcome like any
        // other runnable deterministic evaluator.
        let definition = scenario_gated_plan();

        let projection = project_plan_run(&definition, fixture_uuid(1), "plan", "run")
            .expect("registered scenario gate projects a run");
        assert_eq!(
            projection.scorecard.requirements()[0].evaluator_id,
            "scenario_outcome"
        );

        let expansion = collect_pager_trials(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &fixture_policy(),
        )
        .expect("trial expansion accepts the registered gate");
        assert!(
            !expansion.is_empty(),
            "a runnable scenario-gated plan must mint its bounded trial matrix"
        );
    }

    #[test]
    fn plan_trial_pager_uses_ids_without_copying_simulation_blocks_offline() {
        // Pins: plan fanout stores plan-local IDs and leaves simulator metadata semantic-free.
        let plan_revision_uid = fixture_uuid(1);
        let definition = fixture_plan();
        let trials = collect_pager_trials(
            fixture_uuid(2),
            plan_revision_uid,
            &definition,
            &fixture_policy(),
        )
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
                && trial.trial.simulator.policy == fixture_policy()
                && trial.trial.simulator.token_budget == Some(1_000)
        }));
        assert_eq!(trials[0].trial.scenario_id.as_deref(), Some("damaged-food"));
        assert_eq!(trials[0].trial.data_bundle_ids, vec!["orders".to_string()]);
    }

    #[test]
    fn plan_trial_pager_derives_stable_score_run_ids_offline() {
        // Pins: plan re-expansion cannot mint different score-run IDs for the same trial key.
        let plan_revision_uid = fixture_uuid(1);
        let run_uid = fixture_uuid(2);
        let definition = fixture_plan();

        let first =
            collect_pager_trials(run_uid, plan_revision_uid, &definition, &fixture_policy())
                .expect("valid plan matrix expands");
        let second =
            collect_pager_trials(run_uid, plan_revision_uid, &definition, &fixture_policy())
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
    fn plan_trial_pager_rejects_empty_matrix_dimensions_offline() {
        // Pins: empty plan dimensions fail before the run enters the polling loop.
        let mut definition = fixture_plan();
        definition.simulation.scenarios.clear();

        let error = collect_pager_trials(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &fixture_policy(),
        )
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
    fn plan_variant_config_cannot_inject_an_agent_loop_session_offline() {
        // Pins: a published plan artifact cannot make an expanded trial target
        // attach to a caller-owned session. The agent-loop target carries no
        // session field, so a `session_id` key in the authored variant config is
        // not readable into anything the run can act on.
        let mut definition = fixture_plan();
        definition.target_variants[0].config =
            json!({"prompt": "start", "session_id": fixture_uuid(42)});

        let target = target_for_plan_variant(&definition, &definition.target_variants[0])
            .expect("an agent-loop plan variant projects");

        let encoded = serde_json::to_value(&target).expect("target serializes");
        assert_eq!(
            encoded.get("session_id"),
            None,
            "an expanded agent-loop target must not carry a session: {encoded}"
        );
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
    fn plan_trial_pager_rejects_agent_loop_without_target_model_offline() {
        // Pins: agent-loop plan fanout refuses to admit trials with no target model to drive.
        let mut definition = fixture_plan();
        definition.target_model = None;

        let error = collect_pager_trials(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &fixture_policy(),
        )
        .expect_err("agent-loop plan without target_model should fail expansion");

        assert!(matches!(error, PlanExpansionError::MissingTargetModel));
    }

    #[test]
    fn plan_trial_pager_rejects_execution_template_variant_without_exact_template_offline() {
        // Pins: execution-template variants cannot expand without an exact immutable revision.
        let mut definition = fixture_plan();
        definition.target_variants[0].kind = ExperimentTargetKind::ExecutionTemplate;
        definition.target_variants[0].config = json!({"objective": "Run the template."});

        let error = collect_pager_trials(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &fixture_policy(),
        )
        .expect_err("execution-template variant without template should fail expansion");

        assert!(matches!(
            error,
            PlanExpansionError::MissingExecutionTemplate
        ));
    }

    #[test]
    fn plan_trial_pager_rejects_blank_execution_template_objective_offline() {
        // Pins: execution-template plan fanout requires a meaningful explicit objective.
        let mut definition = fixture_plan();
        definition.target_variants[0].kind = ExperimentTargetKind::ExecutionTemplate;
        definition.target_variants[0].config = json!({
            "template": {
                "skill_ref": "skill://damaged-food-order",
                "revision_uid": fixture_uuid(77),
            },
            "objective": "  \n",
        });

        let error = collect_pager_trials(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &fixture_policy(),
        )
        .expect_err("blank execution-template objective should fail expansion");

        assert!(matches!(
            error,
            PlanExpansionError::MissingExecutionObjective
        ));
    }

    #[test]
    fn execution_template_plan_projection_pins_objective_revision_and_input_offline() {
        // Pins: plan fanout preserves one exact template/objective pair for every trial.
        let mut definition = fixture_plan();
        let revision_uid = fixture_uuid(77);
        definition.target_variants = vec![ExperimentTargetVariant {
            key: "template".to_string(),
            kind: ExperimentTargetKind::ExecutionTemplate,
            config: json!({
                "template": {
                    "skill_ref": "skill://damaged-food-order",
                    "revision_uid": revision_uid,
                },
                "objective": "Resolve the damaged order.",
                "input": {"order_id": "order-123"},
                "session_id": null,
                "idempotency_key": "template-plan-key",
            }),
            ui: json!({}),
        }];

        let projection = project_plan_run(&definition, fixture_uuid(1), "plan", "run")
            .expect("exact execution-template plan should project");

        let ExperimentTarget::ExecutionTemplate {
            template,
            objective,
            input,
            session_id,
            idempotency_key,
        } = projection.target
        else {
            panic!("projection should retain execution-template target");
        };
        assert_eq!(template.revision_uid, revision_uid);
        assert_eq!(objective, "Resolve the damaged order.");
        assert_eq!(input, json!({"order_id": "order-123"}));
        assert!(session_id.is_none());
        assert_eq!(idempotency_key.as_deref(), Some("template-plan-key"));
        assert_eq!(
            projection.variant.execution_template,
            Some(template.clone())
        );
        assert_eq!(
            projection.artifact_revision_uids,
            vec![fixture_uuid(1), revision_uid]
        );
    }

    #[test]
    fn plan_trial_pager_does_not_allow_variant_config_to_override_policy_decoding_offline() {
        // Pins: simulator decoding is certified policy state, not mutable target-variant config.
        let mut definition = fixture_plan();
        definition.target_variants[0].config =
            json!({"prompt": "start", "simulator_temperature": 1e40});

        let trials = collect_pager_trials(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &fixture_policy(),
        )
        .expect("variant metadata cannot alter simulator decoding");

        assert_eq!(
            trials[0]
                .trial
                .simulator
                .policy
                .components
                .decoding
                .temperature_milli,
            200
        );
    }

    #[test]
    fn plan_trial_pager_rejects_unparsable_agent_selector_offline() {
        // Pins: a malformed `agent` selector in variant config surfaces as a validation error,
        // not a panic or a silently dropped selector.
        let mut definition = fixture_plan();
        definition.target_variants[0].config =
            json!({"prompt": "start", "agent": "not-a-selector"});

        let error = collect_pager_trials(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &fixture_policy(),
        )
        .expect_err("malformed agent selector should fail expansion");

        assert!(matches!(
            error,
            PlanExpansionError::InvalidAgentSelector { .. }
        ));
    }

    #[test]
    fn plan_trial_count_refuses_an_overflowing_matrix_before_allocating_offline() {
        // Pins the checked multiplication itself. The dimension ceilings make an
        // overflowing product unreachable through a real plan, which is exactly
        // why the arithmetic has to be proven here: if this ever wraps, a plan
        // whose dimensions are individually legal could report a tiny trial
        // count and then expand forever. Nothing is allocated on this path.
        let error = plan_trial_count(u64::MAX, 2, 1, 1, 1)
            .expect_err("an overflowing matrix must not produce a trial count");
        assert!(
            matches!(error, PlanExpansionError::TrialMatrixOverflow),
            "unexpected error: {error:?}"
        );

        let error = plan_trial_count(u64::MAX, 1, 1, 1, u64::MAX)
            .expect_err("an overflowing repetition factor must not produce a trial count");
        assert!(
            matches!(error, PlanExpansionError::TrialMatrixOverflow),
            "unexpected error: {error:?}"
        );

        // A product that fits in u64 but not in the ceiling is a different,
        // equally refused, outcome.
        let error = plan_trial_count(u64::from(MAX_PLAN_TOTAL_TRIALS) + 1, 1, 1, 1, 1)
            .expect_err("one trial over the ceiling must be refused");
        assert!(
            matches!(
                error,
                PlanExpansionError::TrialMatrixTooLarge { actual, limit }
                    if actual == u64::from(MAX_PLAN_TOTAL_TRIALS) + 1
                        && limit == u64::from(MAX_PLAN_TOTAL_TRIALS)
            ),
            "unexpected error: {error:?}"
        );
        assert_eq!(
            plan_trial_count(u64::from(MAX_PLAN_TOTAL_TRIALS), 1, 1, 1, 1)
                .expect("a matrix exactly at the ceiling is legal"),
            u64::from(MAX_PLAN_TOTAL_TRIALS)
        );
    }

    #[test]
    fn plan_matrix_shape_refuses_a_legal_product_one_trial_over_the_ceiling_offline() {
        // Pins the defect the per-dimension limits do not cover: 50 scenarios
        // and 100 repetitions are each far inside their own ceilings, and their
        // product is exactly at the total-trial ceiling. One more scenario is a
        // 5_100-trial run that no per-dimension check would ever see.
        let at_ceiling = fixture_matrix(50, 1, 1, 1, MAX_PLAN_TRIALS_PER_COMBINATION);
        let shape = plan_matrix_shape(&at_ceiling).expect("a matrix at the ceiling is legal");
        assert_eq!(shape.total_trials, MAX_PLAN_TOTAL_TRIALS);

        let over_ceiling = fixture_matrix(51, 1, 1, 1, MAX_PLAN_TRIALS_PER_COMBINATION);
        let error = plan_matrix_shape(&over_ceiling)
            .expect_err("a matrix over the ceiling must be refused");

        assert!(
            matches!(
                error,
                PlanExpansionError::TrialMatrixTooLarge { actual: 5_100, .. }
            ),
            "unexpected error: {error:?}"
        );
        let projection_error = project_plan_run(&over_ceiling, fixture_uuid(1), "plan", "run")
            .expect_err("run admission must reject the same oversized matrix");
        assert!(matches!(
            projection_error,
            PlanExpansionError::TrialMatrixTooLarge { actual: 5_100, .. }
        ));
    }

    #[test]
    fn plan_matrix_shape_refuses_every_dimension_one_block_over_its_limit_offline() {
        // Pins each matrix dimension's exact upper bound, and that the plan is
        // refused rather than truncated to the limit.
        for (dimension, limit) in [
            ("scenario", MAX_PLAN_SCENARIOS),
            ("persona", MAX_PLAN_PERSONAS),
            ("profile", MAX_PLAN_PROFILES),
            ("target variant", MAX_PLAN_TARGET_VARIANTS),
        ] {
            let at_limit = fixture_dimension(dimension, limit);
            plan_matrix_shape(&at_limit)
                .unwrap_or_else(|error| panic!("{dimension} at its limit must pass: {error}"));

            let over_limit = fixture_dimension(dimension, limit + 1);
            let Err(error) = plan_matrix_shape(&over_limit) else {
                panic!("{dimension} one over its limit must be refused");
            };
            assert!(
                matches!(
                    error,
                    PlanExpansionError::PlanDimensionTooLarge { dimension: named, actual, limit: reported }
                        if named == dimension
                            && actual == u64::from(limit) + 1
                            && reported == u64::from(limit)
                ),
                "unexpected {dimension} error: {error:?}"
            );
        }
    }

    #[test]
    fn plan_matrix_shape_refuses_zero_and_over_limit_scalars_offline() {
        // Pins that repetitions, parallelism, scenario turn caps, and declared
        // budget are all rejected at zero and at one over their ceiling. Zero
        // used to be silently clamped to one repetition, which hid the fact
        // that a plan had declared no usable value at all.
        /// Builds a plan with one scalar field set to `value`.
        type ScalarCase = (
            &'static str,
            Box<dyn Fn(u32) -> ExperimentPlanDefinition>,
            u32,
        );

        let cases: Vec<ScalarCase> = vec![
            (
                "trials_per_combination",
                Box::new(|value| {
                    let mut definition = fixture_plan();
                    definition.trials_per_combination = value;
                    definition
                }),
                MAX_PLAN_TRIALS_PER_COMBINATION,
            ),
            (
                "parallelism",
                Box::new(|value| {
                    let mut definition = fixture_plan();
                    definition.parallelism = value;
                    definition
                }),
                MAX_PLAN_PARALLELISM,
            ),
            (
                "scenario max_turns",
                Box::new(|value| {
                    let mut definition = fixture_plan();
                    definition.simulation.scenarios[0].max_turns = value;
                    definition
                }),
                MAX_SCENARIO_TURNS,
            ),
            (
                "budget.max_total_cents",
                Box::new(|value| {
                    let mut definition = fixture_plan();
                    definition.budget.max_total_cents = value;
                    definition.budget.max_trial_cents = None;
                    definition
                }),
                MAX_PLAN_TOTAL_COST_CENTS,
            ),
        ];

        for (field, build, limit) in cases {
            plan_matrix_shape(&build(limit))
                .unwrap_or_else(|error| panic!("{field} at its limit must pass: {error}"));

            for value in [0, limit + 1] {
                let Err(error) = plan_matrix_shape(&build(value)) else {
                    panic!("{field} = {value} must be refused");
                };
                assert!(
                    matches!(
                        error,
                        PlanExpansionError::PlanValueOutOfRange { field: named, actual, .. }
                            if named == field && actual == u64::from(value)
                    ),
                    "unexpected {field} error: {error:?}"
                );
            }
        }
    }

    #[test]
    fn plan_matrix_shape_refuses_oversized_plan_bytes_offline() {
        // Pins that a plan staying inside every count limit cannot smuggle
        // unbounded bytes through one free-form field, one block, or the
        // document as a whole.
        let mut definition = fixture_plan();
        definition.target_variants[0].config = json!({
            "prompt": "x".repeat(MAX_PLAN_FIELD_BYTES + 1),
        });

        let error = plan_matrix_shape(&definition)
            .expect_err("an oversized variant config must be refused");

        assert!(
            matches!(
                error,
                PlanExpansionError::PlanTooLarge { ref field, limit, .. }
                    if field == "target_variants[0].config" && limit == MAX_PLAN_FIELD_BYTES
            ),
            "unexpected error: {error:?}"
        );

        let mut definition = fixture_plan();
        definition.simulation.scenarios[0].initial_situation = "x".repeat(MAX_PLAN_BLOCK_BYTES + 1);
        let error = plan_matrix_shape(&definition)
            .expect_err("an oversized scenario block must be refused");
        assert!(
            matches!(
                error,
                PlanExpansionError::PlanTooLarge { ref field, limit, .. }
                    if field == "simulation.scenarios[0]" && limit == MAX_PLAN_BLOCK_BYTES
            ),
            "unexpected error: {error:?}"
        );

        let mut definition = fixture_plan();
        definition.simulation.personas = (0..64)
            .map(|index| SimulationPersonaDefinition {
                id: format!("persona-{index}"),
                voice: "x".repeat(MAX_PLAN_BLOCK_BYTES - 1_024),
                ..SimulationPersonaDefinition::default()
            })
            .collect();
        let error =
            plan_matrix_shape(&definition).expect_err("an oversized plan document must be refused");
        assert!(
            matches!(
                error,
                PlanExpansionError::PlanTooLarge { ref field, limit, .. }
                    if field == "definition" && limit == MAX_PLAN_DEFINITION_BYTES
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn plan_trial_pager_emits_the_whole_matrix_one_bounded_page_at_a_time_offline() {
        // Pins that paging is equivalent to the full expansion it replaces and
        // that no page ever exceeds the page bound, so a maximal matrix is
        // dispatched without ever holding 5_000 trials at once.
        let definition = fixture_matrix(50, 1, 1, 1, MAX_PLAN_TRIALS_PER_COMBINATION);
        let policy = fixture_policy();
        let mut pager = PlanTrialPager::new(fixture_uuid(2), fixture_uuid(1), &definition, &policy)
            .expect("a matrix at the ceiling pages");
        assert_eq!(pager.remaining(), MAX_PLAN_TOTAL_TRIALS);

        let mut pages = 0_usize;
        let mut keys = Vec::new();
        loop {
            let page = pager.next_page();
            if page.is_empty() {
                break;
            }
            assert!(
                page.len() <= PLAN_TRIAL_PAGE_TRIALS,
                "page of {} trials exceeds the page bound",
                page.len()
            );
            pages += 1;
            keys.extend(page.into_iter().map(|trial| trial.trial.trial_key));
        }

        assert_eq!(keys.len(), MAX_PLAN_TOTAL_TRIALS as usize);
        assert_eq!(
            pages,
            MAX_PLAN_TOTAL_TRIALS as usize / PLAN_TRIAL_PAGE_TRIALS + 1
        );
        assert_eq!(pager.remaining(), 0);
        let unique = keys.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), keys.len(), "trial keys must stay unique");

        let eager = collect_pager_trials(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &fixture_policy(),
        )
        .expect("the same matrix expands eagerly")
        .into_iter()
        .map(|trial| trial.trial.trial_key)
        .collect::<Vec<_>>();
        assert_eq!(eager, keys, "paged order must match expanded order");
    }

    #[test]
    fn plan_variants_share_the_same_simulator_seed_offline() {
        // Pins: candidate and baseline variants differ only in the target under
        // test. Their simulator randomness is paired by matrix coordinates and
        // cannot drift merely because the variant key differs.
        let definition = fixture_matrix(1, 1, 1, 2, 2);
        let trials = collect_pager_trials(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &fixture_policy(),
        )
        .expect("expand paired variants");

        assert_eq!(trials.len(), 4);
        assert_eq!(trials[0].trial.seed, trials[2].trial.seed);
        assert_eq!(trials[1].trial.seed, trials[3].trial.seed);
        assert_ne!(trials[0].trial.seed, trials[1].trial.seed);
        assert_ne!(trials[0].trial.trial_key, trials[2].trial.trial_key);
    }

    #[test]
    fn release_case_selection_expands_only_approved_sparse_tuples_offline() {
        // Pins: a release case pack selects exact tuples with per-case
        // repetitions. It must not fall back to the plan's Cartesian matrix,
        // and every candidate/control pair must retain the same simulator seed.
        let definition = fixture_matrix(3, 2, 2, 2, 5);
        let selections = vec![
            PlanCaseSelection {
                scenario_id: "scenario-0".to_string(),
                persona_id: "persona-1".to_string(),
                profile_id: "profile-0".to_string(),
                repetitions: 1,
            },
            PlanCaseSelection {
                scenario_id: "scenario-2".to_string(),
                persona_id: "persona-0".to_string(),
                profile_id: "profile-1".to_string(),
                repetitions: 3,
            },
        ];
        let policy = fixture_policy();
        let mut pager = PlanTrialPager::new_selected(
            fixture_uuid(2),
            fixture_uuid(1),
            &definition,
            &policy,
            &selections,
        )
        .expect("approved sparse selection");
        assert_eq!(pager.shape().total_trials, 8);
        let trials = pager.next_page();
        assert_eq!(trials.len(), 8);
        assert!(trials.iter().all(|trial| {
            matches!(
                (
                    trial.trial.scenario_id.as_deref(),
                    trial.trial.persona_id.as_deref(),
                    trial.trial.profile_id.as_deref(),
                ),
                (Some("scenario-0"), Some("persona-1"), Some("profile-0"))
                    | (Some("scenario-2"), Some("persona-0"), Some("profile-1"))
            )
        }));
        assert_eq!(trials[0].trial.seed, trials[1].trial.seed);
        for repetition in 0..3 {
            assert_eq!(
                trials[2 + repetition].trial.seed,
                trials[5 + repetition].trial.seed
            );
        }

        let duplicate = vec![selections[0].clone(), selections[0].clone()];
        let policy = fixture_policy();
        assert!(matches!(
            PlanTrialPager::new_selected(
                fixture_uuid(2),
                fixture_uuid(1),
                &definition,
                &policy,
                &duplicate,
            ),
            Err(PlanExpansionError::InvalidReleaseCaseSelection { .. })
        ));
    }

    /// Builds a plan whose matrix has the requested dimensions.
    fn fixture_matrix(
        scenarios: u32,
        personas: u32,
        profiles: u32,
        target_variants: u32,
        trials_per_combination: u32,
    ) -> ExperimentPlanDefinition {
        let mut definition = fixture_plan();
        definition.simulation.data_bundles.clear();
        definition.simulation.scenarios = (0..scenarios)
            .map(|index| SimulationScenarioDefinition {
                id: format!("scenario-{index}"),
                max_turns: 2,
                ..SimulationScenarioDefinition::default()
            })
            .collect();
        definition.simulation.personas = (0..personas)
            .map(|index| SimulationPersonaDefinition {
                id: format!("persona-{index}"),
                ..SimulationPersonaDefinition::default()
            })
            .collect();
        definition.simulation.profiles = (0..profiles)
            .map(|index| SimulationProfileDefinition {
                id: format!("profile-{index}"),
                ..SimulationProfileDefinition::default()
            })
            .collect();
        definition.target_variants = (0..target_variants)
            .map(|index| ExperimentTargetVariant {
                key: format!("variant-{index}"),
                kind: ExperimentTargetKind::AgentLoop,
                config: json!({"prompt": "start"}),
                ui: json!({}),
            })
            .collect();
        definition.trials_per_combination = trials_per_combination;
        definition
    }

    /// Builds a plan with `count` blocks in `dimension` and one block everywhere else.
    fn fixture_dimension(dimension: &str, count: u32) -> ExperimentPlanDefinition {
        match dimension {
            "scenario" => fixture_matrix(count, 1, 1, 1, 1),
            "persona" => fixture_matrix(1, count, 1, 1, 1),
            "profile" => fixture_matrix(1, 1, count, 1, 1),
            "target variant" => fixture_matrix(1, 1, 1, count, 1),
            other => panic!("unknown matrix dimension {other}"),
        }
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
            simulator_policy: fixture_policy().reference(),
            target_model: Some("gpt-5.1".to_string()),
            parallelism: 2,
            trials_per_combination: 2,
            budget: ExperimentBudget {
                max_total_cents: 100,
                max_trial_cents: Some(25),
                max_total_tokens: Some(10_000),
                max_trial_tokens: Some(1_000),
            },
            scorecard: Some(
                ExperimentScorecard::new(vec![ScorecardRequirement {
                    evaluator_id: "target_completed".to_string(),
                    evaluator_version: "v1".to_string(),
                    config: json!({}),
                    effect: ScorecardEffect::Blocking,
                }])
                .expect("fixture scorecard is valid"),
            ),
            learning_proposals: ExperimentLearningProposalSettings::default(),
            ui: json!({}),
        }
    }

    fn scenario_gated_plan() -> ExperimentPlanDefinition {
        let mut definition = fixture_plan();
        definition.scorecard = Some(
            ExperimentScorecard::new(vec![ScorecardRequirement {
                evaluator_id: "scenario_outcome".to_string(),
                evaluator_version: "v1".to_string(),
                config: json!({}),
                effect: ScorecardEffect::Blocking,
            }])
            .expect("fixture scenario scorecard is valid"),
        );
        definition
    }

    fn fixture_uuid(last_byte: u8) -> Uuid {
        let mut bytes = [0_u8; 16];
        bytes[15] = last_byte;
        Uuid::from_bytes(bytes)
    }
}
