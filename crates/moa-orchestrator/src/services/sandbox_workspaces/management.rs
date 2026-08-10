//! Transactional sandbox-workspace management application service.

use std::{collections::HashSet, sync::Arc};

use moa_authz::{AuthzError, enqueue_raw};
use moa_authz_schema::TupleOp;
use moa_config::{MoaConfig, SandboxWorkspaceMode};
use moa_core::{
    error::MoaError,
    traits::{Identity, IdentityType, SessionStore},
    types::{
        identifiers::{
            ProviderAccountId, SandboxWorkspaceId, TenantId, WorkspaceCheckpointId,
            WorkspaceOperationId,
        },
        sandbox_workspace::{SandboxWorkspaceScope, SandboxWorkspaceState},
        session::SessionMeta,
    },
};
use moa_hands::core::sandbox_workspace::{
    model::{
        CreateWorkspaceRequest, SandboxWorkspace, WorkspaceGrant, WorkspaceGrantRelation,
        WorkspaceGrantSubjectType,
    },
    repository::PostgresWorkspaceRepository,
};
use moa_wire::sandbox_workspaces::{CreateSandboxWorkspaceRequest, SandboxWorkspaceSummary};
use restate_sdk::prelude::{HandlerError, TerminalError};
use uuid::Uuid;

/// Tenant-safe workspace management operations behind Restate handlers.
#[derive(Clone)]
pub(crate) struct SandboxWorkspaceManagement {
    repository: PostgresWorkspaceRepository,
    admission: SandboxWorkspaceAdmissionPolicy,
    router: Arc<moa_hands::ToolRouter>,
    sessions: Arc<dyn SessionStore>,
}

impl SandboxWorkspaceManagement {
    /// Creates the management application service from validated rollout configuration.
    #[must_use]
    pub(crate) fn from_config(
        pool: sqlx::PgPool,
        config: &MoaConfig,
        fenced_tenants: Arc<HashSet<TenantId>>,
        router: Arc<moa_hands::ToolRouter>,
        sessions: Arc<dyn SessionStore>,
    ) -> Self {
        Self {
            repository: PostgresWorkspaceRepository::new(pool),
            admission: SandboxWorkspaceAdmissionPolicy::from_config(config, fenced_tenants),
            router,
            sessions,
        }
    }

    /// Rejects every service surface while the subsystem is dark.
    pub(super) fn require_management(&self) -> Result<(), HandlerError> {
        self.admission.require_management()
    }

    /// Rejects admission outside `admit` and outside the configured tenant canary.
    pub(super) fn require_admission(&self, tenant_id: TenantId) -> Result<(), HandlerError> {
        self.admission.route(tenant_id).map(|_| ())
    }

    /// Rejects admission-mode handlers before authorization or persistence work.
    pub(super) fn require_admission_mode(&self) -> Result<(), HandlerError> {
        self.admission.require_admission_mode()
    }

    /// Creates workspace metadata, desired grants, and tuple outbox writes atomically.
    pub(super) async fn create(
        &self,
        identity: Identity,
        request: CreateSandboxWorkspaceRequest,
    ) -> Result<SandboxWorkspaceSummary, HandlerError> {
        let route = self.admission.route(identity.tenant_id)?;
        let account = self
            .repository
            .resolve_provider_account(route.provider_account_id, route.provider_account_generation)
            .await
            .map_err(repository_error)?
            .ok_or_else(|| {
                TerminalError::new_with_code(
                    503,
                    "configured sandbox canary provider account is unavailable",
                )
            })?;
        if account.provider != route.provider {
            return Err(TerminalError::new_with_code(
                503,
                "configured sandbox canary provider mapping drifted",
            )
            .into());
        }
        let workspace_id = SandboxWorkspaceId(Uuid::now_v7());
        let grants = desired_grants(&identity, &request.scope);
        let create = CreateWorkspaceRequest {
            workspace_id,
            tenant_id: identity.tenant_id,
            scope: request.scope,
            provider: account.provider,
            provider_account_id: account.provider_account_id,
            provider_account_generation: account.generation,
            durability_class: request.durability_class,
            retention_deadline_at: request.retention_deadline_at,
        };
        let mut conn = self
            .repository
            .begin_transaction(identity.tenant_id)
            .await
            .map_err(repository_error)?;
        let workspace = PostgresWorkspaceRepository::create_with_grants_in_transaction(
            conn.as_mut(),
            &create,
            &grants,
        )
        .await
        .map_err(repository_error)?;
        enqueue_grants(
            conn.as_mut(),
            identity.tenant_id,
            workspace_id,
            TupleOp::Write,
            &grants,
        )
        .await?;
        conn.commit().await.map_err(repository_error)?;
        summary(workspace)
    }

