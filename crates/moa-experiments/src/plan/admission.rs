//! Admission quotas for behavior-lab experiment runs.
//!
//! Plan validation bounds one plan: its matrix, its byte sizes, and its declared
//! parallelism. It says nothing about how many plans a caller may run at once,
//! and per-artifact throttling alone is bypassable — a caller that hits a
//! per-plan ceiling can publish a tenth plan artifact and keep going. This
//! module is the decision function that closes that path by holding the same
//! prospective run against three ceilings at once: its own plan artifact, its
//! tenant, and the fleet.
//!
//! The function is pure. It is the caller's job to read
//! [`ExperimentAdmissionUsage`] inside the same transaction that inserts the
//! run, under a lock that serializes admissions; without that, two concurrent
//! admissions both observe pre-admission counts and both are admitted.

use serde::{Deserialize, Serialize};

/// Non-terminal experiment load observed at admission time.
///
/// "Active" means occupying capacity: a run in `accepted` or `running`, and a
/// trial in `accepted`, `dispatched`, or `running`. A terminal row has released
/// whatever it held and never counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExperimentAdmissionUsage {
    /// Active runs already admitted for this run's plan artifact.
    pub artifact_active_runs: u64,
    /// Active trials already admitted for this run's plan artifact.
    pub artifact_active_trials: u64,
    /// Active runs already admitted for this run's tenant.
    pub tenant_active_runs: u64,
    /// Active trials already admitted for this run's tenant.
    pub tenant_active_trials: u64,
    /// Active runs already admitted across every tenant.
    pub fleet_active_runs: u64,
    /// Active trials already admitted across every tenant.
    pub fleet_active_trials: u64,
}

/// Ceilings applied to one prospective experiment run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentAdmissionLimits {
    /// Concurrent active runs allowed per plan artifact.
    pub max_artifact_active_runs: u64,
    /// Concurrent active trials allowed per plan artifact.
    pub max_artifact_active_trials: u64,
    /// Concurrent active runs allowed per tenant.
    pub max_tenant_active_runs: u64,
    /// Concurrent active trials allowed per tenant.
    pub max_tenant_active_trials: u64,
    /// Concurrent active runs allowed across the fleet.
    pub max_fleet_active_runs: u64,
    /// Concurrent active trials allowed across the fleet.
    pub max_fleet_active_trials: u64,
}

/// Concurrent active runs one plan artifact may hold.
pub const DEFAULT_MAX_ARTIFACT_ACTIVE_RUNS: u64 = 4;
/// Concurrent active trials one plan artifact may hold.
pub const DEFAULT_MAX_ARTIFACT_ACTIVE_TRIALS: u64 = 5_000;
/// Concurrent active runs one tenant may hold across every plan artifact.
pub const DEFAULT_MAX_TENANT_ACTIVE_RUNS: u64 = 16;
/// Concurrent active trials one tenant may hold across every plan artifact.
pub const DEFAULT_MAX_TENANT_ACTIVE_TRIALS: u64 = 20_000;
/// Concurrent active runs the fleet may hold across every tenant.
pub const DEFAULT_MAX_FLEET_ACTIVE_RUNS: u64 = 256;
/// Concurrent active trials the fleet may hold across every tenant.
pub const DEFAULT_MAX_FLEET_ACTIVE_TRIALS: u64 = 200_000;

impl Default for ExperimentAdmissionLimits {
    fn default() -> Self {
        Self {
            max_artifact_active_runs: DEFAULT_MAX_ARTIFACT_ACTIVE_RUNS,
            max_artifact_active_trials: DEFAULT_MAX_ARTIFACT_ACTIVE_TRIALS,
            max_tenant_active_runs: DEFAULT_MAX_TENANT_ACTIVE_RUNS,
            max_tenant_active_trials: DEFAULT_MAX_TENANT_ACTIVE_TRIALS,
            max_fleet_active_runs: DEFAULT_MAX_FLEET_ACTIVE_RUNS,
            max_fleet_active_trials: DEFAULT_MAX_FLEET_ACTIVE_TRIALS,
        }
    }
}

/// Scope whose ceiling refused a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentAdmissionScope {
    /// The run's own plan artifact.
    PlanArtifact,
    /// The run's tenant, across every plan artifact it owns.
    Tenant,
    /// Every tenant on this deployment.
    Fleet,
}

