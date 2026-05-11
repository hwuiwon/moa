//! Orchestrator endpoint configuration for thin clients.

use serde::{Deserialize, Serialize};

/// HTTP endpoint configuration for the Restate-backed orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestratorConfig {
    /// Restate ingress URL fronting the `moa-orchestrator` deployment.
    pub endpoint: Option<String>,
    /// Direct health URL for the orchestrator process.
    pub health_url: Option<String>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            endpoint: Some("http://localhost:10010".to_string()),
            health_url: None,
        }
    }
}
