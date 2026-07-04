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
        tenant::{TenantImpl, TenantObject},
        worker::{Worker, WorkerImpl},
    },
    services::{
        action_policy::{ActionPolicy, ActionPolicyImpl},
        action_reviews::{ActionReviews, ActionReviewsImpl},
        admin_maintenance::{AdminMaintenance, AdminMaintenanceImpl},
        agent_definitions::{AgentDefinitions, AgentDefinitionsImpl},
        agents::{Agents, AgentsImpl},
        api_keys::{ApiKeys, ApiKeysImpl},
        artifacts::{Artifacts, ArtifactsImpl},
        authz_admin::{Authz, AuthzImpl},
        authz_challenges::{AuthzChallenges, AuthzChallengesImpl},
        contacts::{Contacts, ContactsImpl},
        graph_memory_maint::{GraphMemoryMaint, GraphMemoryMaintImpl},
        knowledge::{Knowledge, KnowledgeImpl},
        learning_review::{LearningReview, LearningReviewImpl},
        llm_gateway::{LLMGateway, LLMGatewayImpl},
        memory::{Memory, MemoryImpl},
        neon_maint::{NeonMaint, NeonMaintImpl},
        privacy::{Privacy, PrivacyImpl},
        session_store::{RestateSessionStore, SessionStoreImpl},
        skills::{Skills, SkillsImpl},
        tenants::{Tenants, TenantsImpl},
        tool_executor::{ToolExecutor, ToolExecutorImpl},
    },
    workflows::{
        consolidate::{Consolidate, ConsolidateImpl},
        knowledge_sync_ingestion::{KnowledgeSyncIngestion, KnowledgeSyncIngestionImpl},
        procedure_execution::{ProcedureExecution, ProcedureExecutionImpl},
        turn_execution::{TurnExecution, TurnExecutionImpl},
        worker_turn_execution::{WorkerTurnExecution, WorkerTurnExecutionImpl},
    },
};

const CORE_HEAD_SERVICE_NAMES: &[&str] = &[
    "SessionStore",
    "LLMGateway",
    "AgentDefinitions",
    "Agents",
    "AdminMaintenance",
    "Artifacts",
    "ActionReviews",
    "ApiKeys",
    "Authz",
    "AuthzChallenges",
    "Contacts",
];

const CORE_BODY_SERVICE_NAMES: &[&str] = &[
    "IngestionVO",
    "ToolExecutor",
    "ActionPolicy",
    "GraphMemoryMaint",
    "Knowledge",
    "LearningReview",
    "Memory",
    "NeonMaint",
    "Privacy",
    "Skills",
    "CronJob",
    "Session",
    "Worker",
    "Tenants",
    "Tenant",
    "ProcedureExecution",
    "KnowledgeSyncIngestion",
    "Consolidate",
];

const CORE_TAIL_SERVICE_NAMES: &[&str] = &["WorkerTurnExecution", "TurnExecution"];
const EXPERIMENT_WORKFLOW_SERVICE_NAMES: &[&str] = &["ExperimentRun", "ExperimentTrialRun"];

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
    let mut builder = Endpoint::builder()
        .bind(SessionStoreImpl::new(session_store.clone(), pool.clone()).serve())
        .bind(LLMGatewayImpl::new(providers.clone()).serve())
        .bind(AgentDefinitionsImpl.serve())
        .bind(AgentsImpl.serve())
        .bind(AdminMaintenanceImpl.serve())
        .bind(ArtifactsImpl.serve())
        .bind(ActionReviewsImpl.serve())
        .bind(ApiKeysImpl.serve())
        .bind(AuthzImpl.serve())
        .bind(AuthzChallengesImpl.serve())
        .bind(ContactsImpl.serve());

    #[cfg(feature = "internal-eval-runner")]
    {
        builder = builder.bind(EvalImpl.serve());
    }

    #[cfg(feature = "experiments")]
    {
        builder = builder.bind(ExperimentsImpl.serve());
    }

    builder = builder
        .bind(IngestionVOImpl.serve())
        .bind(ToolExecutorImpl::new(tool_router.clone()).serve())
        .bind(ActionPolicyImpl::new(tool_router.clone(), session_store.clone()).serve())
        .bind(GraphMemoryMaintImpl.serve())
        .bind(KnowledgeImpl.serve())
        .bind(LearningReviewImpl.serve())
        .bind(MemoryImpl.serve())
        .bind(NeonMaintImpl.serve())
        .bind(PrivacyImpl.serve())
        .bind(SkillsImpl.serve())
        .bind(CronJobImpl.serve())
        .bind(SessionImpl.serve())
        .bind(WorkerImpl.serve())
        .bind(TenantsImpl.serve())
        .bind(TenantImpl.serve())
        .bind(ProcedureExecutionImpl.serve())
        .bind(KnowledgeSyncIngestionImpl.serve())
        .bind(ConsolidateImpl.serve());

    #[cfg(feature = "skill-learning")]
    {
        builder = builder.bind(SkillLearningImpl.serve());
    }

    #[cfg(feature = "experiments")]
    {
        builder = builder
            .bind(ExperimentRunImpl.serve())
            .bind(ExperimentTrialRunImpl.serve());
    }

    builder
        .bind(WorkerTurnExecutionImpl.serve())
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
    let mut names = Vec::new();
    names.extend(CORE_HEAD_SERVICE_NAMES.iter().copied());
    if internal_eval_enabled {
        names.push("Eval");
    }
    if experiments_enabled {
        names.push("Experiments");
    }
    names.extend(CORE_BODY_SERVICE_NAMES.iter().copied());
    if skill_learning_enabled {
        names.push("SkillLearning");
    }
    if experiments_enabled {
        names.extend(EXPERIMENT_WORKFLOW_SERVICE_NAMES.iter().copied());
    }
    names.extend(CORE_TAIL_SERVICE_NAMES.iter().copied());
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
    use std::collections::HashSet;

    use super::{
        RegisteredDeployment, RegisteredService, expected_service_names,
        expected_service_names_for_features, expected_service_names_for_internal_eval,
        services_registered, services_registered_with_expected,
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
    fn expected_service_names_are_unique_for_compiled_features() {
        let mut names = HashSet::new();

        for name in expected_service_names() {
            assert!(names.insert(name), "duplicate Restate binding name {name}");
        }

        assert_eq!(
            expected_service_names(),
            expected_service_names_for_features(
                cfg!(feature = "experiments"),
                cfg!(feature = "internal-eval-runner"),
                cfg!(feature = "skill-learning"),
            ),
            "readiness names must match compiled feature flags"
        );
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
            names.contains(&"ProcedureExecution"),
            "default product readiness should include ProcedureExecution"
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
    fn internal_eval_registration_requires_eval_when_enabled() {
        let default_names = expected_service_names_for_internal_eval(false);
        let internal_names = expected_service_names_for_internal_eval(true);
        let default_deployment = vec![deployment_with_services(&default_names)];
        let internal_deployment = vec![deployment_with_services(&internal_names)];

        assert!(
            !services_registered_with_expected(&default_deployment, &internal_names),
            "internal eval readiness must reject a deployment missing Eval"
        );
        assert!(
            services_registered_with_expected(&internal_deployment, &internal_names),
            "internal eval readiness should accept Eval when explicitly enabled"
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
