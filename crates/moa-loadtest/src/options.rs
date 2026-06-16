//! Load-test option types and validation.

use crate::*;

/// Execution mode for the load harness.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum LoadMode {
    /// Use the scripted mock provider and exercise only MOA infrastructure.
    Mock,
    /// Use the configured real provider stack.
    Live,
}

/// Session profile family for the generated workload.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
    /// Optional global target rate for starting turns.
    pub target_qps: Option<u32>,
    /// Per-turn timeout.
    pub turn_timeout: Duration,
    /// Final output format.
    pub output: OutputFormat,
    /// Optional explicit model override for turn requests.
    pub model: Option<String>,
    /// Optional Prometheus metrics endpoint used to collect step latency.
    pub metrics_endpoint: Option<String>,
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
        if let Some(metrics_endpoint) = self.metrics_endpoint.as_deref() {
            if metrics_endpoint.trim().is_empty() {
                return Err(MoaError::ValidationError(
                    "metrics_endpoint must be non-empty when set".to_string(),
                ));
            }
            url::Url::parse(metrics_endpoint.trim()).map_err(|error| {
                MoaError::ValidationError(format!("metrics_endpoint is not a valid URL: {error}"))
            })?;
        }
        if self.turn_timeout.is_zero() {
            return Err(MoaError::ValidationError(
                "turn_timeout must be greater than zero".to_string(),
            ));
        }
        if self.target_qps == Some(0) {
            return Err(MoaError::ValidationError(
                "target_qps must be greater than zero when set".to_string(),
            ));
        }
        Ok(())
    }
}

impl LoadMode {
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }
}

impl SessionProfileKind {
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_labels_are_pinned() {
        // Pins: the strum-derived `as_str()` output must stay byte-identical to
        // the previous hand-written tables. These strings appear in load-test
        // reports and CLI value names, so they must not drift.
        let load_modes = [(LoadMode::Mock, "mock"), (LoadMode::Live, "live")];
        for (value, label) in load_modes {
            assert_eq!(value.as_str(), label);
        }

        let profiles = [
            (SessionProfileKind::Short, "short"),
            (SessionProfileKind::Long, "long"),
            (SessionProfileKind::Mixed, "mixed"),
        ];
        for (value, label) in profiles {
            assert_eq!(value.as_str(), label);
        }
    }
}
