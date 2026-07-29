//! Versioned runtime resource envelopes, reservations, and deadlines.
//!
//! This module is the runtime contract for *paid* work: money, tokens, agent
//! turns, model calls, and tool calls, plus an absolute wall-clock deadline.
//! Session, execution, provider, tool, and sandbox layers depend on it so a
//! single caller-supplied [`ResourceEnvelope`] can bound a whole dispatch tree.
//!
//! The enforcement model is *reserve-then-reconcile*, not measure-afterwards:
//!
//! 1. A caller sizes the worst case a unit of work can consume and calls
//!    [`ResourceLedger::try_reserve`] **before** dispatching anything.
//! 2. The ledger admits the reservation only when
//!    `committed + outstanding + request` stays inside every limit and the
//!    deadline has not passed. Otherwise it returns [`ResourceError`] and the
//!    caller must not dispatch.
//! 3. After the work finishes, the caller calls
//!    [`ResourceLedger::reconcile`] with the actual usage (or
//!    [`ResourceLedger::release`] when nothing was consumed), which frees the
//!    unused part of the reservation for later work.
//!
//! Amounts are unsigned integers on purpose. Money is carried as micro-USD so
//! that a hard limit is never decided by floating-point rounding, and every
//! projection uses checked arithmetic so an overflow is a hard rejection rather
//! than a silently wrapped budget.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Wire-format version of the resource envelope, reservation, and ledger
/// contracts defined in this module.
///
/// Persisted or transmitted envelopes carry the version they were authored
/// against; [`ResourceEnvelope::validate`] rejects anything this build does not
/// implement instead of guessing at the missing semantics.
pub const RESOURCE_CONTRACT_VERSION: u32 = 1;

/// Number of micro-USD in one US dollar.
pub const MICRO_USD_PER_DOLLAR: u64 = 1_000_000;

uuid_id!(
    /// Identifier for a single outstanding [`ResourceReservation`].
    pub struct ResourceReservationId
);

/// One metered dimension of a [`ResourceEnvelope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    /// Spend in micro-USD.
    CostMicroUsd,
    /// Model tokens, input plus output.
    Tokens,
    /// Agent turns.
    Turns,
    /// Model (LLM provider) calls.
    ModelCalls,
    /// Tool calls, including sandbox and MCP dispatch.
    ToolCalls,
}

impl ResourceKind {
    /// Every metered dimension, in the deterministic order limits are checked.
    pub const ALL: [Self; 5] = [
        Self::CostMicroUsd,
        Self::Tokens,
        Self::Turns,
        Self::ModelCalls,
        Self::ToolCalls,
    ];

    /// Returns the stable snake-case name of the dimension.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CostMicroUsd => "cost_micro_usd",
            Self::Tokens => "tokens",
            Self::Turns => "turns",
            Self::ModelCalls => "model_calls",
            Self::ToolCalls => "tool_calls",
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A point in the metered resource space: a limit, a reservation, or an actual
/// usage reading, depending on where it is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceAmounts {
    /// Spend in micro-USD.
    pub cost_micro_usd: u64,
    /// Model tokens, input plus output.
    pub tokens: u64,
    /// Agent turns.
    pub turns: u64,
    /// Model provider calls.
    pub model_calls: u64,
    /// Tool calls.
    pub tool_calls: u64,
}

impl ResourceAmounts {
    /// The origin: zero of every dimension.
    pub const ZERO: Self = Self {
        cost_micro_usd: 0,
        tokens: 0,
        turns: 0,
        model_calls: 0,
        tool_calls: 0,
    };

    /// Returns the value of one dimension.
    #[must_use]
    pub const fn get(&self, kind: ResourceKind) -> u64 {
        match kind {
            ResourceKind::CostMicroUsd => self.cost_micro_usd,
            ResourceKind::Tokens => self.tokens,
            ResourceKind::Turns => self.turns,
            ResourceKind::ModelCalls => self.model_calls,
            ResourceKind::ToolCalls => self.tool_calls,
        }
    }

