//! Load-test option types and validation.

use crate::*;

/// Execution mode for the load harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LoadMode {
    /// Use the scripted mock provider and exercise only MOA infrastructure.
    Mock,
    /// Use the configured real provider stack.
    Live,
}

/// Backend target for the load harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LoadTarget {
    /// Run an in-process local orchestrator.
    Local,
    /// Drive a running MOA daemon over its Unix socket.
    Daemon,
}

/// Session profile family for the generated workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SessionProfileKind {
    /// Five simple interactive turns.
    Short,
    /// Forty turns with deterministic read-only tool pressure in mock mode.
    Long,
    /// Stable mixed traffic with both short and long sessions.
    Mixed,
}

/// Output format for the final load-test report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Human-readable report text.
    Human,
    /// Structured JSON.
    Json,
}

/// Synthetic timing applied by the mock provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MockProviderTiming {
    /// Delay before the first streamed provider block.
    pub ttft: Duration,
    /// Delay before the final provider response is available.
    pub total: Duration,
}

impl Default for MockProviderTiming {
    fn default() -> Self {
        Self {
            ttft: Duration::ZERO,
            total: Duration::ZERO,
        }
    }
}

/// User-configurable load-test options.
#[derive(Debug, Clone)]
pub struct LoadTestOptions {
    /// Execution mode.
    pub mode: LoadMode,
    /// Backend target.
    pub target: LoadTarget,
    /// Number of concurrent sessions to simulate.
    pub sessions: usize,
    /// Session profile family.
    pub profile: SessionProfileKind,
    /// Delay inserted between turns inside one session.
    pub inter_message_delay: Duration,
    /// Per-turn timeout.
    pub turn_timeout: Duration,
    /// Final output format.
    pub output: OutputFormat,
    /// Optional explicit model override for local live runs.
    pub model: Option<String>,
    /// Optional explicit config path.
    pub config_path: Option<PathBuf>,
    /// Optional explicit workspace root for local runs.
    pub workspace_root: Option<PathBuf>,
    /// Optional daemon socket path.
    pub daemon_socket: Option<PathBuf>,
    /// Synthetic timing for the mock provider.
    pub mock_provider_timing: MockProviderTiming,
}

impl LoadTestOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1..=1_000).contains(&self.sessions) {
            return Err(MoaError::ValidationError(format!(
                "sessions must be between 1 and 1000; got {}",
                self.sessions
            )));
        }
        if matches!(self.mode, LoadMode::Mock) && matches!(self.target, LoadTarget::Daemon) {
            return Err(MoaError::ValidationError(
                "mock mode supports only the in-process local target".to_string(),
            ));
        }
        if self.turn_timeout.is_zero() {
            return Err(MoaError::ValidationError(
                "turn_timeout must be greater than zero".to_string(),
            ));
        }
        if self.model.is_some() && matches!(self.target, LoadTarget::Daemon) {
            return Err(MoaError::ValidationError(
                "model overrides are only supported for the local in-process target".to_string(),
            ));
        }
        if self.mock_provider_timing.total < self.mock_provider_timing.ttft {
            return Err(MoaError::ValidationError(format!(
                "mock provider total duration {:?} must be greater than or equal to TTFT {:?}",
                self.mock_provider_timing.total, self.mock_provider_timing.ttft
            )));
        }
        Ok(())
    }
}

impl LoadMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Live => "live",
        }
    }
}

impl LoadTarget {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Daemon => "daemon",
        }
    }
}

impl SessionProfileKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Long => "long",
            Self::Mixed => "mixed",
        }
    }
}
