//! Post-experiment durability invariant checks.
//!
//! Chaos experiments and e2e suites run these against the session event log
//! and the authz outbox after injecting faults. Every check reads Postgres
//! directly (never traces, which Restate replay suppresses) and returns
//! violations instead of panicking so callers can assert or report.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::Row as _;
use uuid::Uuid;

/// What a violated invariant means, by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvariantKind {
    /// A session sat in `running` past the stuck deadline.
    StuckSession,
    /// A session's event log has a hole in `sequence_num`.
    EventSequenceGap,
    /// A session persisted two events with the same `sequence_num`.
    DuplicateEventSequence,
    /// The same provider tool-use id was persisted more than once.
    DuplicateToolCall,
    /// The authz outbox still has pending/in-flight rows after the drain
    /// deadline.
    OutboxBacklog,
    /// The authz outbox dead-lettered rows during the experiment.
    OutboxDeadLetter,
}

/// One violated invariant with human-readable evidence.
#[derive(Debug, Clone)]
pub struct InvariantViolation {
    /// Violated invariant kind.
    pub kind: InvariantKind,
    /// Evidence: identifiers and counts for debugging.
    pub detail: String,
}

/// Scope one invariant sweep to an experiment's tenants.
#[derive(Debug, Clone)]
pub struct InvariantScope {
    /// Tenants created by the experiment. Empty means "no tenant filter",
    /// which is only safe against a dedicated/ephemeral database.
    pub tenant_ids: Vec<Uuid>,
    /// How long a session may sit in `running` before it counts as stuck.
    pub stuck_after: Duration,
}

/// Errors raised while sweeping invariants.
#[derive(Debug, thiserror::Error)]
pub enum InvariantError {
    /// Underlying database failure.
    #[error("invariant query failed: {0}")]
    Db(#[from] sqlx::Error),
}

/// Runs every durability invariant and returns the violations found.
pub async fn check_invariants(
    pool: &PgPool,
    scope: &InvariantScope,
) -> Result<Vec<InvariantViolation>, InvariantError> {
    let mut violations = Vec::new();
    stuck_sessions(pool, scope, &mut violations).await?;
    event_sequence_integrity(pool, scope, &mut violations).await?;
    duplicate_tool_calls(pool, scope, &mut violations).await?;
    outbox_state(pool, &mut violations).await?;
    Ok(violations)
}

fn tenant_filter(scope: &InvariantScope) -> (&'static str, Vec<Uuid>) {
    if scope.tenant_ids.is_empty() {
        ("TRUE", Vec::new())
    } else {
        ("s.tenant_id = ANY($1)", scope.tenant_ids.clone())
    }
}

async fn stuck_sessions(
    pool: &PgPool,
    scope: &InvariantScope,
    violations: &mut Vec<InvariantViolation>,
) -> Result<(), InvariantError> {
    let (filter, tenants) = tenant_filter(scope);
    let query = format!(
        "SELECT s.id::text AS id, s.updated_at::text AS updated_at \
         FROM sessions s \
         WHERE {filter} AND s.status = 'running' \
           AND s.updated_at < now() - make_interval(secs => $2)"
    );
    let rows = sqlx::query(&query)
        .bind(&tenants)
        .bind(scope.stuck_after.as_secs_f64())
        .fetch_all(pool)
        .await?;
    for row in rows {
        let id: String = row.get("id");
        let updated_at: String = row.get("updated_at");
        violations.push(InvariantViolation {
            kind: InvariantKind::StuckSession,
            detail: format!("session {id} still running; last update {updated_at}"),
        });
    }
    Ok(())
}

async fn event_sequence_integrity(
    pool: &PgPool,
    scope: &InvariantScope,
    violations: &mut Vec<InvariantViolation>,
) -> Result<(), InvariantError> {
    let (filter, tenants) = tenant_filter(scope);
    let query = format!(
        "SELECT e.session_id::text AS session_id, \
                count(*) AS total, \
                count(DISTINCT e.sequence_num) AS distinct_seq, \
                min(e.sequence_num) AS min_seq, \
                max(e.sequence_num) AS max_seq \
         FROM events e JOIN sessions s ON s.id = e.session_id \
         WHERE {filter} \
         GROUP BY e.session_id \
         HAVING count(*) <> max(e.sequence_num) - min(e.sequence_num) + 1 \
             OR count(*) <> count(DISTINCT e.sequence_num)"
    );
    let rows = sqlx::query(&query).bind(&tenants).fetch_all(pool).await?;
    for row in rows {
        let session_id: String = row.get("session_id");
        let total: i64 = row.get("total");
        let distinct_seq: i64 = row.get("distinct_seq");
        let min_seq: i64 = row.get("min_seq");
        let max_seq: i64 = row.get("max_seq");
        let kind = if total != distinct_seq {
            InvariantKind::DuplicateEventSequence
        } else {
            InvariantKind::EventSequenceGap
        };
        violations.push(InvariantViolation {
            kind,
            detail: format!(
                "session {session_id}: {total} events over sequence [{min_seq},{max_seq}], \
                 {distinct_seq} distinct"
            ),
        });
    }
    Ok(())
}

async fn duplicate_tool_calls(
    pool: &PgPool,
    scope: &InvariantScope,
    violations: &mut Vec<InvariantViolation>,
) -> Result<(), InvariantError> {
    let (filter, tenants) = tenant_filter(scope);
    // Tolerate payload-shape drift: any of the known id locations counts, and
    // rows without a tool-use id are ignored rather than false-positived.
    let query = format!(
        "SELECT e.session_id::text AS session_id, tool_use_id, count(*) AS uses \
         FROM ( \
             SELECT e.session_id, \
                    COALESCE(e.payload->'invocation'->>'id', \
                             e.payload->>'tool_use_id', \
                             e.payload->>'id') AS tool_use_id \
             FROM events e JOIN sessions s ON s.id = e.session_id \
             WHERE {filter} AND e.event_type = 'tool_call' \
         ) e \
         WHERE tool_use_id IS NOT NULL \
         GROUP BY e.session_id, tool_use_id \
         HAVING count(*) > 1"
    );
    let rows = sqlx::query(&query).bind(&tenants).fetch_all(pool).await?;
    for row in rows {
        let session_id: String = row.get("session_id");
        let tool_use_id: String = row.get("tool_use_id");
        let uses: i64 = row.get("uses");
        violations.push(InvariantViolation {
            kind: InvariantKind::DuplicateToolCall,
            detail: format!("session {session_id}: tool-use id {tool_use_id} persisted {uses}x"),
        });
    }
    Ok(())
}

async fn outbox_state(
    pool: &PgPool,
    violations: &mut Vec<InvariantViolation>,
) -> Result<(), InvariantError> {
    let backlog: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM authz_outbox WHERE status IN ('pending', 'in_flight')",
    )
    .fetch_one(pool)
    .await?;
    if backlog > 0 {
        violations.push(InvariantViolation {
            kind: InvariantKind::OutboxBacklog,
            detail: format!("{backlog} authz outbox rows still pending/in-flight"),
        });
    }
    let dead: i64 =
        sqlx::query_scalar("SELECT count(*) FROM authz_outbox WHERE status = 'dead_letter'")
            .fetch_one(pool)
            .await?;
    if dead > 0 {
        violations.push(InvariantViolation {
            kind: InvariantKind::OutboxDeadLetter,
            detail: format!("{dead} authz outbox rows dead-lettered"),
        });
    }
    Ok(())
}