    /// Returns whether every dimension is zero.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.cost_micro_usd == 0
            && self.tokens == 0
            && self.turns == 0
            && self.model_calls == 0
            && self.tool_calls == 0
    }

    /// Adds two points, returning `None` on overflow in any dimension.
    #[must_use]
    pub fn checked_add(&self, other: &Self) -> Option<Self> {
        Some(Self {
            cost_micro_usd: self.cost_micro_usd.checked_add(other.cost_micro_usd)?,
            tokens: self.tokens.checked_add(other.tokens)?,
            turns: self.turns.checked_add(other.turns)?,
            model_calls: self.model_calls.checked_add(other.model_calls)?,
            tool_calls: self.tool_calls.checked_add(other.tool_calls)?,
        })
    }

    /// Scales a point, returning `None` on overflow in any dimension.
    ///
    /// Worst-case projections (`per_case * runs`) must use this: a wrapped
    /// projection would understate the budget and admit unbounded work.
    #[must_use]
    pub fn checked_mul(&self, factor: u64) -> Option<Self> {
        Some(Self {
            cost_micro_usd: self.cost_micro_usd.checked_mul(factor)?,
            tokens: self.tokens.checked_mul(factor)?,
            turns: self.turns.checked_mul(factor)?,
            model_calls: self.model_calls.checked_mul(factor)?,
            tool_calls: self.tool_calls.checked_mul(factor)?,
        })
    }

    /// Subtracts `other`, flooring each dimension at zero.
    #[must_use]
    pub fn saturating_sub(&self, other: &Self) -> Self {
        Self {
            cost_micro_usd: self.cost_micro_usd.saturating_sub(other.cost_micro_usd),
            tokens: self.tokens.saturating_sub(other.tokens),
            turns: self.turns.saturating_sub(other.turns),
            model_calls: self.model_calls.saturating_sub(other.model_calls),
            tool_calls: self.tool_calls.saturating_sub(other.tool_calls),
        }
    }

    /// Returns the first dimension in which `self` is strictly greater than
    /// `limits`, checked in [`ResourceKind::ALL`] order.
    ///
    /// Equality is inside the limit: a projection that lands exactly on the
    /// authored maximum is admitted.
    #[must_use]
    pub fn first_exceeding(&self, limits: &Self) -> Option<ResourceKind> {
        ResourceKind::ALL
            .into_iter()
            .find(|&kind| self.get(kind) > limits.get(kind))
    }

    /// Converts a dollar amount to micro-USD, rounding up.
    ///
    /// Rounding up keeps reconciliation conservative: a fractional micro-dollar
    /// of real spend is never accounted as free.
    pub fn cost_micro_usd_from_dollars(dollars: f64) -> Result<u64, ResourceError> {
        if !dollars.is_finite() || dollars < 0.0 {
            return Err(ResourceError::InvalidCost { dollars });
        }
        let micro = (dollars * MICRO_USD_PER_DOLLAR as f64).ceil();
        if micro >= u64::MAX as f64 {
            return Err(ResourceError::InvalidCost { dollars });
        }
        Ok(micro as u64)
    }
}

/// A versioned bound on everything one unit of work may consume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceEnvelope {
    /// Contract version the envelope was authored against.
    pub version: u32,
    /// Inclusive maximum for every metered dimension.
    pub limits: ResourceAmounts,
    /// Absolute wall-clock deadline, or `None` for an unbounded envelope.
    pub deadline: Option<DateTime<Utc>>,
}

impl ResourceEnvelope {
    /// Creates an envelope at the current [`RESOURCE_CONTRACT_VERSION`].
    #[must_use]
    pub const fn new(limits: ResourceAmounts, deadline: Option<DateTime<Utc>>) -> Self {
        Self {
            version: RESOURCE_CONTRACT_VERSION,
            limits,
            deadline,
        }
    }

    /// Rejects an envelope this build does not implement.
    pub fn validate(&self) -> Result<(), ResourceError> {
        if self.version != RESOURCE_CONTRACT_VERSION {
            return Err(ResourceError::UnsupportedVersion {
                version: self.version,
                supported: RESOURCE_CONTRACT_VERSION,
            });
        }
        Ok(())
    }

