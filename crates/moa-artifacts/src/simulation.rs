//! Behavior-lab experiment plan and embedded simulation definitions.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{document::empty_object, reference::ArtifactRef};

/// Highest scenario turn limit accepted by artifact validation.
pub(crate) const MAX_SCENARIO_TURNS: u32 = 100;
/// Highest experiment-plan parallelism accepted by artifact validation.
pub(crate) const MAX_PLAN_PARALLELISM: u32 = 64;
/// Highest number of trials per matrix combination accepted by artifact validation.
pub(crate) const MAX_PLAN_TRIALS_PER_COMBINATION: u32 = 100;
/// Highest total plan cost in cents accepted by artifact validation.
pub(crate) const MAX_PLAN_TOTAL_COST_CENTS: u32 = 1_000_000;
/// Highest per-trial plan cost in cents accepted by artifact validation.
pub(crate) const MAX_PLAN_TRIAL_COST_CENTS: u32 = 100_000;
/// Highest total plan token budget accepted by artifact validation.
pub(crate) const MAX_PLAN_TOTAL_TOKENS: u32 = 10_000_000;
/// Highest per-trial token budget accepted by artifact validation.
pub(crate) const MAX_PLAN_TRIAL_TOKENS: u32 = 1_000_000;

/// Simulated user persona used by behavior-lab trials.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SimulationPersonaDefinition {
    /// Stable persona identifier within the experiment plan.
    #[serde(default)]
    pub id: String,
    /// How the simulated user speaks.
    #[serde(default)]
    pub voice: String,
    /// User goals this persona tries to accomplish.
    #[serde(default)]
    pub goals: Vec<String>,
    /// Behavioral constraints the simulator must respect.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Optional temperament or interaction style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperament: Option<String>,
    /// Information this user might not know initially.
    #[serde(default)]
    pub likely_missing_information: Vec<String>,
    /// When and how the simulated user should stop.
    #[serde(default)]
    pub stop_behavior: String,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// User/account facts available to a simulation trial.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SimulationProfileDefinition {
    /// Stable profile identifier within the experiment plan.
    #[serde(default)]
    pub id: String,
    /// Structured facts about the simulated user, account, order, or context.
    #[serde(default = "empty_object")]
    pub facts: Value,
    /// Optional notes about data sensitivity or intended use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_classification: Option<String>,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

impl Default for SimulationProfileDefinition {
    fn default() -> Self {
        Self {
            id: String::new(),
            facts: empty_object(),
            data_classification: None,
            ui: empty_object(),
        }
    }
}

/// Data bundle that points to fixtures, mock data, or approved live data scopes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SimulationDataBundleDefinition {
    /// Stable data-bundle identifier within the experiment plan.
    #[serde(default)]
    pub id: String,
    /// Data sources available to the simulation.
    #[serde(default)]
    pub sources: Vec<SimulationDataSource>,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

impl SimulationDataBundleDefinition {
    /// Returns every artifact reference declared by this data bundle with its document path.
    #[must_use]
    pub(crate) fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        self.sources
            .iter()
            .enumerate()
            .filter_map(|(source_index, source)| {
                source.connector_ref.as_ref().map(|artifact_ref| {
                    (
                        format!("sources[{source_index}].connector_ref"),
                        artifact_ref.clone(),
                    )
                })
            })
            .collect()
    }
}

/// One data source inside a simulation data bundle.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SimulationDataSource {
    /// Stable source identifier within the bundle.
    #[serde(default)]
    pub id: String,
    /// Source category.
    pub kind: SimulationDataSourceKind,
    /// Connector artifact that owns the fixture or live data scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector_ref: Option<ArtifactRef>,
    /// Inline fixture or mock payload.
    #[serde(default = "empty_object")]
    pub fixture: Value,
    /// Approved live data-source scope label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Optional human-readable notes.
    #[serde(default)]
    pub notes: String,
}

/// Supported simulation data-source categories.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulationDataSourceKind {
    /// Connector-owned fixture data.
    ConnectorFixture,
    /// Inline mock data supplied by the artifact.
    MockData,
    /// Approved live data scope; secrets stay in the vault.
    LiveDataScope,
}

