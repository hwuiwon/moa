//! Restate service for agent template lifecycle operations.

use chrono::{DateTime, Utc};
use moa_authz::{AuthzCheckError, enqueue_raw, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation, TupleOp};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Request body for creating an agent template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentTemplateRequest {
    /// Tenant-unique template name.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// System instructions used by agents instantiated from this template.
    pub instructions: String,
    /// Tool names this template is allowed to call.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

/// Agent template summary returned by list, get, and create.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AgentTemplateSummary {
    /// Template UUID.
    pub id: Uuid,
    /// Tenant UUID.
    pub tenant_id: Uuid,
    /// Tenant-unique template name.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// System instructions used by agents instantiated from this template.
    pub instructions: String,
    /// Tool names this template is allowed to call.
    pub allowed_tools: Vec<String>,
    /// User who created the template.
    pub created_by_user_id: Uuid,
    /// Template creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Template deactivation timestamp.
    pub deactivated_at: Option<DateTime<Utc>>,
}

/// Restate service surface for agent template management.
#[restate_sdk::service]
#[name = "AgentTemplates"]
pub trait AgentTemplates {
    /// Create an agent template in the caller's tenant.
    async fn create(
        request: Json<CreateAgentTemplateRequest>,
    ) -> Result<Json<AgentTemplateSummary>, HandlerError>;

    /// List active templates visible to the caller's tenant.
    async fn list() -> Result<Json<Vec<AgentTemplateSummary>>, HandlerError>;

    /// Load one template by id.
    async fn get(id: Json<Uuid>) -> Result<Json<AgentTemplateSummary>, HandlerError>;

    /// Deactivate one template.
    async fn deactivate(id: Json<Uuid>) -> Result<(), HandlerError>;
}

/// Concrete agent template service implementation.
#[derive(Clone, Default)]
pub struct AgentTemplatesImpl;

impl AgentTemplates for AgentTemplatesImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn create(
        &self,
        ctx: Context<'_>,
        request: Json<CreateAgentTemplateRequest>,
    ) -> Result<Json<AgentTemplateSummary>, HandlerError> {
        annotate_restate_handler_span("AgentTemplates", "create");
        let identity = require_identity(&ctx)?;
        require_tenant_admin(&identity).await?;
        let request = request.into_inner();
        validate_template_request(&request)?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move {
                create_template_inner(pool, identity, request)
                    .await
                    .map(Json::from)
            })
            .name("agent_templates_create")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn list(
        &self,
        ctx: Context<'_>,
    ) -> Result<Json<Vec<AgentTemplateSummary>>, HandlerError> {
        annotate_restate_handler_span("AgentTemplates", "list");
        let identity = require_identity(&ctx)?;
        require_tenant_member(&identity).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { list_templates_inner(pool, identity).await.map(Json::from) })
            .name("agent_templates_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, id))]
    async fn get(
        &self,
        ctx: Context<'_>,
        id: Json<Uuid>,
    ) -> Result<Json<AgentTemplateSummary>, HandlerError> {
        annotate_restate_handler_span("AgentTemplates", "get");
        let identity = require_identity(&ctx)?;
        let template_id = id.into_inner();
        require_template_creator_or_tenant_admin(&identity, template_id).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move {
                load_template(&pool, template_id)
                    .await
                    .and_then(|template| ensure_same_tenant(template, identity.tenant_id))
                    .map(Json::from)
            })
            .name("agent_templates_get")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, id))]
    async fn deactivate(&self, ctx: Context<'_>, id: Json<Uuid>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("AgentTemplates", "deactivate");
        let identity = require_identity(&ctx)?;
        let template_id = id.into_inner();
        require_template_creator_or_tenant_admin(&identity, template_id).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { deactivate_template_inner(pool, identity, template_id).await })
            .name("agent_templates_deactivate")
            .await?)
    }
}

