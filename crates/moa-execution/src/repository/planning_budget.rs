//! Durable run-budget authorization and attribution for amendment-planner provider calls.

use super::*;
use crate::capability::ExecutionEstimate;

const AMENDMENT_PLANNING_RESERVATION_NAMESPACE: Uuid =
    Uuid::from_u128(0xc6e4_a4f8_a46e_581f_bf54_6d89_31ea_251f);
const AMENDMENT_PLANNING_SETTLEMENT_NAMESPACE: Uuid =
    Uuid::from_u128(0x0630_e60f_f390_5eb8_a266_e6e9_3ddd_f753);

/// Cost and token capacity reserved for one amendment-planner provider call.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentPlanningCallReservation {
    /// Reserved provider cost in micro-US-dollars.
    pub cost_microusd: u64,
    /// Reserved provider tokens.
    pub tokens: u64,
}

/// Actual cost and token usage attributed to one amendment-planner provider call.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningUsage {
    /// Provider cost in micro-US-dollars.
    pub cost_microusd: u64,
    /// Provider tokens.
    pub tokens: u64,
}

/// One exact automatic amendment-planner provider-call reservation request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentPlanningCallReservationRequest {
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Exact active plan revision being amended.
    pub base_plan_revision: u64,
    /// Zero-based provider-call ordinal within this amendment attempt.
    pub call_ordinal: u8,
    /// Conservative provider-call reservation.
    pub reservation: AmendmentPlanningCallReservation,
    /// Journaled authorization time.
    pub now: DateTime<Utc>,
}

/// Why a new amendment-planner call was not authorized.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AmendmentPlanningCallDenial {
    /// The run or plan revision is no longer at its amendment boundary.
    StaleRevision,
    /// A terminal intent already fences new provider work.
    PendingTerminal,
    /// An earlier overrun fail-closes new reservations.
    BudgetOverrun,
    /// The approved run deadline has elapsed.
    DeadlineExceeded,
    /// Cost or token capacity is exhausted.
    BudgetExceeded,
}

/// Append-preserved evidence for one amendment-planner call reservation and settlement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentPlanningCallRecord {
    /// Deterministic reservation identity.
    pub reservation_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional contact scope inherited from the run.
    pub contact_id: Option<ContactId>,
    /// Owning run.
    pub run_uid: Uuid,
    /// Exact amended plan revision.
    pub base_plan_revision: u64,
    /// Provider-call ordinal.
    pub call_ordinal: u8,
    /// Authorized conservative reservation.
    pub reserved: AmendmentPlanningCallReservation,
    /// Reconciled provider usage when settled.
    pub actual: Option<PlanningUsage>,
    /// Whether this settlement exceeded its reservation or approved run budget.
    pub budget_overrun: bool,
    /// First authorization time.
    pub created_at: DateTime<Utc>,
    /// Immutable settlement time.
    pub settled_at: Option<DateTime<Utc>>,
}

/// Idempotent outcome of reserving one amendment-planner provider call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AmendmentPlanningCallReservationOutcome {
    /// A new open reservation was committed.
    Granted(AmendmentPlanningCallRecord),
    /// The identical open authorization already exists; the journaled caller may proceed.
    ReplayedOpen(AmendmentPlanningCallRecord),
    /// The call was already reconciled; the caller may replay the same gateway idempotency key.
    AlreadySettled(AmendmentPlanningCallRecord),
    /// Canonical run state denies new provider work.
    Denied(AmendmentPlanningCallDenial),
    /// The logical call identity exists with another reservation.
    Conflict,
    /// No run is visible in the supplied scope.
    NotFound,
}

/// One exact reconciliation of an authorized amendment-planner provider call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentPlanningCallReconcileRequest {
    /// Owning run.
    pub run_uid: Uuid,
    /// Reserved base plan revision.
    pub base_plan_revision: u64,
    /// Reserved provider-call ordinal.
    pub call_ordinal: u8,
    /// Actual billed cost and tokens.
    pub actual: PlanningUsage,
    /// Journaled settlement time.
    pub settled_at: DateTime<Utc>,
}