    /// Resolves or creates the one workspace for a verified durable tool owner.
    ///
    /// The caller must derive `scope` from loaded worker/run/task state before
    /// entering this seam. Workspace/provider identifiers remain internal, and
    /// exact replay returns the same row and desired grant ledger.
    pub(crate) async fn resolve_or_create_for_tool(
        &self,
        identity: Identity,
        scope: SandboxWorkspaceScope,
    ) -> Result<SandboxWorkspaceSummary, HandlerError> {
        let route = self.admission.route(identity.tenant_id)?;
        if let Some(existing) = self
            .repository
            .get_by_scope(identity.tenant_id, &scope)
            .await
            .map_err(repository_error)?
        {
            ensure_canary_binding(&existing, route)?;
            ensure_locally_accessible(&existing)?;
            return summary(existing);
        }
        let account = self
            .repository
            .resolve_provider_account(route.provider_account_id, route.provider_account_generation)
            .await
            .map_err(repository_error)?
            .ok_or_else(|| {
                TerminalError::new_with_code(
                    503,
                    "configured sandbox canary provider account is unavailable",
                )
            })?;
        if account.provider != route.provider {
            return Err(TerminalError::new_with_code(
                503,
                "configured sandbox canary provider mapping drifted",
            )
            .into());
        }
        let scope_bytes = serde_json::to_vec(&scope).map_err(|error| {
            tracing::error!(%error, "serialize sandbox workspace scope for deterministic identity");
            HandlerError::from(TerminalError::new_with_code(
                500,
                "sandbox workspace scope is invalid",
            ))
        })?;
        let workspace_id = SandboxWorkspaceId(Uuid::new_v5(&identity.tenant_id.0, &scope_bytes));
        let grants = desired_grants(&identity, &scope);
        let create = CreateWorkspaceRequest {
            workspace_id,
            tenant_id: identity.tenant_id,
            scope: scope.clone(),
            provider: account.provider,
            provider_account_id: account.provider_account_id,
            provider_account_generation: account.generation,
            durability_class:
                moa_core::types::sandbox_workspace::DurabilityClass::PortableFilesystem,
            retention_deadline_at: None,
        };
        let mut conn = self
            .repository
            .begin_transaction(identity.tenant_id)
            .await
            .map_err(repository_error)?;
        let workspace = match PostgresWorkspaceRepository::create_with_grants_in_transaction(
            conn.as_mut(),
            &create,
            &grants,
        )
        .await
        {
            Ok(workspace) => {
                enqueue_grants(
                    conn.as_mut(),
                    identity.tenant_id,
                    workspace_id,
                    TupleOp::Write,
                    &grants,
                )
                .await?;
                conn.commit().await.map_err(repository_error)?;
                workspace
            }
            Err(error) => {
                conn.rollback().await.map_err(repository_error)?;
                self.repository
                    .get_by_scope(identity.tenant_id, &scope)
                    .await
                    .map_err(repository_error)?
                    .ok_or_else(|| repository_error(error))?
            }
        };
        ensure_canary_binding(&workspace, route)?;
        ensure_locally_accessible(&workspace)?;
        summary(workspace)
    }

    /// Lists only rows whose IDs were authorized by OpenFGA first.
    pub(super) async fn list(
        &self,
        identity: Identity,
        workspace_ids: Vec<SandboxWorkspaceId>,
    ) -> Result<Vec<SandboxWorkspaceSummary>, HandlerError> {
        self.repository
            .list_authorized(identity.tenant_id, &workspace_ids)
            .await
            .map_err(repository_error)?
            .into_iter()
            .map(summary)
            .collect()
    }

    /// Loads one authorized workspace while enforcing the local access fence.
    pub(super) async fn get_accessible(
        &self,
        identity: Identity,
        workspace_id: SandboxWorkspaceId,
    ) -> Result<SandboxWorkspaceSummary, HandlerError> {
        let workspace = self.load(identity.tenant_id, workspace_id).await?;
        ensure_locally_accessible(&workspace)?;
        summary(workspace)
    }