    /// Returns whether the absolute deadline has been reached at `now`.
    ///
    /// The deadline is exclusive: work may start strictly before it, never at
    /// or after it.
    #[must_use]
    pub fn deadline_passed(&self, now: DateTime<Utc>) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Returns the time left before the deadline, or `None` when unbounded.
    #[must_use]
    pub fn time_remaining(&self, now: DateTime<Utc>) -> Option<StdDuration> {
        self.deadline
            .map(|deadline| (deadline - now).to_std().unwrap_or(StdDuration::ZERO))
    }
}

/// Capacity that has been withheld from an envelope for work not yet dispatched.
///
/// A reservation is intentionally neither `Clone` nor `Copy`: it must be handed
/// back to exactly one [`ResourceLedger::reconcile`] or
/// [`ResourceLedger::release`] call, so double-accounting cannot compile.
#[derive(Debug)]
pub struct ResourceReservation {
    id: ResourceReservationId,
    reserved: ResourceAmounts,
}

impl ResourceReservation {
    /// Returns the reservation identifier.
    #[must_use]
    pub const fn id(&self) -> ResourceReservationId {
        self.id
    }

    /// Returns the worst-case amounts withheld by this reservation.
    #[must_use]
    pub const fn reserved(&self) -> ResourceAmounts {
        self.reserved
    }
}

/// Result of reconciling actual usage against a reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// Actual usage fit inside the reservation.
    WithinReservation,
    /// Actual usage exceeded the reservation by these amounts.
    ///
    /// The overrun is still committed — real spend already happened — so the
    /// envelope shrinks and later reservations fail sooner.
    Overrun(ResourceAmounts),
}

impl ReconcileOutcome {
    /// Returns the overrun amounts, when the reservation was exceeded.
    #[must_use]
    pub const fn overrun(&self) -> Option<ResourceAmounts> {
        match self {
            Self::WithinReservation => None,
            Self::Overrun(amounts) => Some(*amounts),
        }
    }
}

/// Serializable view of a ledger for reporting and persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLedgerSnapshot {
    /// Contract version of the underlying envelope.
    pub version: u32,
    /// Envelope limits.
    pub limits: ResourceAmounts,
    /// Absolute deadline, when one is set.
    pub deadline: Option<DateTime<Utc>>,
    /// Reconciled actual usage.
    pub committed: ResourceAmounts,
    /// Reserved capacity for work that has not reconciled yet.
    pub outstanding: ResourceAmounts,
    /// `limits - (committed + outstanding)`, floored at zero.
    pub remaining: ResourceAmounts,
    /// Number of reservations still open.
    pub open_reservations: usize,
}

/// Reserve-then-reconcile accounting against one [`ResourceEnvelope`].
#[derive(Debug)]
pub struct ResourceLedger {
    envelope: ResourceEnvelope,
    committed: ResourceAmounts,
    outstanding: ResourceAmounts,
    open: HashMap<ResourceReservationId, ResourceAmounts>,
}

impl ResourceLedger {
    /// Creates an empty ledger, rejecting an envelope this build cannot honor.
    pub fn new(envelope: ResourceEnvelope) -> Result<Self, ResourceError> {
        envelope.validate()?;
        Ok(Self {
            envelope,
            committed: ResourceAmounts::ZERO,
            outstanding: ResourceAmounts::ZERO,
            open: HashMap::new(),
        })
    }

    /// Returns the envelope this ledger enforces.
    #[must_use]
    pub const fn envelope(&self) -> &ResourceEnvelope {
        &self.envelope
    }

    /// Returns reconciled actual usage.
    #[must_use]
    pub const fn committed(&self) -> ResourceAmounts {
        self.committed
    }

    /// Returns capacity reserved for work that has not reconciled yet.
    #[must_use]
    pub const fn outstanding(&self) -> ResourceAmounts {
        self.outstanding
    }

    /// Returns capacity still available to reserve.
    #[must_use]
    pub fn remaining(&self) -> ResourceAmounts {
        let used = self
            .committed
            .checked_add(&self.outstanding)
            .unwrap_or(self.envelope.limits);
        self.envelope.limits.saturating_sub(&used)
    }

