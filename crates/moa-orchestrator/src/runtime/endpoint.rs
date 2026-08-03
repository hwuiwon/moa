//! Restate endpoint binding and registration-readiness helpers.

use crate::services::knowledge::{Knowledge, KnowledgeImpl, KnowledgeService};
use crate::services::privacy::{Privacy, PrivacyImpl};
use crate::services::{
    admin_maintenance::{AdminMaintenance, AdminMaintenanceImpl},
    agent_definitions::{AgentDefinitions, AgentDefinitionsImpl},
    agents::{Agents, AgentsImpl},
    api_keys::{ApiKeys, ApiKeysImpl},
    authz_admin::{Authz, AuthzImpl},
    authz_challenges::{AuthzChallenges, AuthzChallengesImpl},
    neon_maint::{NeonMaint, NeonMaintImpl},
    security_events::{SecurityEvents, SecurityEventsImpl},
    tenants::{Tenants, TenantsImpl},
};
use crate::workflows::artifact_release_evaluation::{
    ArtifactReleaseEvaluation, ArtifactReleaseEvaluationImpl,
};
use crate::workflows::knowledge_sync_ingestion::{
    KnowledgeSyncIngestion, KnowledgeSyncIngestionImpl,
};
use crate::workflows::tenant_purge::{TenantPurge, TenantPurgeImpl};
use moa_artifacts::registry::ArtifactRegistry;
use moa_config::MoaConfig;
use restate_sdk::prelude::*;
use serde::Deserialize;
use std::sync::Arc;

