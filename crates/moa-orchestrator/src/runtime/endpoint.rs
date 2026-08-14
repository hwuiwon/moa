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
    sandbox_workspaces::{SandboxWorkspaceManagement, SandboxWorkspaces, SandboxWorkspacesImpl},
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
use std::{sync::Arc, time::Duration};

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
        execution_run_controller::{ExecutionRunController, ExecutionRunControllerImpl},
        ingestion::{IngestionVO, IngestionVOImpl},
        session::{Session, SessionImpl},
        tenant::{TenantImpl, TenantObject},
        worker::{Worker, WorkerImpl},
    },
    services::{
        action_policy::{ActionPolicy, ActionPolicyImpl},
        action_review_dispatcher::{ActionReviewDispatcher, ActionReviewDispatcherImpl},
        action_reviews::{ActionReviews, ActionReviewsImpl},
        artifact_release::{ArtifactRelease, ArtifactReleaseImpl},
        artifacts::{Artifacts, ArtifactsImpl},
        contacts::{Contacts, ContactsImpl},
        durable_timeout::{DurableTimeout, DurableTimeoutImpl},
        execution::{Execution, ExecutionImpl},
        execution_amendment_planner::{ExecutionAmendmentPlanner, ExecutionAmendmentPlannerImpl},
        execution_dispatcher::{
            ExecutionDispatchDrain, ExecutionDispatchDrainImpl, ExecutionDispatchReconciler,
            ExecutionDispatchReconcilerImpl, ExecutionDispatcher, ExecutionDispatcherImpl,
        },
        execution_retention::{ExecutionRetention, ExecutionRetentionImpl},
        execution_schedule::{ExecutionSchedule, ExecutionScheduleImpl},
        execution_trigger::{ExecutionTrigger, ExecutionTriggerImpl},
        graph_memory_maint::{GraphMemoryMaint, GraphMemoryMaintImpl},
        health::{Health, HealthImpl},
        learning_review::{LearningReview, LearningReviewImpl},
        llm_gateway::{LLMGateway, LLMGatewayImpl},
        memory::{Memory, MemoryImpl},
        session_store::{RestateSessionStore, SessionStoreImpl},
        skills::{Skills, SkillsImpl},
        tool_executor::{ToolExecutor, ToolExecutorDependencies, ToolExecutorImpl},
    },
    workflows::{
        consolidate::{Consolidate, ConsolidateImpl},
        execution_compensation_attempt::{
            ExecutionCompensationAttempt, ExecutionCompensationAttemptImpl,
        },
        execution_task_attempt::{ExecutionTaskAttempt, ExecutionTaskAttemptImpl},
        session_retention::{SessionRetention, SessionRetentionImpl},
        turn_events::TurnEventAppender,
        turn_execution::{TurnExecution, implementation::TurnExecutionImpl},
        worker_turn_execution::{WorkerTurnExecution, WorkerTurnExecutionImpl},
    },
};

const CORE_HEAD_SERVICE_NAMES: &[&str] = &[
    "Health",
    "SessionStore",
    "LLMGateway",
    "AgentDefinitions",
    "Agents",
    "AdminMaintenance",
    "Artifacts",
    "ActionReviews",
    "ActionReviewDispatcher",
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
    "ExecutionSchedule",
    "ExecutionRetention",
    "ExecutionTrigger",
    "ExecutionDispatcher",
    "ExecutionDispatchDrain",
    "ExecutionDispatchReconciler",
    "ExecutionAmendmentPlanner",
    "DurableTimeout",
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
    "ExecutionRunController",
    "ExecutionTaskAttempt",
    "ExecutionCompensationAttempt",
    "KnowledgeSyncIngestion",
    "Consolidate",
    "SessionRetention",
    "TenantPurge",
    "SecurityEvents",
];