    /// Withholds worst-case capacity for one unit of work.
    ///
    /// Callers must treat an error as "do not dispatch": no provider call, tool
    /// call, sandbox start, or session turn may be issued without a reservation.
    pub fn try_reserve(
        &mut self,
        request: ResourceAmounts,
        now: DateTime<Utc>,
    ) -> Result<ResourceReservation, ResourceError> {
        if request.is_zero() {
            return Err(ResourceError::EmptyReservation);
        }
        if let Some(deadline) = self.envelope.deadline
            && self.envelope.deadline_passed(now)
        {
            return Err(ResourceError::DeadlineExceeded { deadline });
        }

        let used =
            self.committed
                .checked_add(&self.outstanding)
                .ok_or(ResourceError::Overflow {
                    kind: ResourceKind::CostMicroUsd,
                })?;
        let projected = used.checked_add(&request).ok_or(ResourceError::Overflow {
            kind: ResourceKind::CostMicroUsd,
        })?;

        if let Some(kind) = projected.first_exceeding(&self.envelope.limits) {
            return Err(ResourceError::Exhausted {
                kind,
                requested: request.get(kind),
                remaining: self
                    .envelope
                    .limits
                    .get(kind)
                    .saturating_sub(used.get(kind)),
                limit: self.envelope.limits.get(kind),
            });
        }

        let id = ResourceReservationId::new();
        self.outstanding = projected.saturating_sub(&self.committed);
        self.open.insert(id, request);
        Ok(ResourceReservation {
            id,
            reserved: request,
        })
    }

    /// Commits actual usage and frees the unused part of a reservation.
    pub fn reconcile(
        &mut self,
        reservation: ResourceReservation,
        actual: ResourceAmounts,
    ) -> Result<ReconcileOutcome, ResourceError> {
        let reserved =
            self.open
                .remove(&reservation.id)
                .ok_or(ResourceError::UnknownReservation {
                    id: reservation.id.to_string(),
                })?;
        self.outstanding = self.outstanding.saturating_sub(&reserved);
        self.committed = self
            .committed
            .checked_add(&actual)
            .ok_or(ResourceError::Overflow {
                kind: ResourceKind::CostMicroUsd,
            })?;

        let overrun = actual.saturating_sub(&reserved);
        if overrun.is_zero() {
            Ok(ReconcileOutcome::WithinReservation)
        } else {
            Ok(ReconcileOutcome::Overrun(overrun))
        }
    }

    /// Returns a reservation without committing any usage.
    pub fn release(&mut self, reservation: ResourceReservation) -> Result<(), ResourceError> {
        let reserved =
            self.open
                .remove(&reservation.id)
                .ok_or(ResourceError::UnknownReservation {
                    id: reservation.id.to_string(),
                })?;
        self.outstanding = self.outstanding.saturating_sub(&reserved);
        Ok(())
    }

    /// Returns a serializable snapshot for reporting.
    #[must_use]
    pub fn snapshot(&self) -> ResourceLedgerSnapshot {
        ResourceLedgerSnapshot {
            version: self.envelope.version,
            limits: self.envelope.limits,
            deadline: self.envelope.deadline,
            committed: self.committed,
            outstanding: self.outstanding,
            remaining: self.remaining(),
            open_reservations: self.open.len(),
        }
    }
}

/// A [`ResourceLedger`] shared by concurrently dispatched work.
///
/// The critical sections are pure arithmetic and never await, so a blocking
/// mutex is correct here; a poisoned lock is recovered rather than propagated so
/// one panicking case cannot disable budget enforcement for the rest of a run.
#[derive(Debug, Clone)]
pub struct SharedResourceLedger(Arc<Mutex<ResourceLedger>>);

impl SharedResourceLedger {
    /// Wraps a ledger for concurrent use.
    #[must_use]
    pub fn new(ledger: ResourceLedger) -> Self {
        Self(Arc::new(Mutex::new(ledger)))
    }

    /// Creates a shared ledger directly from an envelope.
    pub fn from_envelope(envelope: ResourceEnvelope) -> Result<Self, ResourceError> {
        Ok(Self::new(ResourceLedger::new(envelope)?))
    }

    /// See [`ResourceLedger::try_reserve`].
    pub fn try_reserve(
        &self,
        request: ResourceAmounts,
        now: DateTime<Utc>,
    ) -> Result<ResourceReservation, ResourceError> {
        self.with(|ledger| ledger.try_reserve(request, now))
    }

