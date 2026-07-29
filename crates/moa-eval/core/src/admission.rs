//! Eval admission policy over the runtime resource contract.
//!
//! Admission runs once, before any case is dispatched, and answers a single
//! question: may this `(suite, configs, parallelism)` matrix run at all? It
//! converts authored eval inputs into the versioned runtime contract in
//! [`moa_core::types::resource`] — a [`ResourceEnvelope`] for the whole run plus
//! the worst-case [`ResourceAmounts`] one case may reserve.
//!
//! Two rules keep this honest:
//!
//! * **Hard reject, never clamp.** An oversized suite, an out-of-range
//!   parallelism, or an over-long timeout is an error the author must fix. A
//!   silently reduced value would run something other than what was authored and
//!   report it as the authored run.
//! * **Checked arithmetic.** Matrix and worst-case projections use checked
//!   multiplication so an overflow is a rejection, not a wrapped budget that
//!   admits unbounded paid work.
//!
//! The policy deliberately lives here rather than in `moa-core`: runtime crates
//! must depend on the resource contract without ever depending on an eval crate.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use moa_core::types::resource::{
    RESOURCE_CONTRACT_VERSION, ResourceAmounts, ResourceEnvelope, ResourceKind,
};
use serde::{Deserialize, Serialize};

use crate::{AgentConfig, TestCase, TestSuite};

/// Version of the eval admission-limits contract.
pub const EVAL_ADMISSION_VERSION: u32 = 1;

/// Hard maximums applied to every eval run.
///
/// Every field is an inclusive maximum: a value landing exactly on the limit is
/// admitted, one unit past it is rejected.
///
/// The defaults are platform guard rails, not an operator budget: `total` is
/// sized as `per_case * max_total_runs` so a legitimately sized suite is never
/// truncated mid-run, while every dimension stays finite and enforced. Callers
/// that want a tighter budget lower `total` and the ledger stops scheduling as
/// soon as the remaining capacity cannot cover another case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvalAdmissionLimits {
    /// Version of this limits contract.
    pub version: u32,
    /// Maximum encoded size of a whole suite document.
    pub max_suite_bytes: usize,
    /// Maximum encoded size of one test case.
    pub max_case_bytes: usize,
    /// Maximum encoded size of one agent config document.
    pub max_agent_config_bytes: usize,
    /// Maximum number of cases in a suite.
    pub max_cases: usize,
    /// Maximum number of agent configs in one run.
    pub max_agent_configs: usize,
    /// Maximum `(config, case)` executions in one run.
    pub max_total_runs: usize,
    /// Maximum cases dispatched concurrently.
    pub max_parallel_cases: usize,
    /// Maximum wall-clock seconds for one case.
    pub max_case_seconds: u64,
    /// Maximum wall-clock seconds for the whole run.
    pub max_total_seconds: u64,
    /// Worst-case resources one case may reserve before dispatch.
    pub per_case: ResourceAmounts,
    /// Resources the whole run may consume.
    pub total: ResourceAmounts,
}

impl Default for EvalAdmissionLimits {
    fn default() -> Self {
        Self {
            version: EVAL_ADMISSION_VERSION,
            max_suite_bytes: 4 * 1024 * 1024,
            max_case_bytes: 256 * 1024,
            max_agent_config_bytes: 256 * 1024,
            max_cases: 1_000,
            max_agent_configs: 32,
            max_total_runs: 2_000,
            max_parallel_cases: 32,
            max_case_seconds: 900,
            max_total_seconds: 6 * 60 * 60,
            per_case: ResourceAmounts {
                cost_micro_usd: 2_000_000,
                tokens: 4_000_000,
                turns: 32,
                model_calls: 64,
                tool_calls: 512,
            },
            total: ResourceAmounts {
                cost_micro_usd: 4_000_000_000,
                tokens: 8_000_000_000,
                turns: 64_000,
                model_calls: 128_000,
                tool_calls: 1_024_000,
            },
        }
    }
}