/// Idempotent outcome of reconciling one amendment-planner provider call.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AmendmentPlanningCallReconcileOutcome {
    /// First settlement and run-budget reconciliation committed.
    Applied(AmendmentPlanningCallRecord),
    /// The identical immutable settlement already exists.
    Replayed(AmendmentPlanningCallRecord),
    /// The logical call was settled with different actual usage.
    Conflict,
    /// No reservation is visible in the supplied scope.
    NotFound,
}

impl ExecutionRepository {
    /// Reserves one exact automatic amendment-planner provider call against the live run budget.
    pub async fn reserve_amendment_planning_call(
        &self,
        scope: ExecutionScope,
        request: AmendmentPlanningCallReservationRequest,
    ) -> Result<AmendmentPlanningCallReservationOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(
            "SELECT *,now() AS observed_at FROM moa.execution_run WHERE run_uid=$1 FOR UPDATE",
        )
        .bind(request.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentPlanningCallReservationOutcome::NotFound);
        };
        let run = rows::run_from_row(&row)?;
        let observed_at = row
            .try_get::<DateTime<Utc>, _>("observed_at")
            .map_err(row_error)?;
        if let Some(existing) = load_planning_call_in_conn(
            conn.as_mut(),
            request.run_uid,
            request.base_plan_revision,
            request.call_ordinal,
        )
        .await?
        {
            conn.commit().await.map_err(storage_error)?;
            if existing.reserved != request.reservation {
                return Ok(AmendmentPlanningCallReservationOutcome::Conflict);
            }
            return Ok(if existing.actual.is_some() {
                AmendmentPlanningCallReservationOutcome::AlreadySettled(existing)
            } else {
                AmendmentPlanningCallReservationOutcome::ReplayedOpen(existing)
            });
        }
        let denial = if run.plan_revision != request.base_plan_revision
            || run.status != ExecutionRunStatus::WaitingReplan
        {
            Some(AmendmentPlanningCallDenial::StaleRevision)
        } else if run.pending_terminal.is_some() {
            Some(AmendmentPlanningCallDenial::PendingTerminal)
        } else if run.budget_overrun {
            Some(AmendmentPlanningCallDenial::BudgetOverrun)
        } else if run
            .approved_budget
            .deadline_at
            .is_some_and(|deadline| deadline <= observed_at)
        {
            Some(AmendmentPlanningCallDenial::DeadlineExceeded)
        } else {
            let mut ledger = projection::budget_ledger(&run);
            ledger
                .try_reserve(ExecutionEstimate {
                    cost_microusd: request.reservation.cost_microusd,
                    tokens: request.reservation.tokens,
                    tasks: 0,
                    tool_calls: 0,
                    retrieved_bytes: 0,
                })
                .err()
                .map(|_| AmendmentPlanningCallDenial::BudgetExceeded)
        };
        if let Some(denial) = denial {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentPlanningCallReservationOutcome::Denied(denial));
        }
        let reservation_uid = amendment_planning_reservation_uid(
            request.run_uid,
            request.base_plan_revision,
            request.call_ordinal,
        );
        sqlx::query(
            "UPDATE moa.execution_run SET reserved_cost_microusd=reserved_cost_microusd+$2, \
             reserved_tokens=reserved_tokens+$3,updated_at=NOW() WHERE run_uid=$1",
        )
        .bind(request.run_uid)
        .bind(to_i64(
            request.reservation.cost_microusd,
            "planning reserved cost",
        )?)
        .bind(to_i64(
            request.reservation.tokens,
            "planning reserved tokens",
        )?)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let created_at: DateTime<Utc> = sqlx::query_scalar(
            "INSERT INTO moa.execution_amendment_planning_reservation \
             (reservation_uid,tenant_id,contact_id,run_uid,base_plan_revision,call_ordinal, \
              reserved_cost_microusd,reserved_tokens,created_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING created_at",
        )
        .bind(reservation_uid)
        .bind(run.tenant_id.0)
        .bind(run.contact_id.map(|contact| contact.0))
        .bind(run.run_uid)
        .bind(to_i64(
            request.base_plan_revision,
            "planning base revision",
        )?)
        .bind(i16::from(request.call_ordinal))
        .bind(to_i64(
            request.reservation.cost_microusd,
            "planning reserved cost",
        )?)
        .bind(to_i64(
            request.reservation.tokens,
            "planning reserved tokens",
        )?)
        .bind(request.now)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let record = AmendmentPlanningCallRecord {
            reservation_uid,
            tenant_id: run.tenant_id,
            contact_id: run.contact_id,
            run_uid: run.run_uid,
            base_plan_revision: request.base_plan_revision,
            call_ordinal: request.call_ordinal,
            reserved: request.reservation,
            actual: None,
            budget_overrun: false,
            created_at,
            settled_at: None,
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(AmendmentPlanningCallReservationOutcome::Granted(record))
    }

    /// Reconciles one exact amendment-planner call and releases its reservation exactly once.
    pub async fn reconcile_amendment_planning_call(
        &self,
        scope: ExecutionScope,
        request: AmendmentPlanningCallReconcileRequest,
    ) -> Result<AmendmentPlanningCallReconcileOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) =
            sqlx::query("SELECT * FROM moa.execution_run WHERE run_uid=$1 FOR UPDATE")
                .bind(request.run_uid)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentPlanningCallReconcileOutcome::NotFound);
        };
        let run = rows::run_from_row(&run_row)?;
        let Some(record) = load_planning_call_in_conn(
            conn.as_mut(),
            request.run_uid,
            request.base_plan_revision,
            request.call_ordinal,
        )
        .await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentPlanningCallReconcileOutcome::NotFound);
        };
        if let Some(actual) = &record.actual {
            conn.commit().await.map_err(storage_error)?;
            return Ok(if actual == &request.actual {
                AmendmentPlanningCallReconcileOutcome::Replayed(record)
            } else {
                AmendmentPlanningCallReconcileOutcome::Conflict
            });
        }
        let ceiling = i64::MAX as u64;
        let next_cost = run
            .consumed
            .cost_microusd
            .saturating_add(request.actual.cost_microusd)
            .min(ceiling);
        let next_tokens = run
            .consumed
            .tokens
            .saturating_add(request.actual.tokens)
            .min(ceiling);
        let budget_overrun = run.budget_overrun
            || request.actual.cost_microusd > record.reserved.cost_microusd
            || request.actual.tokens > record.reserved.tokens
            || run
                .approved_budget
                .max_cost_microusd
                .is_some_and(|limit| next_cost > limit)
            || run
                .approved_budget
                .max_tokens
                .is_some_and(|limit| next_tokens > limit)
            || run.consumed.cost_microusd > ceiling - request.actual.cost_microusd.min(ceiling)
            || run.consumed.tokens > ceiling - request.actual.tokens.min(ceiling);
        let remaining_cost = run
            .reserved
            .cost_microusd
            .checked_sub(record.reserved.cost_microusd)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "amendment planning cost reservation is absent from run ledger"
                    .to_string(),
            })?;
        let remaining_tokens = run
            .reserved
            .tokens
            .checked_sub(record.reserved.tokens)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "amendment planning token reservation is absent from run ledger"
                    .to_string(),
            })?;
        sqlx::query(
            "UPDATE moa.execution_run SET reserved_cost_microusd=$2,reserved_tokens=$3, \
             consumed_cost_microusd=$4,consumed_tokens=$5,budget_overrun=$6,updated_at=NOW() \
             WHERE run_uid=$1",
        )
        .bind(run.run_uid)
        .bind(to_i64(
            remaining_cost,
            "remaining planning cost reservation",
        )?)
        .bind(to_i64(
            remaining_tokens,
            "remaining planning token reservation",
        )?)
        .bind(to_i64(next_cost, "planning consumed cost")?)
        .bind(to_i64(next_tokens, "planning consumed tokens")?)
        .bind(budget_overrun)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let settlement_uid = Uuid::new_v5(
            &AMENDMENT_PLANNING_SETTLEMENT_NAMESPACE,
            record.reservation_uid.as_bytes(),
        );
        let settled_at: DateTime<Utc> = sqlx::query_scalar(
            "INSERT INTO moa.execution_amendment_planning_settlement \
             (settlement_uid,reservation_uid,tenant_id,contact_id,run_uid, \
              actual_cost_microusd,actual_tokens,budget_overrun,settled_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING settled_at",
        )
        .bind(settlement_uid)
        .bind(record.reservation_uid)
        .bind(record.tenant_id.0)
        .bind(record.contact_id.map(|contact| contact.0))
        .bind(record.run_uid)
        .bind(to_i64(
            request.actual.cost_microusd,
            "planning actual cost",
        )?)
        .bind(to_i64(request.actual.tokens, "planning actual tokens")?)
        .bind(budget_overrun)
        .bind(request.settled_at)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let settled = AmendmentPlanningCallRecord {
            actual: Some(request.actual),
            budget_overrun,
            settled_at: Some(settled_at),
            ..record
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(AmendmentPlanningCallReconcileOutcome::Applied(settled))
    }

    /// Loads one visible amendment-planner call attribution record.
    pub async fn load_amendment_planning_call(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        base_plan_revision: u64,
        call_ordinal: u8,
    ) -> Result<Option<AmendmentPlanningCallRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let record =
            load_planning_call_in_conn(conn.as_mut(), run_uid, base_plan_revision, call_ordinal)
                .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }
}