    /// See [`ResourceLedger::reconcile`].
    pub fn reconcile(
        &self,
        reservation: ResourceReservation,
        actual: ResourceAmounts,
    ) -> Result<ReconcileOutcome, ResourceError> {
        self.with(|ledger| ledger.reconcile(reservation, actual))
    }

    /// See [`ResourceLedger::release`].
    pub fn release(&self, reservation: ResourceReservation) -> Result<(), ResourceError> {
        self.with(|ledger| ledger.release(reservation))
    }

    /// See [`ResourceLedger::snapshot`].
    #[must_use]
    pub fn snapshot(&self) -> ResourceLedgerSnapshot {
        self.with(|ledger| ledger.snapshot())
    }

    fn with<T>(&self, action: impl FnOnce(&mut ResourceLedger) -> T) -> T {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        action(&mut guard)
    }
}

/// An absolute deadline bound to a cooperative cancellation token.
///
/// Wrapping work in [`DeadlineGuard::run`] instead of a bare
/// `tokio::time::timeout` is what makes a deadline *propagate*: expiry cancels
/// the shared token, so every provider call, session turn, tool, and sandbox
/// holding a clone observes the cancellation and unwinds. Dropping the outer
/// future alone would leave detached work running and still billing.
#[derive(Debug, Clone)]
pub struct DeadlineGuard {
    cancel: CancellationToken,
    deadline: Option<DateTime<Utc>>,
}

impl DeadlineGuard {
    /// Binds a deadline to an existing cancellation token.
    #[must_use]
    pub const fn new(cancel: CancellationToken, deadline: Option<DateTime<Utc>>) -> Self {
        Self { cancel, deadline }
    }

    /// Creates a guard with a fresh token and no deadline.
    #[must_use]
    pub fn unbounded() -> Self {
        Self {
            cancel: CancellationToken::new(),
            deadline: None,
        }
    }

    /// Returns the token dispatched work must observe.
    #[must_use]
    pub const fn token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Returns the effective deadline, when one is set.
    #[must_use]
    pub const fn deadline(&self) -> Option<DateTime<Utc>> {
        self.deadline
    }

    /// Derives a narrower guard: a child token plus the earlier of the two
    /// deadlines.
    ///
    /// Cancelling the parent cancels the child; cancelling the child leaves
    /// sibling work alone.
    #[must_use]
    pub fn child(&self, deadline: Option<DateTime<Utc>>) -> Self {
        let deadline = match (self.deadline, deadline) {
            (Some(outer), Some(inner)) => Some(outer.min(inner)),
            (Some(outer), None) => Some(outer),
            (None, inner) => inner,
        };
        Self {
            cancel: self.cancel.child_token(),
            deadline,
        }
    }

