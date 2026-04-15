//! Shared MOA types, traits, configuration, and error definitions.

pub mod config;
pub mod daemon;
pub mod error;
pub mod events;
pub mod shell;
pub mod telemetry;
pub mod traits;
pub mod types;
pub mod workspace;

pub use config::{
    CloudConfig, CloudFlyioConfig, CloudHandsConfig, CloudTemporalConfig, CompactionConfig,
    DaemonConfig, DatabaseBackend, DatabaseConfig, DatabaseNeonConfig, GatewayConfig,
    GeneralConfig, LocalConfig, McpCredentialConfig, McpServerConfig, McpTransportConfig,
    MoaConfig, ObservabilityConfig, OtlpProtocol, PermissionsConfig, ProviderCredentialConfig,
    ProvidersConfig, TuiConfig,
};
pub use daemon::{DaemonCommand, DaemonInfo, DaemonReply, DaemonSessionPreview, DaemonStreamEvent};
pub use error::{MoaError, Result};
pub use events::Event;
pub use telemetry::{TelemetryConfig, TelemetryGuard, default_log_path, init_observability};
pub use traits::{
    BlobStore, BrainOrchestrator, BranchManager, BuiltInTool, ContextProcessor, CredentialVault,
    HandProvider, LLMProvider, MemoryStore, PlatformAdapter, SessionStore, ToolContext,
};
pub use types::*;
