//! Restate endpoint binding and registration-readiness helpers.

use moa_hands::ToolRouter;
use moa_memory_ingest::{IngestionVO, IngestionVOImpl};
use moa_providers::ProviderRegistry;
use restate_sdk::endpoint::Builder as EndpointBuilder;
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
        workflows::{Workflows, WorkflowsImpl},
    },
    workflows::{
        artifact_workflow_execution::{ArtifactWorkflowExecution, ArtifactWorkflowExecutionImpl},
        consolidate::{Consolidate, ConsolidateImpl},
        knowledge_sync_ingestion::{KnowledgeSyncIngestion, KnowledgeSyncIngestionImpl},
        turn_execution::{TurnExecution, TurnExecutionImpl},
        worker_turn_execution::{WorkerTurnExecution, WorkerTurnExecutionImpl},
    },
};

type BindEndpointFn = fn(EndpointBuilder, &EndpointBindingContext<'_>) -> EndpointBuilder;

#[derive(Clone, Copy)]
struct RestateBinding {
    name: &'static str,
    bind: Option<BindEndpointFn>,
}

impl RestateBinding {
    const fn enabled(name: &'static str, bind: BindEndpointFn) -> Self {
        Self {
            name,
            bind: Some(bind),
        }
    }

    #[cfg(any(
        not(feature = "experiments"),
        not(feature = "internal-eval-runner"),
        not(feature = "skill-learning")
    ))]
    const fn name_only(name: &'static str) -> Self {
        Self { name, bind: None }
    }
}

struct EndpointBindingContext<'a> {
    session_store: &'a Arc<moa_session::PostgresSessionStore>,
    pool: &'a sqlx::PgPool,
    providers: &'a Arc<ProviderRegistry>,
    tool_router: &'a Arc<ToolRouter>,
}

const CORE_HEAD_BINDINGS: &[RestateBinding] = &[
    RestateBinding::enabled("SessionStore", bind_session_store),
    RestateBinding::enabled("LLMGateway", bind_llm_gateway),
    RestateBinding::enabled("AgentDefinitions", bind_agent_definitions),
    RestateBinding::enabled("Agents", bind_agents),
    RestateBinding::enabled("AdminMaintenance", bind_admin_maintenance),
    RestateBinding::enabled("Artifacts", bind_artifacts),
    RestateBinding::enabled("ActionReviews", bind_action_reviews),
    RestateBinding::enabled("ApiKeys", bind_api_keys),
    RestateBinding::enabled("Authz", bind_authz),
    RestateBinding::enabled("AuthzChallenges", bind_authz_challenges),
    RestateBinding::enabled("Contacts", bind_contacts),
];

const CORE_BODY_BINDINGS: &[RestateBinding] = &[
    RestateBinding::enabled("IngestionVO", bind_ingestion_vo),
    RestateBinding::enabled("ToolExecutor", bind_tool_executor),
    RestateBinding::enabled("ActionPolicy", bind_action_policy),
    RestateBinding::enabled("GraphMemoryMaint", bind_graph_memory_maint),
    RestateBinding::enabled("Knowledge", bind_knowledge),
    RestateBinding::enabled("LearningReview", bind_learning_review),
    RestateBinding::enabled("Memory", bind_memory),
    RestateBinding::enabled("NeonMaint", bind_neon_maint),
    RestateBinding::enabled("Privacy", bind_privacy),
    RestateBinding::enabled("Skills", bind_skills),
    RestateBinding::enabled("CronJob", bind_cron_job),
    RestateBinding::enabled("Session", bind_session),
    RestateBinding::enabled("Worker", bind_worker),
    RestateBinding::enabled("Tenants", bind_tenants),
    RestateBinding::enabled("Tenant", bind_tenant),
    RestateBinding::enabled("Workflows", bind_workflows),
    RestateBinding::enabled(
        "ArtifactWorkflowExecution",
        bind_artifact_workflow_execution,
    ),
    RestateBinding::enabled("KnowledgeSyncIngestion", bind_knowledge_sync_ingestion),
    RestateBinding::enabled("Consolidate", bind_consolidate),
];