    /// Materializes an authorized workspace on its persisted provider binding.
    pub(super) async fn attach(
        &self,
        identity: Identity,
        workspace_id: SandboxWorkspaceId,
    ) -> Result<SandboxWorkspaceSummary, HandlerError> {
        let workspace = self
            .load_accessible(identity.tenant_id, workspace_id)
            .await?;
        let session = self.worker_session(identity.tenant_id, &workspace).await?;
        self.router
            .attach_managed_workspace(&session, &workspace.scope, workspace_id)
            .await
            .map_err(lifecycle_error)?;
        self.get_accessible(identity, workspace_id).await
    }

    /// Publishes one explicit replay-stable checkpoint for an authorized workspace.
    pub(super) async fn checkpoint(
        &self,
        identity: Identity,
        workspace_id: SandboxWorkspaceId,
        operation_id: WorkspaceOperationId,
    ) -> Result<SandboxWorkspaceSummary, HandlerError> {
        let workspace = self
            .load_accessible(identity.tenant_id, workspace_id)
            .await?;
        let session = self.worker_session(identity.tenant_id, &workspace).await?;
        self.router
            .checkpoint_managed_workspace(&session, &workspace.scope, workspace_id, operation_id)
            .await
            .map_err(lifecycle_error)?;
        self.get_accessible(identity, workspace_id).await
    }

    /// Restores the exact current committed checkpoint into fresh compute.
    pub(super) async fn restore(
        &self,
        identity: Identity,
        workspace_id: SandboxWorkspaceId,
        checkpoint_id: WorkspaceCheckpointId,
    ) -> Result<SandboxWorkspaceSummary, HandlerError> {
        let workspace = self
            .load_accessible(identity.tenant_id, workspace_id)
            .await?;
        let session = self.worker_session(identity.tenant_id, &workspace).await?;
        self.router
            .restore_managed_workspace(&session, &workspace.scope, workspace_id, checkpoint_id)
            .await
            .map_err(lifecycle_error)?;
        self.get_accessible(identity, workspace_id).await
    }

    /// Atomically fences local access and enqueues inverse desired tuples.
    pub(super) async fn delete(
        &self,
        identity: Identity,
        workspace_id: SandboxWorkspaceId,
    ) -> Result<SandboxWorkspaceSummary, HandlerError> {
        let current = self.load(identity.tenant_id, workspace_id).await?;
        if current.access_fenced_at.is_some()
            || matches!(
                current.state,
                SandboxWorkspaceState::Deleting | SandboxWorkspaceState::Deleted
            )
        {
            return summary(current);
        }
        let mut conn = self
            .repository
            .begin_transaction(identity.tenant_id)
            .await
            .map_err(repository_error)?;
        let Some((workspace, grants)) =
            PostgresWorkspaceRepository::fence_for_deletion_with_grants_in_transaction(
                conn.as_mut(),
                identity.tenant_id,
                workspace_id,
                current.writer_epoch,
                current.instance_generation,
            )
            .await
            .map_err(repository_error)?
        else {
            return Err(TerminalError::new_with_code(
                409,
                "workspace lifecycle changed before deletion could be fenced",
            )
            .into());
        };
        enqueue_grants(
            conn.as_mut(),
            identity.tenant_id,
            workspace_id,
            TupleOp::Delete,
            &grants,
        )
        .await?;
        conn.commit().await.map_err(repository_error)?;
        summary(workspace)
    }

    async fn load(
        &self,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
    ) -> Result<SandboxWorkspace, HandlerError> {
        self.repository
            .get(tenant_id, workspace_id)
            .await
            .map_err(repository_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "sandbox workspace not found").into())
    }

    async fn load_accessible(
        &self,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
    ) -> Result<SandboxWorkspace, HandlerError> {
        let workspace = self.load(tenant_id, workspace_id).await?;
        ensure_locally_accessible(&workspace)?;
        Ok(workspace)
    }

    async fn worker_session(
        &self,
        tenant_id: TenantId,
        workspace: &SandboxWorkspace,
    ) -> Result<SessionMeta, HandlerError> {
        let SandboxWorkspaceScope::Worker { session_id, .. } = &workspace.scope else {
            return Err(TerminalError::new_with_code(
                409,
                "public workspace lifecycle is unsupported for execution-task scope",
            )
            .into());
        };
        let session = self
            .sessions
            .get_session(*session_id)
            .await
            .map_err(session_error)?;
        if session.tenant_id != tenant_id || session.tenant_id != workspace.tenant_id {
            return Err(TerminalError::new_with_code(
                403,
                "workspace owner session does not belong to the authorized tenant",
            )
            .into());
        }
        Ok(session)
    }
}