/// An admitted run: the resource contract the engine must enforce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedRun {
    /// Version of the limits that admitted this run.
    pub limits_version: u32,
    /// Envelope covering the whole run, including its absolute deadline.
    pub envelope: ResourceEnvelope,
    /// Worst-case amounts reserved before dispatching one case.
    pub per_case: ResourceAmounts,
    /// `per_case * total_runs`, proven not to overflow at admission time.
    pub worst_case_projection: ResourceAmounts,
    /// Bounded concurrency the engine may use.
    pub parallel: usize,
    /// Number of `(config, case)` executions.
    pub total_runs: usize,
    /// Ceiling applied to any single case's wall-clock budget.
    pub max_case_seconds: u64,
}

/// Applies [`EvalAdmissionLimits`] to authored eval inputs.
#[derive(Debug, Clone, Default)]
pub struct EvalAdmissionPolicy {
    limits: EvalAdmissionLimits,
}

impl EvalAdmissionPolicy {
    /// Creates a policy from explicit limits.
    #[must_use]
    pub const fn new(limits: EvalAdmissionLimits) -> Self {
        Self { limits }
    }

    /// Returns the limits this policy enforces.
    #[must_use]
    pub const fn limits(&self) -> &EvalAdmissionLimits {
        &self.limits
    }

    /// Admits or rejects a whole run before any case is dispatched.
    ///
    /// On `Ok`, the caller holds the full resource contract for the run. On
    /// `Err`, nothing may be dispatched.
    pub fn admit(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        parallel: usize,
        now: DateTime<Utc>,
    ) -> Result<AdmittedRun, AdmissionError> {
        self.validate_limits()?;
        self.validate_shape(suite, configs)?;
        let total_runs = self.validate_matrix(suite, configs)?;
        self.check_parallelism(parallel)?;
        self.validate_sizes(suite, configs)?;
        self.validate_timeouts(suite)?;

        let per_case = self.limits.per_case;
        let worst_case_projection =
            per_case
                .checked_mul(total_runs as u64)
                .ok_or(AdmissionError::ResourceOverflow {
                    kind: first_nonzero_kind(&per_case),
                })?;

        let deadline = now
            .checked_add_signed(Duration::seconds(
                i64::try_from(self.limits.max_total_seconds).map_err(|_| {
                    AdmissionError::InvalidLimits(
                        "max_total_seconds does not fit in a signed second count".to_string(),
                    )
                })?,
            ))
            .ok_or_else(|| {
                AdmissionError::InvalidLimits(
                    "max_total_seconds overflows the run deadline".to_string(),
                )
            })?;

        Ok(AdmittedRun {
            limits_version: self.limits.version,
            envelope: ResourceEnvelope::new(self.limits.total, Some(deadline)),
            per_case,
            worst_case_projection,
            parallel,
            total_runs,
            max_case_seconds: self.limits.max_case_seconds,
        })
    }

    /// Resolves the wall-clock budget for one already-admitted case.
    ///
    /// An absent timeout falls back to the policy ceiling; it is not an authored
    /// value being clamped. An authored value that violates the policy is
    /// rejected by [`Self::admit`] before this is ever called.
    pub fn effective_case_seconds(
        &self,
        case: &TestCase,
        suite: &TestSuite,
    ) -> Result<u64, AdmissionError> {
        let authored = match case.timeout_seconds {
            Some(0) => {
                return Err(AdmissionError::InvalidCaseTimeout {
                    case: case.name.clone(),
                });
            }
            Some(seconds) => Some(seconds),
            None if suite.default_timeout_seconds > 0 => Some(suite.default_timeout_seconds),
            None => None,
        };

        match authored {
            Some(seconds) if seconds > self.limits.max_case_seconds => {
                Err(AdmissionError::CaseTimeoutTooLong {
                    case: case.name.clone(),
                    seconds,
                    limit: self.limits.max_case_seconds,
                })
            }
            Some(seconds) => Ok(seconds),
            None => Ok(self.limits.max_case_seconds),
        }
    }

