//! Hand provisioning and sandbox lifecycle types.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{MoaError, Result};

/// Sandbox isolation tier for a hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxTier {
    /// No sandbox.
    None,
    /// Container sandbox.
    Container,
    /// MicroVM sandbox.
    MicroVM,
    /// Direct host execution.
    Local,
}

/// Resource requirements for a provisioned hand.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandResources {
    /// Requested CPU in millicores.
    pub cpu_millicores: u32,
    /// Requested memory in megabytes.
    pub memory_mb: u32,
}

/// Specification for provisioning a hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandSpec {
    /// Required sandbox tier.
    pub sandbox_tier: SandboxTier,
    /// Optional image identifier.
    pub image: Option<String>,
    /// Resource requirements.
    pub resources: HandResources,
    /// Environment variables passed to the hand.
    pub env: HashMap<String, String>,
    /// Optional workspace mount path.
    pub workspace_mount: Option<PathBuf>,
    /// Idle timeout.
    pub idle_timeout: Duration,
    /// Maximum lifetime.
    pub max_lifetime: Duration,
}

/// Opaque handle to a provisioned hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandHandle {
    /// Local host execution sandbox.
    Local { sandbox_dir: PathBuf },
    /// Docker container-backed sandbox.
    Docker { container_id: String },
    /// Daytona workspace handle.
    Daytona { workspace_id: String },
    /// E2B sandbox handle.
    E2B { sandbox_id: String },
}

impl HandHandle {
    /// Creates a local hand handle.
    pub fn local(sandbox_dir: PathBuf) -> Self {
        Self::Local { sandbox_dir }
    }

    /// Creates a Docker hand handle.
    pub fn docker(container_id: impl Into<String>) -> Self {
        Self::Docker {
            container_id: container_id.into(),
        }
    }

    /// Creates a Daytona hand handle.
    pub fn daytona(workspace_id: impl Into<String>) -> Self {
        Self::Daytona {
            workspace_id: workspace_id.into(),
        }
    }

    /// Creates an E2B hand handle.
    pub fn e2b(sandbox_id: impl Into<String>) -> Self {
        Self::E2B {
            sandbox_id: sandbox_id.into(),
        }
    }

    /// Returns the Daytona workspace identifier when the handle is Daytona-backed.
    pub fn daytona_id(&self) -> Result<&str> {
        match self {
            Self::Daytona { workspace_id } => Ok(workspace_id.as_str()),
            _ => Err(MoaError::ProviderError(
                "hand handle is not a Daytona workspace".to_string(),
            )),
        }
    }
}

/// Observed lifecycle state of a hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandStatus {
    /// Provisioning is in progress.
    Provisioning,
    /// Ready to accept tool calls.
    Running,
    /// Temporarily paused.
    Paused,
    /// Stopped but recoverable.
    Stopped,
    /// Permanently destroyed.
    Destroyed,
    /// Failed.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::SandboxTier;

    #[test]
    fn all_sandbox_tiers_exist() {
        let _ = SandboxTier::None;
        let _ = SandboxTier::Container;
        let _ = SandboxTier::MicroVM;
        let _ = SandboxTier::Local;
    }
}