#[derive(Clone)]
struct SandboxWorkspaceAdmissionPolicy {
    mode: SandboxWorkspaceMode,
    canary: Option<SandboxWorkspaceCanaryRoute>,
    fenced_tenants: Arc<HashSet<TenantId>>,
}

impl SandboxWorkspaceAdmissionPolicy {
    fn from_config(config: &MoaConfig, fenced_tenants: Arc<HashSet<TenantId>>) -> Self {
        let canary = config
            .sandbox_workspaces
            .canary
            .as_ref()
            .and_then(|canary| {
                let account = config.sandbox_workspace_provider_account(
                    canary.provider_account_id,
                    canary.provider_account_generation,
                )?;
                if account.isolation_cell != canary.isolation_cell {
                    return None;
                }
                Some(SandboxWorkspaceCanaryRoute {
                    provider_account_id: canary.provider_account_id,
                    provider_account_generation: canary.provider_account_generation,
                    provider: account.provider.to_string(),
                    tenant_allowlist: canary.tenant_allowlist.clone(),
                })
            });
        Self {
            mode: config.sandbox_workspaces.mode,
            canary,
            fenced_tenants,
        }
    }

    fn require_management(&self) -> Result<(), HandlerError> {
        if self.mode.maintenance_enabled() {
            return Ok(());
        }
        Err(TerminalError::new_with_code(503, "sandbox workspace service is disabled").into())
    }

    fn route(&self, tenant_id: TenantId) -> Result<&SandboxWorkspaceCanaryRoute, HandlerError> {
        self.require_admission_mode()?;
        if self.fenced_tenants.contains(&tenant_id) {
            return Err(TerminalError::new_with_code(
                503,
                "sandbox workspace admission is blocked by an active tenant destruction fence",
            )
            .into());
        }
        let route = self.canary.as_ref().ok_or_else(|| {
            HandlerError::from(TerminalError::new_with_code(
                503,
                "sandbox workspace canary route is unavailable",
            ))
        })?;
        if !route.tenant_allowlist.contains(&tenant_id) {
            return Err(TerminalError::new_with_code(
                403,
                "tenant is not enabled for sandbox workspace canary admission",
            )
            .into());
        }
        Ok(route)
    }

    fn require_admission_mode(&self) -> Result<(), HandlerError> {
        if self.mode.admission_enabled() {
            return Ok(());
        }
        Err(TerminalError::new_with_code(503, "sandbox workspace admission is disabled").into())
    }
}

#[derive(Clone)]
struct SandboxWorkspaceCanaryRoute {
    provider_account_id: ProviderAccountId,
    provider_account_generation: u64,
    provider: String,
    tenant_allowlist: Vec<TenantId>,
}