async fn create_template_inner(
    pool: PgPool,
    identity: Identity,
    request: CreateAgentTemplateRequest,
) -> Result<AgentTemplateSummary, HandlerError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let template: AgentTemplateSummary = sqlx::query_as(
        r#"
        INSERT INTO agent_templates
            (tenant_id, name, description, instructions, allowed_tools, created_by_user_id)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, tenant_id, name, description, instructions, allowed_tools,
                  created_by_user_id, created_at, deactivated_at
        "#,
    )
    .bind(identity.tenant_id)
    .bind(request.name.trim())
    .bind(request.description.as_deref())
    .bind(request.instructions)
    .bind(request.allowed_tools)
    .bind(identity.id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("create agent template: {error}")))?;

    enqueue_template_tuples(&mut transaction, TupleOp::Write, &template).await?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(template)
}

async fn list_templates_inner(
    pool: PgPool,
    identity: Identity,
) -> Result<Vec<AgentTemplateSummary>, HandlerError> {
    sqlx::query_as(
        r#"
        SELECT id, tenant_id, name, description, instructions, allowed_tools,
               created_by_user_id, created_at, deactivated_at
        FROM agent_templates
        WHERE tenant_id = $1 AND deactivated_at IS NULL
        ORDER BY created_at DESC
        LIMIT 100
        "#,
    )
    .bind(identity.tenant_id)
    .fetch_all(&pool)
    .await
    .map_err(|error| TerminalError::new(format!("list agent templates: {error}")).into())
}

async fn deactivate_template_inner(
    pool: PgPool,
    identity: Identity,
    template_id: Uuid,
) -> Result<(), HandlerError> {
    let template = load_template(&pool, template_id).await?;
    let template = ensure_same_tenant(template, identity.tenant_id)?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    sqlx::query(
        r#"
        UPDATE agent_templates
        SET deactivated_at = COALESCE(deactivated_at, NOW())
        WHERE id = $1
        "#,
    )
    .bind(template.id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("deactivate agent template: {error}")))?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(())
}

async fn load_template(
    pool: &PgPool,
    template_id: Uuid,
) -> Result<AgentTemplateSummary, HandlerError> {
    sqlx::query_as(
        r#"
        SELECT id, tenant_id, name, description, instructions, allowed_tools,
               created_by_user_id, created_at, deactivated_at
        FROM agent_templates
        WHERE id = $1
        "#,
    )
    .bind(template_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| TerminalError::new(format!("load agent template: {error}")))?
    .ok_or_else(|| TerminalError::new_with_code(404, "agent template not found").into())
}

fn ensure_same_tenant(
    template: AgentTemplateSummary,
    tenant_id: Uuid,
) -> Result<AgentTemplateSummary, HandlerError> {
    if template.tenant_id == tenant_id {
        return Ok(template);
    }
    Err(TerminalError::new_with_code(404, "agent template not found").into())
}

async fn enqueue_template_tuples(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    op: TupleOp,
    template: &AgentTemplateSummary,
) -> Result<(), HandlerError> {
    enqueue_raw(
        &mut **transaction,
        op,
        &format!("tenant:{}", template.tenant_id),
        "tenant",
        &format!("agent_template:{}", template.id),
        Some(template.tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("agent template tenant outbox: {error}")))?;
    enqueue_raw(
        &mut **transaction,
        op,
        &format!("user:{}", template.created_by_user_id),
        "creator",
        &format!("agent_template:{}", template.id),
        Some(template.tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("agent template creator outbox: {error}")))?;
    Ok(())
}

fn validate_template_request(request: &CreateAgentTemplateRequest) -> Result<(), HandlerError> {
    if request.name.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "template name is required").into());
    }
    if request.instructions.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "template instructions are required").into());
    }
    Ok(())
}

async fn require_tenant_member(identity: &Identity) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Member,
    )
    .await
    .map_err(translate_authz_error)
}

async fn require_tenant_admin(identity: &Identity) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

async fn require_template_creator_or_tenant_admin(
    identity: &Identity,
    template_id: Uuid,
) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    match require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::AgentTemplate,
        template_id,
        Relation::Creator,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(AuthzCheckError::Forbidden { .. }) => require_tenant_admin(identity).await,
        Err(error) => Err(translate_authz_error(error)),
    }
}