    fn validate_limits(&self) -> Result<(), AdmissionError> {
        if self.limits.version != EVAL_ADMISSION_VERSION {
            return Err(AdmissionError::UnsupportedVersion {
                version: self.limits.version,
                supported: EVAL_ADMISSION_VERSION,
            });
        }
        if self.limits.max_parallel_cases == 0 {
            return Err(AdmissionError::InvalidLimits(
                "max_parallel_cases must be at least 1".to_string(),
            ));
        }
        if self.limits.max_case_seconds == 0 || self.limits.max_total_seconds == 0 {
            return Err(AdmissionError::InvalidLimits(
                "case and total second budgets must be at least 1".to_string(),
            ));
        }
        if self.limits.max_case_seconds > self.limits.max_total_seconds {
            return Err(AdmissionError::InvalidLimits(
                "max_case_seconds must not exceed max_total_seconds".to_string(),
            ));
        }
        if self.limits.per_case.is_zero() {
            return Err(AdmissionError::InvalidLimits(
                "per_case limits must reserve at least one resource".to_string(),
            ));
        }
        if let Some(kind) = self.limits.per_case.first_exceeding(&self.limits.total) {
            return Err(AdmissionError::PerCaseExceedsTotal { kind });
        }
        Ok(())
    }

    fn validate_shape(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
    ) -> Result<(), AdmissionError> {
        if suite.cases.is_empty() {
            return Err(AdmissionError::EmptySuite);
        }
        if configs.is_empty() {
            return Err(AdmissionError::NoAgentConfigs);
        }

        let mut case_names = HashSet::with_capacity(suite.cases.len());
        for case in &suite.cases {
            if case.name.trim().is_empty() {
                return Err(AdmissionError::UnnamedCase);
            }
            if !case_names.insert(case.name.as_str()) {
                return Err(AdmissionError::DuplicateCaseName {
                    name: case.name.clone(),
                });
            }
        }

        let mut config_names = HashSet::with_capacity(configs.len());
        for config in configs {
            if config.name.trim().is_empty() {
                return Err(AdmissionError::UnnamedAgentConfig);
            }
            if !config_names.insert(config.name.as_str()) {
                return Err(AdmissionError::DuplicateAgentConfigName {
                    name: config.name.clone(),
                });
            }
        }

        Ok(())
    }

    fn validate_matrix(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
    ) -> Result<usize, AdmissionError> {
        if suite.cases.len() > self.limits.max_cases {
            return Err(AdmissionError::TooManyCases {
                count: suite.cases.len(),
                limit: self.limits.max_cases,
            });
        }
        if configs.len() > self.limits.max_agent_configs {
            return Err(AdmissionError::TooManyAgentConfigs {
                count: configs.len(),
                limit: self.limits.max_agent_configs,
            });
        }

        let total_runs =
            configs
                .len()
                .checked_mul(suite.cases.len())
                .ok_or(AdmissionError::MatrixOverflow {
                    configs: configs.len(),
                    cases: suite.cases.len(),
                })?;
        if total_runs > self.limits.max_total_runs {
            return Err(AdmissionError::MatrixTooLarge {
                total_runs,
                limit: self.limits.max_total_runs,
            });
        }
        Ok(total_runs)
    }

    /// Rejects a parallelism request outside `1..=max_parallel_cases`.
    ///
    /// Exposed so an engine can refuse an out-of-range option at construction
    /// time rather than at the first run.
    pub fn check_parallelism(&self, parallel: usize) -> Result<(), AdmissionError> {
        if parallel == 0 {
            return Err(AdmissionError::InvalidParallelism);
        }
        if parallel > self.limits.max_parallel_cases {
            return Err(AdmissionError::ParallelismTooHigh {
                requested: parallel,
                limit: self.limits.max_parallel_cases,
            });
        }
        Ok(())
    }

