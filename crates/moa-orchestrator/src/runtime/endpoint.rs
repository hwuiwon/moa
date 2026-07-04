//! Restate endpoint binding and registration-readiness helpers.

use moa_hands::ToolRouter;
use moa_memory_ingest::{IngestionVO, IngestionVOImpl};
use moa_providers::ProviderRegistry;
use restate_sdk::prelude::*;
use serde::Deserialize;
use std::sync::Arc;

use crate::services::eval::{Eval, EvalImpl};
use crate::services::experiments::{Experiments, ExperimentsImpl};
use crate::workflows::experiment_run::{ExperimentRun, ExperimentRunImpl};
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
        .bind(ContactsImpl.serve())
        .bind(EvalImpl.serve())
        .bind(ExperimentsImpl.serve());

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

    builder = builder
        .bind(ExperimentRunImpl.serve())
        .bind(ExperimentTrialRunImpl.serve());

    builder
        .bind(WorkerTurnExecutionImpl.serve())
        .bind(TurnExecutionImpl.serve())
        .build()
}

/// Returns the service names expected for readiness in this build.
#[must_use]
pub fn expected_service_names() -> Vec<&'static str> {
    expected_service_names_for_features(cfg!(feature = "skill-learning"))
}

fn expected_service_names_for_features(skill_learning_enabled: bool) -> Vec<&'static str> {
    let mut names = Vec::new();
    names.extend(CORE_HEAD_SERVICE_NAMES.iter().copied());
    names.push("Eval");
    names.push("Experiments");
    names.extend(CORE_BODY_SERVICE_NAMES.iter().copied());
    if skill_learning_enabled {
        names.push("SkillLearning");
    }
    names.extend(EXPERIMENT_WORKFLOW_SERVICE_NAMES.iter().copied());
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
        expected_service_names_for_features, services_registered,
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
    fn expected_service_names_are_unique_for_compiled_features() {
        let mut names = HashSet::new();

        for name in expected_service_names() {
            assert!(names.insert(name), "duplicate Restate binding name {name}");
        }

        assert_eq!(
            expected_service_names(),
            expected_service_names_for_features(cfg!(feature = "skill-learning")),
            "readiness names must match compiled feature flags"
        );
    }

    #[test]
    fn product_expected_services_include_eval_and_experiments() {
        let names = expected_service_names_for_features(false);

        assert_eq!(names.iter().filter(|name| **name == "Eval").count(), 1);
        assert_eq!(
            names.iter().filter(|name| **name == "Experiments").count(),
            1
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "ExperimentRun")
                .count(),
            1
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "ExperimentTrialRun")
                .count(),
            1
        );
        assert!(
            names.contains(&"ProcedureExecution"),
            "product readiness should include ProcedureExecution"
        );
    }

    #[test]
    fn registration_check_requires_all_expected_services() {
        let names = expected_service_names();
        let deployments = vec![deployment_with_services(&names)];

        assert!(services_registered(&deployments));
    }

    #[test]
    fn registration_check_rejects_partial_deployments() {
        let deployments = vec![deployment_with_services(&["Health", "SessionStore"])];

        assert!(!services_registered(&deployments));
    }

    #[test]
    fn registration_check_rejects_deployment_missing_product_services() {
        let names = expected_service_names();
        let deployment_without_eval = names
            .iter()
            .copied()
            .filter(|name| *name != "Eval")
            .collect::<Vec<_>>();
        let deployment_without_experiment = names
            .iter()
            .copied()
            .filter(|name| *name != "ExperimentRun")
            .collect::<Vec<_>>();

        assert!(
            !services_registered_with_expected(
                &[deployment_with_services(&deployment_without_eval)],
                &names
            ),
            "readiness must reject a deployment missing Eval"
        );
        assert!(
            !services_registered_with_expected(
                &[deployment_with_services(&deployment_without_experiment)],
                &names
            ),
            "readiness must reject a deployment missing ExperimentRun"
        );
    }

    #[test]
    fn skill_learning_feature_adds_skill_learning_workflow() {
        let names = expected_service_names_for_features(true);

        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "SkillLearning")
                .count(),
            1,
            "skill-learning feature should add SkillLearning exactly once"
        );
        assert!(
            !expected_service_names_for_features(false).contains(&"SkillLearning"),
            "builds without the feature must not expect SkillLearning"
        );
    }
}
