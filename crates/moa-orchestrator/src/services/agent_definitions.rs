//! Restate service for tenant-configurable agent definitions and deployments.

use moa_agents::AgentResolver;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, ArtifactScopeParts, StoredArtifactRevision};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::Identity;
use moa_core::wire::agents::{
    AgentDefinitionListRequest, AgentDefinitionListResponse, AgentDefinitionSummary,
    AgentDeployRequest, AgentDeployResponse, AgentDeploymentListRequest,
    AgentDeploymentListResponse, AgentDeploymentSummary, AgentInstallRequest, AgentInstallResponse,
    AgentInstallationListRequest, AgentInstallationListResponse, AgentInstallationSummary,
};
use moa_core::{ActionRuleScope, MoaError, StoragePartitionId, TenantId};
use moa_db::ScopedConn;
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde_json::Value;
use sqlx::{PgPool, Row, types::Json as SqlJson};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{
    authorize_tenant, require_fga_client, require_identity, translate_authz_error,
};
use crate::workflows::errors::{bad_request, moa_error_to_handler_error};

const DEFAULT_DEPLOYMENT_LIST_LIMIT: i64 = 50;

/// Restate service surface for tenant-configurable agent lifecycle operations.
#[restate_sdk::service]
#[name = "AgentDefinitions"]
pub trait AgentDefinitions {
    /// Lists visible agent definition revisions.
    async fn list_definitions(
        request: Json<AgentDefinitionListRequest>,
    ) -> Result<Json<AgentDefinitionListResponse>, HandlerError>;

    /// Installs and deploys a published agent revision in a tenant.
    async fn install(
        request: Json<AgentInstallRequest>,
    ) -> Result<Json<AgentInstallResponse>, HandlerError>;

    /// Lists installed agents in a tenant.
    async fn list_installations(
        request: Json<AgentInstallationListRequest>,
    ) -> Result<Json<AgentInstallationListResponse>, HandlerError>;

    /// Moves an installed agent to an exact published revision.
    async fn deploy(
        request: Json<AgentDeployRequest>,
    ) -> Result<Json<AgentDeployResponse>, HandlerError>;

    /// Lists deployment history for an installed agent.
    async fn list_deployments(
        request: Json<AgentDeploymentListRequest>,
    ) -> Result<Json<AgentDeploymentListResponse>, HandlerError>;
}

/// Concrete tenant-configurable agent service implementation.
#[derive(Clone, Default)]
pub struct AgentDefinitionsImpl;

impl AgentDefinitions for AgentDefinitionsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_definitions(
        &self,
        ctx: Context<'_>,
        request: Json<AgentDefinitionListRequest>,
    ) -> Result<Json<AgentDefinitionListResponse>, HandlerError> {
        annotate_restate_handler_span("AgentDefinitions", "list_definitions");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { list_definitions_inner(pool, request).await.map(Json::from) })
            .name("agent_definitions_list_definitions")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn install(
        &self,
        ctx: Context<'_>,
        request: Json<AgentInstallRequest>,
    ) -> Result<Json<AgentInstallResponse>, HandlerError> {
        annotate_restate_handler_span("AgentDefinitions", "install");
        let request = request.into_inner();
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        if let Some(agent_id) = request.agent_id {
            authorize_agent_operator(&ctx, agent_id).await?;
        }
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { install_inner(pool, request, identity).await.map(Json::from) })
            .name("agent_definitions_install")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_installations(
        &self,
        ctx: Context<'_>,
        request: Json<AgentInstallationListRequest>,
    ) -> Result<Json<AgentInstallationListResponse>, HandlerError> {
        annotate_restate_handler_span("AgentDefinitions", "list_installations");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                list_installations_inner(pool, request)
                    .await
                    .map(Json::from)
            })
            .name("agent_definitions_list_installations")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn deploy(
        &self,
        ctx: Context<'_>,
        request: Json<AgentDeployRequest>,
    ) -> Result<Json<AgentDeployResponse>, HandlerError> {
        annotate_restate_handler_span("AgentDefinitions", "deploy");
        let request = request.into_inner();
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { deploy_inner(pool, request, identity).await.map(Json::from) })
            .name("agent_definitions_deploy")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_deployments(
        &self,
        ctx: Context<'_>,
        request: Json<AgentDeploymentListRequest>,
    ) -> Result<Json<AgentDeploymentListResponse>, HandlerError> {
        annotate_restate_handler_span("AgentDefinitions", "list_deployments");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { list_deployments_inner(pool, request).await.map(Json::from) })
            .name("agent_definitions_list_deployments")
            .await?)
    }
}