/// Scenario definition for a simulated behavior-lab conversation.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SimulationScenarioDefinition {
    /// Stable scenario identifier within the experiment plan.
    #[serde(default)]
    pub id: String,
    /// Starting situation shown to the simulated user model.
    #[serde(default)]
    pub initial_situation: String,
    /// Scenario-specific user goals.
    #[serde(default)]
    pub goals: Vec<String>,
    /// User intents allowed during this scenario.
    #[serde(default)]
    pub allowed_user_intents: Vec<String>,
    /// Observable success criteria.
    #[serde(default)]
    pub success_criteria: Vec<String>,
    /// Observable failure criteria.
    #[serde(default)]
    pub failure_criteria: Vec<String>,
    /// Maximum target-agent turns in one trial.
    #[serde(default)]
    pub max_turns: u32,
    /// Behavior when the target enters tenant-admin review.
    #[serde(default)]
    pub admin_review_behavior: SimulationReviewBehavior,
    /// Optional scoring rubric used by a judge.
    #[serde(default = "empty_object")]
    pub scoring_rubric: Value,
    /// Data bundle IDs required by this scenario.
    #[serde(default)]
    pub data_bundle_ids: Vec<String>,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// Admin-review behavior for a simulation scenario.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulationReviewBehavior {
    /// Stop the trial when the target queues admin review.
    #[default]
    StopOnAdminReview,
    /// Continue by returning a synthetic clearance.
    ContinueWithSyntheticClearance,
    /// Continue by returning a synthetic denial.
    ContinueWithSyntheticDenial,
}

/// Experiment plan describing a matrix of behavior-lab trials.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExperimentPlanDefinition {
    /// Embedded simulation matrix included in the plan.
    #[serde(default)]
    pub simulation: ExperimentSimulationDefinition,
    /// Target variants under test.
    #[serde(default)]
    pub target_variants: Vec<ExperimentTargetVariant>,
    /// Simulator model identifier.
    #[serde(default)]
    pub simulator_model: String,
    /// Optional target model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_model: Option<String>,
    /// Maximum number of concurrently running trials.
    #[serde(default)]
    pub parallelism: u32,
    /// Number of trials per scenario/persona/profile/variant combination.
    #[serde(default)]
    pub trials_per_combination: u32,
    /// Cost and token budget guardrails.
    #[serde(default)]
    pub budget: ExperimentBudget,
    /// Scorecard or judge configuration.
    #[serde(default = "empty_object")]
    pub scorecard: Value,
    /// Learning proposal behavior for completed runs.
    #[serde(default)]
    pub learning_proposals: ExperimentLearningProposalSettings,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

impl ExperimentPlanDefinition {
    /// Returns every artifact reference declared by this plan with its document path.
    #[must_use]
    pub(crate) fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        let mut refs = self.simulation.reference_paths();
        refs.extend(self.target_variants.iter().enumerate().filter_map(
            |(variant_index, variant)| {
                variant.workflow_ref.as_ref().map(|artifact_ref| {
                    (
                        format!("definition.spec.target_variants[{variant_index}].workflow_ref"),
                        artifact_ref.clone(),
                    )
                })
            },
        ));
        refs
    }
}

/// Embedded simulation matrix for one experiment plan.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExperimentSimulationDefinition {
    /// Scenario definitions included in the trial matrix.
    #[serde(default)]
    pub scenarios: Vec<SimulationScenarioDefinition>,
    /// Persona definitions included in the trial matrix.
    #[serde(default)]
    pub personas: Vec<SimulationPersonaDefinition>,
    /// Profile definitions included in the trial matrix.
    #[serde(default)]
    pub profiles: Vec<SimulationProfileDefinition>,
    /// Data bundles available to scenarios in this plan.
    #[serde(default)]
    pub data_bundles: Vec<SimulationDataBundleDefinition>,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

impl ExperimentSimulationDefinition {
    /// Returns every artifact reference declared by embedded simulation blocks.
    #[must_use]
    pub(crate) fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        let mut refs = Vec::new();
        for (bundle_index, bundle) in self.data_bundles.iter().enumerate() {
            refs.extend(
                bundle
                    .reference_paths()
                    .into_iter()
                    .map(|(path, artifact_ref)| {
                        (
                            format!(
                                "definition.spec.simulation.data_bundles[{bundle_index}].{path}"
                            ),
                            artifact_ref,
                        )
                    }),
            );
        }
        refs
    }
}

/// One target variant in an experiment plan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExperimentTargetVariant {
    /// Stable variant key within the plan.
    #[serde(default)]
    pub key: String,
    /// Target runtime kind.
    pub kind: ExperimentTargetKind,
    /// Workflow artifact for workflow targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_ref: Option<ArtifactRef>,
    /// Runtime configuration for this variant.
    #[serde(default = "empty_object")]
    pub config: Value,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// Runtime target kind for one experiment-plan variant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentTargetKind {
    /// Existing open-ended agent-loop session path.
    AgentLoop,
    /// Artifact-backed workflow runtime path.
    Workflow,
}

