//! Restate workflow that runs one tenant's terminal-session retention pass.
//!
//! The pass owns scheduling and reporting only. Every rule that decides whether
//! a session's live history may be deleted — terminal status, the retention
//! boundary, legal hold, an in-flight erasure, and the archive's own integrity —
//! is enforced inside `moa-session`'s archival transaction, under the session
//! row lock and the destruction advisory lock. Enforcing them here as well
//! would create a second copy that can drift, and would make neither copy
//! falsifiable on its own.
//!
//! What this workflow adds is durability and bounded blast radius: each session
//! is archived behind its own journaled step, so a crashed pass resumes without
//! redoing committed work, and a pass is capped at a caller-supplied number of
//! sessions.

use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use moa_core::types::identifiers::{SessionId, TenantId};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_session::PostgresSessionStore;
use moa_session::archive::{ArchiveOutcome, ArchiveRefusal};

use restate_sdk::prelude::*;
use std::sync::Arc;

/// Smallest retention window a pass will accept, in days.
///
/// Zero would mean "archive a conversation the moment it ends", which turns a
/// misconfigured schedule into an immediate mass delete of history users are
/// still looking at. There is no operational need for it: a session that ended
/// seconds ago costs nothing to keep, and the whole point of retention is the
/// long tail.
pub const MIN_RETENTION_DAYS: u32 = 1;

/// Returns the durable workflow ID for one tenant/date retention pass.
#[must_use]
pub fn session_retention_workflow_id(tenant_id: &TenantId, target_date: NaiveDate) -> String {
    format!("{tenant_id}:{target_date}")
}

/// Workflow input for one tenant retention pass.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRetentionRequest {
    /// Tenant whose terminal sessions should be archived.
    pub tenant_id: TenantId,
    /// Days a terminal session's history stays in the live event table.
    pub retain_terminal_sessions_for_days: u32,
    /// Maximum number of sessions this pass may archive.
    pub max_sessions: u32,
}

/// One session a pass declined to archive, and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRetentionRefusal {
    /// Session that was left untouched.
    pub session_id: SessionId,
    /// Reason the archival transaction refused it.
    pub refusal: ArchiveRefusal,
}

/// Serializable outcome of one retention pass.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionRetentionReport {
    /// Retention boundary this pass was evaluated against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<DateTime<Utc>>,
    /// Number of candidate sessions the scan offered.
    pub candidates_scanned: u64,
    /// Number of sessions archived.
    pub sessions_archived: u64,
    /// Number of events moved into the archive.
    pub events_archived: u64,
    /// Number of sessions already archived by an earlier pass.
    pub already_archived: u64,
    /// Sessions the archival transaction refused, in scan order.
    #[serde(default)]
    pub refusals: Vec<SessionRetentionRefusal>,
}

/// Durable dispatch handle for one retention pass.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRetentionDispatch {
    /// Durable workflow instance the pass runs under.
    pub workflow_id: String,
    /// Logical UTC date the pass owns.
    pub target_date: NaiveDate,
}

/// Restate workflow surface for one-shot tenant retention passes.
#[restate_sdk::workflow]
pub trait SessionRetention {
    /// Runs one durable terminal-session retention pass.
    async fn run(
        request: Json<SessionRetentionRequest>,
    ) -> Result<Json<SessionRetentionReport>, HandlerError>;
}

/// Concrete workflow implementation.
#[derive(Clone)]
pub struct SessionRetentionImpl {
    store: Arc<PostgresSessionStore>,
}

impl SessionRetentionImpl {
    /// Creates a retention workflow backed by the shared session store.
    #[must_use]
    pub fn new(store: Arc<PostgresSessionStore>) -> Self {
        Self { store }
    }
}

impl SessionRetention for SessionRetentionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: the dispatching `SessionStore/start_session_retention` handler
    // authorizes tenant admin on the target tenant before this workflow is ever
    // submitted, and the archival transaction re-checks every preservation rule
    // itself. This handler carries no caller-supplied scope beyond the tenant it
    // was dispatched for.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<SessionRetentionRequest>,
    ) -> Result<Json<SessionRetentionReport>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SessionRetention", "run");
        let mut steps = RestateSessionRetentionSteps {
            ctx: &ctx,
            store: self.store.clone(),
        };
        let report = run_session_retention_workflow(&mut steps, request.into_inner()).await?;
        Ok(Json::from(report))
    }
}

