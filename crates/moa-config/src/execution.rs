//! Pure execution-run resource and planner defaults.

use serde::{Deserialize, Serialize};

use moa_core::error::{MoaError, Result};

use super::require_positive_limit;

/// Provisional physical execution-task window pending the measured T3.3 default.
const DEFAULT_MAX_IN_FLIGHT_TASKS: usize = 64;
const MAX_TERMINAL_DRAIN_PAGE_TASKS: usize = 1_000;

/// Tenant-independent defaults for execution planning and resource envelopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionConfig {
    /// Maximum planner repair calls after the initial candidate.
    pub planner_repair_attempts: u32,
    /// Number of identical failure fingerprints that stops replanning.
    pub repeated_failure_limit: u32,
    /// Maximum live execution-task invocations owned by one run.
    pub max_in_flight_tasks: usize,
    /// Maximum accepted duration of a Durable run, in seconds.
    pub maximum_horizon_seconds: u64,
    /// Maximum scheduler transitions performed by one controller activation.
    pub maximum_activation_steps: usize,
    /// Maximum ready tasks dispatched by one controller activation.
    pub dispatch_batch_size: usize,
    /// Maximum duration of one active task attempt, in seconds.
    pub active_attempt_timeout_seconds: u64,
    /// Floor of the window without durable attempt progress after which an attempt is
    /// stalled, in seconds.
    ///
    /// This is a floor, not the whole window. The heartbeat is written at step boundaries
    /// and not while a step runs, so the effective window is the larger of this value and
    /// the bound the in-flight step declared plus
    /// `moa_execution::repository::ATTEMPT_STEP_BOUND_MARGIN_SECONDS`. A step that declares
    /// a long timeout therefore widens only its own window, instead of forcing every
    /// attempt to wait out the slowest step the platform allows.
    ///
    /// It must stay strictly below `active_attempt_timeout_seconds`: a floor at or beyond
    /// the attempt deadline can never classify a stall before the deadline does.
    pub attempt_heartbeat_staleness_seconds: u64,
    /// Maximum non-parked execution runs admitted for one tenant.
    pub max_tenant_active_runs: u32,
    /// Maximum non-parked execution runs admitted across the fleet.
    pub max_fleet_active_runs: u32,
    /// Maximum active task attempts admitted for one tenant.
    pub max_tenant_active_tasks: u32,
    /// Maximum active task attempts admitted across the fleet.
    pub max_fleet_active_tasks: u32,
    /// Maximum combined active and parked run residency for one tenant.
    pub max_tenant_parked_runs: u32,
    /// Maximum combined active and parked run residency across the fleet.
    pub max_fleet_parked_runs: u32,
    /// Maximum pending scheduled execution triggers retained for one tenant.
    pub max_tenant_scheduled_triggers: u32,
    /// Maximum pending scheduled execution triggers retained across the fleet.
    pub max_fleet_scheduled_triggers: u32,
    /// Maximum nonterminal external execution jobs retained for one tenant.
    pub max_tenant_external_jobs: u32,
    /// Maximum nonterminal external execution jobs retained across the fleet.
    pub max_fleet_external_jobs: u32,
    /// Cadence for repairing due trigger delivery, in seconds.
    pub trigger_reconciliation_cadence_seconds: u64,
    /// Days to retain detailed terminal execution rows before bounded compaction.
    pub terminal_detail_retention_days: u64,
    /// Default maximum logical tasks in one approved run.
    pub max_tasks: u64,
    /// Default maximum model tokens in one approved run.
    pub max_tokens: u64,
    /// Default maximum governed tool calls in one approved run.
    pub max_tool_calls: u64,
    /// Default maximum retrieved bytes in one approved run.
    pub max_retrieved_bytes: u64,
    /// Default maximum run cost in integer micro-US-dollars.
    pub max_cost_microusd: u64,
    /// Cost threshold above which a run requires owning-user confirmation.
    pub unattended_max_cost_microusd: u64,
    /// Conservative cost estimate for one bounded agent turn.
    pub agent_turn_cost_microusd: u64,
    /// Conservative token estimate for one bounded agent turn.
    pub agent_turn_tokens: u64,
    /// Conservative tool-call estimate for one bounded agent turn.
    pub agent_turn_tool_calls: u64,
    /// Conservative retrieval estimate for one bounded agent turn.
    pub agent_turn_retrieved_bytes: u64,
    /// Conservative cost estimate for one verifier turn.
    pub verifier_turn_cost_microusd: u64,
    /// Conservative token estimate for one verifier turn.
    pub verifier_turn_tokens: u64,
    /// Conservative tool-call estimate for one verifier turn.
    pub verifier_turn_tool_calls: u64,
    /// Conservative retrieval estimate for one verifier turn.
    pub verifier_turn_retrieved_bytes: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            planner_repair_attempts: 1,
            repeated_failure_limit: 3,
            max_in_flight_tasks: DEFAULT_MAX_IN_FLIGHT_TASKS,
            maximum_horizon_seconds: 30 * 24 * 60 * 60,
            maximum_activation_steps: 128,
            dispatch_batch_size: DEFAULT_MAX_IN_FLIGHT_TASKS,
            active_attempt_timeout_seconds: 10 * 60,
            // Covers a step that declares no bound of its own: a model turn, and any tool
            // call that does not ask for longer than the default sandbox command timeout.
            // A step that declares more widens its own window instead of this floor.
            attempt_heartbeat_staleness_seconds: 2 * 60,
            max_tenant_active_runs: 100,
            max_fleet_active_runs: 1_000,
            max_tenant_active_tasks: 256,
            max_fleet_active_tasks: 4_096,
            max_tenant_parked_runs: 10_000,
            max_fleet_parked_runs: 100_000,
            max_tenant_scheduled_triggers: 50_000,
            max_fleet_scheduled_triggers: 500_000,
            max_tenant_external_jobs: 1_000,
            max_fleet_external_jobs: 10_000,
            trigger_reconciliation_cadence_seconds: 60,
            terminal_detail_retention_days: 30,
            max_tasks: 10_000,
            max_tokens: 10_000_000,
            max_tool_calls: 100_000,
            max_retrieved_bytes: 10_000_000_000,
            max_cost_microusd: 100_000_000,
            unattended_max_cost_microusd: 5_000_000,
            agent_turn_cost_microusd: 100_000,
            agent_turn_tokens: 8_000,
            agent_turn_tool_calls: 8,
            agent_turn_retrieved_bytes: 10_000_000,
            verifier_turn_cost_microusd: 200_000,
            verifier_turn_tokens: 16_000,
            verifier_turn_tool_calls: 4,
            verifier_turn_retrieved_bytes: 1_000_000,
        }
    }
}