use crate::handlers::authz_shim::AuthzEnforcer;
use crate::runtime::deps::RuntimeDeps;
use crate::services::connectors::restate::{ConnectorConnections, ConnectorConnectionsImpl};
use crate::services::experiments::{Experiments, ExperimentsImpl};
use crate::workflows::experiment_run::{ExperimentRun, ExperimentRunImpl};
use crate::workflows::experiment_trial_run::{ExperimentTrialRun, ExperimentTrialRunImpl};
use crate::workflows::skill_learning::{SkillLearning, SkillLearningImpl};
use crate::{
    objects::{
        cron_job::{CronJob, CronJobImpl},
        ingestion::{IngestionVO, IngestionVOImpl},
        session::{Session, SessionImpl},
        tenant::{TenantImpl, TenantObject},
        worker::{Worker, WorkerImpl},
    },
    services::{
        action_policy::{ActionPolicy, ActionPolicyImpl},
        action_reviews::{ActionReviews, ActionReviewsImpl},
        artifact_release::{ArtifactRelease, ArtifactReleaseImpl},
        artifacts::{Artifacts, ArtifactsImpl},
        contacts::{Contacts, ContactsImpl},
        execution::{Execution, ExecutionImpl},
        graph_memory_maint::{GraphMemoryMaint, GraphMemoryMaintImpl},
        learning_review::{LearningReview, LearningReviewImpl},
        llm_gateway::{LLMGateway, LLMGatewayImpl},
        memory::{Memory, MemoryImpl},
        session_store::{RestateSessionStore, SessionStoreImpl},
        skills::{Skills, SkillsImpl},
        tool_executor::{ToolExecutor, ToolExecutorImpl},
    },
    workflows::{
        consolidate::{Consolidate, ConsolidateImpl},
        execution_run::{ExecutionRun, ExecutionRunImpl},
        execution_task::{ExecutionTask, ExecutionTaskImpl},
        session_retention::{SessionRetention, SessionRetentionImpl},
        turn_events::TurnEventAppender,
        turn_execution::{TurnExecution, implementation::TurnExecutionImpl},
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
    "ConnectorConnections",
];

const CORE_BODY_SERVICE_NAMES: &[&str] = &[
    "IngestionVO",
    "ToolExecutor",
    "ActionPolicy",
    "Execution",
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
    "ExecutionRun",
    "ExecutionTask",
    "KnowledgeSyncIngestion",
    "Consolidate",
    "SessionRetention",
    "TenantPurge",
    "SecurityEvents",
];

const CORE_TAIL_SERVICE_NAMES: &[&str] = &["WorkerTurnExecution", "TurnExecution"];
const EXPERIMENT_WORKFLOW_SERVICE_NAMES: &[&str] = &[
    "ExperimentRun",
    "ExperimentTrialRun",
    // Bound next to the experiment workflows because it dispatches into them: a
    // release evaluation is an `Experiments/run` on a pinned plan, so readiness
    // that admits one without the other would advertise a release surface whose
    // dispatch target is missing.
    "ArtifactReleaseEvaluation",
];

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
pub fn build_endpoint(runtime_deps: &RuntimeDeps) -> Endpoint {
    let session_store = runtime_deps.session_store.clone();
    let pool = runtime_deps.pool.clone();
    let background_pool = runtime_deps.background_pool.clone();
    let kms = runtime_deps.kms.provider();
    let fga_client = runtime_deps.fga_client.clone();
    let providers = runtime_deps.providers.clone();
    let tool_router = runtime_deps.tool_router.clone();
    let config = runtime_deps.config.clone();
    let session_limits = config.session_limits.clone();
    let contact_token_issuer = runtime_deps.contact_token_issuer.clone();
    let credential_vault = runtime_deps.credential_vault.clone();
    let lineage = runtime_deps.lineage.handle.clone();
    let embedding_provider = runtime_deps.embedding_provider.clone();
    let channel_adapters = Arc::new(runtime_deps.channel_adapters.clone());
    let runtime_cache = runtime_deps.runtime_cache.clone();
    let score_lineage = runtime_deps.lineage.score_handle();
    let connector_catalogs = runtime_deps.connector_catalogs.clone();
    let connector_completion = runtime_deps.connector_completion.clone();
    let authz = AuthzEnforcer::new(fga_client.clone());
    let mut builder = Endpoint::builder()
        .bind(
            SessionStoreImpl::new(
                session_store.clone(),
                pool.clone(),
                config.clone(),
                runtime_cache.clone(),
                authz.clone(),
            )
            .serve(),
        )
        .bind(
            LLMGatewayImpl::new(providers.clone())
                .with_session_limits(session_limits.clone())
                .serve(),
        )
        .bind(
            AgentDefinitionsImpl::new(pool.clone(), connector_catalogs.clone(), authz.clone())
                .serve(),
        )
        .bind(AgentsImpl::new(pool.clone(), fga_client.clone()).serve())
        .bind(AdminMaintenanceImpl::new(pool.clone(), config.clone(), authz.clone()).serve())
        .bind(ArtifactsImpl::new(ArtifactRegistry::new(pool.clone()), authz.clone()).serve())
        .bind(
            ArtifactReleaseImpl::new(pool.clone(), connector_catalogs.clone(), authz.clone())
                .serve(),
        )
        .bind(
            ActionReviewsImpl::new(
                pool.clone(),
                session_store.clone(),
                action_review_timeout_secs(&config),
                authz.clone(),
            )
            .serve(),
        )
        .bind(ApiKeysImpl::new(pool.clone(), fga_client.clone()).serve())
        .bind(AuthzImpl::new(pool.clone(), authz.clone()).serve())
        .bind(AuthzChallengesImpl::new(pool.clone()).serve())
        .bind(
            ContactsImpl::new(
                pool.clone(),
                session_store.clone(),
                config.clone(),
                contact_token_issuer,
                runtime_deps.delivery_sink.clone(),
                authz.clone(),
            )
            .serve(),
        );

    builder = builder
        .bind(ConnectorConnectionsImpl::new(runtime_deps.connector_management.clone()).serve());

    builder = builder.bind(
        ExperimentsImpl::new(
            pool.clone(),
            providers.clone(),
            session_store.clone(),
            authz.clone(),
        )
        .serve(),
    );

    builder = builder
        .bind(IngestionVOImpl::new(runtime_deps.ingest_runtime.clone()).serve())
        .bind(
            ToolExecutorImpl::new(
                tool_router.clone(),
                connector_catalogs.clone(),
                connector_completion,
                session_store.clone(),
                session_store.clone(),
            )
            .serve(),
        )
        .bind(
            ActionPolicyImpl::new(
                tool_router.clone(),
                connector_catalogs.clone(),
                session_store.clone(),
                authz.clone(),
            )
            .serve(),
        )
        .bind(
            ExecutionImpl::new(
                pool.clone(),
                connector_catalogs.clone(),
                config.execution.clone(),
                session_store.clone(),
                authz.clone(),
            )
            .serve(),
        )
        .bind(GraphMemoryMaintImpl::new(pool.clone(), config.clone()).serve())
        .bind(SecurityEventsImpl::new(pool.clone()).serve())
        .bind(
            KnowledgeImpl::new(
                KnowledgeService::from_config(
                    pool.clone(),
                    kms.clone(),
                    credential_vault.clone(),
                    config.as_ref(),
                    runtime_cache.clone(),
                )
                .with_connector_connections(runtime_deps.connector_connections.clone()),
                authz.clone(),
            )
            .serve(),
        )
        .bind(
            LearningReviewImpl::new(
                session_store.clone(),
                pool.clone(),
                config.clone(),
                providers.clone(),
                tool_router.clone(),
                authz.clone(),
            )
            .serve(),
        )
        .bind(
            MemoryImpl::from_retrieval_engine(
                pool.clone(),
                kms.clone(),
                session_store.clone(),
                runtime_deps.retrieval_engine.clone(),
                authz.clone(),
            )
            .serve(),
        )
        .bind(NeonMaintImpl::new(config.clone()).serve())
        .bind(
            PrivacyImpl::new(
                pool.clone(),
                background_pool,
                config.compliance.clone(),
                kms.clone(),
                authz.clone(),
            )
            .serve(),
        )
        .bind(SkillsImpl::new(pool.clone(), authz.clone()).serve())
        .bind(CronJobImpl.serve())
        .bind(
            SessionImpl::new(
                session_store.clone(),
                pool.clone(),
                config.clone(),
                session_limits.clone(),
                runtime_cache.clone(),
                authz.clone(),
            )
            .serve(),
        )
        .bind(
            WorkerImpl::new(
                session_store.clone(),
                session_limits.clone(),
                providers.clone(),
                connector_catalogs.clone(),
                authz.clone(),
            )
            .serve(),
        )
        .bind(TenantsImpl::new(pool.clone(), fga_client.clone()).serve())
        .bind(TenantImpl::new(pool.clone()).serve())
        .bind(TenantPurgeImpl::new(pool.clone(), credential_vault.clone(), config.as_ref()).serve())
        .bind(
            ExecutionRunImpl::new(
                pool.clone(),
                config.execution.clone(),
                moa_core::types::identifiers::ModelId::new(
                    config
                        .models
                        .auxiliary
                        .clone()
                        .unwrap_or_else(|| config.models.main.clone()),
                ),
            )
            .serve(),
        )
        .bind(
            ExecutionTaskImpl::new(
                pool.clone(),
                session_store.clone(),
                session_limits.clone(),
                channel_adapters.clone(),
            )
            .serve(),
        )
        .bind(
            KnowledgeSyncIngestionImpl::new(
                pool.clone(),
                kms.clone(),
                credential_vault.clone(),
                config.clone(),
                runtime_cache.clone(),
            )
            .serve(),
        )
        .bind(SessionRetentionImpl::new(session_store.clone()).serve())
        .bind(ConsolidateImpl::new(pool.clone(), kms, config.clone(), embedding_provider).serve());

    {
        builder = builder.bind(
            SkillLearningImpl::new(
                session_store.clone(),
                config.clone(),
                providers.clone(),
                runtime_cache,
            )
            .serve(),
        );
    }

    builder = builder
        .bind(ArtifactReleaseEvaluationImpl::new(pool.clone()).serve())
        .bind(ExperimentRunImpl::new(pool.clone()).serve())
        .bind(
            ExperimentTrialRunImpl::new(
                pool.clone(),
                session_store.clone(),
                providers.clone(),
                score_lineage,
                config.clone(),
                authz,
            )
            .serve(),
        );

    // One durable event-append dependency, built here and owned by both turn
    // workflows, so neither reaches into global runtime state to persist events.
    let event_appender = TurnEventAppender::new(
        session_store.clone(),
        config.session.direct_turn_event_append,
    );

    builder
        .bind(
            WorkerTurnExecutionImpl::new(
                session_limits,
                session_store.clone(),
                channel_adapters.clone(),
                event_appender.clone(),
            )
            .serve(),
        )
        .bind(
            TurnExecutionImpl::new(
                session_store,
                config,
                tool_router,
                lineage,
                channel_adapters,
                event_appender,
                runtime_deps.turn_request_preparer.clone(),
            )
            .serve(),
        )
        .build()
}

/// Configured tenant action-review timeout in seconds, clamped to `i64`.
fn action_review_timeout_secs(config: &MoaConfig) -> i64 {
    i64::try_from(config.async_authz.action_review_timeout_secs).unwrap_or(i64::MAX)
}

/// Returns the service names expected for readiness in this build.
#[must_use]
pub fn expected_service_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    names.extend(CORE_HEAD_SERVICE_NAMES.iter().copied());
    names.push("Experiments");
    names.extend(CORE_BODY_SERVICE_NAMES.iter().copied());
    names.push("SkillLearning");
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
        RegisteredDeployment, RegisteredService, expected_service_names, services_registered,
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
    }

    #[test]
    fn product_expected_services_include_experiments() {
        let names = expected_service_names();

        assert!(
            !names.contains(&"Eval"),
            "the hosted tenant Eval service must not be registered"
        );
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
            names.contains(&"ExecutionRun") && names.contains(&"ExecutionTask"),
            "product readiness should include both durable execution workflows"
        );
        assert!(
            names.contains(&"Execution"),
            "product readiness should include the canonical execution service"
        );
        assert!(
            names.contains(&"TenantPurge"),
            "product readiness should include TenantPurge"
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
        let deployment_without_experiments = names
            .iter()
            .copied()
            .filter(|name| *name != "Experiments")
            .collect::<Vec<_>>();
        let deployment_without_experiment = names
            .iter()
            .copied()
            .filter(|name| *name != "ExperimentRun")
            .collect::<Vec<_>>();

        assert!(
            !services_registered_with_expected(
                &[deployment_with_services(&deployment_without_experiments)],
                &names
            ),
            "readiness must reject a deployment missing Experiments"
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
    fn skill_learning_workflow_is_always_expected() {
        // Pins: skill learning is always on — readiness requires the SkillLearning
        // workflow in every build, exactly once.
        let names = expected_service_names();

        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "SkillLearning")
                .count(),
            1,
            "readiness must expect SkillLearning exactly once"
        );
    }
}