/// Durable operations used by the retention workflow body.
#[async_trait]
pub trait SessionRetentionSteps {
    /// Captures the pass timestamp behind a journaled durable step.
    async fn capture_now(&mut self) -> Result<DateTime<Utc>, HandlerError>;

    /// Lists sessions eligible for archival at `boundary`.
    async fn list_candidates(
        &mut self,
        tenant_id: TenantId,
        boundary: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<SessionId>, HandlerError>;

    /// Archives one session, returning what the archival transaction decided.
    async fn archive_session(
        &mut self,
        session_id: SessionId,
        boundary: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<SessionArchiveOutcome, HandlerError>;
}

/// Runs one retention pass against durable steps.
///
/// The retention boundary is derived once, from the timestamp the pass captured
/// as its first durable step, and the same boundary is passed to the scan and to
/// every archival call. A replayed pass therefore re-derives the boundary it
/// originally used instead of drifting forward with the wall clock, and a
/// session cannot become eligible partway through a pass.
pub async fn run_session_retention_workflow<S: SessionRetentionSteps + Send>(
    steps: &mut S,
    request: SessionRetentionRequest,
) -> Result<SessionRetentionReport, HandlerError> {
    if request.retain_terminal_sessions_for_days < MIN_RETENTION_DAYS {
        return Err(HandlerError::from(TerminalError::new(format!(
            "session retention window of {} days is below the {MIN_RETENTION_DAYS}-day minimum; \
             refusing to archive history that has just been written",
            request.retain_terminal_sessions_for_days
        ))));
    }
    if request.max_sessions == 0 {
        return Ok(SessionRetentionReport::default());
    }

    let now = steps.capture_now().await?;
    let boundary = now - Duration::days(i64::from(request.retain_terminal_sessions_for_days));
    let candidates = steps
        .list_candidates(request.tenant_id, boundary, i64::from(request.max_sessions))
        .await?;

    let mut report = SessionRetentionReport {
        boundary: Some(boundary),
        candidates_scanned: candidates.len() as u64,
        ..SessionRetentionReport::default()
    };
    for session_id in candidates {
        match steps.archive_session(session_id, boundary, now).await? {
            SessionArchiveOutcome::Archived { event_count } => {
                report.sessions_archived += 1;
                report.events_archived += event_count.max(0) as u64;
            }
            SessionArchiveOutcome::AlreadyArchived => report.already_archived += 1,
            SessionArchiveOutcome::Refused(refusal) => {
                report.refusals.push(SessionRetentionRefusal {
                    session_id,
                    refusal,
                });
            }
        }
    }
    Ok(report)
}

/// Restate-backed durable steps.
struct RestateSessionRetentionSteps<'a, 'ctx> {
    ctx: &'a WorkflowContext<'ctx>,
    store: Arc<PostgresSessionStore>,
}

#[async_trait]
impl SessionRetentionSteps for RestateSessionRetentionSteps<'_, '_> {
    async fn capture_now(&mut self) -> Result<DateTime<Utc>, HandlerError> {
        self.ctx
            .run(|| async move { Ok(Json::from(Utc::now())) })
            .name("session_retention_now")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn list_candidates(
        &mut self,
        tenant_id: TenantId,
        boundary: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<SessionId>, HandlerError> {
        let store = self.store.clone();
        let candidates = self
            .ctx
            .run(|| async move {
                store
                    .list_session_archival_candidates(tenant_id, boundary, limit)
                    .await
                    .map(Json::from)
                    .map_err(retention_store_error)
            })
            .name("list_session_retention_candidates")
            .await?;
        Ok(candidates.into_inner())
    }

    async fn archive_session(
        &mut self,
        session_id: SessionId,
        boundary: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<SessionArchiveOutcome, HandlerError> {
        let store = self.store.clone();
        // The archival transaction is the durable unit: it either commits the
        // archive together with the delete, or leaves the session untouched.
        // Journaling it per session means a crashed pass resumes at the next
        // session rather than re-running the whole scan, and a replay that does
        // re-run one sees `AlreadyArchived`.
        let outcome = self
            .ctx
            .run(|| async move {
                store
                    .archive_terminal_session(session_id, boundary, now)
                    .await
                    .map(|outcome| Json::from(SessionArchiveOutcome::from(outcome)))
                    .map_err(retention_store_error)
            })
            .name("archive_terminal_session")
            .await?;
        Ok(outcome.into_inner())
    }
}

/// Keeps transient retention storage failures retryable while rejecting invalid
/// requests and impossible identities without replaying them forever.
fn retention_store_error(error: moa_core::error::MoaError) -> HandlerError {
    match error {
        moa_core::error::MoaError::ValidationError(_)
        | moa_core::error::MoaError::SessionNotFound(_)
        | moa_core::error::MoaError::SerializationError(_)
        | moa_core::error::MoaError::SerdeJson(_)
        | moa_core::error::MoaError::Uuid(_) => TerminalError::new(error.to_string()).into(),
        other => HandlerError::from(other),
    }
}

/// What one session's archival attempt decided, as a pass records it.
///
/// This is the journal-safe projection of [`ArchiveOutcome`]: the archived
/// bytes, their digest, and the session's identity stay in the database and are
/// never copied into a workflow journal. Only the event count travels, because
/// that is the one number a retention report is about.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SessionArchiveOutcome {
    /// History was archived and the live rows were deleted.
    Archived {
        /// Number of events moved into the archive.
        event_count: i64,
    },
    /// The session was already archived by an earlier pass.
    AlreadyArchived,
    /// The archival transaction refused the session.
    Refused(ArchiveRefusal),
}

impl From<ArchiveOutcome> for SessionArchiveOutcome {
    fn from(outcome: ArchiveOutcome) -> Self {
        match outcome {
            ArchiveOutcome::Archived(archive) => Self::Archived {
                event_count: archive.event_count,
            },
            ArchiveOutcome::AlreadyArchived => Self::AlreadyArchived,
            ArchiveOutcome::Refused(refusal) => Self::Refused(refusal),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeSteps {
        now: DateTime<Utc>,
        candidates: Vec<SessionId>,
        outcomes: HashMap<SessionId, SessionArchiveOutcome>,
        observed_boundaries: Vec<DateTime<Utc>>,
        observed_limit: Option<i64>,
        scans: u32,
    }

    impl FakeSteps {
        fn new(now: DateTime<Utc>, candidates: Vec<SessionId>) -> Self {
            Self {
                now,
                candidates,
                outcomes: HashMap::new(),
                observed_boundaries: Vec::new(),
                observed_limit: None,
                scans: 0,
            }
        }

        fn with_outcome(mut self, session_id: SessionId, outcome: SessionArchiveOutcome) -> Self {
            self.outcomes.insert(session_id, outcome);
            self
        }
    }

    fn archived(event_count: i64) -> SessionArchiveOutcome {
        SessionArchiveOutcome::Archived { event_count }
    }

    #[async_trait]
    impl SessionRetentionSteps for FakeSteps {
        async fn capture_now(&mut self) -> Result<DateTime<Utc>, HandlerError> {
            Ok(self.now)
        }

        async fn list_candidates(
            &mut self,
            _tenant_id: TenantId,
            boundary: DateTime<Utc>,
            limit: i64,
        ) -> Result<Vec<SessionId>, HandlerError> {
            self.scans += 1;
            self.observed_boundaries.push(boundary);
            self.observed_limit = Some(limit);
            Ok(self.candidates.clone())
        }

        async fn archive_session(
            &mut self,
            session_id: SessionId,
            boundary: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Result<SessionArchiveOutcome, HandlerError> {
            self.observed_boundaries.push(boundary);
            Ok(self
                .outcomes
                .remove(&session_id)
                .unwrap_or_else(|| archived(2)))
        }
    }

    fn session(index: u128) -> SessionId {
        SessionId(uuid::Uuid::from_u128(index))
    }

    fn request(days: u32, max_sessions: u32) -> SessionRetentionRequest {
        SessionRetentionRequest {
            tenant_id: TenantId::from(uuid::Uuid::from_u128(99)),
            retain_terminal_sessions_for_days: days,
            max_sessions,
        }
    }

    // Pins: database and integrity failures remain retryable Restate failures,
    // while invalid input is terminal. Retention must recover from a transient
    // database outage without replaying a permanently invalid request forever.
    #[test]
    fn retention_store_errors_preserve_retryability_offline() {
        let retryable = retention_store_error(moa_core::error::MoaError::StorageError(
            "database unavailable".to_string(),
        ));
        let terminal = retention_store_error(moa_core::error::MoaError::ValidationError(
            "invalid retention request".to_string(),
        ));

        assert!(
            format!("{retryable:?}").contains("Retryable"),
            "storage failures must remain retryable, observed {retryable:?}"
        );
        assert!(
            format!("{terminal:?}").contains("Terminal"),
            "validation failures must be terminal, observed {terminal:?}"
        );
    }

    // Pins: the retention boundary is the captured pass timestamp minus the
    // configured window, and the SAME boundary reaches the scan and every
    // archival call. A pass that re-read the clock per session could archive a
    // session that was not eligible when the pass began.
    #[tokio::test]
    async fn the_boundary_is_derived_once_from_the_captured_clock_offline() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).expect("representable pass timestamp");
        let mut steps = FakeSteps::new(now, vec![session(1), session(2)]);
        let report = run_session_retention_workflow(&mut steps, request(30, 10))
            .await
            .expect("retention pass should run");

        let expected = now - Duration::days(30);
        assert_eq!(
            report.boundary,
            Some(expected),
            "the report must carry the boundary the pass used"
        );
        assert_eq!(
            steps.observed_boundaries,
            vec![expected; 3],
            "the scan and both archival calls must all see the same boundary"
        );
        assert_eq!(
            steps.observed_limit,
            Some(10),
            "the pass must cap the scan at the requested session count"
        );
        assert_eq!(
            (report.sessions_archived, report.events_archived),
            (2, 4),
            "both candidates must be archived and their events counted, observed {report:?}"
        );
    }

    // Pins: a zero-day retention window is refused outright and the pass never
    // reaches the scan. A schedule misconfigured to zero would otherwise delete
    // the live history of every session the moment it ended.
    #[tokio::test]
    async fn a_zero_day_retention_window_is_refused_before_any_scan_offline() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).expect("representable pass timestamp");
        let mut steps = FakeSteps::new(now, vec![session(1)]);
        let error = run_session_retention_workflow(&mut steps, request(0, 10))
            .await
            .expect_err("a zero-day retention window must be refused");
        let rendered = format!("{error:?}");
        assert!(
            rendered.contains("below the"),
            "refusal must name the minimum window, observed: {rendered}"
        );
        assert_eq!(
            steps.scans, 0,
            "a refused pass must not scan for candidates"
        );
    }

    // Pins: a refusal from the archival transaction is reported and the pass
    // continues. One held session must not stop retention for the tenant, and a
    // refusal must never be silently counted as an archive.
    #[tokio::test]
    async fn a_refused_session_is_reported_and_does_not_stop_the_pass_offline() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).expect("representable pass timestamp");
        let mut steps = FakeSteps::new(now, vec![session(1), session(2), session(3)])
            .with_outcome(
                session(2),
                SessionArchiveOutcome::Refused(ArchiveRefusal::LegalHold),
            )
            .with_outcome(session(3), SessionArchiveOutcome::AlreadyArchived);
        let report = run_session_retention_workflow(&mut steps, request(7, 10))
            .await
            .expect("retention pass should run");

        assert_eq!(
            report.sessions_archived, 1,
            "only the unrefused session may be counted as archived, observed {report:?}"
        );
        assert_eq!(
            report.already_archived, 1,
            "an already-archived session must be reported separately, observed {report:?}"
        );
        assert_eq!(
            report.refusals,
            vec![SessionRetentionRefusal {
                session_id: session(2),
                refusal: ArchiveRefusal::LegalHold,
            }],
            "the refusal must name the session and the reason, observed {report:?}"
        );
    }

    // Pins: a pass capped at zero sessions does nothing at all, including no
    // scan. This is the operator's off switch for a scheduled pass.
    #[tokio::test]
    async fn a_pass_capped_at_zero_sessions_does_nothing_offline() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).expect("representable pass timestamp");
        let mut steps = FakeSteps::new(now, vec![session(1)]);
        let report = run_session_retention_workflow(&mut steps, request(30, 0))
            .await
            .expect("a capped-at-zero pass should return an empty report");
        assert_eq!(
            report,
            SessionRetentionReport::default(),
            "a capped-at-zero pass must report nothing, observed {report:?}"
        );
        assert_eq!(steps.scans, 0, "a capped-at-zero pass must not scan");
    }
}