impl ExecutionConfig {
    /// Validates execution envelopes, activation bounds, and admission capacities.
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            (
                "execution.repeated_failure_limit",
                u64::from(self.repeated_failure_limit),
            ),
            (
                "execution.max_in_flight_tasks",
                usize_as_u64("execution.max_in_flight_tasks", self.max_in_flight_tasks)?,
            ),
            (
                "execution.maximum_horizon_seconds",
                self.maximum_horizon_seconds,
            ),
            (
                "execution.maximum_activation_steps",
                usize_as_u64(
                    "execution.maximum_activation_steps",
                    self.maximum_activation_steps,
                )?,
            ),
            (
                "execution.dispatch_batch_size",
                usize_as_u64("execution.dispatch_batch_size", self.dispatch_batch_size)?,
            ),
            (
                "execution.active_attempt_timeout_seconds",
                self.active_attempt_timeout_seconds,
            ),
            (
                "execution.attempt_heartbeat_staleness_seconds",
                self.attempt_heartbeat_staleness_seconds,
            ),
            (
                "execution.max_tenant_active_runs",
                u64::from(self.max_tenant_active_runs),
            ),
            (
                "execution.max_fleet_active_runs",
                u64::from(self.max_fleet_active_runs),
            ),
            (
                "execution.max_tenant_active_tasks",
                u64::from(self.max_tenant_active_tasks),
            ),
            (
                "execution.max_fleet_active_tasks",
                u64::from(self.max_fleet_active_tasks),
            ),
            (
                "execution.max_tenant_parked_runs",
                u64::from(self.max_tenant_parked_runs),
            ),
            (
                "execution.max_fleet_parked_runs",
                u64::from(self.max_fleet_parked_runs),
            ),
            (
                "execution.max_tenant_scheduled_triggers",
                u64::from(self.max_tenant_scheduled_triggers),
            ),
            (
                "execution.max_fleet_scheduled_triggers",
                u64::from(self.max_fleet_scheduled_triggers),
            ),
            (
                "execution.max_tenant_external_jobs",
                u64::from(self.max_tenant_external_jobs),
            ),
            (
                "execution.max_fleet_external_jobs",
                u64::from(self.max_fleet_external_jobs),
            ),
            (
                "execution.trigger_reconciliation_cadence_seconds",
                self.trigger_reconciliation_cadence_seconds,
            ),
            (
                "execution.terminal_detail_retention_days",
                self.terminal_detail_retention_days,
            ),
            ("execution.max_tasks", self.max_tasks),
            ("execution.max_tokens", self.max_tokens),
            ("execution.max_tool_calls", self.max_tool_calls),
            ("execution.max_retrieved_bytes", self.max_retrieved_bytes),
            ("execution.max_cost_microusd", self.max_cost_microusd),
            (
                "execution.agent_turn_cost_microusd",
                self.agent_turn_cost_microusd,
            ),
            ("execution.agent_turn_tokens", self.agent_turn_tokens),
            (
                "execution.agent_turn_tool_calls",
                self.agent_turn_tool_calls,
            ),
            (
                "execution.agent_turn_retrieved_bytes",
                self.agent_turn_retrieved_bytes,
            ),
            (
                "execution.verifier_turn_cost_microusd",
                self.verifier_turn_cost_microusd,
            ),
            ("execution.verifier_turn_tokens", self.verifier_turn_tokens),
            (
                "execution.verifier_turn_tool_calls",
                self.verifier_turn_tool_calls,
            ),
            (
                "execution.verifier_turn_retrieved_bytes",
                self.verifier_turn_retrieved_bytes,
            ),
        ] {
            require_positive_limit(name, value)?;
        }

        if self.dispatch_batch_size > self.max_in_flight_tasks {
            return Err(MoaError::ConfigError(
                "execution.dispatch_batch_size must not exceed execution.max_in_flight_tasks"
                    .to_string(),
            ));
        }
        if self.max_in_flight_tasks > MAX_TERMINAL_DRAIN_PAGE_TASKS {
            return Err(MoaError::ConfigError(format!(
                "execution.max_in_flight_tasks must not exceed {MAX_TERMINAL_DRAIN_PAGE_TASKS} so one terminal drain page fences every current task owner"
            )));
        }
        if self.dispatch_batch_size < 3 {
            return Err(MoaError::ConfigError(
                "execution.dispatch_batch_size must be at least 3 so every reconciliation lane makes progress"
                    .to_string(),
            ));
        }
        if self.active_attempt_timeout_seconds > self.maximum_horizon_seconds {
            return Err(MoaError::ConfigError(
                "execution.active_attempt_timeout_seconds must not exceed execution.maximum_horizon_seconds"
                    .to_string(),
            ));
        }
        if self.attempt_heartbeat_staleness_seconds >= self.active_attempt_timeout_seconds {
            return Err(MoaError::ConfigError(
                "execution.attempt_heartbeat_staleness_seconds must be less than execution.active_attempt_timeout_seconds because a staleness window at or beyond the attempt deadline can never classify a stall before the deadline does"
                    .to_string(),
            ));
        }
        if self.trigger_reconciliation_cadence_seconds > self.active_attempt_timeout_seconds {
            return Err(MoaError::ConfigError(
                "execution.trigger_reconciliation_cadence_seconds must not exceed execution.active_attempt_timeout_seconds"
                    .to_string(),
            ));
        }

        for (tenant_name, tenant, fleet_name, fleet) in [
            (
                "execution.max_tenant_active_runs",
                self.max_tenant_active_runs,
                "execution.max_fleet_active_runs",
                self.max_fleet_active_runs,
            ),
            (
                "execution.max_tenant_active_tasks",
                self.max_tenant_active_tasks,
                "execution.max_fleet_active_tasks",
                self.max_fleet_active_tasks,
            ),
            (
                "execution.max_tenant_parked_runs",
                self.max_tenant_parked_runs,
                "execution.max_fleet_parked_runs",
                self.max_fleet_parked_runs,
            ),
            (
                "execution.max_tenant_scheduled_triggers",
                self.max_tenant_scheduled_triggers,
                "execution.max_fleet_scheduled_triggers",
                self.max_fleet_scheduled_triggers,
            ),
            (
                "execution.max_tenant_external_jobs",
                self.max_tenant_external_jobs,
                "execution.max_fleet_external_jobs",
                self.max_fleet_external_jobs,
            ),
        ] {
            if tenant > fleet {
                return Err(MoaError::ConfigError(format!(
                    "{tenant_name} must not exceed {fleet_name}"
                )));
            }
        }

        if self.max_tenant_active_runs > self.max_tenant_parked_runs {
            return Err(MoaError::ConfigError(
                "execution.max_tenant_active_runs must not exceed execution.max_tenant_parked_runs because every admitted run needs parking entitlement"
                    .to_string(),
            ));
        }
        if self.max_fleet_active_runs > self.max_fleet_parked_runs {
            return Err(MoaError::ConfigError(
                "execution.max_fleet_active_runs must not exceed execution.max_fleet_parked_runs because every admitted run needs parking entitlement"
                    .to_string(),
            ));
        }

        Ok(())
    }
}

