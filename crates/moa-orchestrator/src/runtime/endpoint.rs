//! Restate endpoint binding and registration-readiness helpers.

use moa_hands::ToolRouter;
use moa_memory_ingest::{IngestionVO, IngestionVOImpl};
use moa_providers::ProviderRegistry;
use restate_sdk::prelude::*;
use serde::Deserialize;
use std::sync::Arc;

#[cfg(feature = "internal-eval-runner")]
use crate::services::eval::{Eval, EvalImpl};
#[cfg(feature = "experiments")]
use crate::services::experiments::{Experiments, ExperimentsImpl};
#[cfg(feature = "internal-eval-runner")]
use crate::workflows::eval_run::{EvalRun, EvalRunImpl};
#[cfg(feature = "experiments")]
use crate::workflows::experiment_run::{ExperimentRun, ExperimentRunImpl};
#[cfg(feature = "experiments")]
use crate::workflows::experiment_trial_run::{ExperimentTrialRun, ExperimentTrialRunImpl};
#[cfg(feature = "skill-learning")]
use crate::workflows::skill_learning::{SkillLearning, SkillLearningImpl};
use crate::{
    objects::{
        cron_job::{CronJob, CronJobImpl},
        session::{Session, SessionImpl},
        sub_agent::{SubAgent, SubAgentImpl},
        workspace::{WorkspaceImpl, WorkspaceObject},
    },
    services::{
        action_reviews::{ActionReviews, ActionReviewsImpl},
        admin_maintenance::{AdminMaintenance, AdminMaintenanceImpl},
        agent_definitions::{AgentDefinitions, AgentDefinitionsImpl},
        agents::{Agents, AgentsImpl},
        analytics::{Analytics, AnalyticsImpl},
        api_keys::{ApiKeys, ApiKeysImpl},
        artifacts::{Artifacts, ArtifactsImpl},
        audit::{Audit, AuditImpl},
        authz_admin::{Authz, AuthzImpl},
        authz_challenges::{AuthzChallenges, AuthzChallengesImpl},
        contacts::{Contacts, ContactsImpl},
        graph_memory_maint::{GraphMemoryMaint, GraphMemoryMaintImpl},
        health::{Health, HealthImpl},
        learning_review::{LearningReview, LearningReviewImpl},
        lineage_admin::{LineageAdmin, LineageAdminImpl},
        llm_gateway::{LLMGateway, LLMGatewayImpl},
        memory::{Memory, MemoryImpl},
        neon_maint::{NeonMaint, NeonMaintImpl},
        privacy::{Privacy, PrivacyImpl},
        session_store::{RestateSessionStore, SessionStoreImpl},
        skills::{Skills, SkillsImpl},
        tenants::{Tenants, TenantsImpl},
        tool_executor::{ToolExecutor, ToolExecutorImpl},
        whoami::{Whoami, WhoamiImpl},
        workflows::{Workflows, WorkflowsImpl},
        workspace_store::{WorkspaceStore, WorkspaceStoreImpl},
    },
    workflows::{
        artifact_workflow_execution::{ArtifactWorkflowExecution, ArtifactWorkflowExecutionImpl},
        consolidate::{Consolidate, ConsolidateImpl},
        sub_agent_turn_execution::{SubAgentTurnExecution, SubAgentTurnExecutionImpl},
        turn_execution::{TurnExecution, TurnExecutionImpl},
    },
};

const DEFAULT_EXPECTED_SERVICE_NAMES: &[&str] = &[
    "Agents",
    "AdminMaintenance",
    "Analytics",
    "ActionReviews",
    "AgentDefinitions",
    "ArtifactWorkflowExecution",
    "Artifacts",
    "ApiKeys",
    "Audit",
    "Authz",
    "AuthzChallenges",
    "Consolidate",
    "Contacts",
    "CronJob",
    "GraphMemoryMaint",
    "Health",
    "IngestionVO",
    "LearningReview",
    "LineageAdmin",
    "LLMGateway",
    "Memory",
    "NeonMaint",
    "Privacy",
    "Session",
    "SessionStore",
    "Skills",
    "SubAgent",
    "SubAgentTurnExecution",
    "Tenants",
    "ToolExecutor",
    "TurnExecution",
    "Workspace",
    "WorkspaceStore",
    "Whoami",
    "Workflows",
];
const EXPERIMENT_SERVICE_NAMES: &[&str] = &["Experiments", "ExperimentRun", "ExperimentTrialRun"];
const INTERNAL_EVAL_SERVICE_NAMES: &[&str] = &["Eval", "EvalRun"];
const SKILL_LEARNING_SERVICE_NAMES: &[&str] = &["SkillLearning"];

/// Restate admin deployment-list response.
#[derive(Debug, Deserialize)]
pub struct DeploymentListResponse {
    /// Registered deployments returned by Restate admin.
    pub deployments: Vec<RegisteredDeployment>,
}

