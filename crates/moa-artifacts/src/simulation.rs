//! Behavior-lab experiment plan and embedded simulation definitions.
//!
//! Scenario prose — `initial_situation`, `goals`, `success_criteria`, and
//! `failure_criteria` — guides the simulator and human reviewers. It is not a
//! machine-verifiable outcome contract.

use moa_core::types::experiments::ExperimentScorecard;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{document::empty_object, reference::ArtifactRef};

/// Highest scenario turn limit accepted by artifact validation.
pub const MAX_SCENARIO_TURNS: u32 = 100;
/// Highest experiment-plan parallelism accepted by artifact validation.
pub const MAX_PLAN_PARALLELISM: u32 = 64;
/// Highest number of trials per matrix combination accepted by artifact validation.
pub const MAX_PLAN_TRIALS_PER_COMBINATION: u32 = 100;
/// Highest total plan cost in cents accepted by artifact validation.
pub const MAX_PLAN_TOTAL_COST_CENTS: u32 = 1_000_000;
/// Highest per-trial plan cost in cents accepted by artifact validation.
pub const MAX_PLAN_TRIAL_COST_CENTS: u32 = 100_000;
/// Highest total plan token budget accepted by artifact validation.
pub const MAX_PLAN_TOTAL_TOKENS: u32 = 10_000_000;
/// Highest per-trial token budget accepted by artifact validation.
pub const MAX_PLAN_TRIAL_TOKENS: u32 = 1_000_000;

/// Highest serialized size accepted for one whole experiment-plan definition.
///
/// The per-dimension limits below bound how many blocks a plan may declare;
/// this bounds how much memory one plan document may occupy regardless of how
/// that size is distributed across those blocks.
pub const MAX_PLAN_DEFINITION_BYTES: usize = 1_048_576;
/// Highest serialized size accepted for one scenario, persona, profile, data
/// bundle, or target variant block.
pub const MAX_PLAN_BLOCK_BYTES: usize = 65_536;
/// Highest serialized size accepted for one free-form plan field.
///
/// Applies to caller-authored JSON that the platform never interprets
/// structurally: target-variant config, profile facts, and data-source fixtures.
pub const MAX_PLAN_FIELD_BYTES: usize = 16_384;
/// Highest number of simulation scenarios accepted in one experiment plan.
pub const MAX_PLAN_SCENARIOS: u32 = 200;
/// Highest number of simulation personas accepted in one experiment plan.
pub const MAX_PLAN_PERSONAS: u32 = 100;
/// Highest number of simulation profiles accepted in one experiment plan.
pub const MAX_PLAN_PROFILES: u32 = 100;
/// Highest number of simulation data bundles accepted in one experiment plan.
pub const MAX_PLAN_DATA_BUNDLES: u32 = 100;
/// Highest number of target variants accepted in one experiment plan.
pub const MAX_PLAN_TARGET_VARIANTS: u32 = 16;
/// Highest number of trials one experiment plan run may mint.
///
/// This is the bound the per-dimension limits do not provide: the trial matrix
/// is the product of every dimension, so each dimension can be individually
/// legal while their product is not.
pub const MAX_PLAN_TOTAL_TRIALS: u32 = 5_000;
/// Provider calls one trial turn may issue: one simulator turn plus one target turn.
///
/// Used to derive the provider-call rate a plan's declared parallelism implies.
pub const PLAN_PROVIDER_CALLS_PER_TRIAL_TURN: u32 = 2;
/// Highest provider-call rate one experiment plan run may imply.
///
/// Derived as `parallelism * PLAN_PROVIDER_CALLS_PER_TRIAL_TURN`, treating one
/// trial turn per second per parallel trial as the worst case.
pub const MAX_PLAN_PROVIDER_CALL_QPS: u32 = 128;

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
#[serde(deny_unknown_fields)]
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
    /// Human-readable success criteria.
    #[serde(default)]
    pub success_criteria: Vec<String>,
    /// Human-readable failure criteria.
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
    /// Exact certified simulator policy used by every expanded trial.
    #[serde(default)]
    pub simulator_policy: SimulatorPolicyReference,
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
    /// Typed scorecard every trial expanded from this plan must satisfy.
    ///
    /// `None` is the state of a draft that has not declared its evidence
    /// requirements yet; validation reports it as an error, and plan expansion
    /// refuses it. There is no untyped scorecard form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scorecard: Option<ExperimentScorecard>,
    /// Learning proposal behavior for completed runs.
    #[serde(default)]
    pub learning_proposals: ExperimentLearningProposalSettings,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// Exact simulator-policy revision selected by an experiment plan.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SimulatorPolicyReference {
    /// Stable policy identifier.
    pub policy_uid: Uuid,
    /// Exact positive revision.
    pub revision: i32,
}