/// Lists visible agent definition revisions after caller authorization has passed.
pub async fn list_definitions_inner(
    pool: PgPool,
    request: AgentDefinitionListRequest,
) -> Result<AgentDefinitionListResponse, HandlerError> {
    let scope = tenant_scope(request.tenant_id);
    let status = request
        .status
        .as_deref()
        .map(parse_status)
        .transpose()?
        .unwrap_or(ArtifactStatus::Published);
    let registry = ArtifactRegistry::new(pool);
    let summaries = registry
        .list_visible(&scope, Some(ArtifactKind::Agent), Some(status))
        .await
        .map_err(moa_error_to_handler_error)?;
    let mut agents = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let revision = registry
            .load_revision(&scope, summary.revision_uid)
            .await
            .map_err(moa_error_to_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(500, "agent definition disappeared"))?;
        let display_name = agent_definition(&revision)?.display_name.clone();
        agents.push(AgentDefinitionSummary {
            artifact_uid: summary.artifact_uid,
            revision_uid: summary.revision_uid,
            scope: summary.scope,
            name: summary.name.clone(),
            definition_ref: format!("agent://{}", summary.name),
            description: summary.description,
            display_name,
            tags: summary.tags,
            status: summary.status.to_string(),
            version: summary.version,
            updated_at: summary.updated_at,
        });
    }
    Ok(AgentDefinitionListResponse {
        tenant_id: request.tenant_id,
        agents,
    })
}

/// Installs and deploys a published agent revision after caller authorization has passed.
pub async fn install_inner(
    pool: PgPool,
    request: AgentInstallRequest,
    identity: Identity,
) -> Result<AgentInstallResponse, HandlerError> {
    let scope = tenant_scope(request.tenant_id);
    let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
    let revision =
        load_published_agent_revision(pool.clone(), &scope, request.revision_uid).await?;
    let definition = agent_definition(&revision)?;
    let display_name = request
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| definition.display_name.clone());
    let definition_ref = format!("agent://{}", revision.name);
    let policy = AgentResolver::new(pool.clone())
        .resolve_exact_revision(&scope, request.revision_uid)
        .await
        .map_err(moa_error_to_handler_error)?;
    let installed_by = Some(identity.id.to_string());
    let metadata = object_or_empty(request.metadata);
    let parts = ArtifactScopeParts::from_scope(&scope);
    let installation_uid = Uuid::now_v7();
    let deployment_uid = Uuid::now_v7();

    let mut conn = scoped_conn_for_scope(&pool, &scope)
        .await
        .map_err(moa_error_to_handler_error)?;
    ensure_no_active_installation(conn.as_mut(), &parts, &definition_ref).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.agent_installation (
            installation_uid, storage_partition_id, user_id, agent_id, artifact_uid, definition_ref,
            display_name, status, current_revision_uid, last_deployment_uid, last_deployed_at,
            installed_by, deployment_metadata
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, 'active', $8, $9, now(), $10, $11)
        "#,
    )
    .bind(installation_uid)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(request.agent_id)
    .bind(revision.artifact_uid)
    .bind(&definition_ref)
    .bind(&display_name)
    .bind(request.revision_uid)
    .bind(deployment_uid)
    .bind(installed_by.as_deref())
    .bind(SqlJson(metadata))
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_handler_error)?;
    insert_deployment(
        conn.as_mut(),
        NewDeployment {
            deployment_uid,
            installation_uid,
            storage_partition_id: storage_partition_id.as_str(),
            user_id: parts.user_id.as_deref(),
            revision_uid: request.revision_uid,
            deployed_by: installed_by.as_deref(),
            reason: request.reason.as_deref(),
            revision_lock: &policy.revision_lock,
        },
    )
    .await?;
    conn.commit().await.map_err(moa_error_to_handler_error)?;

    Ok(AgentInstallResponse {
        tenant_id: request.tenant_id,
        installation_uid,
        deployment_uid,
        revision_uid: request.revision_uid,
        policy_hash: policy.revision_lock.canonical_policy_hash,
    })
}

