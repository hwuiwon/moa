//! Pure integer run-budget reservation and reconciliation.

use moa_artifacts::execution_plan::{ExecutionBudgetLimit, ExecutionUsage};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, capability::ExecutionEstimate};

/// Pure run-level budget ledger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetLedger {
    /// Approved run limits.
    pub limit: ExecutionBudgetLimit,
    /// Worst-case resources held by nonterminal logical tasks.
    pub reserved: ExecutionEstimate,
    /// Reconciled actual resources and completed logical tasks.
    pub consumed: ExecutionEstimate,
    /// Whether actual usage exceeded a reservation or configured limit.
    pub overrun: bool,
}

impl BudgetLedger {
    /// Creates an empty ledger for one approved limit.
    #[must_use]
    pub fn new(limit: ExecutionBudgetLimit) -> Self {
        Self {
            limit,
            reserved: ExecutionEstimate::default(),
            consumed: ExecutionEstimate::default(),
            overrun: false,
        }
    }

    /// Atomically reserves one logical task's worst-case estimate in memory.
    ///
    /// The ledger is unchanged when any configured dimension would be exceeded.
    pub fn try_reserve(&mut self, request: ExecutionEstimate) -> Result<()> {
        if self.overrun {
            return Err(Error::BudgetExceeded {
                dimension: "overrun ledger",
            });
        }

        let next = self.reserved.checked_add(request, "budget reservation")?;
        ensure_within_limit(self.consumed, next, &self.limit, "budget reservation")?;
        self.reserved = next;
        Ok(())
    }

    /// Releases one complete reservation and reconciles cumulative actual usage.
    ///
    /// Actual resource counters use saturating addition, while logical task
    /// consumption increases exactly once regardless of attempts or turns.
    pub fn reconcile(
        &mut self,
        reservation: ExecutionEstimate,
        actual: &ExecutionUsage,
    ) -> Result<()> {
        let zero = zero_usage();
        self.reconcile_cumulative(reservation, &zero, actual, true)
            .map(|_| ())
    }

    /// Reconciles one cumulative outcome and returns the task's remaining reservation.
    ///
    /// Nonterminal outcomes move only the nonnegative cumulative delta from
    /// reserved to consumed resources while retaining the task and its
    /// unconsumed reserve. A terminal outcome releases the complete remaining
    /// reservation and consumes exactly one logical task.
    pub fn reconcile_cumulative(
        &mut self,
        reservation: ExecutionEstimate,
        previous: &ExecutionUsage,
        cumulative: &ExecutionUsage,
        terminal: bool,
    ) -> Result<ExecutionEstimate> {
        if reservation.tasks != 1 {
            return Err(Error::InvalidBudgetLedger {
                message: "one logical task reconciliation requires a one-task reservation"
                    .to_string(),
            });
        }
        ensure_reservation_present(self.reserved, reservation)?;
        let delta = cumulative_delta(previous, cumulative)?;
        let charged_reservation = estimate_from_usage(&delta);
        let actual_overrun = actual_overrun_dimension(&delta, reservation);

        let release = if terminal {
            reservation
        } else {
            ExecutionEstimate {
                cost_microusd: charged_reservation
                    .cost_microusd
                    .min(reservation.cost_microusd),
                tokens: charged_reservation.tokens.min(reservation.tokens),
                tool_calls: charged_reservation.tool_calls.min(reservation.tool_calls),
                retrieved_bytes: charged_reservation
                    .retrieved_bytes
                    .min(reservation.retrieved_bytes),
                tasks: 0,
            }
        };

        self.reserved = ExecutionEstimate {
            cost_microusd: self.reserved.cost_microusd - release.cost_microusd,
            tokens: self.reserved.tokens - release.tokens,
            tool_calls: self.reserved.tool_calls - release.tool_calls,
            retrieved_bytes: self.reserved.retrieved_bytes - release.retrieved_bytes,
            tasks: self.reserved.tasks - release.tasks,
        };

        let arithmetic_overrun = addition_overrun_dimension(self.consumed, &delta, terminal);
        self.consumed = ExecutionEstimate {
            cost_microusd: self
                .consumed
                .cost_microusd
                .saturating_add(delta.cost_microusd),
            tokens: self.consumed.tokens.saturating_add(delta.tokens),
            tool_calls: self.consumed.tool_calls.saturating_add(delta.tool_calls),
            retrieved_bytes: self
                .consumed
                .retrieved_bytes
                .saturating_add(delta.retrieved_bytes),
            tasks: self.consumed.tasks.saturating_add(u64::from(terminal)),
        };

        let remaining = if terminal {
            ExecutionEstimate::default()
        } else {
            ExecutionEstimate {
                cost_microusd: reservation
                    .cost_microusd
                    .saturating_sub(delta.cost_microusd),
                tokens: reservation.tokens.saturating_sub(delta.tokens),
                tool_calls: reservation.tool_calls.saturating_sub(delta.tool_calls),
                retrieved_bytes: reservation
                    .retrieved_bytes
                    .saturating_sub(delta.retrieved_bytes),
                tasks: 1,
            }
        };

        let overrun_dimension = actual_overrun
            .or(arithmetic_overrun)
            .or_else(|| limit_overrun_dimension(self.consumed, self.reserved, &self.limit));
        if let Some(dimension) = overrun_dimension {
            self.overrun = true;
            return Err(Error::BudgetOverrun { dimension });
        }
        Ok(remaining)
    }

