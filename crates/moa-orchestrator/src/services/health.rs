//! Minimal Restate health service used to validate orchestrator registration.

use restate_sdk::prelude::*;

use crate::observability::annotate_restate_handler_span;

const RESTATE_SDK_VERSION: &str = "0.8";

/// Health and version RPCs exposed through Restate.
#[restate_sdk::service]
pub trait Health {
    /// Returns a fixed liveness response.
    async fn ping() -> Result<String, HandlerError>;

    /// Returns build-time version metadata for the running orchestrator.
    async fn version() -> Result<Json<VersionInfo>, HandlerError>;
}

/// Version payload returned by the `Health/version` handler.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VersionInfo {
    /// Version of the `moa-orchestrator` crate.
    pub crate_version: String,
    /// Restate SDK version used by this binary.
    pub restate_sdk_version: String,
    /// Optional Git SHA injected at build time.
    pub git_sha: Option<String>,
}

impl VersionInfo {
    /// Returns the build metadata for the current orchestrator binary.
    pub fn current() -> Self {
        Self {
            crate_version: env!("CARGO_PKG_VERSION").to_string(),
            restate_sdk_version: RESTATE_SDK_VERSION.to_string(),
            git_sha: option_env!("GIT_SHA").map(str::to_owned),
        }
    }
}

/// Concrete implementation of the Restate health service.
pub struct HealthImpl;

impl Health for HealthImpl {
    #[tracing::instrument(skip(self, _ctx))]
    async fn ping(&self, _ctx: Context<'_>) -> Result<String, HandlerError> {
        annotate_restate_handler_span("Health", "ping");
        Ok("pong".to_string())
    }

    #[tracing::instrument(skip(self, _ctx))]
    async fn version(&self, _ctx: Context<'_>) -> Result<Json<VersionInfo>, HandlerError> {
        annotate_restate_handler_span("Health", "version");
        Ok(Json(VersionInfo::current()))
    }
}

#[cfg(test)]
mod tests {
    use super::{RESTATE_SDK_VERSION, VersionInfo};

    #[test]
    fn version_info_reports_expected_versions() {
        let info = VersionInfo::current();

        assert!(
            !info.crate_version.is_empty(),
            "crate version should not be empty"
        );
        assert_eq!(info.restate_sdk_version, RESTATE_SDK_VERSION);
    }
}