impl ExperimentTargetKind {
    /// Returns the persisted database representation for this target kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentLoop => "agent_loop",
            Self::Workflow => "workflow",
        }
    }

    /// Parses a target kind loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "agent_loop" => Some(Self::AgentLoop),
            "workflow" => Some(Self::Workflow),
            _ => None,
        }
    }
}

/// Cost and token limits for an experiment plan.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExperimentBudget {
    /// Maximum total spend for one run, in cents.
    #[serde(default)]
    pub max_total_cents: u32,
    /// Optional maximum spend for one trial, in cents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_trial_cents: Option<u32>,
    /// Optional maximum total model tokens for one run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u32>,
    /// Optional maximum model tokens for one trial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_trial_tokens: Option<u32>,
}

/// Learning-candidate proposal settings for an experiment plan.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExperimentLearningProposalSettings {
    /// Whether completed runs may propose human-reviewed learning candidates.
    #[serde(default)]
    pub enabled: bool,
    /// Minimum aggregate score delta required before proposal creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score_delta: Option<u32>,
}

/// Returns the provider-facing JSON Schema for generated experiment-plan artifacts.
#[must_use]
pub fn experiment_plan_response_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["api_version", "kind", "metadata", "status", "definition"],
        "properties": {
            "api_version": { "const": "moa.artifact/v1" },
            "kind": { "const": "experiment_plan" },
            "metadata": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "minLength": 1 },
                    "description": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "version": { "type": "string" }
                }
            },
            "status": { "const": "draft" },
            "definition": {
                "type": "object",
                "additionalProperties": false,
                "required": ["type", "spec"],
                "properties": {
                    "type": { "const": "experiment_plan" },
                    "spec": {
                        "type": "object",
                        "additionalProperties": true,
                        "required": [
                            "simulation",
                            "target_variants",
                            "simulator_model",
                            "parallelism",
                            "trials_per_combination",
                            "budget"
                        ],
                        "properties": {
                            "simulation": {
                                "type": "object",
                                "additionalProperties": true,
                                "required": ["scenarios", "personas", "profiles"],
                                "properties": {
                                    "scenarios": {
                                        "type": "array",
                                        "minItems": 1,
                                        "items": {
                                            "type": "object",
                                            "required": ["id"],
                                            "properties": {
                                                "id": { "type": "string", "minLength": 1 }
                                            }
                                        }
                                    },
                                    "personas": {
                                        "type": "array",
                                        "minItems": 1,
                                        "items": {
                                            "type": "object",
                                            "required": ["id"],
                                            "properties": {
                                                "id": { "type": "string", "minLength": 1 }
                                            }
                                        }
                                    },
                                    "profiles": {
                                        "type": "array",
                                        "minItems": 1,
                                        "items": {
                                            "type": "object",
                                            "required": ["id"],
                                            "properties": {
                                                "id": { "type": "string", "minLength": 1 }
                                            }
                                        }
                                    },
                                    "data_bundles": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "required": ["id"],
                                            "properties": {
                                                "id": { "type": "string", "minLength": 1 }
                                            }
                                        }
                                    },
                                    "ui": { "type": "object" }
                                }
                            },
                            "target_variants": {
                                "type": "array",
                                "minItems": 1,
                                "items": {
                                    "type": "object",
                                    "additionalProperties": true,
                                    "required": ["key", "kind"],
                                    "properties": {
                                        "key": { "type": "string", "minLength": 1 },
                                        "kind": { "enum": ["agent_loop", "workflow"] },
                                        "workflow_ref": {
                                            "type": "string",
                                            "pattern": "^workflow://.+"
                                        },
                                        "config": { "type": "object" },
                                        "ui": { "type": "object" }
                                    }
                                }
                            },
                            "simulator_model": { "type": "string", "minLength": 1 },
                            "target_model": { "type": "string", "minLength": 1 },
                            "parallelism": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": MAX_PLAN_PARALLELISM
                            },
                            "trials_per_combination": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": MAX_PLAN_TRIALS_PER_COMBINATION
                            },
                            "budget": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["max_total_cents"],
                                "properties": {
                                    "max_total_cents": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": MAX_PLAN_TOTAL_COST_CENTS
                                    },
                                    "max_trial_cents": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": MAX_PLAN_TRIAL_COST_CENTS
                                    },
                                    "max_total_tokens": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": MAX_PLAN_TOTAL_TOKENS
                                    },
                                    "max_trial_tokens": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": MAX_PLAN_TRIAL_TOKENS
                                    }
                                }
                            },
                            "scorecard": { "type": "object" },
                            "learning_proposals": { "type": "object" },
                            "ui": { "type": "object" }
                        }
                    }
                }
            }
        }
    })
}