fn desired_grants(identity: &Identity, scope: &SandboxWorkspaceScope) -> Vec<WorkspaceGrant> {
    let mut grants = vec![grant(
        WorkspaceGrantSubjectType::Tenant,
        identity.tenant_id.0,
        WorkspaceGrantRelation::Tenant,
    )];
    if let SandboxWorkspaceScope::Worker { session_id, .. } = scope {
        grants.push(grant(
            WorkspaceGrantSubjectType::Session,
            session_id.0,
            WorkspaceGrantRelation::Session,
        ));
    }

    let (subject_type, subject_id) = if let Some(api_key_id) = identity.api_key_id {
        (WorkspaceGrantSubjectType::ApiKey, api_key_id)
    } else {
        (
            match identity.identity_type {
                IdentityType::Operator => WorkspaceGrantSubjectType::Operator,
                IdentityType::Contact => WorkspaceGrantSubjectType::Contact,
                IdentityType::Agent => WorkspaceGrantSubjectType::Agent,
                IdentityType::Service => return grants,
            },
            identity.id,
        )
    };
    match subject_type {
        WorkspaceGrantSubjectType::Operator | WorkspaceGrantSubjectType::ApiKey => {
            for relation in [
                WorkspaceGrantRelation::Owner,
                WorkspaceGrantRelation::Manage,
                WorkspaceGrantRelation::Use,
            ] {
                grants.push(grant(subject_type, subject_id, relation));
            }
        }
        WorkspaceGrantSubjectType::Contact => {
            grants.push(grant(
                subject_type,
                subject_id,
                WorkspaceGrantRelation::Owner,
            ));
            grants.push(grant(subject_type, subject_id, WorkspaceGrantRelation::Use));
        }
        WorkspaceGrantSubjectType::Agent => {
            grants.push(grant(subject_type, subject_id, WorkspaceGrantRelation::Use));
        }
        WorkspaceGrantSubjectType::Tenant | WorkspaceGrantSubjectType::Session => {}
    }
    grants
}

fn grant(
    subject_type: WorkspaceGrantSubjectType,
    subject_id: Uuid,
    relation: WorkspaceGrantRelation,
) -> WorkspaceGrant {
    WorkspaceGrant {
        grant_id: Uuid::now_v7(),
        subject_type,
        subject_id,
        subject_relation: None,
        relation,
    }
}

async fn enqueue_grants(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    workspace_id: SandboxWorkspaceId,
    op: TupleOp,
    grants: &[WorkspaceGrant],
) -> Result<(), HandlerError> {
    let object = format!("sandbox_workspace:{workspace_id}");
    for grant in grants {
        enqueue_raw(
            &mut *conn,
            op,
            &grant.subject_wire(),
            grant.relation.as_str(),
            &object,
            Some(tenant_id.0),
        )
        .await
        .map_err(authz_outbox_error)?;
    }
    Ok(())
}

fn ensure_locally_accessible(workspace: &SandboxWorkspace) -> Result<(), HandlerError> {
    if workspace.access_fenced_at.is_some()
        || matches!(
            workspace.state,
            SandboxWorkspaceState::Deleting | SandboxWorkspaceState::Deleted
        )
    {
        return Err(TerminalError::new_with_code(409, "sandbox workspace access is fenced").into());
    }
    Ok(())
}

fn ensure_canary_binding(
    workspace: &SandboxWorkspace,
    route: &SandboxWorkspaceCanaryRoute,
) -> Result<(), HandlerError> {
    if workspace.provider_account_id != route.provider_account_id
        || u64::try_from(workspace.provider_account_generation).ok()
            != Some(route.provider_account_generation)
        || workspace.provider != route.provider
    {
        return Err(TerminalError::new_with_code(
            409,
            "workspace is outside the configured sandbox canary route",
        )
        .into());
    }
    Ok(())
}

fn summary(workspace: SandboxWorkspace) -> Result<SandboxWorkspaceSummary, HandlerError> {
    Ok(SandboxWorkspaceSummary {
        workspace_id: workspace.workspace_id,
        scope: workspace.scope,
        durability_class: workspace.durability_class,
        state: workspace.state,
        writer_epoch: u64::try_from(workspace.writer_epoch).map_err(invalid_generation)?,
        instance_generation: u64::try_from(workspace.instance_generation)
            .map_err(invalid_generation)?,
        checkpoint_generation: u64::try_from(workspace.checkpoint_generation)
            .map_err(invalid_generation)?,
        checkpoint_id: workspace.checkpoint_id,
        retention_deadline_at: workspace.retention_deadline_at,
        access_fenced: workspace.access_fenced_at.is_some(),
    })
}

fn invalid_generation(_: std::num::TryFromIntError) -> HandlerError {
    TerminalError::new_with_code(500, "workspace contains an invalid generation").into()
}

fn repository_error(error: MoaError) -> HandlerError {
    tracing::error!(%error, "sandbox workspace repository operation failed");
    TerminalError::new_with_code(500, "sandbox workspace persistence failed").into()
}

fn session_error(error: MoaError) -> HandlerError {
    match error {
        MoaError::SessionNotFound(_) => {
            TerminalError::new_with_code(409, "workspace owner session is unavailable").into()
        }
        error => {
            tracing::error!(%error, "sandbox workspace owner session load failed");
            TerminalError::new_with_code(500, "sandbox workspace session persistence failed").into()
        }
    }
}