/// Lists installed agents after caller authorization has passed.
pub async fn list_installations_inner(
    pool: PgPool,
    request: AgentInstallationListRequest,
) -> Result<AgentInstallationListResponse, HandlerError> {
    let scope = tenant_scope(request.tenant_id);
    let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
    let mut conn = scoped_conn_for_scope(&pool, &scope)
        .await
        .map_err(moa_error_to_handler_error)?;
    let rows = sqlx::query(
        r#"
        SELECT installation_uid, agent_id, artifact_uid, definition_ref, display_name, status,
               current_revision_uid, last_deployment_uid, last_deployed_at, created_at, updated_at
        FROM moa.agent_installation
        WHERE storage_partition_id = $1
          AND status <> 'retired'
        ORDER BY updated_at DESC, installation_uid DESC
        "#,
    )
    .bind(storage_partition_id.as_str())
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_handler_error)?;
    conn.commit().await.map_err(moa_error_to_handler_error)?;

    let installations = rows
        .iter()
        .map(installation_summary_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AgentInstallationListResponse {
        tenant_id: request.tenant_id,
        installations,
    })
}

/// Deploys an exact published agent revision after caller authorization has passed.
pub async fn deploy_inner(
    pool: PgPool,
    request: AgentDeployRequest,
    identity: Identity,
) -> Result<AgentDeployResponse, HandlerError> {
    let scope = tenant_scope(request.tenant_id);
    let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
    let revision =
        load_published_agent_revision(pool.clone(), &scope, request.revision_uid).await?;
    let policy = AgentResolver::new(pool.clone())
        .resolve_exact_revision(&scope, request.revision_uid)
        .await
        .map_err(moa_error_to_handler_error)?;
    let deployment_uid = Uuid::now_v7();
    let deployed_by = Some(identity.id.to_string());
    let mut conn = scoped_conn_for_scope(&pool, &scope)
        .await
        .map_err(moa_error_to_handler_error)?;
    let installation = load_installation_for_update(
        conn.as_mut(),
        &storage_partition_id,
        request.installation_uid,
    )
    .await?;
    if installation.artifact_uid != revision.artifact_uid {
        return Err(TerminalError::new_with_code(
            400,
            "agent revision belongs to a different agent definition",
        )
        .into());
    }
    sqlx::query(
        r#"
        UPDATE moa.agent_deployment
        SET status = 'superseded'
        WHERE installation_uid = $1
          AND status = 'active'
        "#,
    )
    .bind(request.installation_uid)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_handler_error)?;
    insert_deployment(
        conn.as_mut(),
        NewDeployment {
            deployment_uid,
            installation_uid: request.installation_uid,
            storage_partition_id: storage_partition_id.as_str(),
            user_id: installation.user_id.as_deref(),
            revision_uid: request.revision_uid,
            deployed_by: deployed_by.as_deref(),
            reason: request.reason.as_deref(),
            revision_lock: &policy.revision_lock,
        },
    )
    .await?;
    sqlx::query(
        r#"
        UPDATE moa.agent_installation
        SET current_revision_uid = $2,
            last_deployment_uid = $3,
            last_deployed_at = now(),
            updated_at = now()
        WHERE installation_uid = $1
        "#,
    )
    .bind(request.installation_uid)
    .bind(request.revision_uid)
    .bind(deployment_uid)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_handler_error)?;
    conn.commit().await.map_err(moa_error_to_handler_error)?;

    Ok(AgentDeployResponse {
        tenant_id: request.tenant_id,
        installation_uid: request.installation_uid,
        deployment_uid,
        revision_uid: request.revision_uid,
        policy_hash: policy.revision_lock.canonical_policy_hash,
    })
}