const CORE_TAIL_SERVICE_NAMES: &[&str] = &["WorkerTurnExecution", "TurnExecution"];
#[cfg(test)]
const INGRESS_PRIVATE_SERVICE_NAMES: &[&str] = &[
    "LLMGateway",
    "ToolExecutor",
    "TurnExecution",
    "WorkerTurnExecution",
    "ExecutionRunController",
    "ExecutionRetention",
    "ExecutionTaskAttempt",
    "ExecutionCompensationAttempt",
    "ExecutionTrigger",
    "ExecutionDispatcher",
    "ExecutionDispatchDrain",
    "ExecutionDispatchReconciler",
    "ExecutionAmendmentPlanner",
    "DurableTimeout",
];
#[cfg(test)]
const INGRESS_PRIVATE_HANDLER_NAMES: &[(&str, &str)] = &[("ExecutionSchedule", "fire_occurrence")];
const EXPERIMENT_WORKFLOW_SERVICE_NAMES: &[&str] = &[
    "ExperimentRun",
    "ExperimentTrialRun",
    // Bound next to the experiment workflows because it dispatches into them: a
    // release evaluation is an `Experiments/run` on a pinned plan, so readiness
    // that admits one without the other would advertise a release surface whose
    // dispatch target is missing.
    "ArtifactReleaseEvaluation",
];

const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const HIGH_COST_INACTIVITY_TIMEOUT: Duration =
    Duration::from_secs(moa_config::SANDBOX_TENANT_PURGE_INACTIVITY_TIMEOUT_SECONDS);
const ABORT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_INITIAL_INTERVAL: Duration = Duration::from_millis(50);
const RETRY_MAX_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_MAX_ATTEMPTS: u64 = 70;

fn service_options() -> ServiceOptions {
    ServiceOptions::new()
        .idempotency_retention(RETENTION)
        .journal_retention(RETENTION)
        .retry_policy_initial_interval(RETRY_INITIAL_INTERVAL)
        .retry_policy_exponentiation_factor(2.0)
        .retry_policy_max_interval(RETRY_MAX_INTERVAL)
        .retry_policy_max_attempts(RETRY_MAX_ATTEMPTS)
        .retry_policy_pause_on_max_attempts()
}

fn bootstrap_entry_service_options() -> ServiceOptions {
    service_options()
}

fn workflow_options() -> ServiceOptions {
    service_options().handler("run", HandlerOptions::new().workflow_retention(RETENTION))
}

fn execution_schedule_service_options() -> ServiceOptions {
    service_options().handler(
        "fire_occurrence",
        HandlerOptions::new().ingress_private(true),
    )
}

fn high_cost_internal_service_options() -> ServiceOptions {
    service_options()
        .inactivity_timeout(HIGH_COST_INACTIVITY_TIMEOUT)
        .abort_timeout(ABORT_CLEANUP_TIMEOUT)
        .ingress_private(true)
}

fn high_cost_internal_workflow_options() -> ServiceOptions {
    high_cost_internal_service_options()
        .handler("run", HandlerOptions::new().workflow_retention(RETENTION))
}

fn execution_run_controller_options() -> ServiceOptions {
    high_cost_internal_service_options()
}

fn high_cost_public_workflow_options() -> ServiceOptions {
    service_options()
        .inactivity_timeout(HIGH_COST_INACTIVITY_TIMEOUT)
        .abort_timeout(ABORT_CLEANUP_TIMEOUT)
        .handler("run", HandlerOptions::new().workflow_retention(RETENTION))
}

