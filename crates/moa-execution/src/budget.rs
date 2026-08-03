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

/// Persistence-ready evidence produced by one cumulative budget reconciliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetReconciliation {
    /// Reservation that remains attached to the logical task.
    pub remaining_task_reservation: ExecutionEstimate,
    /// Updated run-level reservation total.
    pub run_reserved: ExecutionEstimate,
    /// Updated run-level cumulative consumption.
    pub run_consumed: ExecutionEstimate,
    /// Whether this or an earlier transition exceeded a reservation, limit, or counter ceiling.
    pub budget_overrun: bool,
    /// Whether this transition released the logical task completely.
    pub terminal: bool,
    /// First resource dimension that caused this transition to overrun.
    pub overrun_dimension: Option<&'static str>,
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
        let evidence = self.reconcile_cumulative(reservation, &zero, actual, true)?;
        if let Some(dimension) = evidence.overrun_dimension {
            return Err(Error::BudgetOverrun { dimension });
        }
        Ok(())
    }

    /// Reconciles one cumulative outcome and returns the task's remaining reservation.
    ///
    /// Nonterminal outcomes move only the nonnegative cumulative delta from
    /// reserved to consumed resources while retaining the task and its
    /// unconsumed reserve. A terminal outcome releases the complete remaining
    /// reservation and consumes exactly one logical task; a task terminalized
    /// before reservation may supply an empty reservation.
    pub fn reconcile_cumulative(
        &mut self,
        reservation: ExecutionEstimate,
        previous: &ExecutionUsage,
        cumulative: &ExecutionUsage,
        terminal: bool,
    ) -> Result<BudgetReconciliation> {
        self.reconcile_cumulative_with_ceiling(
            reservation,
            previous,
            cumulative,
            terminal,
            u64::MAX,
        )
    }

    /// Reconciles cumulative usage while saturating persisted counters at an explicit ceiling.
    ///
    /// PostgreSQL-backed repositories pass their integer ceiling and then validate every returned
    /// value before binding it. Keeping the ceiling in this pure transition avoids a second,
    /// storage-specific implementation of reservation release and cumulative charging.
    pub fn reconcile_cumulative_with_ceiling(
        &mut self,
        reservation: ExecutionEstimate,
        previous: &ExecutionUsage,
        cumulative: &ExecutionUsage,
        terminal: bool,
        counter_ceiling: u64,
    ) -> Result<BudgetReconciliation> {
        let is_unreserved_terminal = terminal && reservation == ExecutionEstimate::default();
        if reservation.tasks != 1 && !is_unreserved_terminal {
            return Err(Error::InvalidBudgetLedger {
                message: "logical task reconciliation requires a one-task reservation or an unreserved terminal task"
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

        let arithmetic_overrun =
            addition_overrun_dimension(self.consumed, &delta, terminal, counter_ceiling);
        self.consumed = ExecutionEstimate {
            cost_microusd: saturating_add_to_ceiling(
                self.consumed.cost_microusd,
                delta.cost_microusd,
                counter_ceiling,
            ),
            tokens: saturating_add_to_ceiling(self.consumed.tokens, delta.tokens, counter_ceiling),
            tool_calls: saturating_add_to_ceiling(
                self.consumed.tool_calls,
                delta.tool_calls,
                counter_ceiling,
            ),
            retrieved_bytes: saturating_add_to_ceiling(
                self.consumed.retrieved_bytes,
                delta.retrieved_bytes,
                counter_ceiling,
            ),
            tasks: saturating_add_to_ceiling(
                self.consumed.tasks,
                u64::from(terminal),
                counter_ceiling,
            ),
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
        if overrun_dimension.is_some() {
            self.overrun = true;
        }
        Ok(BudgetReconciliation {
            remaining_task_reservation: remaining,
            run_reserved: self.reserved,
            run_consumed: self.consumed,
            budget_overrun: self.overrun,
            terminal,
            overrun_dimension,
        })
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
    counter_ceiling: u64,
) -> Option<&'static str> {
    if exceeds_counter_ceiling(
        consumed.cost_microusd,
        actual.cost_microusd,
        counter_ceiling,
    ) {
        Some("cost_microusd")
    } else if exceeds_counter_ceiling(consumed.tokens, actual.tokens, counter_ceiling) {
        Some("tokens")
    } else if exceeds_counter_ceiling(consumed.tool_calls, actual.tool_calls, counter_ceiling) {
        Some("tool_calls")
    } else if exceeds_counter_ceiling(
        consumed.retrieved_bytes,
        actual.retrieved_bytes,
        counter_ceiling,
    ) {
        Some("retrieved_bytes")
    } else if terminal && exceeds_counter_ceiling(consumed.tasks, 1, counter_ceiling) {
        Some("tasks")
    } else {
        None
    }
}

fn saturating_add_to_ceiling(left: u64, right: u64, ceiling: u64) -> u64 {
    u128::from(left)
        .saturating_add(u128::from(right))
        .min(u128::from(ceiling)) as u64
}

fn exceeds_counter_ceiling(left: u64, right: u64, ceiling: u64) -> bool {
    u128::from(left) + u128::from(right) > u128::from(ceiling)
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