impl ExperimentPlanDefinition {
    /// Returns every artifact reference declared by this plan with its document path.
    #[must_use]
    pub(crate) fn reference_paths(&self) -> Vec<(String, ArtifactRef)> {
        self.simulation.reference_paths()
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
    /// Exact pinned skill execution-template path.
    ExecutionTemplate,
}

impl ExperimentTargetKind {
    /// Returns the persisted database representation for this target kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentLoop => "agent_loop",
            Self::ExecutionTemplate => "execution_template",
        }
    }

    /// Parses a target kind loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "agent_loop" => Some(Self::AgentLoop),
            "execution_template" => Some(Self::ExecutionTemplate),
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
                            "simulator_policy",
                            "parallelism",
                            "trials_per_combination",
                            "budget",
                            "scorecard"
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
                                            "additionalProperties": false,
                                            "required": ["id", "initial_situation", "goals", "success_criteria", "max_turns"],
                                            "properties": {
                                                "id": { "type": "string", "minLength": 1, "description": "Stable scenario identifier." },
                                                "initial_situation": { "type": "string", "minLength": 1, "description": "Non-empty starting situation shown to the simulated user." },
                                                "goals": { "type": "array", "minItems": 1, "items": { "type": "string" }, "description": "At least one user goal." },
                                                "allowed_user_intents": { "type": "array", "items": { "type": "string" } },
                                                "success_criteria": { "type": "array", "minItems": 1, "items": { "type": "string" }, "description": "At least one human-readable success criterion." },
                                                "failure_criteria": { "type": "array", "items": { "type": "string" } },
                                                "max_turns": { "type": "integer", "minimum": 1, "maximum": MAX_SCENARIO_TURNS, "description": "Maximum target-agent turns, from 1 through 100." },
                                                "admin_review_behavior": { "enum": ["stop_on_admin_review", "continue_with_synthetic_clearance", "continue_with_synthetic_denial"] },
                                                "scoring_rubric": { "type": "object" },
                                                "data_bundle_ids": { "type": "array", "items": { "type": "string" } },
                                                "ui": { "type": "object" }
                                            }
                                        }
                                    },
                                    "personas": {
                                        "type": "array",
                                        "minItems": 1,
                                        "items": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "required": ["id", "voice", "goals", "stop_behavior"],
                                            "properties": {
                                                "id": { "type": "string", "minLength": 1, "description": "Stable persona identifier." },
                                                "voice": { "type": "string", "minLength": 1, "description": "Non-empty description of how the simulated user speaks." },
                                                "goals": { "type": "array", "minItems": 1, "items": { "type": "string" }, "description": "At least one user goal." },
                                                "constraints": { "type": "array", "items": { "type": "string" } },
                                                "temperament": { "type": "string" },
                                                "likely_missing_information": { "type": "array", "items": { "type": "string" } },
                                                "stop_behavior": { "type": "string", "minLength": 1, "description": "Non-empty rule for when and how the simulated user stops." },
                                                "ui": { "type": "object" }
                                            }
                                        }
                                    },
                                    "profiles": {
                                        "type": "array",
                                        "minItems": 1,
                                        "items": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "required": ["id", "facts"],
                                            "properties": {
                                                "id": { "type": "string", "minLength": 1, "description": "Stable profile identifier." },
                                                "facts": {
                                                    "type": "object",
                                                    "additionalProperties": false,
                                                    "required": ["summary"],
                                                    "properties": {
                                                        "summary": { "type": "string", "minLength": 1, "description": "Non-empty summary of the simulated user or account facts." }
                                                    }
                                                },
                                                "data_classification": { "type": "string" },
                                                "ui": { "type": "object" }
                                            }
                                        }
                                    },
                                    "data_bundles": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "required": ["id", "sources"],
                                            "properties": {
                                                "id": { "type": "string", "minLength": 1 },
                                                "sources": {
                                                    "type": "array",
                                                    "items": {
                                                        "type": "object",
                                                        "additionalProperties": false,
                                                        "required": ["id", "kind"],
                                                        "properties": {
                                                            "id": { "type": "string", "minLength": 1 },
                                                            "kind": { "enum": ["connector_fixture", "mock_data", "live_data_scope"] },
                                                            "connector_ref": { "type": "string" },
                                                            "fixture": { "type": "object" },
                                                            "scope": { "type": "string" },
                                                            "notes": { "type": "string" }
                                                        }
                                                    }
                                                },
                                                "ui": { "type": "object" }
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
                                        "kind": { "enum": ["agent_loop"] },
                                        "config": { "type": "object" },
                                        "ui": { "type": "object" }
                                    }
                                }
                            },
                            "simulator_policy": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["policy_uid", "revision"],
                                "properties": {
                                    "policy_uid": { "type": "string", "format": "uuid" },
                                    "revision": { "type": "integer", "minimum": 1 }
                                }
                            },
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
                            "scorecard": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["requirements"],
                                "properties": {
                                    "requirements": {
                                        "type": "array",
                                        "minItems": 1,
                                        "items": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "required": [
                                                "evaluator_id",
                                                "evaluator_version",
                                                "score_name",
                                                "value_type",
                                                "effect"
                                            ],
                                            "properties": {
                                                "evaluator_id": { "type": "string", "minLength": 1 },
                                                "evaluator_version": { "type": "string", "minLength": 1 },
                                                "score_name": { "type": "string", "minLength": 1 },
                                                "value_type": {
                                                    "enum": ["numeric", "boolean", "categorical"]
                                                },
                                                "config": { "type": "object" },
                                                "effect": { "enum": ["blocking", "informational"] }
                                            }
                                        }
                                    }
                                }
                            },
                            "learning_proposals": { "type": "object" },
                            "ui": { "type": "object" }
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        ExperimentPlanDefinition, ExperimentSimulationDefinition, ExperimentTargetKind,
        ExperimentTargetVariant, MAX_SCENARIO_TURNS, SimulationDataBundleDefinition,
        SimulationDataSource, SimulationDataSourceKind, SimulationScenarioDefinition,
        experiment_plan_response_schema,
    };
    use crate::document::{ArtifactDocument, ArtifactStatus};
    use crate::reference::ArtifactRef;
    use crate::validation::validate_for_status;

    fn data_source(
        id: &str,
        kind: SimulationDataSourceKind,
        connector_ref: Option<ArtifactRef>,
    ) -> SimulationDataSource {
        SimulationDataSource {
            id: id.to_string(),
            kind,
            connector_ref,
            fixture: json!({}),
            scope: None,
            notes: String::new(),
        }
    }

    #[test]
    fn simulation_reference_paths_expand_with_nested_index_prefixes() {
        // Pins: bundle -> simulation -> plan reference expansion prefixes each ref path with its container index.
        let connector_ref = ArtifactRef::connector("orders");

        // Only the source carrying a connector_ref contributes, and it keeps its own source index.
        let bundle = SimulationDataBundleDefinition {
            id: "fixtures".to_string(),
            sources: vec![
                data_source("mock", SimulationDataSourceKind::MockData, None),
                data_source(
                    "live",
                    SimulationDataSourceKind::ConnectorFixture,
                    Some(connector_ref.clone()),
                ),
            ],
            ..Default::default()
        };
        assert_eq!(
            bundle.reference_paths(),
            vec![(
                "sources[1].connector_ref".to_string(),
                connector_ref.clone()
            )]
        );

        // The simulation level prefixes the bundle index and the `definition.spec` root.
        let simulation = ExperimentSimulationDefinition {
            data_bundles: vec![bundle],
            ..Default::default()
        };
        assert_eq!(
            simulation.reference_paths(),
            vec![(
                "definition.spec.simulation.data_bundles[0].sources[1].connector_ref".to_string(),
                connector_ref.clone(),
            )]
        );

        // The plan level surfaces embedded-simulation refs prefixed under `definition.spec`.
        let plan = ExperimentPlanDefinition {
            simulation,
            target_variants: vec![ExperimentTargetVariant {
                key: "baseline".to_string(),
                kind: ExperimentTargetKind::AgentLoop,
                config: json!({}),
                ui: json!({}),
            }],
            ..Default::default()
        };
        assert_eq!(
            plan.reference_paths(),
            vec![(
                "definition.spec.simulation.data_bundles[0].sources[1].connector_ref".to_string(),
                connector_ref,
            )]
        );
    }

    fn plan_doc_with_max_turns(max_turns: u32) -> ArtifactDocument {
        ArtifactDocument::from_json(
            &json!({
                "api_version": "moa.artifact/v1",
                "kind": "experiment_plan",
                "metadata": { "name": "bound-plan" },
                "definition": {
                    "type": "experiment_plan",
                    "spec": {
                        "simulation": {
                            "scenarios": [{
                                "id": "scenario",
                                "initial_situation": "A user has a question.",
                                "goals": ["Resolve the question."],
                                "success_criteria": ["The agent answers."],
                                "max_turns": max_turns
                            }],
                            "personas": [{
                                "id": "persona",
                                "voice": "Direct.",
                                "goals": ["Get an answer."],
                                "stop_behavior": "Stop once answered."
                            }],
                            "profiles": [{ "id": "profile", "facts": { "tier": "standard" } }]
                        },
                        "target_variants": [{ "key": "agent", "kind": "agent_loop" }],
                        "simulator_policy": {
                            "policy_uid": "10000000-0000-0000-0000-000000000001",
                            "revision": 1
                        },
                        "parallelism": 1,
                        "trials_per_combination": 1,
                        "budget": { "max_total_cents": 100 }
                    }
                }
            })
            .to_string(),
        )
        .expect("experiment plan document parses")
    }

    #[test]
    fn scenario_max_turns_bound_is_enforced_at_the_constant() {
        // Pins: scenario max_turns accepts exactly MAX_SCENARIO_TURNS and rejects one above it.
        let max_turns_path = "definition.spec.simulation.scenarios[0].max_turns";

        let at_limit = validate_for_status(
            &plan_doc_with_max_turns(MAX_SCENARIO_TURNS),
            ArtifactStatus::Draft,
        );
        assert!(
            at_limit
                .errors
                .iter()
                .all(|error| error.path != max_turns_path),
            "max_turns at the limit should be accepted: {:?}",
            at_limit.errors
        );

        let over_limit = validate_for_status(
            &plan_doc_with_max_turns(MAX_SCENARIO_TURNS + 1),
            ArtifactStatus::Draft,
        );
        assert!(
            over_limit.errors.iter().any(|error| {
                error.path == max_turns_path
                    && error.message
                        == format!("scenario max_turns must be between 1 and {MAX_SCENARIO_TURNS}")
            }),
            "max_turns above the limit must reject with the bound message: {:?}",
            over_limit.errors
        );
    }

    #[test]
    fn generated_plan_schema_exposes_every_draft_required_simulation_field() {
        // Pins: strict provider compilation cannot close scenario/persona/profile
        // objects before exposing the fields artifact validation requires.
        let schema = experiment_plan_response_schema();
        let simulation = &schema["properties"]["definition"]["properties"]["spec"]["properties"]["simulation"]
            ["properties"];

        assert_eq!(
            simulation["scenarios"]["items"]["required"],
            json!([
                "id",
                "initial_situation",
                "goals",
                "success_criteria",
                "max_turns"
            ])
        );
        assert!(
            simulation["scenarios"]["items"]["properties"]
                .get("admin_review_behavior")
                .is_some()
        );
        assert_eq!(
            simulation["personas"]["items"]["required"],
            json!(["id", "voice", "goals", "stop_behavior"])
        );
        assert_eq!(
            simulation["profiles"]["items"]["required"],
            json!(["id", "facts"])
        );
        assert_eq!(
            simulation["profiles"]["items"]["properties"]["facts"]["required"],
            json!(["summary"])
        );
    }

    #[test]
    fn scenario_assertion_declarations_are_not_a_supported_plan_surface() {
        // Pins: there is no durable assertion-evidence producer. The artifact
        // type and generated schema both reject the deleted field instead of
        // accepting a declaration that execution can never honor.
        let error = serde_json::from_value::<SimulationScenarioDefinition>(json!({
            "id": "refund",
            "assertions": []
        }))
        .expect_err("unsupported assertion declarations must not deserialize");
        assert!(error.to_string().contains("unknown field `assertions`"));

        let schema = experiment_plan_response_schema();
        let properties = &schema["properties"]["definition"]["properties"]["spec"]["properties"]["simulation"]
            ["properties"]["scenarios"]["items"]["properties"];
        assert!(properties.get("assertions").is_none());
    }
}