    fn validate_sizes(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
    ) -> Result<(), AdmissionError> {
        let suite_bytes = encoded_bytes(suite)?;
        if suite_bytes > self.limits.max_suite_bytes {
            return Err(AdmissionError::SuiteTooLarge {
                bytes: suite_bytes,
                limit: self.limits.max_suite_bytes,
            });
        }
        for case in &suite.cases {
            let bytes = encoded_bytes(case)?;
            if bytes > self.limits.max_case_bytes {
                return Err(AdmissionError::CaseTooLarge {
                    case: case.name.clone(),
                    bytes,
                    limit: self.limits.max_case_bytes,
                });
            }
        }
        for config in configs {
            let bytes = encoded_bytes(config)?;
            if bytes > self.limits.max_agent_config_bytes {
                return Err(AdmissionError::AgentConfigTooLarge {
                    config: config.name.clone(),
                    bytes,
                    limit: self.limits.max_agent_config_bytes,
                });
            }
        }
        Ok(())
    }

    fn validate_timeouts(&self, suite: &TestSuite) -> Result<(), AdmissionError> {
        if suite.default_timeout_seconds > self.limits.max_case_seconds {
            return Err(AdmissionError::SuiteTimeoutTooLong {
                seconds: suite.default_timeout_seconds,
                limit: self.limits.max_case_seconds,
            });
        }
        for case in &suite.cases {
            self.effective_case_seconds(case, suite)?;
        }
        Ok(())
    }
}

/// Returns the encoded byte length of an authored eval document.
fn encoded_bytes<T: Serialize>(value: &T) -> Result<usize, AdmissionError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|error| AdmissionError::Encode(error.to_string()))
}

fn first_nonzero_kind(amounts: &ResourceAmounts) -> ResourceKind {
    ResourceKind::ALL
        .into_iter()
        .find(|&kind| amounts.get(kind) > 0)
        .unwrap_or(ResourceKind::CostMicroUsd)
}