/// Lists deployment history for an installation after caller authorization has passed.
pub async fn list_deployments_inner(
    pool: PgPool,
    request: AgentDeploymentListRequest,
) -> Result<AgentDeploymentListResponse, HandlerError> {
    let scope = tenant_scope(request.tenant_id);
    let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
    let limit = request
        .limit
        .map(|limit| i64::try_from(limit).map_err(|_| bad_request("limit is too large")))
        .transpose()?
        .unwrap_or(DEFAULT_DEPLOYMENT_LIST_LIMIT);
    let mut conn = scoped_conn_for_scope(&pool, &scope)
        .await
        .map_err(moa_error_to_handler_error)?;
    ensure_installation_visible(
        conn.as_mut(),
        &storage_partition_id,
        request.installation_uid,
    )
    .await?;
    let rows = sqlx::query(
        r#"
        SELECT deployment_uid, revision_uid, deployed_by, deployed_at, status, reason,
               dependency_lock_hash
        FROM moa.agent_deployment
        WHERE storage_partition_id = $1
          AND installation_uid = $2
        ORDER BY deployed_at DESC, deployment_uid DESC
        LIMIT $3
        "#,
    )
    .bind(storage_partition_id.as_str())
    .bind(request.installation_uid)
    .bind(limit)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_handler_error)?;
    conn.commit().await.map_err(moa_error_to_handler_error)?;
    let deployments = rows
        .iter()
        .map(deployment_summary_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(AgentDeploymentListResponse {
        tenant_id: request.tenant_id,
        installation_uid: request.installation_uid,
        deployments,
    })
}

struct InstallationForDeploy {
    artifact_uid: Uuid,
    user_id: Option<String>,
}

struct NewDeployment<'a> {
    deployment_uid: Uuid,
    installation_uid: Uuid,
    storage_partition_id: &'a str,
    user_id: Option<&'a str>,
    revision_uid: Uuid,
    deployed_by: Option<&'a str>,
    reason: Option<&'a str>,
    revision_lock: &'a moa_core::AgentRevisionLock,
}

async fn insert_deployment(
    conn: &mut sqlx::PgConnection,
    deployment: NewDeployment<'_>,
) -> Result<(), HandlerError> {
    sqlx::query(
        r#"
        INSERT INTO moa.agent_deployment (
            deployment_uid, installation_uid, storage_partition_id, user_id, revision_uid,
            deployed_by, status, reason, dependency_lock, dependency_lock_hash
        )
        VALUES ($1, $2, $3, $4, $5, $6, 'active', $7, $8, $9)
        "#,
    )
    .bind(deployment.deployment_uid)
    .bind(deployment.installation_uid)
    .bind(deployment.storage_partition_id)
    .bind(deployment.user_id)
    .bind(deployment.revision_uid)
    .bind(deployment.deployed_by)
    .bind(deployment.reason)
    .bind(SqlJson((*deployment.revision_lock).clone()))
    .bind(&deployment.revision_lock.canonical_policy_hash)
    .execute(conn)
    .await
    .map_err(sqlx_handler_error)?;
    Ok(())
}

async fn ensure_no_active_installation(
    conn: &mut sqlx::PgConnection,
    parts: &ArtifactScopeParts,
    definition_ref: &str,
) -> Result<(), HandlerError> {
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM moa.agent_installation
            WHERE storage_partition_id = $1
              AND user_id IS NOT DISTINCT FROM $2
              AND definition_ref = $3
              AND status <> 'retired'
        )
        "#,
    )
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(definition_ref)
    .fetch_one(conn)
    .await
    .map_err(sqlx_handler_error)?;
    if exists {
        return Err(TerminalError::new_with_code(
            409,
            format!("agent definition `{definition_ref}` is already installed"),
        )
        .into());
    }
    Ok(())
}

async fn load_installation_for_update(
    conn: &mut sqlx::PgConnection,
    storage_partition_id: &StoragePartitionId,
    installation_uid: Uuid,
) -> Result<InstallationForDeploy, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT artifact_uid, user_id
        FROM moa.agent_installation
        WHERE storage_partition_id = $1
          AND installation_uid = $2
          AND status = 'active'
        FOR UPDATE
        "#,
    )
    .bind(storage_partition_id.as_str())
    .bind(installation_uid)
    .fetch_optional(conn)
    .await
    .map_err(sqlx_handler_error)?
    .ok_or_else(|| TerminalError::new_with_code(404, "agent installation not found"))?;

    Ok(InstallationForDeploy {
        artifact_uid: row.try_get("artifact_uid").map_err(sqlx_handler_error)?,
        user_id: row.try_get("user_id").map_err(sqlx_handler_error)?,
    })
}

async fn ensure_installation_visible(
    conn: &mut sqlx::PgConnection,
    storage_partition_id: &StoragePartitionId,
    installation_uid: Uuid,
) -> Result<(), HandlerError> {
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM moa.agent_installation
            WHERE storage_partition_id = $1
              AND installation_uid = $2
              AND status <> 'retired'
        )
        "#,
    )
    .bind(storage_partition_id.as_str())
    .bind(installation_uid)
    .fetch_one(conn)
    .await
    .map_err(sqlx_handler_error)?;
    if !exists {
        return Err(TerminalError::new_with_code(404, "agent installation not found").into());
    }
    Ok(())
}