impl ExperimentAdmissionScope {
    /// Returns the stable snake-case name of the scope.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlanArtifact => "plan_artifact",
            Self::Tenant => "tenant",
            Self::Fleet => "fleet",
        }
    }
}

/// Counted dimension a ceiling applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentAdmissionDimension {
    /// Whole experiment runs.
    Runs,
    /// Individual trials inside runs.
    Trials,
}

impl ExperimentAdmissionDimension {
    /// Returns the stable snake-case name of the dimension.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runs => "runs",
            Self::Trials => "trials",
        }
    }
}

/// A refused admission, naming the exact ceiling that refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error(
    "experiment admission refused: {scope} {dimension} quota is full ({active} active + {requested} requested exceeds {limit})",
    scope = self.scope.as_str(),
    dimension = self.dimension.as_str()
)]
pub struct ExperimentAdmissionRejection {
    /// Scope whose ceiling refused the run.
    pub scope: ExperimentAdmissionScope,
    /// Dimension the ceiling counts.
    pub dimension: ExperimentAdmissionDimension,
    /// Load already active in that scope.
    pub active: u64,
    /// Load the prospective run adds.
    pub requested: u64,
    /// Configured ceiling.
    pub limit: u64,
}

/// Decides whether one prospective run fits inside every admission ceiling.
///
/// Ceilings are checked narrowest first — plan artifact, then tenant, then fleet
/// — and in each scope runs before trials, so the reported rejection is the
/// tightest one the caller can act on. Every check is `active + requested >
/// limit`: a run that lands exactly on a ceiling is admitted, and one unit past
/// it is refused rather than clamped.
///
/// `requested_trials` is the trial count the run's plan matrix will mint. A
/// run that mints no trials still consumes a run slot.
///
/// # Errors
///
/// Returns the first [`ExperimentAdmissionRejection`] the run violates.
pub fn admit_experiment_run(
    usage: &ExperimentAdmissionUsage,
    limits: &ExperimentAdmissionLimits,
    requested_trials: u64,
) -> Result<(), ExperimentAdmissionRejection> {
    let checks = [
        (
            ExperimentAdmissionScope::PlanArtifact,
            ExperimentAdmissionDimension::Runs,
            usage.artifact_active_runs,
            1,
            limits.max_artifact_active_runs,
        ),
        (
            ExperimentAdmissionScope::PlanArtifact,
            ExperimentAdmissionDimension::Trials,
            usage.artifact_active_trials,
            requested_trials,
            limits.max_artifact_active_trials,
        ),
        (
            ExperimentAdmissionScope::Tenant,
            ExperimentAdmissionDimension::Runs,
            usage.tenant_active_runs,
            1,
            limits.max_tenant_active_runs,
        ),
        (
            ExperimentAdmissionScope::Tenant,
            ExperimentAdmissionDimension::Trials,
            usage.tenant_active_trials,
            requested_trials,
            limits.max_tenant_active_trials,
        ),
        (
            ExperimentAdmissionScope::Fleet,
            ExperimentAdmissionDimension::Runs,
            usage.fleet_active_runs,
            1,
            limits.max_fleet_active_runs,
        ),
        (
            ExperimentAdmissionScope::Fleet,
            ExperimentAdmissionDimension::Trials,
            usage.fleet_active_trials,
            requested_trials,
            limits.max_fleet_active_trials,
        ),
    ];

    for (scope, dimension, active, requested, limit) in checks {
        // Saturating: an overflowed projection must refuse, never wrap into a
        // small number that looks admissible.
        if active.saturating_add(requested) > limit {
            return Err(ExperimentAdmissionRejection {
                scope,
                dimension,
                active,
                requested,
                limit,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ExperimentAdmissionLimits {
        ExperimentAdmissionLimits {
            max_artifact_active_runs: 2,
            max_artifact_active_trials: 20,
            max_tenant_active_runs: 5,
            max_tenant_active_trials: 50,
            max_fleet_active_runs: 8,
            max_fleet_active_trials: 80,
        }
    }

    #[test]
    fn ten_plan_artifacts_cannot_outrun_the_tenant_ceiling() {
        // Pins: per-artifact throttling alone is bypassable by minting more plan
        // artifacts. Each of ten fresh artifacts reports zero artifact-scoped
        // load, so only the tenant total can refuse the eleventh run.
        let limits = limits();
        let mut usage = ExperimentAdmissionUsage::default();

        let mut admitted = 0_u64;
        for _ in 0..10 {
            // A brand-new plan artifact: its own scope is always empty.
            usage.artifact_active_runs = 0;
            usage.artifact_active_trials = 0;
            match admit_experiment_run(&usage, &limits, 4) {
                Ok(()) => {
                    admitted += 1;
                    usage.tenant_active_runs += 1;
                    usage.tenant_active_trials += 4;
                    usage.fleet_active_runs += 1;
                    usage.fleet_active_trials += 4;
                }
                Err(rejection) => {
                    assert_eq!(rejection.scope, ExperimentAdmissionScope::Tenant);
                    assert_eq!(rejection.dimension, ExperimentAdmissionDimension::Runs);
                    break;
                }
            }
        }

        assert_eq!(
            admitted, 5,
            "one artifact per run must not buy more than the tenant run ceiling"
        );
    }

    #[test]
    fn one_tenant_cannot_exhaust_the_fleet_trial_ceiling() {
        // Pins: the fleet total is read from the same snapshot, so a tenant that
        // stays inside its own ceilings still cannot fill the deployment.
        let limits = ExperimentAdmissionLimits {
            max_artifact_active_runs: 1_000,
            max_artifact_active_trials: 1_000,
            max_tenant_active_runs: 1_000,
            max_tenant_active_trials: 1_000,
            ..limits()
        };
        let usage = ExperimentAdmissionUsage {
            fleet_active_trials: 79,
            ..ExperimentAdmissionUsage::default()
        };

        let rejection = admit_experiment_run(&usage, &limits, 2)
            .expect_err("a run that would push the fleet past its trial ceiling is refused");

        assert_eq!(rejection.scope, ExperimentAdmissionScope::Fleet);
        assert_eq!(rejection.dimension, ExperimentAdmissionDimension::Trials);
        assert_eq!(rejection.limit, 80);
    }

    #[test]
    fn landing_exactly_on_a_ceiling_is_admitted_and_one_past_it_is_not() {
        // Pins: the comparison is `active + requested > limit`, so an exactly
        // sized run is not spuriously refused and the next unit is refused
        // rather than clamped down to a smaller matrix.
        let limits = limits();
        let exact = ExperimentAdmissionUsage {
            artifact_active_trials: 16,
            ..ExperimentAdmissionUsage::default()
        };
        admit_experiment_run(&exact, &limits, 4).expect("exactly-sized run is admitted");

        let over = ExperimentAdmissionUsage {
            artifact_active_trials: 17,
            ..ExperimentAdmissionUsage::default()
        };
        let rejection = admit_experiment_run(&over, &limits, 4)
            .expect_err("one trial past the artifact ceiling is refused");
        assert_eq!(rejection.scope, ExperimentAdmissionScope::PlanArtifact);
        assert_eq!(rejection.dimension, ExperimentAdmissionDimension::Trials);
        assert_eq!(rejection.active, 17);
        assert_eq!(rejection.requested, 4);
    }

    #[test]
    fn an_overflowing_trial_request_refuses_instead_of_wrapping() {
        // Pins: a projection that would overflow `u64` must not wrap into a
        // small admissible number.
        let usage = ExperimentAdmissionUsage {
            artifact_active_trials: u64::MAX,
            ..ExperimentAdmissionUsage::default()
        };
        let rejection = admit_experiment_run(&usage, &limits(), u64::MAX)
            .expect_err("an overflowing projection must be refused");
        assert_eq!(rejection.dimension, ExperimentAdmissionDimension::Trials);
    }

    #[test]
    fn the_narrowest_full_ceiling_is_the_one_reported() {
        // Pins: when several ceilings are full the caller is told the tightest
        // one, so the reported remedy is the one that actually unblocks it.
        let usage = ExperimentAdmissionUsage {
            artifact_active_runs: 2,
            tenant_active_runs: 5,
            fleet_active_runs: 8,
            ..ExperimentAdmissionUsage::default()
        };

        let rejection = admit_experiment_run(&usage, &limits(), 1)
            .expect_err("every run ceiling is already full");

        assert_eq!(rejection.scope, ExperimentAdmissionScope::PlanArtifact);
    }
}
