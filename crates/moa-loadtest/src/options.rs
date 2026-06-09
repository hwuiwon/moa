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

/// User-configurable load-test options.
#[derive(Debug, Clone)]
pub struct LoadTestOptions {
    /// Execution mode.
    pub mode: LoadMode,
    /// Restate ingress endpoint fronting `moa-orchestrator`.
    pub endpoint: String,
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
    /// Optional explicit model override for turn requests.
    pub model: Option<String>,
}

impl LoadTestOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1..=1_000).contains(&self.sessions) {
            return Err(MoaError::ValidationError(format!(
                "sessions must be between 1 and 1000; got {}",
                self.sessions
            )));
        }
        if self.endpoint.trim().is_empty() {
            return Err(MoaError::ValidationError(
                "endpoint must be non-empty".to_string(),
            ));
        }
        url::Url::parse(self.endpoint.trim()).map_err(|error| {
            MoaError::ValidationError(format!("endpoint is not a valid URL: {error}"))
        })?;
        if self.turn_timeout.is_zero() {
            return Err(MoaError::ValidationError(
                "turn_timeout must be greater than zero".to_string(),
            ));
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

impl SessionProfileKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Long => "long",
            Self::Mixed => "mixed",
        }
    }
}