async fn load_published_agent_revision(
    pool: PgPool,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision, HandlerError> {
    let revision = ArtifactRegistry::new(pool)
        .load_revision(scope, revision_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "agent revision not found"))?;
    if revision.kind != ArtifactKind::Agent {
        return Err(TerminalError::new_with_code(400, "revision is not an agent").into());
    }
    if revision.status != ArtifactStatus::Published {
        return Err(TerminalError::new_with_code(400, "agent revision must be published").into());
    }
    Ok(revision)
}

fn agent_definition(
    revision: &StoredArtifactRevision,
) -> Result<&moa_artifacts::agent::AgentDefinition, HandlerError> {
    let ArtifactDefinition::Agent(definition) = &revision.document.definition else {
        return Err(TerminalError::new_with_code(400, "revision is not an agent").into());
    };
    Ok(definition)
}

fn installation_summary_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AgentInstallationSummary, HandlerError> {
    Ok(AgentInstallationSummary {
        installation_uid: row
            .try_get("installation_uid")
            .map_err(sqlx_handler_error)?,
        agent_id: row.try_get("agent_id").map_err(sqlx_handler_error)?,
        artifact_uid: row.try_get("artifact_uid").map_err(sqlx_handler_error)?,
        definition_ref: row.try_get("definition_ref").map_err(sqlx_handler_error)?,
        display_name: row.try_get("display_name").map_err(sqlx_handler_error)?,
        status: row.try_get("status").map_err(sqlx_handler_error)?,
        current_revision_uid: row
            .try_get("current_revision_uid")
            .map_err(sqlx_handler_error)?,
        last_deployment_uid: row
            .try_get("last_deployment_uid")
            .map_err(sqlx_handler_error)?,
        last_deployed_at: row
            .try_get("last_deployed_at")
            .map_err(sqlx_handler_error)?,
        created_at: row.try_get("created_at").map_err(sqlx_handler_error)?,
        updated_at: row.try_get("updated_at").map_err(sqlx_handler_error)?,
    })
}

fn deployment_summary_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<AgentDeploymentSummary, HandlerError> {
    Ok(AgentDeploymentSummary {
        deployment_uid: row.try_get("deployment_uid").map_err(sqlx_handler_error)?,
        revision_uid: row.try_get("revision_uid").map_err(sqlx_handler_error)?,
        status: row.try_get("status").map_err(sqlx_handler_error)?,
        deployed_by: row.try_get("deployed_by").map_err(sqlx_handler_error)?,
        reason: row.try_get("reason").map_err(sqlx_handler_error)?,
        dependency_lock_hash: row
            .try_get("dependency_lock_hash")
            .map_err(sqlx_handler_error)?,
        deployed_at: row.try_get("deployed_at").map_err(sqlx_handler_error)?,
    })
}

fn tenant_scope(tenant_id: TenantId) -> ActionRuleScope {
    ActionRuleScope::Tenant { tenant_id }
}

fn storage_partition_id_for_tenant(tenant_id: TenantId) -> StoragePartitionId {
    StoragePartitionId::for_tenant(tenant_id)
}

async fn scoped_conn_for_scope<'p>(
    pool: &'p PgPool,
    scope: &ActionRuleScope,
) -> moa_core::Result<ScopedConn<'p>> {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => ScopedConn::begin_tenant(pool, *tenant_id).await,
    }
}

fn object_or_empty(value: Value) -> Value {
    if value.is_object() {
        value
    } else {
        serde_json::json!({})
    }
}

fn parse_status(status: &str) -> Result<ArtifactStatus, HandlerError> {
    status
        .parse::<ArtifactStatus>()
        .map_err(|error| TerminalError::new_with_code(400, error.to_string()).into())
}

async fn authorize_agent_operator(
    ctx: &impl RequestHeaders,
    agent_id: Uuid,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Agent,
        agent_id,
        Relation::Operator,
    )
    .await
    .map_err(translate_authz_error)
}

fn sqlx_handler_error(error: sqlx::Error) -> HandlerError {
    HandlerError::from(MoaError::StorageError(error.to_string()))
}