async fn load_planning_call_in_conn(
    conn: &mut PgConnection,
    run_uid: Uuid,
    base_plan_revision: u64,
    call_ordinal: u8,
) -> Result<Option<AmendmentPlanningCallRecord>> {
    let row = sqlx::query(
        "SELECT reservation.*,settlement.actual_cost_microusd,settlement.actual_tokens, \
                settlement.budget_overrun,settlement.settled_at \
         FROM moa.execution_amendment_planning_reservation AS reservation \
         LEFT JOIN moa.execution_amendment_planning_settlement AS settlement \
           ON settlement.reservation_uid=reservation.reservation_uid \
         WHERE reservation.run_uid=$1 AND reservation.base_plan_revision=$2 \
           AND reservation.call_ordinal=$3",
    )
    .bind(run_uid)
    .bind(to_i64(base_plan_revision, "planning base revision")?)
    .bind(i16::from(call_ordinal))
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    row.as_ref().map(planning_call_from_row).transpose()
}

fn planning_call_from_row(row: &PgRow) -> Result<AmendmentPlanningCallRecord> {
    let actual_cost = rows::optional_u64(row, "actual_cost_microusd")?;
    let actual_tokens = rows::optional_u64(row, "actual_tokens")?;
    if actual_cost.is_some() != actual_tokens.is_some() {
        return Err(Error::InvalidRepositoryData {
            message: "amendment planning settlement has partial actual usage".to_string(),
        });
    }
    let actual = actual_cost
        .zip(actual_tokens)
        .map(|(cost_microusd, tokens)| PlanningUsage {
            cost_microusd,
            tokens,
        });
    Ok(AmendmentPlanningCallRecord {
        reservation_uid: row.try_get("reservation_uid").map_err(row_error)?,
        tenant_id: TenantId(row.try_get("tenant_id").map_err(row_error)?),
        contact_id: row
            .try_get::<Option<Uuid>, _>("contact_id")
            .map_err(row_error)?
            .map(ContactId),
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        base_plan_revision: rows::required_u64(row, "base_plan_revision")?,
        call_ordinal: u8::try_from(row.try_get::<i16, _>("call_ordinal").map_err(row_error)?)
            .map_err(|_| Error::InvalidRepositoryData {
                message: "amendment planning call ordinal exceeds u8".to_string(),
            })?,
        reserved: AmendmentPlanningCallReservation {
            cost_microusd: rows::required_u64(row, "reserved_cost_microusd")?,
            tokens: rows::required_u64(row, "reserved_tokens")?,
        },
        actual,
        budget_overrun: row
            .try_get::<Option<bool>, _>("budget_overrun")
            .map_err(row_error)?
            .unwrap_or(false),
        created_at: row.try_get("created_at").map_err(row_error)?,
        settled_at: row.try_get("settled_at").map_err(row_error)?,
    })
}

fn amendment_planning_reservation_uid(run_uid: Uuid, revision: u64, ordinal: u8) -> Uuid {
    Uuid::new_v5(
        &AMENDMENT_PLANNING_RESERVATION_NAMESPACE,
        format!("{run_uid}:{revision}:{ordinal}").as_bytes(),
    )
}