    /// Returns the unconsumed and unreserved resource limit.
    pub fn remaining_limit(&self) -> Result<ExecutionBudgetLimit> {
        Ok(ExecutionBudgetLimit {
            max_cost_microusd: remaining_dimension(
                self.limit.max_cost_microusd,
                self.consumed.cost_microusd,
                self.reserved.cost_microusd,
                "cost_microusd",
            )?,
            max_tokens: remaining_dimension(
                self.limit.max_tokens,
                self.consumed.tokens,
                self.reserved.tokens,
                "tokens",
            )?,
            max_tasks: remaining_dimension(
                self.limit.max_tasks,
                self.consumed.tasks,
                self.reserved.tasks,
                "tasks",
            )?,
            max_tool_calls: remaining_dimension(
                self.limit.max_tool_calls,
                self.consumed.tool_calls,
                self.reserved.tool_calls,
                "tool_calls",
            )?,
            max_retrieved_bytes: remaining_dimension(
                self.limit.max_retrieved_bytes,
                self.consumed.retrieved_bytes,
                self.reserved.retrieved_bytes,
                "retrieved_bytes",
            )?,
            deadline_at: self.limit.deadline_at,
        })
    }
}

/// Returns whether an estimate fits every configured resource dimension.
pub fn estimate_fits_limit(
    estimate: ExecutionEstimate,
    limit: &ExecutionBudgetLimit,
) -> Result<()> {
    ensure_within_limit(ExecutionEstimate::default(), estimate, limit, "estimate")
}

fn ensure_within_limit(
    consumed: ExecutionEstimate,
    reserved: ExecutionEstimate,
    limit: &ExecutionBudgetLimit,
    context: &str,
) -> Result<()> {
    check_dimension(
        consumed.cost_microusd,
        reserved.cost_microusd,
        limit.max_cost_microusd,
        "cost_microusd",
        context,
    )?;
    check_dimension(
        consumed.tokens,
        reserved.tokens,
        limit.max_tokens,
        "tokens",
        context,
    )?;
    check_dimension(
        consumed.tasks,
        reserved.tasks,
        limit.max_tasks,
        "tasks",
        context,
    )?;
    check_dimension(
        consumed.tool_calls,
        reserved.tool_calls,
        limit.max_tool_calls,
        "tool_calls",
        context,
    )?;
    check_dimension(
        consumed.retrieved_bytes,
        reserved.retrieved_bytes,
        limit.max_retrieved_bytes,
        "retrieved_bytes",
        context,
    )
}

fn check_dimension(
    consumed: u64,
    reserved: u64,
    limit: Option<u64>,
    dimension: &'static str,
    context: &str,
) -> Result<()> {
    let total = consumed
        .checked_add(reserved)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: format!("{context} {dimension}"),
        })?;
    if limit.is_some_and(|limit| total > limit) {
        return Err(Error::BudgetExceeded { dimension });
    }
    Ok(())
}

fn ensure_reservation_present(
    reserved: ExecutionEstimate,
    release: ExecutionEstimate,
) -> Result<()> {
    if release.cost_microusd > reserved.cost_microusd
        || release.tokens > reserved.tokens
        || release.tool_calls > reserved.tool_calls
        || release.retrieved_bytes > reserved.retrieved_bytes
        || release.tasks > reserved.tasks
    {
        return Err(Error::InvalidBudgetLedger {
            message: "released reservation exceeds the ledger reservation".to_string(),
        });
    }
    Ok(())
}