    /// Returns whether this scope has already been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Cancels this scope and every scope derived from it.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Runs `future` under the deadline and cancellation token.
    ///
    /// On expiry the token is cancelled *before* the error is returned, so
    /// cooperating work stops rather than being orphaned.
    pub async fn run<F>(&self, future: F) -> Result<F::Output, ResourceError>
    where
        F: Future,
    {
        if self.cancel.is_cancelled() {
            return Err(ResourceError::Cancelled);
        }

        let deadline = self.deadline;
        tokio::pin!(future);
        let expiry = async move {
            match deadline {
                Some(deadline) => {
                    let remaining = (deadline - Utc::now())
                        .to_std()
                        .unwrap_or(StdDuration::ZERO);
                    tokio::time::sleep(remaining).await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(expiry);

        tokio::select! {
            output = &mut future => Ok(output),
            () = self.cancel.cancelled() => Err(ResourceError::Cancelled),
            () = &mut expiry => {
                self.cancel.cancel();
                Err(ResourceError::DeadlineExceeded {
                    deadline: deadline.unwrap_or_else(Utc::now),
                })
            }
        }
    }
}

/// Errors raised while admitting, reserving, or reconciling resources.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResourceError {
    /// The envelope declares a contract version this build does not implement.
    #[error("unsupported resource contract version {version} (supported: {supported})")]
    UnsupportedVersion {
        /// Version carried by the envelope.
        version: u32,
        /// Version implemented by this build.
        supported: u32,
    },
    /// The request would push a dimension past its limit.
    #[error(
        "resource envelope exhausted for {kind}: requested {requested}, {remaining} remaining of {limit}"
    )]
    Exhausted {
        /// Dimension that ran out.
        kind: ResourceKind,
        /// Amount the caller asked to reserve.
        requested: u64,
        /// Amount still available before the request.
        remaining: u64,
        /// Configured maximum.
        limit: u64,
    },
    /// The absolute deadline has passed.
    #[error("resource deadline {deadline} has passed")]
    DeadlineExceeded {
        /// Deadline that was missed.
        deadline: DateTime<Utc>,
    },
    /// Work was cancelled through the shared token.
    #[error("resource scope cancelled")]
    Cancelled,
    /// Projecting the request overflowed the accounting integers.
    #[error("resource accounting overflowed while projecting {kind}")]
    Overflow {
        /// Dimension whose projection overflowed.
        kind: ResourceKind,
    },
    /// A reservation must consume at least one metered dimension.
    #[error("resource reservation is empty")]
    EmptyReservation,
    /// The reservation is not open on this ledger.
    #[error("unknown resource reservation {id}")]
    UnknownReservation {
        /// Reservation identifier that was not open.
        id: String,
    },
    /// A dollar amount was negative, NaN, infinite, or unrepresentable.
    #[error("invalid cost amount {dollars}")]
    InvalidCost {
        /// Offending dollar value.
        dollars: f64,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        DeadlineGuard, ReconcileOutcome, ResourceAmounts, ResourceEnvelope, ResourceError,
        ResourceKind, ResourceLedger, SharedResourceLedger,
    };
    use chrono::{Duration, Utc};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn amounts(
        cost: u64,
        tokens: u64,
        turns: u64,
        model_calls: u64,
        tool_calls: u64,
    ) -> ResourceAmounts {
        ResourceAmounts {
            cost_micro_usd: cost,
            tokens,
            turns,
            model_calls,
            tool_calls,
        }
    }

    fn ledger(limits: ResourceAmounts) -> ResourceLedger {
        ResourceLedger::new(ResourceEnvelope::new(limits, None)).expect("current-version envelope")
    }

    #[test]
    fn reservation_landing_exactly_on_the_limit_is_admitted() {
        // Pins: the admission comparison is `projected > limit`, not `>=`, so an
        // exactly-sized run is not spuriously rejected.
        let mut ledger = ledger(amounts(1_000, 500, 4, 8, 16));

        let reservation = ledger
            .try_reserve(amounts(1_000, 500, 4, 8, 16), Utc::now())
            .expect("exact-limit reservation is admitted");

        assert_eq!(reservation.reserved(), amounts(1_000, 500, 4, 8, 16));
        assert_eq!(ledger.remaining(), ResourceAmounts::ZERO);
    }

    #[test]
    fn one_unit_over_the_limit_reserves_nothing() {
        // Pins: upper-bound-plus-one in each dimension is rejected and leaves the
        // ledger untouched, so no work can be dispatched against it.
        let limits = amounts(1_000, 500, 4, 8, 16);
        for kind in ResourceKind::ALL {
            let mut ledger = ledger(limits);
            let mut request = limits;
            match kind {
                ResourceKind::CostMicroUsd => request.cost_micro_usd += 1,
                ResourceKind::Tokens => request.tokens += 1,
                ResourceKind::Turns => request.turns += 1,
                ResourceKind::ModelCalls => request.model_calls += 1,
                ResourceKind::ToolCalls => request.tool_calls += 1,
            }

            let error = ledger
                .try_reserve(request, Utc::now())
                .expect_err("over-limit reservation must be rejected");
            assert!(
                matches!(error, ResourceError::Exhausted { kind: rejected, .. } if rejected == kind),
                "expected {kind} exhaustion, got {error}"
            );
            assert_eq!(ledger.outstanding(), ResourceAmounts::ZERO);
            assert_eq!(ledger.snapshot().open_reservations, 0);
        }
    }

