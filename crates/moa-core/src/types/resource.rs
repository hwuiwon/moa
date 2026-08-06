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

use crate::error::MoaError;

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

    /// Returns the dimension-wise minimum of two points.
    ///
    /// This is the restrictive intersection used when a child scope narrows a
    /// parent allowance: no combination of layers can hand back more than the
    /// tightest one already permitted.
    #[must_use]
    pub fn restrict(&self, other: &Self) -> Self {
        Self {
            cost_micro_usd: self.cost_micro_usd.min(other.cost_micro_usd),
            tokens: self.tokens.min(other.tokens),
            turns: self.turns.min(other.turns),
            model_calls: self.model_calls.min(other.model_calls),
            tool_calls: self.tool_calls.min(other.tool_calls),
        }
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

/// The slice of an envelope one in-flight dispatch may still spend.
///
/// This is the value that threads a budget *downwards*, through layers that
/// hold no ledger of their own: a hand provision, a tool executor, a sandbox
/// command. It is deliberately [`Copy`] and free of interior mutability so it
/// can sit in a [`crate::types::hands::HandSpec`], cross an `async_trait`
/// boundary, and be re-derived per attempt without any layer being able to
/// widen it or hand it back changed.
///
/// `None` in either field means *unbounded in that dimension*, which is not the
/// same as zero: `remaining: Some(ResourceAmounts::ZERO)` says "nothing left to
/// spend", while `remaining: None` says "no metered allowance applies here".
/// Collapsing the two would turn an unmetered call into a refused one, or —
/// far worse — an exhausted one into an unlimited one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceBudget {
    /// Absolute wall-clock deadline, or `None` when unbounded.
    pub deadline: Option<DateTime<Utc>>,
    /// Allowance left in every metered dimension, or `None` when unmetered.
    pub remaining: Option<ResourceAmounts>,
}

impl ResourceBudget {
    /// A budget that bounds nothing: no deadline and no metered allowance.
    pub const UNBOUNDED: Self = Self {
        deadline: None,
        remaining: None,
    };

    /// Creates a budget from an optional deadline and optional allowance.
    #[must_use]
    pub const fn new(deadline: Option<DateTime<Utc>>, remaining: Option<ResourceAmounts>) -> Self {
        Self {
            deadline,
            remaining,
        }
    }

    /// Creates an unmetered budget bounded only by an absolute deadline.
    #[must_use]
    pub const fn until(deadline: DateTime<Utc>) -> Self {
        Self {
            deadline: Some(deadline),
            remaining: None,
        }
    }

    /// Returns whether this budget bounds nothing at all.
    #[must_use]
    pub const fn is_unbounded(&self) -> bool {
        self.deadline.is_none() && self.remaining.is_none()
    }

    /// Returns whether the absolute deadline has been reached at `now`.
    ///
    /// The deadline is exclusive, matching [`ResourceEnvelope::deadline_passed`]:
    /// work may start strictly before it, never at or after it.
    #[must_use]
    pub fn deadline_passed(&self, now: DateTime<Utc>) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Returns the time left before the deadline, or `None` when unbounded.
    ///
    /// An already-passed deadline yields `Some(Duration::ZERO)`, never `None`:
    /// "out of time" and "no deadline" must not collapse into one value, or an
    /// expired scope would run unbounded.
    #[must_use]
    pub fn time_remaining(&self, now: DateTime<Utc>) -> Option<StdDuration> {
        self.deadline
            .map(|deadline| (deadline - now).to_std().unwrap_or(StdDuration::ZERO))
    }

    /// Returns the first dimension `request` would overspend, when metered.
    #[must_use]
    pub fn first_exceeding(&self, request: &ResourceAmounts) -> Option<ResourceKind> {
        self.remaining
            .and_then(|remaining| request.first_exceeding(&remaining))
    }