fn usize_as_u64(name: &str, value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_| MoaError::ConfigError(format!("{name} is too large")))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MAX_IN_FLIGHT_TASKS, ExecutionConfig, MAX_TERMINAL_DRAIN_PAGE_TASKS};

    #[test]
    fn execution_config_defaults_match_the_resource_contract() {
        // Pins: run admission and compiler estimates share the documented safety defaults.
        assert_eq!(
            ExecutionConfig::default(),
            ExecutionConfig {
                planner_repair_attempts: 1,
                repeated_failure_limit: 3,
                max_in_flight_tasks: DEFAULT_MAX_IN_FLIGHT_TASKS,
                maximum_horizon_seconds: 30 * 24 * 60 * 60,
                maximum_activation_steps: 128,
                dispatch_batch_size: DEFAULT_MAX_IN_FLIGHT_TASKS,
                active_attempt_timeout_seconds: 10 * 60,
                attempt_heartbeat_staleness_seconds: 2 * 60,
                max_tenant_active_runs: 100,
                max_fleet_active_runs: 1_000,
                max_tenant_active_tasks: 256,
                max_fleet_active_tasks: 4_096,
                max_tenant_parked_runs: 10_000,
                max_fleet_parked_runs: 100_000,
                max_tenant_scheduled_triggers: 50_000,
                max_fleet_scheduled_triggers: 500_000,
                max_tenant_external_jobs: 1_000,
                max_fleet_external_jobs: 10_000,
                trigger_reconciliation_cadence_seconds: 60,
                terminal_detail_retention_days: 30,
                max_tasks: 10_000,
                max_tokens: 10_000_000,
                max_tool_calls: 100_000,
                max_retrieved_bytes: 10_000_000_000,
                max_cost_microusd: 100_000_000,
                unattended_max_cost_microusd: 5_000_000,
                agent_turn_cost_microusd: 100_000,
                agent_turn_tokens: 8_000,
                agent_turn_tool_calls: 8,
                agent_turn_retrieved_bytes: 10_000_000,
                verifier_turn_cost_microusd: 200_000,
                verifier_turn_tokens: 16_000,
                verifier_turn_tool_calls: 4,
                verifier_turn_retrieved_bytes: 1_000_000,
            }
        );

        let encoded = serde_json::to_value(ExecutionConfig::default())
            .expect("serialize execution config defaults");
        assert_eq!(encoded["max_in_flight_tasks"], DEFAULT_MAX_IN_FLIGHT_TASKS);
        assert_eq!(encoded["dispatch_batch_size"], DEFAULT_MAX_IN_FLIGHT_TASKS);
        assert_eq!(
            serde_json::from_value::<ExecutionConfig>(encoded)
                .expect("deserialize execution config defaults")
                .max_in_flight_tasks,
            DEFAULT_MAX_IN_FLIGHT_TASKS
        );
    }

    #[test]
    fn execution_config_rejects_inconsistent_activation_and_capacity_limits() {
        // Pins: one activation cannot overrun its task window, timeout hierarchy,
        // or a fleet ceiling through a larger tenant-local limit.
        let mut batch = ExecutionConfig::default();
        batch.dispatch_batch_size = batch.max_in_flight_tasks + 1;
        assert!(batch.validate().is_err());

        let oversized_in_flight = ExecutionConfig {
            max_in_flight_tasks: MAX_TERMINAL_DRAIN_PAGE_TASKS + 1,
            ..ExecutionConfig::default()
        };
        let error = oversized_in_flight
            .validate()
            .expect_err("one terminal page must cover every current task owner");
        assert!(
            error
                .to_string()
                .contains("max_in_flight_tasks must not exceed 1000")
        );

        let starving_batch = ExecutionConfig {
            dispatch_batch_size: 2,
            ..ExecutionConfig::default()
        };
        let error = starving_batch
            .validate()
            .expect_err("a batch smaller than the three reconciliation lanes must fail");
        assert!(
            error
                .to_string()
                .contains("dispatch_batch_size must be at least 3")
        );

        let mut timeout = ExecutionConfig::default();
        timeout.active_attempt_timeout_seconds = timeout.maximum_horizon_seconds + 1;
        assert!(timeout.validate().is_err());

        let zero_heartbeat = ExecutionConfig {
            attempt_heartbeat_staleness_seconds: 0,
            ..ExecutionConfig::default()
        };
        assert!(zero_heartbeat.validate().is_err());

        let unreachable_heartbeat = ExecutionConfig {
            attempt_heartbeat_staleness_seconds: ExecutionConfig::default()
                .active_attempt_timeout_seconds,
            ..ExecutionConfig::default()
        };
        assert!(
            unreachable_heartbeat
                .validate()
                .expect_err("a staleness window at the attempt deadline can never fire first")
                .to_string()
                .contains(
                    "attempt_heartbeat_staleness_seconds must be less than execution.active_attempt_timeout_seconds"
                )
        );

        let mut reconciliation = ExecutionConfig::default();
        reconciliation.trigger_reconciliation_cadence_seconds =
            reconciliation.active_attempt_timeout_seconds + 1;
        assert!(reconciliation.validate().is_err());

        let retention = ExecutionConfig {
            terminal_detail_retention_days: 0,
            ..ExecutionConfig::default()
        };
        assert!(retention.validate().is_err());

        let mut capacity = ExecutionConfig::default();
        capacity.max_tenant_active_tasks = capacity.max_fleet_active_tasks + 1;
        assert!(capacity.validate().is_err());

        let tenant_parking_entitlement = ExecutionConfig {
            max_tenant_active_runs: 101,
            max_tenant_parked_runs: 100,
            ..ExecutionConfig::default()
        };
        assert!(
            tenant_parking_entitlement
                .validate()
                .expect_err("every active tenant run must retain parking entitlement")
                .to_string()
                .contains(
                    "max_tenant_active_runs must not exceed execution.max_tenant_parked_runs"
                )
        );

        let fleet_parking_entitlement = ExecutionConfig {
            max_fleet_active_runs: 1_001,
            max_fleet_parked_runs: 1_000,
            max_tenant_parked_runs: 1_000,
            ..ExecutionConfig::default()
        };
        assert!(
            fleet_parking_entitlement
                .validate()
                .expect_err("every active fleet run must retain parking entitlement")
                .to_string()
                .contains("max_fleet_active_runs must not exceed execution.max_fleet_parked_runs")
        );
    }
}