fn sandbox_workspace_service_options(config: &MoaConfig) -> ServiceOptions {
    let retention = Duration::from_secs(config.sandbox_workspaces.operation_retention_seconds);
    ServiceOptions::new()
        .idempotency_retention(retention)
        .journal_retention(retention)
        .retry_policy_initial_interval(RETRY_INITIAL_INTERVAL)
        .retry_policy_exponentiation_factor(2.0)
        .retry_policy_max_interval(RETRY_MAX_INTERVAL)
        .retry_policy_max_attempts(RETRY_MAX_ATTEMPTS)
        .retry_policy_pause_on_max_attempts()
}

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
    let sandbox_workspace_management = SandboxWorkspaceManagement::from_config(
        pool.clone(),
        config.as_ref(),
        runtime_deps.sandbox_workspace_fenced_tenants.clone(),
        tool_router.clone(),
        session_store.clone(),
    );
    let mut builder = Endpoint::builder()
        .bind_with_options(HealthImpl.serve(), bootstrap_entry_service_options())
        .bind_with_options(
            SessionStoreImpl::new(
                session_store.clone(),
                pool.clone(),
                config.clone(),
                runtime_cache.clone(),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            LLMGatewayImpl::new(providers.clone())
                .with_runtime_cache(runtime_cache.clone())
                .serve(),
            high_cost_internal_service_options(),
        )
        .bind_with_options(
            AgentDefinitionsImpl::new(pool.clone(), connector_catalogs.clone(), authz.clone())
                .serve(),
            service_options(),
        )
        .bind_with_options(
            AgentsImpl::new(pool.clone(), fga_client.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            AdminMaintenanceImpl::new(pool.clone(), config.clone(), authz.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            ArtifactsImpl::new(ArtifactRegistry::new(pool.clone()), authz.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            ArtifactReleaseImpl::new(pool.clone(), connector_catalogs.clone(), authz.clone())
                .serve(),
            service_options(),
        )
        .bind_with_options(
            ActionReviewsImpl::new(
                pool.clone(),
                session_store.clone(),
                action_review_timeout_secs(&config),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            ActionReviewDispatcherImpl::new(pool.clone(), config.execution.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            ApiKeysImpl::new(pool.clone(), fga_client.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            AuthzImpl::new(pool.clone(), authz.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            AuthzChallengesImpl::new(pool.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            ContactsImpl::new(
                pool.clone(),
                session_store.clone(),
                config.clone(),
                contact_token_issuer,
                runtime_deps.delivery_sink.clone(),
                authz.clone(),
            )
            .serve(),
            service_options(),
        );

    builder = builder.bind_with_options(
        ConnectorConnectionsImpl::new(runtime_deps.connector_management.clone()).serve(),
        service_options(),
    );

    if config.sandbox_workspaces.mode.maintenance_enabled() {
        builder = builder.bind_with_options(
            SandboxWorkspacesImpl::new(sandbox_workspace_management.clone(), fga_client.clone())
                .serve(),
            sandbox_workspace_service_options(config.as_ref()),
        );
    }

    builder = builder.bind_with_options(
        ExperimentsImpl::new(
            pool.clone(),
            providers.clone(),
            session_store.clone(),
            authz.clone(),
        )
        .serve(),
        service_options(),
    );

    builder = builder
        .bind_with_options(
            IngestionVOImpl::new(runtime_deps.ingest_runtime.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            ToolExecutorImpl::new(ToolExecutorDependencies {
                router: tool_router.clone(),
                connector_catalogs: connector_catalogs.clone(),
                connector_completion,
                sessions: session_store.clone(),
                events: session_store.clone(),
                pool: pool.clone(),
                workspace_management: sandbox_workspace_management,
                external_job_adapters: runtime_deps.external_job_adapters.clone(),
                execution_config: config.execution.clone(),
            })
            .serve(),
            high_cost_internal_service_options(),
        )
        .bind_with_options(
            ActionPolicyImpl::new(
                tool_router.clone(),
                connector_catalogs.clone(),
                session_store.clone(),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            ExecutionImpl::new(
                pool.clone(),
                connector_catalogs.clone(),
                config.execution.clone(),
                session_store.clone(),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            ExecutionScheduleImpl::new(pool.clone(), authz.clone(), config.execution.clone())
                .serve(),
            execution_schedule_service_options(),
        )
        .bind_with_options(
            ExecutionRetentionImpl::new(pool.clone(), &config.execution).serve(),
            high_cost_internal_service_options(),
        )
        .bind_with_options(
            GraphMemoryMaintImpl::new(pool.clone(), config.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            SecurityEventsImpl::new(pool.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            KnowledgeImpl::new(
                KnowledgeService::from_config(
                    pool.clone(),
                    kms.clone(),
                    credential_vault.clone(),
                    config.as_ref(),
                    runtime_cache.clone(),
                    Arc::new(runtime_deps.connector_connections.clone()),
                ),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            LearningReviewImpl::new(
                session_store.clone(),
                pool.clone(),
                config.clone(),
                providers.clone(),
                tool_router.clone(),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            MemoryImpl::from_retrieval_engine(
                pool.clone(),
                kms.clone(),
                session_store.clone(),
                runtime_deps.retrieval_engine.clone(),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            NeonMaintImpl::new(config.clone()).serve(),
            service_options(),
        )
        .bind_with_options(
            PrivacyImpl::new(
                pool.clone(),
                background_pool,
                config.compliance.clone(),
                kms.clone(),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            SkillsImpl::new(pool.clone(), authz.clone()).serve(),
            service_options(),
        )
        .bind_with_options(CronJobImpl.serve(), service_options())
        .bind_with_options(
            SessionImpl::new(
                session_store.clone(),
                pool.clone(),
                config.clone(),
                session_limits.clone(),
                runtime_cache.clone(),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            WorkerImpl::new(
                session_store.clone(),
                session_limits.clone(),
                providers.clone(),
                connector_catalogs.clone(),
                authz.clone(),
            )
            .serve(),
            service_options(),
        )
        .bind_with_options(
            TenantsImpl::new(pool.clone(), fga_client.clone()).serve(),
            service_options(),
        )
        .bind_with_options(TenantImpl::new(pool.clone()).serve(), service_options())
        .bind_with_options(
            TenantPurgeImpl::new(
                pool.clone(),
                credential_vault.clone(),
                config.as_ref(),
                runtime_deps.workspace_maintenance.clone(),
            )
            .serve(),
            high_cost_public_workflow_options(),
        )
        .bind_with_options(
            ExecutionTriggerImpl::new(pool.clone(), &config.execution).serve(),
            high_cost_internal_service_options(),
        )
        .bind_with_options(
            ExecutionDispatcherImpl::new(pool.clone()).serve(),
            high_cost_internal_service_options(),
        )
        .bind_with_options(
            ExecutionDispatchDrainImpl::new(pool.clone(), &config.execution).serve(),
            high_cost_internal_service_options(),
        )
        .bind_with_options(
            ExecutionDispatchReconcilerImpl::new(pool.clone(), &config.execution).serve(),
            high_cost_internal_service_options(),
        )
        .bind_with_options(
            ExecutionAmendmentPlannerImpl::new(
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
            high_cost_internal_service_options(),
        )
        .bind_with_options(
            DurableTimeoutImpl::new(pool.clone()).serve(),
            high_cost_internal_service_options(),
        )
        .bind_with_options(
            ExecutionRunControllerImpl::new(pool.clone(), config.execution.clone()).serve(),
            execution_run_controller_options(),
        )
        .bind_with_options(
            ExecutionTaskAttemptImpl::new(
                pool.clone(),
                config.execution.clone(),
                session_store.clone(),
                session_limits.clone(),
                channel_adapters.clone(),
            )
            .serve(),
            high_cost_internal_workflow_options(),
        )
        .bind_with_options(
            ExecutionCompensationAttemptImpl::new(
                pool.clone(),
                session_store.clone(),
                session_limits.clone(),
                channel_adapters.clone(),
            )
            .serve(),
            high_cost_internal_workflow_options(),
        )
        .bind_with_options(
            KnowledgeSyncIngestionImpl::new(
                pool.clone(),
                kms.clone(),
                credential_vault.clone(),
                config.clone(),
                runtime_cache.clone(),
            )
            .serve(),
            workflow_options(),
        )
        .bind_with_options(
            SessionRetentionImpl::new(session_store.clone()).serve(),
            workflow_options(),
        )
        .bind_with_options(
            ConsolidateImpl::new(pool.clone(), kms, config.clone(), embedding_provider).serve(),
            workflow_options(),
        );

    {
        builder = builder.bind_with_options(
            SkillLearningImpl::new(
                session_store.clone(),
                config.clone(),
                providers.clone(),
                runtime_cache,
            )
            .serve(),
            workflow_options(),
        );
    }

    builder = builder
        .bind_with_options(
            ArtifactReleaseEvaluationImpl::new(pool.clone()).serve(),
            workflow_options(),
        )
        .bind_with_options(
            ExperimentRunImpl::new(pool.clone()).serve(),
            workflow_options(),
        )
        .bind_with_options(
            ExperimentTrialRunImpl::new(
                pool.clone(),
                session_store.clone(),
                providers.clone(),
                score_lineage,
                config.clone(),
                authz,
            )
            .serve(),
            workflow_options(),
        );

    // One durable event-append dependency, built here and owned by both turn
    // workflows, so neither reaches into global runtime state to persist events.
    let event_appender = TurnEventAppender::new(
        session_store.clone(),
        config.session.direct_turn_event_append,
    );

    let builder = builder
        .bind_with_options(
            WorkerTurnExecutionImpl::new(
                session_limits,
                session_store.clone(),
                channel_adapters.clone(),
                event_appender.clone(),
            )
            .serve(),
            high_cost_internal_workflow_options(),
        )
        .bind_with_options(
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
            high_cost_internal_workflow_options(),
        );

    builder.build()
}

/// Configured tenant action-review timeout in seconds, clamped to `i64`.
fn action_review_timeout_secs(config: &MoaConfig) -> i64 {
    i64::try_from(config.async_authz.action_review_timeout_secs).unwrap_or(i64::MAX)
}

/// Returns the service names expected for readiness in this rollout mode.
#[must_use]
pub fn expected_service_names(
    sandbox_workspace_mode: moa_config::SandboxWorkspaceMode,
) -> Vec<&'static str> {
    let mut names = Vec::new();
    names.extend(CORE_HEAD_SERVICE_NAMES.iter().copied());
    if sandbox_workspace_mode.maintenance_enabled() {
        names.push("SandboxWorkspaces");
    }
    names.push("Experiments");
    names.extend(CORE_BODY_SERVICE_NAMES.iter().copied());
    names.push("SkillLearning");
    names.extend(EXPERIMENT_WORKFLOW_SERVICE_NAMES.iter().copied());
    names.extend(CORE_TAIL_SERVICE_NAMES.iter().copied());
    names
}

/// Returns true when a Restate deployment contains every service required by
/// the selected sandbox-workspace rollout mode.
#[must_use]
pub fn services_registered_for_mode(
    deployments: &[RegisteredDeployment],
    sandbox_workspace_mode: moa_config::SandboxWorkspaceMode,
) -> bool {
    let expected_services = expected_service_names(sandbox_workspace_mode);
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

    use axum::{body::Body, http::Request};
    use restate_sdk::prelude::*;

    use super::{
        INGRESS_PRIVATE_HANDLER_NAMES, INGRESS_PRIVATE_SERVICE_NAMES, RegisteredDeployment,
        RegisteredService, bootstrap_entry_service_options, execution_run_controller_options,
        execution_schedule_service_options, expected_service_names,
        high_cost_internal_service_options, high_cost_internal_workflow_options,
        high_cost_public_workflow_options, sandbox_workspace_service_options,
        services_registered_for_mode, services_registered_with_expected,
    };
    use crate::services::health::{Health, HealthImpl};
    use moa_config::{MoaConfig, SandboxWorkspaceMode};

    #[restate_sdk::service]
    #[name = "ServicePolicyProbe"]
    trait ServicePolicyProbe {
        async fn call() -> Result<(), HandlerError>;
    }

    struct ServicePolicyProbeImpl;

    impl ServicePolicyProbe for ServicePolicyProbeImpl {
        async fn call(&self, _ctx: Context<'_>) -> Result<(), HandlerError> {
            Ok(())
        }
    }

    #[restate_sdk::service]
    #[name = "ExecutionSchedule"]
    trait ExecutionSchedulePolicyProbe {
        async fn create() -> Result<(), HandlerError>;

        async fn fire_occurrence() -> Result<(), HandlerError>;
    }

    struct ExecutionSchedulePolicyProbeImpl;

    impl ExecutionSchedulePolicyProbe for ExecutionSchedulePolicyProbeImpl {
        async fn create(&self, _ctx: Context<'_>) -> Result<(), HandlerError> {
            Ok(())
        }

        async fn fire_occurrence(&self, _ctx: Context<'_>) -> Result<(), HandlerError> {
            Ok(())
        }
    }

    #[restate_sdk::object]
    #[name = "ExecutionRunController"]
    trait ExecutionRunControllerPolicyProbe {
        async fn advance() -> Result<(), HandlerError>;
    }

    struct ExecutionRunControllerPolicyProbeImpl;

    impl ExecutionRunControllerPolicyProbe for ExecutionRunControllerPolicyProbeImpl {
        async fn advance(&self, _ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
            Ok(())
        }
    }

    #[restate_sdk::workflow]
    #[name = "WorkflowPolicyProbe"]
    trait WorkflowPolicyProbe {
        async fn run() -> Result<(), HandlerError>;
    }

    struct WorkflowPolicyProbeImpl;

    impl WorkflowPolicyProbe for WorkflowPolicyProbeImpl {
        async fn run(&self, _ctx: WorkflowContext<'_>) -> Result<(), HandlerError> {
            Ok(())
        }
    }

    #[restate_sdk::workflow]
    #[name = "TenantPurge"]
    trait TenantPurgePolicyProbe {
        async fn run() -> Result<(), HandlerError>;
    }

    struct TenantPurgePolicyProbeImpl;

    impl TenantPurgePolicyProbe for TenantPurgePolicyProbeImpl {
        async fn run(&self, _ctx: WorkflowContext<'_>) -> Result<(), HandlerError> {
            Ok(())
        }
    }

    async fn v4_manifest(endpoint: Endpoint) -> serde_json::Value {
        let response = endpoint.handle(
            Request::builder()
                .uri("/discover")
                .header("accept", "application/vnd.restate.endpointmanifest.v4+json")
                .body(Body::empty())
                .expect("discovery request should build"),
        );
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
            .await
            .expect("v4 discovery body should read");
        serde_json::from_slice(&bytes).expect("v4 discovery body should decode")
    }

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

        for name in expected_service_names(SandboxWorkspaceMode::Admit) {
            assert!(names.insert(name), "duplicate Restate binding name {name}");
        }
    }

    #[tokio::test]
    async fn v4_discovery_reports_exact_high_cost_internal_policy() {
        // Pins: SDK discovery is the deploy-time contract consumed by Restate;
        // timeout, retention, retry, and privacy values must remain exact.
        let endpoint = Endpoint::builder()
            .bind_with_options(
                ServicePolicyProbeImpl.serve(),
                high_cost_internal_service_options(),
            )
            .build();
        let manifest = v4_manifest(endpoint).await;
        let service = &manifest["services"][0];

        assert_eq!(service["inactivityTimeout"], 360_000);
        assert_eq!(service["abortTimeout"], 60_000);
        assert_eq!(service["idempotencyRetention"], 86_400_000);
        assert_eq!(service["journalRetention"], 86_400_000);
        assert_eq!(service["retryPolicyInitialInterval"], 50);
        assert_eq!(service["retryPolicyExponentiationFactor"], 2.0);
        assert_eq!(service["retryPolicyMaxInterval"], 60_000);
        assert_eq!(service["retryPolicyMaxAttempts"], 70);
        assert_eq!(service["retryPolicyOnMaxAttempts"], "PAUSE");
        assert_eq!(service["ingressPrivate"], true);
    }

    #[tokio::test]
    async fn v4_discovery_reports_exact_public_tenant_purge_policy() {
        // Pins: tenant offboarding is public through the authenticated edge but
        // its separated absence observations must outlive generic 1s defaults.
        let endpoint = Endpoint::builder()
            .bind_with_options(
                TenantPurgePolicyProbeImpl.serve(),
                high_cost_public_workflow_options(),
            )
            .build();
        let manifest = v4_manifest(endpoint).await;
        let service = &manifest["services"][0];

        assert_eq!(service["name"], "TenantPurge");
        assert_eq!(service["inactivityTimeout"], 360_000);
        assert_eq!(service["abortTimeout"], 60_000);
        assert_eq!(service["idempotencyRetention"], 86_400_000);
        assert_eq!(service["journalRetention"], 86_400_000);
        assert_eq!(service["retryPolicyInitialInterval"], 50);
        assert_eq!(service["retryPolicyExponentiationFactor"], 2.0);
        assert_eq!(service["retryPolicyMaxInterval"], 60_000);
        assert_eq!(service["retryPolicyMaxAttempts"], 70);
        assert_eq!(service["retryPolicyOnMaxAttempts"], "PAUSE");
        assert_ne!(service["ingressPrivate"], true);
        let run = service["handlers"]
            .as_array()
            .expect("workflow handlers should be an array")
            .iter()
            .find(|handler| handler["name"] == "run")
            .expect("workflow run handler should exist");
        assert_eq!(run["workflowCompletionRetention"], 86_400_000);
    }

    #[tokio::test]
    async fn execution_schedule_keeps_crud_public_and_fire_occurrence_ingress_private() {
        // Pins: tenant schedule CRUD remains callable through public ingress, while only trusted
        // outbox delivery can invoke the trigger-consuming occurrence handler.
        let endpoint = Endpoint::builder()
            .bind_with_options(
                ExecutionSchedulePolicyProbeImpl.serve(),
                execution_schedule_service_options(),
            )
            .build();
        let manifest = v4_manifest(endpoint).await;
        let service = &manifest["services"][0];
        assert_eq!(service["name"], "ExecutionSchedule");
        assert_ne!(service["ingressPrivate"], true);

        let handlers = service["handlers"]
            .as_array()
            .expect("schedule handlers should be an array");
        let create = handlers
            .iter()
            .find(|handler| handler["name"] == "create")
            .expect("public create handler should exist");
        let fire_occurrence = handlers
            .iter()
            .find(|handler| handler["name"] == "fire_occurrence")
            .expect("private fire-occurrence handler should exist");

        assert_ne!(create["ingressPrivate"], true);
        assert_eq!(fire_occurrence["ingressPrivate"], true);
    }

    #[tokio::test]
    async fn execution_run_controller_options_match_the_advance_handler() {
        // Pins: the bounded run controller is a virtual object with `advance`, so endpoint
        // discovery must not apply workflow-only options for a nonexistent `run` handler.
        let endpoint = Endpoint::builder()
            .bind_with_options(
                ExecutionRunControllerPolicyProbeImpl.serve(),
                execution_run_controller_options(),
            )
            .build();
        let manifest = v4_manifest(endpoint).await;
        let service = &manifest["services"][0];
        assert_eq!(service["name"], "ExecutionRunController");
        assert_eq!(service["ingressPrivate"], true);
        let advance = service["handlers"]
            .as_array()
            .expect("controller handlers should be an array")
            .iter()
            .find(|handler| handler["name"] == "advance")
            .expect("controller advance handler should exist");
        assert_eq!(
            advance["workflowCompletionRetention"],
            serde_json::Value::Null
        );
    }

    #[tokio::test]
    async fn sandbox_workspace_service_uses_configured_durable_retention() {
        // Pins: Restate cannot evict a workspace operation owner on the generic
        // one-day service default while Postgres still requires reconciliation.
        let mut config = MoaConfig::default();
        config.sandbox_workspaces.operation_retention_seconds = 9 * 24 * 60 * 60;
        let endpoint = Endpoint::builder()
            .bind_with_options(
                ServicePolicyProbeImpl.serve(),
                sandbox_workspace_service_options(&config),
            )
            .build();
        let manifest = v4_manifest(endpoint).await;
        let service = &manifest["services"][0];

        assert_eq!(service["idempotencyRetention"], 777_600_000);
        assert_eq!(service["journalRetention"], 777_600_000);
    }

    #[tokio::test]
    async fn product_health_remains_ingress_public() {
        // Pins: edge startup enters only through the steady-state product
        // endpoint; the migration-only endpoint deliberately has no Health.
        let endpoint = Endpoint::builder()
            .bind_with_options(HealthImpl.serve(), bootstrap_entry_service_options())
            .build();
        let manifest = v4_manifest(endpoint).await;

        let service = &manifest["services"][0];
        assert_eq!(service["name"], "Health");
        assert_ne!(service["ingressPrivate"], true);
    }

    #[tokio::test]
    async fn every_workflow_run_declares_24_hour_completion_retention() {
        // Pins: completed workflow responses remain addressable for one explicit
        // day instead of inheriting a server-side default that can drift.
        let endpoint = Endpoint::builder()
            .bind_with_options(
                WorkflowPolicyProbeImpl.serve(),
                high_cost_internal_workflow_options(),
            )
            .build();
        let manifest = v4_manifest(endpoint).await;
        let run = manifest["services"][0]["handlers"]
            .as_array()
            .expect("workflow handlers should be an array")
            .iter()
            .find(|handler| handler["name"] == "run")
            .expect("workflow run handler should exist");

        assert_eq!(run["workflowCompletionRetention"], 86_400_000);
    }

    #[test]
    fn ingress_private_services_are_exactly_the_internal_high_cost_set() {
        assert_eq!(
            INGRESS_PRIVATE_SERVICE_NAMES,
            [
                "LLMGateway",
                "ToolExecutor",
                "TurnExecution",
                "WorkerTurnExecution",
                "ExecutionRunController",
                "ExecutionRetention",
                "ExecutionTaskAttempt",
                "ExecutionCompensationAttempt",
                "ExecutionTrigger",
                "ExecutionDispatcher",
                "ExecutionDispatchDrain",
                "ExecutionDispatchReconciler",
                "ExecutionAmendmentPlanner",
                "DurableTimeout",
            ]
        );
    }

    #[test]
    fn ingress_private_handlers_are_exactly_the_mixed_service_internal_set() {
        assert_eq!(
            INGRESS_PRIVATE_HANDLER_NAMES,
            [("ExecutionSchedule", "fire_occurrence")]
        );
    }

    #[test]
    fn product_expected_services_include_experiments() {
        let names = expected_service_names(SandboxWorkspaceMode::Disabled);

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
            names.contains(&"ExecutionRunController")
                && names.contains(&"ExecutionTaskAttempt")
                && names.contains(&"ExecutionCompensationAttempt"),
            "product readiness should include every bounded execution owner"
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
        let names = expected_service_names(SandboxWorkspaceMode::Disabled);
        let deployments = vec![deployment_with_services(&names)];

        assert!(services_registered_for_mode(
            &deployments,
            SandboxWorkspaceMode::Disabled,
        ));
    }

    #[test]
    fn registration_check_rejects_partial_deployments() {
        let deployments = vec![deployment_with_services(&["Health", "SessionStore"])];

        assert!(!services_registered_for_mode(
            &deployments,
            SandboxWorkspaceMode::Disabled,
        ));
    }

    #[test]
    fn registration_check_rejects_deployment_missing_product_services() {
        let names = expected_service_names(SandboxWorkspaceMode::Disabled);
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
        let deployment_without_controller = names
            .iter()
            .copied()
            .filter(|name| *name != "ExecutionRunController")
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
        assert!(
            !services_registered_with_expected(
                &[deployment_with_services(&deployment_without_controller)],
                &names
            ),
            "readiness must reject a deployment missing ExecutionRunController"
        );
    }

    #[test]
    fn skill_learning_workflow_is_always_expected() {
        // Pins: skill learning is always on — readiness requires the SkillLearning
        // workflow in every build, exactly once.
        let names = expected_service_names(SandboxWorkspaceMode::Disabled);

        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "SkillLearning")
                .count(),
            1,
            "readiness must expect SkillLearning exactly once"
        );
    }

    #[test]
    fn workspace_registration_expectation_matches_rollout_mode() {
        // Pins: a dark deployment neither binds nor waits forever for the
        // workspace service, while cleanup and admission deployments must prove
        // that the service was registered.
        let disabled = expected_service_names(SandboxWorkspaceMode::Disabled);
        assert!(!disabled.contains(&"SandboxWorkspaces"));
        assert!(services_registered_for_mode(
            &[deployment_with_services(&disabled)],
            SandboxWorkspaceMode::Disabled,
        ));

        for mode in [
            SandboxWorkspaceMode::Maintenance,
            SandboxWorkspaceMode::Admit,
        ] {
            let enabled = expected_service_names(mode);
            assert!(enabled.contains(&"SandboxWorkspaces"));
            assert!(!services_registered_for_mode(
                &[deployment_with_services(&disabled)],
                mode,
            ));
            assert!(services_registered_for_mode(
                &[deployment_with_services(&enabled)],
                mode,
            ));
        }
    }
}