    /// Returns the budget left after charging `usage` at `now`.
    ///
    /// The charge is all-or-nothing: an expired deadline or an amount above any
    /// remaining dimension returns an error and leaves the original copy
    /// available to the caller. Unmetered budgets retain an unmetered allowance.
    pub fn try_consume_at(
        self,
        usage: ResourceAmounts,
        now: DateTime<Utc>,
    ) -> Result<Self, ResourceError> {
        if let Some(deadline) = self.deadline
            && self.deadline_passed(now)
        {
            return Err(ResourceError::DeadlineExceeded { deadline });
        }

        let Some(remaining) = self.remaining else {
            return Ok(self);
        };
        if let Some(kind) = usage.first_exceeding(&remaining) {
            return Err(ResourceError::Exhausted {
                kind,
                requested: usage.get(kind),
                remaining: remaining.get(kind),
                limit: remaining.get(kind),
            });
        }

        Ok(Self {
            deadline: self.deadline,
            remaining: Some(remaining.saturating_sub(&usage)),
        })
    }

    /// Returns the budget left after charging `usage` against the current time.
    pub fn try_consume(self, usage: ResourceAmounts) -> Result<Self, ResourceError> {
        self.try_consume_at(usage, Utc::now())
    }

    /// Narrows this budget by another: the earlier deadline and the smaller
    /// allowance in every dimension.
    ///
    /// A `None` on either side is the identity element, so restricting an
    /// unbounded budget by a bounded one yields the bounded one.
    #[must_use]
    pub fn restrict(self, other: Self) -> Self {
        let deadline = match (self.deadline, other.deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        };
        let remaining = match (self.remaining, other.remaining) {
            (Some(left), Some(right)) => Some(left.restrict(&right)),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        };
        Self {
            deadline,
            remaining,
        }
    }