    #[test]
    fn empty_reservation_is_rejected_without_growing_open_state() {
        // Pins: a zero request cannot allocate an unmetered reservation entry
        // that repeated callers could use to grow the ledger without bound.
        let mut ledger = ledger(amounts(1_000, 500, 4, 8, 16));

        for _ in 0..3 {
            let error = ledger
                .try_reserve(ResourceAmounts::ZERO, Utc::now())
                .expect_err("an empty reservation must be rejected");
            assert_eq!(error, ResourceError::EmptyReservation);
        }

        assert_eq!(ledger.outstanding(), ResourceAmounts::ZERO);
        assert_eq!(ledger.snapshot().open_reservations, 0);
    }

    #[test]
    fn accumulated_reservations_exhaust_the_envelope() {
        let mut ledger = ledger(amounts(1_000, 0, 0, 0, 0));
        let first = ledger
            .try_reserve(amounts(600, 0, 0, 0, 0), Utc::now())
            .expect("first reservation fits");

        let error = ledger
            .try_reserve(amounts(401, 0, 0, 0, 0), Utc::now())
            .expect_err("second reservation exceeds the remainder");
        assert!(matches!(
            error,
            ResourceError::Exhausted {
                kind: ResourceKind::CostMicroUsd,
                requested: 401,
                remaining: 400,
                limit: 1_000,
            }
        ));

        // Reconciling under budget frees the unused reservation.
        let outcome = ledger
            .reconcile(first, amounts(100, 0, 0, 0, 0))
            .expect("reconcile open reservation");
        assert_eq!(outcome, ReconcileOutcome::WithinReservation);
        assert_eq!(ledger.remaining().cost_micro_usd, 900);
        ledger
            .try_reserve(amounts(401, 0, 0, 0, 0), Utc::now())
            .expect("freed capacity admits the retry");
    }

    #[test]
    fn worst_case_projection_overflow_is_rejected_not_wrapped() {
        // Pins: `per_case * runs` uses checked arithmetic. A wrapped projection
        // would look small and admit unbounded paid work.
        let per_case = amounts(u64::MAX / 2, 1, 1, 1, 1);
        assert_eq!(per_case.checked_mul(3), None);
        assert_eq!(
            per_case
                .checked_add(&per_case)
                .and_then(|sum| sum.checked_add(&per_case)),
            None
        );

        let mut ledger = ledger(amounts(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX));
        let _first = ledger
            .try_reserve(amounts(u64::MAX - 1, 0, 0, 0, 0), Utc::now())
            .expect("first reservation fits");
        let error = ledger
            .try_reserve(amounts(u64::MAX, 0, 0, 0, 0), Utc::now())
            .expect_err("projection past u64::MAX must be rejected");
        assert!(matches!(
            error,
            ResourceError::Overflow { .. } | ResourceError::Exhausted { .. }
        ));
    }

    #[test]
    fn reconcile_commits_overrun_and_shrinks_the_envelope() {
        let mut ledger = ledger(amounts(1_000, 0, 0, 0, 0));
        let reservation = ledger
            .try_reserve(amounts(300, 0, 0, 0, 0), Utc::now())
            .expect("reservation fits");

        let outcome = ledger
            .reconcile(reservation, amounts(500, 0, 0, 0, 0))
            .expect("reconcile");
        assert_eq!(outcome.overrun().map(|over| over.cost_micro_usd), Some(200));
        assert_eq!(ledger.committed().cost_micro_usd, 500);
        assert_eq!(ledger.remaining().cost_micro_usd, 500);
    }

    #[test]
    fn released_reservation_restores_capacity_and_cannot_be_reused() {
        let mut ledger = ledger(amounts(1_000, 0, 0, 0, 0));
        let reservation = ledger
            .try_reserve(amounts(1_000, 0, 0, 0, 0), Utc::now())
            .expect("reservation fits");
        let id = reservation.id();
        ledger.release(reservation).expect("release");
        assert_eq!(ledger.remaining().cost_micro_usd, 1_000);
        assert_eq!(ledger.snapshot().open_reservations, 0);

        let stale = ledger
            .try_reserve(amounts(1, 0, 0, 0, 0), Utc::now())
            .expect("fresh reservation");
        assert_ne!(stale.id(), id);
    }

