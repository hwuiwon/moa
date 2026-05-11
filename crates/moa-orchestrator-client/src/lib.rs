//! Thin HTTP client for talking to `moa-orchestrator` through Restate ingress.

pub mod client;
pub mod error;
pub mod session;
pub mod types;

pub use client::{
    AgentSummary, AgentTemplateSummary, AuditVerifyResponse, ClientConfig,
    CreateAgentTemplateRequest, OrchestratorClient, RegisterAgentRequest,
    SetAuditDestinationRequest,
};
pub use error::{Error, Result};
pub use session::{SessionHandle, SnapshotPoller};
pub use types::*;