    /// Builds the budget a ledger snapshot still permits.
    #[must_use]
    pub fn from_snapshot(snapshot: &ResourceLedgerSnapshot) -> Self {
        Self {
            deadline: snapshot.deadline,
            remaining: Some(snapshot.remaining),
        }
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
    ///
    /// Consumes the reservation so each linear token can settle exactly once.
    // The owned parameter is intentional: borrowing this non-`Clone`, non-`Copy`
    // token would permit the caller to retain and reuse the settlement handle.
    #[allow(clippy::needless_pass_by_value)]
    pub fn reconcile(
        &mut self,
        reservation: ResourceReservation,
        actual: ResourceAmounts,
    ) -> Result<ReconcileOutcome, ResourceError> {
        let ResourceReservation { id, .. } = reservation;
        let reserved = self
            .open
            .remove(&id)
            .ok_or(ResourceError::UnknownReservation { id: id.to_string() })?;
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
    ///
    /// Consumes the reservation so each linear token can settle exactly once.
    // The owned parameter is intentional: borrowing this non-`Clone`, non-`Copy`
    // token would permit the caller to retain and reuse the settlement handle.
    #[allow(clippy::needless_pass_by_value)]
    pub fn release(&mut self, reservation: ResourceReservation) -> Result<(), ResourceError> {
        let ResourceReservation { id, .. } = reservation;
        let reserved = self
            .open
            .remove(&id)
            .ok_or(ResourceError::UnknownReservation { id: id.to_string() })?;
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

    /// Returns the [`ResourceBudget`] this ledger still permits.
    ///
    /// This is the hand-off point between the ledger (which admits work) and the
    /// dispatch tree (which must not outlive what was admitted).
    #[must_use]
    pub fn budget(&self) -> ResourceBudget {
        ResourceBudget::from_snapshot(&self.snapshot())
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
    budget: ResourceBudget,
}

impl DeadlineGuard {
    /// Binds a deadline to an existing cancellation token.
    #[must_use]
    pub const fn new(cancel: CancellationToken, deadline: Option<DateTime<Utc>>) -> Self {
        Self {
            cancel,
            budget: ResourceBudget::new(deadline, None),
        }
    }

    /// Binds a full [`ResourceBudget`] to an existing cancellation token.
    #[must_use]
    pub const fn from_budget(cancel: CancellationToken, budget: ResourceBudget) -> Self {
        Self { cancel, budget }
    }

    /// Creates a guard with a fresh token and no deadline.
    #[must_use]
    pub fn unbounded() -> Self {
        Self {
            cancel: CancellationToken::new(),
            budget: ResourceBudget::UNBOUNDED,
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
        self.budget.deadline
    }

    /// Returns the `Copy` budget this scope may still spend.
    ///
    /// This is what layers below the guard carry: they cannot cancel the scope,
    /// only observe how much of it is left.
    #[must_use]
    pub const fn budget(&self) -> ResourceBudget {
        self.budget
    }

    /// Derives a narrower guard: a child token plus the earlier of the two
    /// deadlines.
    ///
    /// Cancelling the parent cancels the child; cancelling the child leaves
    /// sibling work alone.
    #[must_use]
    pub fn child(&self, deadline: Option<DateTime<Utc>>) -> Self {
        self.child_budget(ResourceBudget::new(deadline, None))
    }

    /// Derives a narrower guard from a child token and a restricted budget.
    #[must_use]
    pub fn child_budget(&self, budget: ResourceBudget) -> Self {
        Self {
            cancel: self.cancel.child_token(),
            budget: self.budget.restrict(budget),
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

    /// Synchronous pre-dispatch gate: may this scope start paid work at `now`?
    ///
    /// This is what makes "a cancelled scope dispatches nothing" a property
    /// rather than a race. Racing a provider call against a token in a
    /// `select!` still polls the call, and a fast provider can complete before
    /// the cancelled branch is chosen; asking first cannot. An expired deadline
    /// also cancels the token here, so siblings that are only watching the
    /// token still unwind.
    pub fn admit_at(&self, now: DateTime<Utc>) -> Result<(), ResourceError> {
        if self.cancel.is_cancelled() {
            return Err(ResourceError::Cancelled);
        }
        if let Some(deadline) = self.budget.deadline
            && self.budget.deadline_passed(now)
        {
            self.cancel.cancel();
            return Err(ResourceError::DeadlineExceeded { deadline });
        }
        Ok(())
    }

    /// Pre-dispatch gate against the current wall clock. See [`Self::admit_at`].
    pub fn admit(&self) -> Result<(), ResourceError> {
        self.admit_at(Utc::now())
    }

    /// Resolves as soon as this scope is cancelled or its deadline expires.
    ///
    /// This is the primitive a streaming loop selects on: it never resolves
    /// while the scope is live, and expiry cancels the shared token *before*
    /// returning, so a producer task the caller is about to drop has already
    /// been told to stop instead of being silently orphaned.
    pub async fn cancelled_or_expired(&self) -> ResourceError {
        let deadline = self.budget.deadline;
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
            () = self.cancel.cancelled() => ResourceError::Cancelled,
            () = &mut expiry => {
                self.cancel.cancel();
                ResourceError::DeadlineExceeded {
                    deadline: deadline.unwrap_or_else(Utc::now),
                }
            }
        }
    }

    /// Runs `future` under the deadline and cancellation token.
    ///
    /// On expiry the token is cancelled *before* the error is returned, so
    /// cooperating work stops rather than being orphaned.
    pub async fn run<F>(&self, future: F) -> Result<F::Output, ResourceError>
    where
        F: Future,
    {
        self.admit()?;

        tokio::pin!(future);
        tokio::select! {
            // If work and its deadline become ready together, expiry wins: a
            // deadline is an exclusive bound and must cancel sibling work
            // before any terminal output is accepted.
            biased;
            error = self.cancelled_or_expired() => Err(error),
            output = &mut future => Ok(output),
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

impl From<ResourceError> for MoaError {
    /// Projects a resource refusal onto the shared runtime error.
    ///
    /// Cancellation stays cancellation. Everything else — an expired deadline,
    /// an exhausted dimension, an overflowed projection — maps to
    /// [`MoaError::BudgetExhausted`], which
    /// [`crate::error::classify_tool_error`] already treats as fatal. That is
    /// the point: none of these are transient, and a retry loop that read them
    /// as retryable would keep dispatching paid work the ledger already
    /// refused. Malformed contract inputs are caller mistakes and surface as
    /// validation errors instead.
    fn from(error: ResourceError) -> Self {
        match error {
            ResourceError::Cancelled => Self::Cancelled,
            ResourceError::DeadlineExceeded { .. }
            | ResourceError::Exhausted { .. }
            | ResourceError::Overflow { .. }
            | ResourceError::EmptyReservation => Self::BudgetExhausted(error.to_string()),
            ResourceError::UnsupportedVersion { .. }
            | ResourceError::UnknownReservation { .. }
            | ResourceError::InvalidCost { .. } => Self::ValidationError(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeadlineGuard, ReconcileOutcome, ResourceAmounts, ResourceBudget, ResourceEnvelope,
        ResourceError, ResourceKind, ResourceLedger, SharedResourceLedger,
    };
    use crate::error::MoaError;
    use chrono::{Duration, Utc};
    use std::sync::Arc;
    use std::time::Duration as StdDuration;
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

    #[tokio::test]
    async fn an_expired_deadline_never_polls_immediately_ready_work() {
        // Pins: the synchronous admission check runs before `select!` can poll a
        // fast work future. Repeating the immediate-ready race mutation-checks
        // the old implementation, whose randomly selected work branch could
        // perform a side effect after the deadline.
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for _ in 0..64 {
            let guard = DeadlineGuard::new(
                CancellationToken::new(),
                Some(Utc::now() - Duration::seconds(1)),
            );
            let polls_in_future = Arc::clone(&polls);
            let error = guard
                .run(async move {
                    polls_in_future.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
                .await
                .expect_err("an expired deadline must refuse work before polling it");

            assert!(matches!(error, ResourceError::DeadlineExceeded { .. }));
            assert!(guard.is_cancelled(), "expiry must cancel the shared scope");
        }

        assert_eq!(
            polls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "expired work must not perform even an immediately-ready side effect"
        );
    }

    #[test]
    fn an_unmetered_budget_is_not_an_exhausted_one() {
        // Pins: `remaining: None` and `remaining: Some(ZERO)` stay distinct.
        // Collapsing them would either refuse every unmetered call or, far
        // worse, admit an exhausted one as unlimited.
        let unmetered = ResourceBudget::UNBOUNDED;
        let exhausted = ResourceBudget::new(None, Some(ResourceAmounts::ZERO));
        let one_call = amounts(0, 0, 0, 1, 0);

        assert_eq!(unmetered.first_exceeding(&one_call), None);
        assert_eq!(
            exhausted.first_exceeding(&one_call),
            Some(ResourceKind::ModelCalls)
        );
        assert!(unmetered.is_unbounded());
        assert!(!exhausted.is_unbounded());
    }

    #[test]
    fn budget_consumption_accepts_exact_boundary_and_rejects_one_more() {
        // Pins: a downward budget copy decrements every dimension atomically,
        // admits equality, and rejects the next unit without widening or
        // partially charging any other dimension.
        let now = Utc::now();
        let initial = ResourceBudget::new(None, Some(amounts(10, 20, 2, 3, 4)));
        let exhausted = initial
            .try_consume_at(amounts(10, 20, 2, 3, 4), now)
            .expect("the exact remaining boundary is admissible");

        assert_eq!(exhausted.remaining, Some(ResourceAmounts::ZERO));
        assert!(matches!(
            exhausted.try_consume_at(amounts(0, 0, 0, 1, 0), now),
            Err(ResourceError::Exhausted {
                kind: ResourceKind::ModelCalls,
                requested: 1,
                remaining: 0,
                ..
            })
        ));
        assert_eq!(initial.remaining, Some(amounts(10, 20, 2, 3, 4)));
    }

    #[test]
    fn restricting_a_budget_only_ever_tightens_it() {
        // Pins: the intersection takes the earlier deadline and the smaller
        // allowance in every dimension, and an unbounded side is the identity
        // element rather than a widening one.
        let now = Utc::now();
        let outer = ResourceBudget::new(
            Some(now + Duration::seconds(60)),
            Some(amounts(1_000, 500, 4, 8, 16)),
        );
        let inner = ResourceBudget::new(
            Some(now + Duration::seconds(5)),
            Some(amounts(9_000, 100, 9, 2, 99)),
        );

        let narrowed = outer.restrict(inner);
        assert_eq!(narrowed.deadline, Some(now + Duration::seconds(5)));
        assert_eq!(narrowed.remaining, Some(amounts(1_000, 100, 4, 2, 16)));

        // A wider sibling cannot widen an already-bounded scope.
        let wider = ResourceBudget::new(
            Some(now + Duration::seconds(600)),
            Some(amounts(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX)),
        );
        assert_eq!(outer.restrict(wider), outer);
        assert_eq!(ResourceBudget::UNBOUNDED.restrict(outer), outer);
        assert_eq!(outer.restrict(ResourceBudget::UNBOUNDED), outer);
    }

    #[test]
    fn an_expired_budget_reports_zero_time_left_rather_than_no_deadline() {
        let now = Utc::now();
        assert_eq!(ResourceBudget::UNBOUNDED.time_remaining(now), None);
        assert_eq!(
            ResourceBudget::until(now - Duration::seconds(1)).time_remaining(now),
            Some(StdDuration::ZERO)
        );
        assert_eq!(
            ResourceBudget::until(now + Duration::seconds(30)).time_remaining(now),
            Some(StdDuration::from_secs(30))
        );
        // The deadline is exclusive: exactly at it is already too late.
        assert!(ResourceBudget::until(now).deadline_passed(now));
        assert!(!ResourceBudget::until(now).deadline_passed(now - Duration::milliseconds(1)));
    }

    #[test]
    fn a_ledger_hands_down_exactly_what_it_still_permits() {
        let ledger = SharedResourceLedger::from_envelope(ResourceEnvelope::new(
            amounts(0, 0, 0, 4, 0),
            None,
        ))
        .expect("ledger");
        assert_eq!(ledger.budget().remaining, Some(amounts(0, 0, 0, 4, 0)));

        let reservation = ledger
            .try_reserve(amounts(0, 0, 0, 3, 0), Utc::now())
            .expect("reservation fits");
        assert_eq!(
            ledger.budget().remaining,
            Some(amounts(0, 0, 0, 1, 0)),
            "an outstanding reservation is already spent from the downstream budget"
        );
        ledger
            .reconcile(reservation, amounts(0, 0, 0, 1, 0))
            .expect("reconcile");
        assert_eq!(ledger.budget().remaining, Some(amounts(0, 0, 0, 3, 0)));
    }

    #[test]
    fn admission_refuses_a_cancelled_or_expired_scope_without_awaiting() {
        // Pins: the pre-dispatch gate is a synchronous question, so a caller
        // cannot dispatch and then "lose the race" to a cancellation branch.
        let now = Utc::now();
        let live = DeadlineGuard::new(CancellationToken::new(), Some(now + Duration::seconds(10)));
        assert!(live.admit_at(now).is_ok());

        let cancelled = DeadlineGuard::unbounded();
        cancelled.cancel();
        assert_eq!(cancelled.admit_at(now), Err(ResourceError::Cancelled));

        let expired = DeadlineGuard::new(CancellationToken::new(), Some(now));
        assert!(matches!(
            expired.admit_at(now),
            Err(ResourceError::DeadlineExceeded { .. })
        ));
        assert!(
            expired.is_cancelled(),
            "an expired deadline must cancel the shared token, not merely refuse the caller"
        );
    }

    #[tokio::test]
    async fn cancelled_or_expired_never_resolves_while_the_scope_is_live() {
        let guard = DeadlineGuard::new(
            CancellationToken::new(),
            Some(Utc::now() + Duration::seconds(30)),
        );
        let result =
            tokio::time::timeout(StdDuration::from_millis(30), guard.cancelled_or_expired()).await;
        assert!(result.is_err(), "a live scope must not report cancellation");
        assert!(!guard.is_cancelled());

        guard.cancel();
        assert_eq!(guard.cancelled_or_expired().await, ResourceError::Cancelled);
    }

    #[test]
    fn resource_refusals_project_onto_fatal_runtime_errors() {
        // Pins: an exhausted or expired scope must not look retryable to the
        // shared tool/turn retry classifiers, or a refused dispatch would be
        // reattempted until the budget it already exceeded is billed again.
        assert!(matches!(
            MoaError::from(ResourceError::Cancelled),
            MoaError::Cancelled
        ));
        for error in [
            ResourceError::DeadlineExceeded {
                deadline: Utc::now(),
            },
            ResourceError::Exhausted {
                kind: ResourceKind::Tokens,
                requested: 2,
                remaining: 1,
                limit: 3,
            },
            ResourceError::Overflow {
                kind: ResourceKind::Tokens,
            },
            ResourceError::EmptyReservation,
        ] {
            let mapped = MoaError::from(error);
            assert!(
                matches!(mapped, MoaError::BudgetExhausted(_)),
                "expected a budget refusal, got {mapped:?}"
            );
            assert!(matches!(
                crate::error::classify_tool_error(&mapped, 0),
                crate::error::ToolFailureClass::Fatal { .. }
            ));
        }
        assert!(matches!(
            MoaError::from(ResourceError::UnsupportedVersion {
                version: 9,
                supported: 1
            }),
            MoaError::ValidationError(_)
        ));
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