    #[test]
    fn expired_deadline_blocks_every_reservation() {
        let now = Utc::now();
        let envelope = ResourceEnvelope::new(amounts(1_000, 0, 0, 0, 0), Some(now));
        let mut ledger = ResourceLedger::new(envelope).expect("ledger");

        // Exactly at the deadline is already too late.
        let error = ledger
            .try_reserve(amounts(1, 0, 0, 0, 0), now)
            .expect_err("deadline is exclusive");
        assert!(matches!(error, ResourceError::DeadlineExceeded { .. }));

        ledger
            .try_reserve(amounts(1, 0, 0, 0, 0), now - Duration::milliseconds(1))
            .expect("strictly before the deadline is admitted");
    }

    #[test]
    fn unsupported_envelope_version_is_rejected() {
        let envelope = ResourceEnvelope {
            version: 999,
            limits: ResourceAmounts::ZERO,
            deadline: None,
        };
        let error = ResourceLedger::new(envelope).expect_err("unknown version");
        assert!(matches!(
            error,
            ResourceError::UnsupportedVersion { version: 999, .. }
        ));
    }

    #[test]
    fn dollar_conversion_rejects_invalid_amounts_and_rounds_up() {
        assert_eq!(
            ResourceAmounts::cost_micro_usd_from_dollars(0.0000015).expect("valid"),
            2
        );
        assert!(matches!(
            ResourceAmounts::cost_micro_usd_from_dollars(-0.01),
            Err(ResourceError::InvalidCost { .. })
        ));
        assert!(matches!(
            ResourceAmounts::cost_micro_usd_from_dollars(f64::NAN),
            Err(ResourceError::InvalidCost { .. })
        ));
    }

    #[tokio::test]
    async fn deadline_expiry_cancels_the_shared_token() {
        // Pins: an expired deadline propagates cancellation to work holding the
        // token instead of merely dropping the outer future.
        let token = CancellationToken::new();
        let guard =
            DeadlineGuard::new(token.clone(), Some(Utc::now() + Duration::milliseconds(20)));
        let inner = token.child_token();
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed_in_task = Arc::clone(&observed);
        let watcher = tokio::spawn(async move {
            inner.cancelled().await;
            observed_in_task.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let error = guard
            .run(async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            })
            .await
            .expect_err("deadline must fire");
        assert!(matches!(error, ResourceError::DeadlineExceeded { .. }));

        watcher.await.expect("watcher observes cancellation");
        assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn external_cancellation_stops_the_scope_before_it_runs() {
        let guard = DeadlineGuard::unbounded();
        guard.cancel();
        let error = guard
            .run(async { unreachable!("cancelled scope must not dispatch") })
            .await
            .expect_err("cancelled scope");
        assert!(matches!(error, ResourceError::Cancelled));
    }

    #[test]
    fn child_guard_takes_the_earlier_deadline() {
        let now = Utc::now();
        let parent =
            DeadlineGuard::new(CancellationToken::new(), Some(now + Duration::seconds(10)));
        let child = parent.child(Some(now + Duration::seconds(2)));
        assert_eq!(child.deadline(), Some(now + Duration::seconds(2)));

        let wider = parent.child(Some(now + Duration::seconds(60)));
        assert_eq!(wider.deadline(), Some(now + Duration::seconds(10)));

        parent.cancel();
        assert!(child.is_cancelled());
    }

    #[tokio::test]
    async fn shared_ledger_admits_exactly_the_envelope_under_contention() {
        // Pins: concurrent reservations never oversubscribe the envelope.
        let ledger = SharedResourceLedger::from_envelope(ResourceEnvelope::new(
            amounts(0, 0, 0, 10, 0),
            None,
        ))
        .expect("ledger");

        let mut handles = Vec::new();
        for _ in 0..32 {
            let ledger = ledger.clone();
            handles.push(tokio::spawn(async move {
                match ledger.try_reserve(amounts(0, 0, 0, 1, 0), Utc::now()) {
                    Ok(reservation) => {
                        ledger
                            .reconcile(reservation, amounts(0, 0, 0, 1, 0))
                            .expect("reconcile");
                        true
                    }
                    Err(_) => false,
                }
            }));
        }

        let mut admitted = 0usize;
        for handle in handles {
            if handle.await.expect("join") {
                admitted += 1;
            }
        }

        assert_eq!(admitted, 10);
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.committed.model_calls, 10);
        assert_eq!(snapshot.remaining.model_calls, 0);
        assert_eq!(snapshot.open_reservations, 0);
    }
}