/// Restate deployment registration projection used by readiness checks.
#[derive(Debug, Deserialize)]
pub struct RegisteredDeployment {
    /// Restate deployment id.
    pub id: String,
    /// Services registered by this deployment.
    pub services: Vec<RegisteredService>,
    /// Handler URI registered for this deployment.
    pub uri: Option<String>,
}

/// Restate service registration projection used by readiness checks.
#[derive(Debug, Deserialize)]
pub struct RegisteredService {
    /// Registered service name.
    pub name: String,
}

/// Builds the Restate endpoint with the production binding order.
pub fn build_endpoint(
    session_store: Arc<moa_session::PostgresSessionStore>,
    pool: sqlx::PgPool,
    providers: Arc<ProviderRegistry>,
    tool_router: Arc<ToolRouter>,
) -> Endpoint {
    let endpoint = Endpoint::builder()
        .bind(HealthImpl.serve())
        .bind(SessionStoreImpl::new(session_store.clone(), pool).serve())
        .bind(LLMGatewayImpl::new(providers).serve())
        .bind(AgentDefinitionsImpl.serve())
        .bind(AgentsImpl.serve())
        .bind(AdminMaintenanceImpl.serve())
        .bind(AnalyticsImpl.serve())
        .bind(ArtifactsImpl.serve())
        .bind(ActionReviewsImpl.serve())
        .bind(ApiKeysImpl.serve())
        .bind(AuditImpl.serve())
        .bind(AuthzImpl.serve())
        .bind(AuthzChallengesImpl.serve())
        .bind(ContactsImpl.serve());
    #[cfg(feature = "internal-eval-runner")]
    let endpoint = endpoint.bind(EvalImpl.serve());
    #[cfg(feature = "experiments")]
    let endpoint = endpoint.bind(ExperimentsImpl.serve());
    let endpoint = endpoint
        .bind(IngestionVOImpl.serve())
        .bind(ToolExecutorImpl::new(tool_router.clone()).serve())
        .bind(WorkspaceStoreImpl::new(tool_router.clone()).serve())
        .bind(GraphMemoryMaintImpl.serve())
        .bind(LearningReviewImpl.serve())
        .bind(LineageAdminImpl.serve())
        .bind(MemoryImpl.serve())
        .bind(NeonMaintImpl.serve())
        .bind(PrivacyImpl.serve())
        .bind(SkillsImpl.serve())
        .bind(CronJobImpl.serve())
        .bind(SessionImpl.serve())
        .bind(SubAgentImpl.serve())
        .bind(TenantsImpl.serve())
        .bind(WorkspaceImpl.serve())
        .bind(WhoamiImpl.serve())
        .bind(WorkflowsImpl.serve())
        .bind(ArtifactWorkflowExecutionImpl.serve())
        .bind(ConsolidateImpl.serve());
    #[cfg(feature = "internal-eval-runner")]
    let endpoint = endpoint.bind(EvalRunImpl.serve());
    #[cfg(feature = "skill-learning")]
    let endpoint = endpoint.bind(SkillLearningImpl.serve());
    #[cfg(feature = "experiments")]
    let endpoint = endpoint
        .bind(ExperimentRunImpl.serve())
        .bind(ExperimentTrialRunImpl.serve());
    endpoint
        .bind(SubAgentTurnExecutionImpl.serve())
        .bind(TurnExecutionImpl.serve())
        .build()
}

/// Returns the service names expected for readiness in this build.
#[must_use]
pub fn expected_service_names() -> Vec<&'static str> {
    expected_service_names_for_features(
        cfg!(feature = "experiments"),
        cfg!(feature = "internal-eval-runner"),
        cfg!(feature = "skill-learning"),
    )
}

#[cfg(test)]
fn expected_service_names_for_internal_eval(internal_eval_enabled: bool) -> Vec<&'static str> {
    expected_service_names_for_features(
        cfg!(feature = "experiments"),
        internal_eval_enabled,
        cfg!(feature = "skill-learning"),
    )
}

fn expected_service_names_for_features(
    experiments_enabled: bool,
    internal_eval_enabled: bool,
    skill_learning_enabled: bool,
) -> Vec<&'static str> {
    let mut names = DEFAULT_EXPECTED_SERVICE_NAMES.to_vec();
    if experiments_enabled {
        names.extend_from_slice(EXPERIMENT_SERVICE_NAMES);
    }
    if internal_eval_enabled {
        names.extend_from_slice(INTERNAL_EVAL_SERVICE_NAMES);
    }
    if skill_learning_enabled {
        names.extend_from_slice(SKILL_LEARNING_SERVICE_NAMES);
    }
    names
}

/// Returns true when any Restate deployment contains every expected service.
#[must_use]
pub fn services_registered(deployments: &[RegisteredDeployment]) -> bool {
    let expected_services = expected_service_names();
    services_registered_with_expected(deployments, &expected_services)
}

