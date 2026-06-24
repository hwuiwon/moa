//! Restate service that returns the trusted inbound identity.

use moa_core::traits::Identity;
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;

use crate::handlers::authz_shim::require_identity;

/// Restate surface for identity pipeline diagnostics.
#[restate_sdk::service]
#[name = "Whoami"]
pub trait Whoami {
    /// Return the identity headers resolved by the orchestrator.
    async fn whoami() -> Result<Json<Identity>, HandlerError>;
}

/// Concrete whoami service implementation.
#[derive(Clone, Default)]
pub struct WhoamiImpl;

impl Whoami for WhoamiImpl {
    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: Informational identity diagnostic; `require_identity` rejects unauthenticated requests.
    async fn whoami(&self, ctx: Context<'_>) -> Result<Json<Identity>, HandlerError> {
        annotate_restate_handler_span("Whoami", "whoami");
        Ok(Json::from(require_identity(&ctx)?))
    }
}
