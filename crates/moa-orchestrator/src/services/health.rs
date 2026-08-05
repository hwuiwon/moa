//! Public, side-effect-free Restate registration probe.

use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

/// Empty request accepted by the registration probe.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckRequest {}

/// Stable response returned after Restate can route to this deployment.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckResponse {
    /// Fixed service state.
    pub status: String,
}

/// Public Restate surface used only to prove deployment registration and routing.
#[restate_sdk::service]
#[name = "Health"]
pub trait Health {
    /// Returns success without reading or mutating product state.
    async fn check(
        request: Json<HealthCheckRequest>,
    ) -> Result<Json<HealthCheckResponse>, HandlerError>;
}

/// Side-effect-free health implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct HealthImpl;

impl Health for HealthImpl {
    #[tracing::instrument(skip(self, _ctx, _request))]
    // SAFETY: health/observability handler; it reads and mutates no caller-owned data.
    async fn check(
        &self,
        _ctx: Context<'_>,
        _request: Json<HealthCheckRequest>,
    ) -> Result<Json<HealthCheckResponse>, HandlerError> {
        Ok(Json(HealthCheckResponse {
            status: "ok".to_string(),
        }))
    }
}