fn lifecycle_error(error: MoaError) -> HandlerError {
    match error {
        MoaError::PermissionDenied(_) => {
            TerminalError::new_with_code(403, "sandbox workspace scope is not authorized").into()
        }
        MoaError::ValidationError(_) | MoaError::Unsupported(_) => {
            TerminalError::new_with_code(409, error.to_string()).into()
        }
        MoaError::ExternalEffectUnknownOutcome { operation_id } => TerminalError::new_with_code(
            409,
            format!("sandbox workspace operation {operation_id} requires reconciliation"),
        )
        .into(),
        error => {
            tracing::error!(%error, "sandbox workspace lifecycle operation failed");
            TerminalError::new_with_code(500, "sandbox workspace lifecycle operation failed").into()
        }
    }
}

fn authz_outbox_error(error: AuthzError) -> HandlerError {
    tracing::error!(%error, "sandbox workspace authorization outbox write failed");
    TerminalError::new_with_code(500, "sandbox workspace authorization persistence failed").into()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_config::{
        CloudHandProviderAccountConfig, CloudHandProviderKind, CloudHandsConfig,
        ProviderSecretFileSelector, SandboxWorkspaceCanaryConfig,
    };
    use moa_core::types::{
        identifiers::{ProviderAccountId, SessionId},
        sandbox_workspace::DurabilityClass,
    };

    use super::*;

    fn identity(identity_type: IdentityType) -> Identity {
        Identity {
            identity_type,
            id: Uuid::from_u128(2),
            tenant_id: TenantId(Uuid::from_u128(1)),
            api_key_id: None,
            acting_on_behalf_of: None,
        }
    }

    fn rollout_config(mode: SandboxWorkspaceMode) -> MoaConfig {
        let provider_account_id = ProviderAccountId(Uuid::from_u128(5));
        let tenant_id = TenantId(Uuid::from_u128(1));
        let mut config = MoaConfig::default();
        config.cloud.hands = Some(CloudHandsConfig {
            default_provider: Some("e2b".to_string()),
            fallback_providers: Vec::new(),
            provider_accounts: vec![CloudHandProviderAccountConfig {
                provider_account_id,
                generation: 3,
                provider: CloudHandProviderKind::E2b,
                isolation_cell: "canary-a".to_string(),
                api_origin: "https://api.e2b.dev".to_string(),
                toolbox_origin: None,
                sandbox_domain: Some("e2b.app".to_string()),
                default_runtime: Some("base".to_string()),
                project_fingerprint: Some("project:canary-a".to_string()),
                credential: ProviderSecretFileSelector {
                    path: "/test/e2b-credential".into(),
                    owner_uid: 10_001,
                },
            }],
        });
        config.sandbox_workspaces.mode = mode;
        config.sandbox_workspaces.canary = Some(SandboxWorkspaceCanaryConfig {
            provider_account_id,
            provider_account_generation: 3,
            isolation_cell: "canary-a".to_string(),
            tenant_allowlist: vec![tenant_id],
        });
        config
    }

    fn admission_policy(config: &MoaConfig) -> SandboxWorkspaceAdmissionPolicy {
        SandboxWorkspaceAdmissionPolicy::from_config(config, Arc::new(HashSet::new()))
    }

    #[test]
    fn rollout_policy_is_dark_by_default_and_maintenance_rejects_admission() {
        // Pins: accidentally binding the service cannot bypass disabled or
        // maintenance rollout semantics before any provider/database call.
        let disabled = admission_policy(&MoaConfig::default());
        assert!(disabled.require_management().is_err());
        assert!(disabled.route(TenantId(Uuid::from_u128(1))).is_err());

        let maintenance = admission_policy(&rollout_config(SandboxWorkspaceMode::Maintenance));
        maintenance
            .require_management()
            .expect("maintenance keeps management and deletion available");
        assert!(maintenance.route(TenantId(Uuid::from_u128(1))).is_err());
    }

    #[test]
    fn admit_policy_routes_only_allowlisted_tenant_to_exact_canary() {
        // Pins: no caller/provider fallback can replace the deployment-owned
        // provider account generation selected for the canary.
        let policy = admission_policy(&rollout_config(SandboxWorkspaceMode::Admit));
        let route = policy
            .route(TenantId(Uuid::from_u128(1)))
            .expect("allowlisted canary tenant should route");
        assert_eq!(
            route.provider_account_id,
            ProviderAccountId(Uuid::from_u128(5))
        );
        assert_eq!(route.provider_account_generation, 3);
        assert_eq!(route.provider, "e2b");
        assert!(policy.route(TenantId(Uuid::from_u128(99))).is_err());

        let mut drifted = rollout_config(SandboxWorkspaceMode::Admit);
        drifted
            .sandbox_workspaces
            .canary
            .as_mut()
            .expect("fixture has canary")
            .isolation_cell = "wrong-cell".to_string();
        assert!(
            admission_policy(&drifted)
                .route(TenantId(Uuid::from_u128(1)))
                .is_err()
        );
    }

    #[test]
    fn admit_policy_rejects_tenant_skipped_by_fenced_bootstrap() {
        // Pins: a maintenance restart may skip tenant quota bootstrap to finish
        // purge work, but the same process cannot admit new workspace state for
        // that tenant even when it remains on the configured canary allowlist.
        let tenant_id = TenantId(Uuid::from_u128(1));
        let policy = SandboxWorkspaceAdmissionPolicy::from_config(
            &rollout_config(SandboxWorkspaceMode::Admit),
            Arc::new(std::iter::once(tenant_id).collect()),
        );
        assert!(
            policy.route(tenant_id).is_err(),
            "actively purged tenant must fail closed at admission"
        );
    }

    #[test]
    fn contact_workspace_grants_are_private_and_agent_use_is_explicit() {
        // Pins: a contact-owned workspace has no tenant-wide use edge, while a
        // delegated agent receives only its own direct use tuple.
        let scope = SandboxWorkspaceScope::Worker {
            session_id: SessionId(Uuid::from_u128(3)),
            worker_id: "worker-1".to_string(),
        };
        let contact = desired_grants(&identity(IdentityType::Contact), &scope);
        assert_eq!(
            contact
                .iter()
                .map(|grant| (grant.subject_type, grant.relation))
                .collect::<Vec<_>>(),
            vec![
                (
                    WorkspaceGrantSubjectType::Tenant,
                    WorkspaceGrantRelation::Tenant
                ),
                (
                    WorkspaceGrantSubjectType::Session,
                    WorkspaceGrantRelation::Session
                ),
                (
                    WorkspaceGrantSubjectType::Contact,
                    WorkspaceGrantRelation::Owner
                ),
                (
                    WorkspaceGrantSubjectType::Contact,
                    WorkspaceGrantRelation::Use
                ),
            ]
        );
        let agent = desired_grants(&identity(IdentityType::Agent), &scope);
        assert_eq!(
            agent
                .last()
                .map(|grant| (grant.subject_type, grant.relation)),
            Some((
                WorkspaceGrantSubjectType::Agent,
                WorkspaceGrantRelation::Use
            ))
        );
    }

    #[test]
    fn deleting_workspace_is_locally_fenced_even_after_remote_allow() {
        // Pins: a cached OpenFGA allow cannot reopen a workspace after the
        // local deletion fence is visible.
        let workspace = SandboxWorkspace {
            workspace_id: SandboxWorkspaceId(Uuid::from_u128(4)),
            tenant_id: TenantId(Uuid::from_u128(1)),
            scope: SandboxWorkspaceScope::Worker {
                session_id: SessionId(Uuid::from_u128(3)),
                worker_id: "worker-1".to_string(),
            },
            provider: "test".to_string(),
            provider_account_id: ProviderAccountId(Uuid::from_u128(5)),
            provider_account_generation: 1,
            durability_class: DurabilityClass::PortableFilesystem,
            state: SandboxWorkspaceState::Deleting,
            writer_epoch: 1,
            instance_generation: 1,
            checkpoint_generation: 0,
            checkpoint_id: None,
            retention_deadline_at: None,
            delete_generation: 1,
            access_fenced_at: Some(Utc::now()),
        };
        let error = ensure_locally_accessible(&workspace)
            .expect_err("deleting workspace must remain inaccessible");
        assert!(format!("{error:?}").contains("workspace access is fenced"));
    }
}