/// Reasons an eval run is refused before any work is dispatched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    /// The limits declare a contract version this build does not implement.
    #[error("unsupported eval admission version {version} (supported: {supported})")]
    UnsupportedVersion {
        /// Version carried by the limits.
        version: u32,
        /// Version implemented by this build.
        supported: u32,
    },
    /// The configured limits are internally inconsistent.
    #[error("invalid eval admission limits: {0}")]
    InvalidLimits(String),
    /// A per-case limit is larger than the whole-run limit.
    #[error("per-case {kind} limit exceeds the total run limit")]
    PerCaseExceedsTotal {
        /// Dimension that is misconfigured.
        kind: ResourceKind,
    },
    /// The suite has no cases.
    #[error("eval suite contains no cases")]
    EmptySuite,
    /// The run has no agent configs.
    #[error("eval run has no agent configs")]
    NoAgentConfigs,
    /// A case has a blank name.
    #[error("eval suite contains a case with a blank name")]
    UnnamedCase,
    /// An agent config has a blank name.
    #[error("eval run contains an agent config with a blank name")]
    UnnamedAgentConfig,
    /// Two cases share a name, which would make results ambiguous.
    #[error("duplicate eval case name '{name}'")]
    DuplicateCaseName {
        /// Repeated case name.
        name: String,
    },
    /// Two agent configs share a name.
    #[error("duplicate eval agent config name '{name}'")]
    DuplicateAgentConfigName {
        /// Repeated config name.
        name: String,
    },
    /// The suite has more cases than the policy allows.
    #[error("eval suite has {count} cases, more than the {limit} allowed")]
    TooManyCases {
        /// Authored case count.
        count: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// The run has more agent configs than the policy allows.
    #[error("eval run has {count} agent configs, more than the {limit} allowed")]
    TooManyAgentConfigs {
        /// Authored config count.
        count: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// The matrix size overflowed while multiplying configs by cases.
    #[error("eval matrix size overflowed for {configs} configs and {cases} cases")]
    MatrixOverflow {
        /// Config count.
        configs: usize,
        /// Case count.
        cases: usize,
    },
    /// The matrix has more executions than the policy allows.
    #[error("eval matrix has {total_runs} runs, more than the {limit} allowed")]
    MatrixTooLarge {
        /// Computed execution count.
        total_runs: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// Parallelism was zero.
    #[error("eval parallelism must be at least 1")]
    InvalidParallelism,
    /// Parallelism exceeded the policy bound.
    #[error("eval parallelism {requested} exceeds the {limit} allowed")]
    ParallelismTooHigh {
        /// Requested concurrency.
        requested: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// The encoded suite is larger than the policy allows.
    #[error("eval suite is {bytes} bytes, larger than the {limit} allowed")]
    SuiteTooLarge {
        /// Encoded size.
        bytes: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// An encoded case is larger than the policy allows.
    #[error("eval case '{case}' is {bytes} bytes, larger than the {limit} allowed")]
    CaseTooLarge {
        /// Case name.
        case: String,
        /// Encoded size.
        bytes: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// An encoded agent config is larger than the policy allows.
    #[error("eval agent config '{config}' is {bytes} bytes, larger than the {limit} allowed")]
    AgentConfigTooLarge {
        /// Config name.
        config: String,
        /// Encoded size.
        bytes: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// A case authored a zero-second timeout.
    #[error("eval case '{case}' authored a zero-second timeout")]
    InvalidCaseTimeout {
        /// Case name.
        case: String,
    },
    /// A case authored a timeout beyond the policy ceiling.
    #[error("eval case '{case}' timeout of {seconds}s exceeds the {limit}s allowed")]
    CaseTimeoutTooLong {
        /// Case name.
        case: String,
        /// Authored seconds.
        seconds: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// The suite default timeout is beyond the policy ceiling.
    #[error("eval suite default timeout of {seconds}s exceeds the {limit}s allowed")]
    SuiteTimeoutTooLong {
        /// Authored seconds.
        seconds: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// The worst-case resource projection overflowed.
    #[error("eval worst-case {kind} projection overflowed")]
    ResourceOverflow {
        /// Dimension whose projection overflowed.
        kind: ResourceKind,
    },
    /// Encoding an authored document for size measurement failed.
    #[error("failed to encode eval document for admission: {0}")]
    Encode(String),
}

/// Returns the resource contract version this admission policy targets.
#[must_use]
pub const fn resource_contract_version() -> u32 {
    RESOURCE_CONTRACT_VERSION
}

#[cfg(test)]
mod tests {
    use super::{AdmissionError, EVAL_ADMISSION_VERSION, EvalAdmissionLimits, EvalAdmissionPolicy};
    use crate::{AgentConfig, TestCase, TestSuite};
    use chrono::Utc;
    use moa_core::types::resource::{ResourceAmounts, ResourceKind};

    fn case(name: &str) -> TestCase {
        TestCase {
            name: name.to_string(),
            input: "hello".to_string(),
            ..TestCase::default()
        }
    }

    fn config(name: &str) -> AgentConfig {
        AgentConfig {
            name: name.to_string(),
            ..AgentConfig::default()
        }
    }

    fn suite(cases: Vec<TestCase>) -> TestSuite {
        TestSuite {
            name: "suite".to_string(),
            cases,
            ..TestSuite::default()
        }
    }

    #[test]
    fn matrix_exactly_at_every_limit_is_admitted() {
        // Pins: admission compares with `>`, so a matrix sized exactly at the
        // limits runs. Weakening any comparison to `>=` fails here.
        let limits = EvalAdmissionLimits {
            max_cases: 2,
            max_agent_configs: 2,
            max_total_runs: 4,
            max_parallel_cases: 2,
            ..EvalAdmissionLimits::default()
        };
        let policy = EvalAdmissionPolicy::new(limits);

        let admitted = policy
            .admit(
                &suite(vec![case("a"), case("b")]),
                &[config("baseline"), config("variant")],
                2,
                Utc::now(),
            )
            .expect("exactly-at-limit matrix is admitted");

        assert_eq!(admitted.total_runs, 4);
        assert_eq!(admitted.parallel, 2);
        assert_eq!(admitted.limits_version, EVAL_ADMISSION_VERSION);
        assert!(admitted.envelope.deadline.is_some());
    }

    #[test]
    fn one_case_over_the_case_limit_is_rejected() {
        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            max_cases: 1,
            ..EvalAdmissionLimits::default()
        });

        let error = policy
            .admit(
                &suite(vec![case("a"), case("b")]),
                &[config("baseline")],
                1,
                Utc::now(),
            )
            .expect_err("two cases exceed a one-case limit");
        assert!(matches!(
            error,
            AdmissionError::TooManyCases { count: 2, limit: 1 }
        ));
    }

    #[test]
    fn one_run_over_the_matrix_limit_is_rejected() {
        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            max_total_runs: 3,
            ..EvalAdmissionLimits::default()
        });

        let error = policy
            .admit(
                &suite(vec![case("a"), case("b")]),
                &[config("baseline"), config("variant")],
                1,
                Utc::now(),
            )
            .expect_err("four runs exceed a three-run limit");
        assert!(matches!(
            error,
            AdmissionError::MatrixTooLarge {
                total_runs: 4,
                limit: 3
            }
        ));
    }

    #[test]
    fn parallelism_is_bounded_at_both_ends() {
        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            max_parallel_cases: 4,
            ..EvalAdmissionLimits::default()
        });
        let suite = suite(vec![case("a")]);
        let configs = [config("baseline")];

        assert!(matches!(
            policy.admit(&suite, &configs, 0, Utc::now()),
            Err(AdmissionError::InvalidParallelism)
        ));
        policy
            .admit(&suite, &configs, 4, Utc::now())
            .expect("parallelism exactly at the bound is admitted");
        assert!(matches!(
            policy.admit(&suite, &configs, 5, Utc::now()),
            Err(AdmissionError::ParallelismTooHigh {
                requested: 5,
                limit: 4
            })
        ));
    }

    #[test]
    fn oversized_documents_are_rejected_not_truncated() {
        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            max_case_bytes: 64,
            ..EvalAdmissionLimits::default()
        });
        let mut oversized = case("a");
        oversized.input = "x".repeat(4_096);

        let error = policy
            .admit(
                &suite(vec![oversized]),
                &[config("baseline")],
                1,
                Utc::now(),
            )
            .expect_err("oversized case is rejected");
        assert!(matches!(error, AdmissionError::CaseTooLarge { .. }));

        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            max_suite_bytes: 16,
            ..EvalAdmissionLimits::default()
        });
        assert!(matches!(
            policy.admit(
                &suite(vec![case("a")]),
                &[config("baseline")],
                1,
                Utc::now()
            ),
            Err(AdmissionError::SuiteTooLarge { .. })
        ));
    }

    #[test]
    fn worst_case_projection_overflow_is_rejected() {
        // Pins: `per_case * total_runs` uses checked multiplication, so an
        // overflowing projection refuses the run instead of wrapping to a small
        // budget.
        let huge = ResourceAmounts {
            cost_micro_usd: u64::MAX,
            tokens: u64::MAX,
            turns: u64::MAX,
            model_calls: u64::MAX,
            tool_calls: u64::MAX,
        };
        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            per_case: huge,
            total: huge,
            ..EvalAdmissionLimits::default()
        });

        let error = policy
            .admit(
                &suite(vec![case("a"), case("b")]),
                &[config("baseline")],
                1,
                Utc::now(),
            )
            .expect_err("overflowing projection is rejected");
        assert!(matches!(
            error,
            AdmissionError::ResourceOverflow {
                kind: ResourceKind::CostMicroUsd
            }
        ));
    }

    #[test]
    fn per_case_limits_above_the_total_are_rejected() {
        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            per_case: ResourceAmounts {
                cost_micro_usd: 10,
                ..ResourceAmounts::ZERO
            },
            total: ResourceAmounts {
                cost_micro_usd: 9,
                ..ResourceAmounts::ZERO
            },
            ..EvalAdmissionLimits::default()
        });

        assert!(matches!(
            policy.admit(
                &suite(vec![case("a")]),
                &[config("baseline")],
                1,
                Utc::now()
            ),
            Err(AdmissionError::PerCaseExceedsTotal {
                kind: ResourceKind::CostMicroUsd
            })
        ));
    }

    #[test]
    fn timeouts_are_rejected_rather_than_clamped() {
        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            max_case_seconds: 60,
            ..EvalAdmissionLimits::default()
        });

        let mut too_long = case("a");
        too_long.timeout_seconds = Some(61);
        assert!(matches!(
            policy.admit(&suite(vec![too_long]), &[config("baseline")], 1, Utc::now()),
            Err(AdmissionError::CaseTimeoutTooLong {
                seconds: 61,
                limit: 60,
                ..
            })
        ));

        let mut exact = case("a");
        exact.timeout_seconds = Some(60);
        let admitted_suite = suite(vec![exact.clone()]);
        policy
            .admit(&admitted_suite, &[config("baseline")], 1, Utc::now())
            .expect("a timeout exactly at the ceiling is admitted");
        assert_eq!(
            policy
                .effective_case_seconds(&exact, &admitted_suite)
                .expect("resolved"),
            60
        );

        let mut zero = case("a");
        zero.timeout_seconds = Some(0);
        assert!(matches!(
            policy.admit(&suite(vec![zero]), &[config("baseline")], 1, Utc::now()),
            Err(AdmissionError::InvalidCaseTimeout { .. })
        ));
    }

    #[test]
    fn absent_timeout_falls_back_to_the_policy_ceiling() {
        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            max_case_seconds: 120,
            ..EvalAdmissionLimits::default()
        });
        let suite = suite(vec![case("a")]);
        assert_eq!(
            policy
                .effective_case_seconds(&suite.cases[0], &suite)
                .expect("resolved"),
            120
        );
    }

    #[test]
    fn duplicate_and_empty_identifiers_are_rejected() {
        let policy = EvalAdmissionPolicy::default();
        assert!(matches!(
            policy.admit(
                &suite(vec![case("a"), case("a")]),
                &[config("baseline")],
                1,
                Utc::now()
            ),
            Err(AdmissionError::DuplicateCaseName { .. })
        ));
        assert!(matches!(
            policy.admit(&suite(Vec::new()), &[config("baseline")], 1, Utc::now()),
            Err(AdmissionError::EmptySuite)
        ));
        assert!(matches!(
            policy.admit(&suite(vec![case("a")]), &[], 1, Utc::now()),
            Err(AdmissionError::NoAgentConfigs)
        ));
        assert!(matches!(
            policy.admit(
                &suite(vec![case("  ")]),
                &[config("baseline")],
                1,
                Utc::now()
            ),
            Err(AdmissionError::UnnamedCase)
        ));
    }

    #[test]
    fn unsupported_limits_version_is_rejected() {
        let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
            version: 99,
            ..EvalAdmissionLimits::default()
        });
        assert!(matches!(
            policy.admit(
                &suite(vec![case("a")]),
                &[config("baseline")],
                1,
                Utc::now()
            ),
            Err(AdmissionError::UnsupportedVersion { version: 99, .. })
        ));
    }
}
