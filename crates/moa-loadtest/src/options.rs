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

/// Maximum concurrent session pool size.
const MAX_SESSIONS: usize = 100_000;

/// User-configurable load-test options.
#[derive(Debug, Clone)]
pub struct LoadTestOptions {
    /// Execution mode.
    pub mode: LoadMode,
    /// Restate ingress endpoint fronting `moa-orchestrator`.
    pub endpoint: String,
    /// Optional moa-edge endpoint. When set, turns are driven through the
    /// production edge SSE path; the ingress endpoint is still used for
    /// verification reads.
    pub edge_endpoint: Option<String>,
    /// Concurrent session pool size (finished sessions are replaced while the
    /// schedule is still running).
    pub sessions: usize,
    /// Number of synthetic tenants in the caller pool.
    pub tenants: usize,
    /// Identities created per tenant.
    pub identities_per_tenant: usize,
    /// Session profile family.
    pub profile: SessionProfileKind,
    /// Think time before a session becomes eligible for its next turn.
    pub think_time: Duration,
    /// Offered turn-start rate in turns/second (open loop).
    pub rate: f64,
    /// Inter-arrival process for the schedule.
    pub arrival: ArrivalProcess,
    /// Load window duration (schedule length).
    pub duration: Duration,
    /// Warmup prefix excluded from aggregate percentiles. Defaults to 10% of
    /// the duration capped at 30s when unset.
    pub warmup: Option<Duration>,
    /// Per-turn timeout.
    pub turn_timeout: Duration,
    /// Final output format.
    pub output: OutputFormat,
    /// Optional explicit model override for turn requests.
    pub model: Option<String>,
    /// Optional Prometheus metrics endpoint used to collect step latency.
    pub metrics_endpoint: Option<String>,
    /// RNG seed for schedules, tenant sampling, and plan generation.
    pub seed: u64,
}

impl LoadTestOptions {
    /// Resolved warmup duration.
    pub(crate) fn resolved_warmup(&self) -> Duration {
        self.warmup
            .unwrap_or_else(|| (self.duration / 10).min(Duration::from_secs(30)))
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if !(1..=MAX_SESSIONS).contains(&self.sessions) {
            return Err(MoaError::ValidationError(format!(
                "sessions must be between 1 and {MAX_SESSIONS}; got {}",
                self.sessions
            )));
        }
        if self.tenants == 0 || self.identities_per_tenant == 0 {
            return Err(MoaError::ValidationError(
                "tenants and identities_per_tenant must be greater than zero".to_string(),
            ));
        }
        if self.endpoint.trim().is_empty() {
            return Err(MoaError::ValidationError(
                "endpoint must be non-empty".to_string(),
            ));
        }
        url::Url::parse(self.endpoint.trim()).map_err(|error| {
            MoaError::ValidationError(format!("endpoint is not a valid URL: {error}"))
        })?;
        if let Some(edge_endpoint) = self.edge_endpoint.as_deref() {
            url::Url::parse(edge_endpoint.trim()).map_err(|error| {
                MoaError::ValidationError(format!("edge_endpoint is not a valid URL: {error}"))
            })?;
        }
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
        if self.rate <= 0.0 || !self.rate.is_finite() {
            return Err(MoaError::ValidationError(format!(
                "rate must be a positive finite number; got {}",
                self.rate
            )));
        }
        if self.duration.is_zero() {
            return Err(MoaError::ValidationError(
                "duration must be greater than zero".to_string(),
            ));
        }
        if let Some(warmup) = self.warmup
            && warmup >= self.duration
        {
            return Err(MoaError::ValidationError(
                "warmup must be shorter than duration".to_string(),
            ));
        }
        Ok(())
    }
}