const CORE_TAIL_BINDINGS: &[RestateBinding] = &[
    RestateBinding::enabled("WorkerTurnExecution", bind_worker_turn_execution),
    RestateBinding::enabled("TurnExecution", bind_turn_execution),
];

#[cfg(feature = "experiments")]
const EXPERIMENT_SERVICE_BINDINGS: &[RestateBinding] =
    &[RestateBinding::enabled("Experiments", bind_experiments)];
#[cfg(not(feature = "experiments"))]
const EXPERIMENT_SERVICE_BINDINGS: &[RestateBinding] = &[RestateBinding::name_only("Experiments")];

#[cfg(feature = "experiments")]
const EXPERIMENT_WORKFLOW_BINDINGS: &[RestateBinding] = &[
    RestateBinding::enabled("ExperimentRun", bind_experiment_run),
    RestateBinding::enabled("ExperimentTrialRun", bind_experiment_trial_run),
];
#[cfg(not(feature = "experiments"))]
const EXPERIMENT_WORKFLOW_BINDINGS: &[RestateBinding] = &[
    RestateBinding::name_only("ExperimentRun"),
    RestateBinding::name_only("ExperimentTrialRun"),
];

#[cfg(feature = "internal-eval-runner")]
const INTERNAL_EVAL_SERVICE_BINDINGS: &[RestateBinding] =
    &[RestateBinding::enabled("Eval", bind_eval)];
#[cfg(not(feature = "internal-eval-runner"))]
const INTERNAL_EVAL_SERVICE_BINDINGS: &[RestateBinding] = &[RestateBinding::name_only("Eval")];

#[cfg(feature = "skill-learning")]
const SKILL_LEARNING_WORKFLOW_BINDINGS: &[RestateBinding] = &[RestateBinding::enabled(
    "SkillLearning",
    bind_skill_learning,
)];
#[cfg(not(feature = "skill-learning"))]
const SKILL_LEARNING_WORKFLOW_BINDINGS: &[RestateBinding] =
    &[RestateBinding::name_only("SkillLearning")];

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
    let context = EndpointBindingContext {
        session_store: &session_store,
        pool: &pool,
        providers: &providers,
        tool_router: &tool_router,
    };

    restate_bindings_for_features(
        cfg!(feature = "experiments"),
        cfg!(feature = "internal-eval-runner"),
        cfg!(feature = "skill-learning"),
    )
    .into_iter()
    .fold(Endpoint::builder(), |builder, binding| {
        let bind = binding
            .bind
            .expect("compiled feature bindings must include a bind function");
        bind(builder, &context)
    })
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
    restate_bindings_for_features(
        experiments_enabled,
        internal_eval_enabled,
        skill_learning_enabled,
    )
    .into_iter()
    .map(|binding| binding.name)
    .collect()
}

fn restate_bindings_for_features(
    experiments_enabled: bool,
    internal_eval_enabled: bool,
    skill_learning_enabled: bool,
) -> Vec<&'static RestateBinding> {
    let mut bindings = Vec::new();
    bindings.extend(CORE_HEAD_BINDINGS);
    if internal_eval_enabled {
        bindings.extend(INTERNAL_EVAL_SERVICE_BINDINGS);
    }
    if experiments_enabled {
        bindings.extend(EXPERIMENT_SERVICE_BINDINGS);
    }
    bindings.extend(CORE_BODY_BINDINGS);
    if skill_learning_enabled {
        bindings.extend(SKILL_LEARNING_WORKFLOW_BINDINGS);
    }
    if experiments_enabled {
        bindings.extend(EXPERIMENT_WORKFLOW_BINDINGS);
    }
    bindings.extend(CORE_TAIL_BINDINGS);
    bindings
}

fn bind_session_store(
    builder: EndpointBuilder,
    context: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(SessionStoreImpl::new(context.session_store.clone(), context.pool.clone()).serve())
}

fn bind_llm_gateway(
    builder: EndpointBuilder,
    context: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(LLMGatewayImpl::new(context.providers.clone()).serve())
}