fn actual_overrun_dimension(
    actual: &ExecutionUsage,
    reservation: ExecutionEstimate,
) -> Option<&'static str> {
    if actual.cost_microusd > reservation.cost_microusd {
        Some("cost_microusd")
    } else if actual.tokens > reservation.tokens {
        Some("tokens")
    } else if actual.tool_calls > reservation.tool_calls {
        Some("tool_calls")
    } else if actual.retrieved_bytes > reservation.retrieved_bytes {
        Some("retrieved_bytes")
    } else {
        None
    }
}

fn addition_overrun_dimension(
    consumed: ExecutionEstimate,
    actual: &ExecutionUsage,
    terminal: bool,
) -> Option<&'static str> {
    if consumed
        .cost_microusd
        .checked_add(actual.cost_microusd)
        .is_none()
    {
        Some("cost_microusd")
    } else if consumed.tokens.checked_add(actual.tokens).is_none() {
        Some("tokens")
    } else if consumed.tool_calls.checked_add(actual.tool_calls).is_none() {
        Some("tool_calls")
    } else if consumed
        .retrieved_bytes
        .checked_add(actual.retrieved_bytes)
        .is_none()
    {
        Some("retrieved_bytes")
    } else if terminal && consumed.tasks == u64::MAX {
        Some("tasks")
    } else {
        None
    }
}

fn limit_overrun_dimension(
    consumed: ExecutionEstimate,
    reserved: ExecutionEstimate,
    limit: &ExecutionBudgetLimit,
) -> Option<&'static str> {
    if exceeds_limit(
        consumed.cost_microusd,
        reserved.cost_microusd,
        limit.max_cost_microusd,
    ) {
        Some("cost_microusd")
    } else if exceeds_limit(consumed.tokens, reserved.tokens, limit.max_tokens) {
        Some("tokens")
    } else if exceeds_limit(consumed.tasks, reserved.tasks, limit.max_tasks) {
        Some("tasks")
    } else if exceeds_limit(
        consumed.tool_calls,
        reserved.tool_calls,
        limit.max_tool_calls,
    ) {
        Some("tool_calls")
    } else if exceeds_limit(
        consumed.retrieved_bytes,
        reserved.retrieved_bytes,
        limit.max_retrieved_bytes,
    ) {
        Some("retrieved_bytes")
    } else {
        None
    }
}

fn exceeds_limit(consumed: u64, reserved: u64, limit: Option<u64>) -> bool {
    limit.is_some_and(|limit| consumed.saturating_add(reserved) > limit)
}

fn cumulative_delta(
    previous: &ExecutionUsage,
    cumulative: &ExecutionUsage,
) -> Result<ExecutionUsage> {
    Ok(ExecutionUsage {
        cost_microusd: usage_delta(
            previous.cost_microusd,
            cumulative.cost_microusd,
            "cost_microusd",
        )?,
        tokens: usage_delta(previous.tokens, cumulative.tokens, "tokens")?,
        tool_calls: usage_delta(previous.tool_calls, cumulative.tool_calls, "tool_calls")?,
        retrieved_bytes: usage_delta(
            previous.retrieved_bytes,
            cumulative.retrieved_bytes,
            "retrieved_bytes",
        )?,
    })
}

fn usage_delta(previous: u64, cumulative: u64, dimension: &'static str) -> Result<u64> {
    cumulative
        .checked_sub(previous)
        .ok_or_else(|| Error::InvalidBudgetLedger {
            message: format!("cumulative {dimension} usage decreased"),
        })
}

const fn estimate_from_usage(usage: &ExecutionUsage) -> ExecutionEstimate {
    ExecutionEstimate {
        cost_microusd: usage.cost_microusd,
        tokens: usage.tokens,
        tool_calls: usage.tool_calls,
        retrieved_bytes: usage.retrieved_bytes,
        tasks: 0,
    }
}

const fn zero_usage() -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: 0,
        tokens: 0,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

fn remaining_dimension(
    limit: Option<u64>,
    consumed: u64,
    reserved: u64,
    dimension: &'static str,
) -> Result<Option<u64>> {
    let used = consumed
        .checked_add(reserved)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: format!("remaining {dimension}"),
        })?;
    limit
        .map(|limit| {
            limit
                .checked_sub(used)
                .ok_or(Error::BudgetExceeded { dimension })
        })
        .transpose()
}
