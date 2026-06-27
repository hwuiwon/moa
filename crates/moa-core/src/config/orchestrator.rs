//! Orchestrator endpoint configuration for thin clients.

use serde::{Deserialize, Serialize};

/// HTTP endpoint configuration for the Restate-backed orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestratorConfig {
    /// Restate ingress URL fronting the `moa-orchestrator` deployment.
    pub endpoint: Option<String>,
    /// Restate ingress URL used by hosted runtime clients and tests.
    pub restate_ingress_url: Option<String>,
    /// Restate admin API base URL used for deployment registration and probes.
    pub restate_admin_url: Option<String>,
    /// Optional LLM gateway URL for direct service calls.
    pub llm_gateway_url: Option<String>,
    /// Direct health URL for the orchestrator process.
    pub health_url: Option<String>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            endpoint: Some("http://localhost:10010".to_string()),
            restate_ingress_url: Some("http://localhost:10010".to_string()),
            restate_admin_url: Some("http://localhost:10011".to_string()),
            llm_gateway_url: None,
            health_url: None,
        }
    }
}

impl super::MoaEnvOverlay {
    /// Applies Restate and orchestrator endpoint environment overrides.
    pub(in crate::config) fn apply_orchestrator_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::set_option_if_some;

        if let Some(restate_ingress_url) = &self.restate_ingress_url {
            config.orchestrator.restate_ingress_url = Some(restate_ingress_url.clone());
            config.orchestrator.endpoint = Some(restate_ingress_url.clone());
        }
        if let Some(endpoint) = &self.orchestrator_endpoint {
            config.orchestrator.endpoint = Some(endpoint.clone());
        }
        set_option_if_some(
            &mut config.orchestrator.restate_admin_url,
            &self.restate_admin_url,
        );
        set_option_if_some(
            &mut config.orchestrator.llm_gateway_url,
            &self.restate_llm_gateway_url,
        );
        set_option_if_some(
            &mut config.orchestrator.health_url,
            &self.orchestrator_health_url,
        );
    }
}