fn services_registered_with_expected(
    deployments: &[RegisteredDeployment],
    expected_services: &[&str],
) -> bool {
    deployments.iter().any(|deployment| {
        expected_services.iter().all(|expected| {
            deployment
                .services
                .iter()
                .any(|service| service.name == *expected)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{
        RegisteredDeployment, RegisteredService, expected_service_names_for_features,
        expected_service_names_for_internal_eval, services_registered,
        services_registered_with_expected,
    };

    fn deployment_with_services(services: &[&str]) -> RegisteredDeployment {
        RegisteredDeployment {
            id: "dp_test".to_string(),
            uri: Some("http://localhost:10020".to_string()),
            services: services
                .iter()
                .map(|name| RegisteredService {
                    name: (*name).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn default_expected_services_hide_hosted_eval() {
        let names = expected_service_names_for_internal_eval(false);

        assert!(
            !names.contains(&"Eval"),
            "default product readiness must not expect hosted Eval service"
        );
        assert!(
            !names.contains(&"EvalRun"),
            "default product readiness must not expect hosted EvalRun workflow"
        );
        assert_eq!(
            names.contains(&"Experiments"),
            cfg!(feature = "experiments"),
            "default product readiness should match the experiments feature"
        );
        assert_eq!(
            names.contains(&"ExperimentRun"),
            cfg!(feature = "experiments"),
            "default product readiness should match the experiments feature"
        );
        assert_eq!(
            names.contains(&"ExperimentTrialRun"),
            cfg!(feature = "experiments"),
            "default product readiness should match the experiments feature"
        );
        assert!(
            names.contains(&"ArtifactWorkflowExecution"),
            "default product readiness should include ArtifactWorkflowExecution"
        );
    }

    #[test]
    fn internal_eval_gate_adds_hosted_eval_services() {
        let names = expected_service_names_for_internal_eval(true);

        assert_eq!(
            names.iter().filter(|name| **name == "Eval").count(),
            1,
            "internal eval gate should add Eval exactly once"
        );
        assert_eq!(
            names.iter().filter(|name| **name == "EvalRun").count(),
            1,
            "internal eval gate should add EvalRun exactly once"
        );
        assert_eq!(
            names.contains(&"Experiments"),
            cfg!(feature = "experiments"),
            "internal eval mode should preserve the experiments feature state"
        );
        assert_eq!(
            names.contains(&"ExperimentRun"),
            cfg!(feature = "experiments"),
            "internal eval mode should preserve the experiments feature state"
        );
        assert_eq!(
            names.contains(&"ExperimentTrialRun"),
            cfg!(feature = "experiments"),
            "internal eval mode should preserve the experiments feature state"
        );
    }

    #[test]
    fn experiments_feature_adds_experiment_services() {
        let names = expected_service_names_for_features(true, false, false);

        assert_eq!(
            names.iter().filter(|name| **name == "Experiments").count(),
            1,
            "experiments feature should add Experiments exactly once"
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "ExperimentRun")
                .count(),
            1,
            "experiments feature should add ExperimentRun exactly once"
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "ExperimentTrialRun")
                .count(),
            1,
            "experiments feature should add ExperimentTrialRun exactly once"
        );
        assert!(
            !expected_service_names_for_features(false, false, false).contains(&"Experiments"),
            "builds without the feature must not expect Experiments"
        );
    }

    #[test]
    fn registration_check_requires_all_expected_services() {
        let names =
            expected_service_names_for_internal_eval(cfg!(feature = "internal-eval-runner"));
        let deployments = vec![deployment_with_services(&names)];

        assert!(services_registered(&deployments));
    }

    #[test]
    fn registration_check_rejects_partial_deployments() {
        let deployments = vec![deployment_with_services(&["Health", "SessionStore"])];

        assert!(!services_registered(&deployments));
    }

    #[test]
    fn internal_eval_registration_requires_eval_and_eval_run_when_enabled() {
        let default_names = expected_service_names_for_internal_eval(false);
        let internal_names = expected_service_names_for_internal_eval(true);
        let default_deployment = vec![deployment_with_services(&default_names)];
        let internal_deployment = vec![deployment_with_services(&internal_names)];

        assert!(
            !services_registered_with_expected(&default_deployment, &internal_names),
            "internal eval readiness must reject a deployment missing Eval and EvalRun"
        );
        assert!(
            services_registered_with_expected(&internal_deployment, &internal_names),
            "internal eval readiness should accept Eval and EvalRun when explicitly enabled"
        );
    }

    #[test]
    fn skill_learning_feature_adds_skill_learning_workflow() {
        let names = expected_service_names_for_features(false, false, true);

        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "SkillLearning")
                .count(),
            1,
            "skill-learning feature should add SkillLearning exactly once"
        );
        assert!(
            !expected_service_names_for_features(false, false, false).contains(&"SkillLearning"),
            "builds without the feature must not expect SkillLearning"
        );
    }
}