impl LoadMode {
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// Default offered rate when the CLI does not pass one.
    pub fn default_rate(self) -> f64 {
        match self {
            LoadMode::Mock => 50.0,
            LoadMode::Live => 1.0,
        }
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

    pub(crate) fn valid_options() -> LoadTestOptions {
        LoadTestOptions {
            mode: LoadMode::Mock,
            endpoint: "http://localhost:8080".to_string(),
            edge_endpoint: None,
            sessions: 4,
            tenants: 2,
            identities_per_tenant: 1,
            profile: SessionProfileKind::Short,
            think_time: Duration::from_millis(0),
            rate: 10.0,
            arrival: ArrivalProcess::Constant,
            duration: Duration::from_secs(5),
            warmup: Some(Duration::from_secs(1)),
            turn_timeout: Duration::from_secs(30),
            output: OutputFormat::Json,
            model: None,
            metrics_endpoint: None,
            seed: 42,
        }
    }

    #[track_caller]
    fn assert_validation_error(result: Result<()>, needle: &str) {
        match result {
            Err(MoaError::ValidationError(message)) => assert!(
                message.contains(needle),
                "expected validation error containing {needle:?}, got {message:?}"
            ),
            other => panic!("expected a ValidationError containing {needle:?}, got {other:?}"),
        }
    }

    #[test]
    fn validate_baseline_options_are_accepted() {
        // Pins: the baseline used by the rejection tests is itself valid, so each
        // rejection isolates exactly one mutated field.
        valid_options()
            .validate()
            .expect("baseline options validate");
    }

    #[test]
    fn validate_rejects_sessions_outside_supported_range() {
        // Pins: the harness refuses zero sessions and counts above the ceiling.
        let mut zero = valid_options();
        zero.sessions = 0;
        assert_validation_error(zero.validate(), "sessions must be between 1 and");

        let mut too_many = valid_options();
        too_many.sessions = MAX_SESSIONS + 1;
        assert_validation_error(too_many.validate(), "sessions must be between 1 and");
    }

    #[test]
    fn validate_rejects_non_url_endpoint() {
        // Pins: a non-empty but unparsable endpoint is rejected as an invalid URL.
        let mut options = valid_options();
        options.endpoint = "not-a-url".to_string();
        assert_validation_error(options.validate(), "endpoint is not a valid URL");
    }

    #[test]
    fn validate_rejects_blank_metrics_endpoint() {
        // Pins: an explicitly-set metrics endpoint must carry a value, not whitespace.
        let mut options = valid_options();
        options.metrics_endpoint = Some("   ".to_string());
        assert_validation_error(
            options.validate(),
            "metrics_endpoint must be non-empty when set",
        );
    }

    #[test]
    fn validate_rejects_zero_turn_timeout() {
        // Pins: a zero per-turn timeout would make every turn time out instantly.
        let mut options = valid_options();
        options.turn_timeout = Duration::ZERO;
        assert_validation_error(options.validate(), "turn_timeout must be greater than zero");
    }

    #[test]
    fn validate_rejects_non_positive_rate_and_warmup_at_or_past_duration() {
        // Pins: open-loop pacing needs a positive finite rate, and warmup must
        // leave a measurement window.
        let mut zero_rate = valid_options();
        zero_rate.rate = 0.0;
        assert_validation_error(zero_rate.validate(), "rate must be a positive finite");

        let mut long_warmup = valid_options();
        long_warmup.warmup = Some(Duration::from_secs(5));
        assert_validation_error(long_warmup.validate(), "warmup must be shorter");
    }

    #[test]
    fn default_warmup_is_ten_percent_capped_at_thirty_seconds() {
        // Pins: unset warmup derives from duration so short smoke runs are not
        // fully swallowed by warmup exclusion.
        let mut options = valid_options();
        options.warmup = None;
        options.duration = Duration::from_secs(100);
        assert_eq!(options.resolved_warmup(), Duration::from_secs(10));

        options.duration = Duration::from_secs(3_600);
        assert_eq!(options.resolved_warmup(), Duration::from_secs(30));
    }
}