fn bind_agent_definitions(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(AgentDefinitionsImpl.serve())
}

fn bind_agents(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(AgentsImpl.serve())
}

fn bind_admin_maintenance(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(AdminMaintenanceImpl.serve())
}

fn bind_artifacts(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(ArtifactsImpl.serve())
}

fn bind_action_reviews(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(ActionReviewsImpl.serve())
}

fn bind_api_keys(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(ApiKeysImpl.serve())
}

fn bind_authz(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(AuthzImpl.serve())
}

fn bind_authz_challenges(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(AuthzChallengesImpl.serve())
}

fn bind_contacts(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(ContactsImpl.serve())
}

#[cfg(feature = "internal-eval-runner")]
fn bind_eval(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(EvalImpl.serve())
}

#[cfg(feature = "experiments")]
fn bind_experiments(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(ExperimentsImpl.serve())
}

fn bind_ingestion_vo(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(IngestionVOImpl.serve())
}

fn bind_tool_executor(
    builder: EndpointBuilder,
    context: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(ToolExecutorImpl::new(context.tool_router.clone()).serve())
}

fn bind_action_policy(
    builder: EndpointBuilder,
    context: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(
        ActionPolicyImpl::new(context.tool_router.clone(), context.session_store.clone()).serve(),
    )
}

fn bind_graph_memory_maint(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(GraphMemoryMaintImpl.serve())
}

fn bind_knowledge(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(KnowledgeImpl.serve())
}

fn bind_learning_review(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(LearningReviewImpl.serve())
}

fn bind_memory(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(MemoryImpl.serve())
}

fn bind_neon_maint(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(NeonMaintImpl.serve())
}

fn bind_privacy(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(PrivacyImpl.serve())
}

fn bind_skills(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(SkillsImpl.serve())
}

fn bind_cron_job(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(CronJobImpl.serve())
}

fn bind_session(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(SessionImpl.serve())
}

fn bind_worker(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(WorkerImpl.serve())
}

fn bind_tenants(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(TenantsImpl.serve())
}

fn bind_tenant(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(TenantImpl.serve())
}

fn bind_workflows(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(WorkflowsImpl.serve())
}

fn bind_artifact_workflow_execution(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(ArtifactWorkflowExecutionImpl.serve())
}

fn bind_knowledge_sync_ingestion(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(KnowledgeSyncIngestionImpl.serve())
}

fn bind_consolidate(builder: EndpointBuilder, _: &EndpointBindingContext<'_>) -> EndpointBuilder {
    builder.bind(ConsolidateImpl.serve())
}

#[cfg(feature = "skill-learning")]
fn bind_skill_learning(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(SkillLearningImpl.serve())
}

#[cfg(feature = "experiments")]
fn bind_experiment_run(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(ExperimentRunImpl.serve())
}

#[cfg(feature = "experiments")]
fn bind_experiment_trial_run(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(ExperimentTrialRunImpl.serve())
}

fn bind_worker_turn_execution(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(WorkerTurnExecutionImpl.serve())
}

fn bind_turn_execution(
    builder: EndpointBuilder,
    _: &EndpointBindingContext<'_>,
) -> EndpointBuilder {
    builder.bind(TurnExecutionImpl.serve())
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
        restate_bindings_for_features, services_registered, services_registered_with_expected,
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
    fn compiled_bindings_are_the_expected_name_source_of_truth() {
        let bindings = restate_bindings_for_features(
            cfg!(feature = "experiments"),
            cfg!(feature = "internal-eval-runner"),
            cfg!(feature = "skill-learning"),
        );
        let mut names = HashSet::new();

        for binding in &bindings {
            assert!(
                binding.bind.is_some(),
                "compiled binding {} must have a bind function",
                binding.name
            );
            assert!(
                names.insert(binding.name),
                "duplicate Restate binding name {}",
                binding.name
            );
        }

        assert_eq!(
            expected_service_names(),
            bindings
                .iter()
                .map(|binding| binding.name)
                .collect::<Vec<_>>(),
            "readiness names must come from the compiled binding descriptors"
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
