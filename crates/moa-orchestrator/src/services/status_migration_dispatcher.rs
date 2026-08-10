//! Bootstrap-only bridge into the private Session status migration handler.

use restate_sdk::prelude::*;

use crate::objects::session_status_migrator::{
    SessionStatusIdleMigrationRequest, SessionStatusIdleMigrationResponse,
    SessionStatusMigratorClient,
};

/// Restate service used by the dedicated cutover bootstrap identity.
#[restate_sdk::service]
#[name = "StatusMigrationDispatcher"]
pub trait StatusMigrationDispatcher {
    /// Migrates one Postgres-enumerated Session virtual object.
    async fn migrate(
        request: Json<SessionStatusIdleMigrationRequest>,
    ) -> Result<Json<SessionStatusIdleMigrationResponse>, HandlerError>;
}

/// Service-to-service dispatcher for the private Session migration operation.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatusMigrationDispatcherImpl;

impl StatusMigrationDispatcher for StatusMigrationDispatcherImpl {
    #[tracing::instrument(skip(self, ctx, request), fields(session_id = %request.0.session_id))]
    // SAFETY: bootstrap-only maintenance surface; Kubernetes ingress policy admits its caller and it can only rewrite the retired lifecycle label for the exact supplied session.
    async fn migrate(
        &self,
        ctx: Context<'_>,
        request: Json<SessionStatusIdleMigrationRequest>,
    ) -> Result<Json<SessionStatusIdleMigrationResponse>, HandlerError> {
        let session_id = request.0.session_id;
        let response = crate::restate_identity::replay_safe_request(
            ctx.object_client::<SessionStatusMigratorClient>(session_id.to_string())
                .migrate_status_idle(request),
        )
        .call()
        .await?;
        Ok(response)
    }
}
