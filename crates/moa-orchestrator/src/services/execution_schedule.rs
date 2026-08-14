//! Authenticated tenant control surface for recurring durable executions.

use chrono::{DateTime, Duration, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;
use moa_authz_schema::Relation;
use moa_config::ExecutionConfig;
use moa_core::types::execution_planning::{
    ExecutionScheduleCreateRequest, ExecutionScheduleDstPolicy, ExecutionScheduleListRequest,
    ExecutionScheduleMissedFirePolicy, ExecutionScheduleOriginSource, ExecutionSchedulePage,
    ExecutionSchedulePolicy, ExecutionScheduleRecord, ExecutionScheduleRequest,
    ExecutionScheduleUpdateRequest,
};
use moa_execution::repository::{
    ExecutionRepository, ExecutionScope,
    schedule::{
        ExecutionScheduleCreateOutcome, ExecutionScheduleMutationOutcome,
        ExecutionScheduleOccurrence, ExecutionScheduleRunAdmission,
        ExecutionScheduleRunAdmissionOutcome, execution_schedule_run_blueprint,
    },
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    handlers::authz_shim::AuthzEnforcer,
    services::execution_dispatcher::{DispatchExecutionsRequest, ExecutionDispatcherClient},
    workflows::errors::execution_error_to_handler_error,
};

/// Restate service surface for tenant recurring execution schedules.
#[restate_sdk::service]
#[name = "ExecutionSchedule"]
pub trait ExecutionSchedule {
    /// Creates one immutable pinned schedule and arms its first occurrence.
    async fn create(
        request: Json<ExecutionScheduleCreateRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError>;

    /// Loads one schedule status after tenant authorization.
    async fn status(
        request: Json<ExecutionScheduleRequest>,
    ) -> Result<Json<Option<ExecutionScheduleRecord>>, HandlerError>;

    /// Lists one bounded stable page of tenant schedules.
    async fn list(
        request: Json<ExecutionScheduleListRequest>,
    ) -> Result<Json<ExecutionSchedulePage>, HandlerError>;

    /// Replaces mutable timing/resource policy behind an incarnation fence.
    async fn update(
        request: Json<ExecutionScheduleUpdateRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError>;

    /// Pauses future occurrences without changing immutable inputs.
    async fn pause(
        request: Json<ExecutionScheduleRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError>;

    /// Resumes a paused schedule using its persisted missed-fire policy.
    async fn resume(
        request: Json<ExecutionScheduleRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError>;

    /// Permanently fences future occurrences while retaining audit state.
    async fn cancel(
        request: Json<ExecutionScheduleRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError>;

    /// Consumes one exact persisted schedule-occurrence trigger.
    async fn fire_occurrence(
        request: Json<crate::runtime::execution_dispatch::ExecutionTriggerDeliveryRequest>,
    ) -> Result<Json<ExecutionScheduleFireResponse>, HandlerError>;
}

/// Durable disposition of one trusted schedule-occurrence delivery.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExecutionScheduleFireResponse {
    /// A fresh run and its initial controller activation committed.
    Admitted {
        /// Deterministic fresh occurrence run.
        run_uid: Uuid,
        /// Initial controller activation outbox identity.
        activation_dispatch_uid: Uuid,
    },
    /// Overlap policy consumed the occurrence without creating a run.
    Skipped,
    /// The same deterministic occurrence already committed.
    Replayed {
        /// Existing run when the occurrence was admitted rather than skipped.
        run_uid: Option<Uuid>,
        /// Existing activation outbox when a run was admitted.
        activation_dispatch_uid: Option<Uuid>,
    },
    /// A pause, cancellation, or newer incarnation fenced the trigger.
    Stale,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct JournaledScheduleWrite {
    record: ExecutionScheduleRecord,
    dispatcher_kick_uid: Option<Uuid>,
}

/// Concrete authenticated recurring-execution schedule service.
#[derive(Clone)]
pub struct ExecutionScheduleImpl {
    repository: ExecutionRepository,
    authz: AuthzEnforcer,
    config: ExecutionConfig,
}

impl ExecutionScheduleImpl {
    /// Creates the service over the shared runtime Postgres pool.
    #[must_use]
    pub(crate) fn new(pool: PgPool, authz: AuthzEnforcer, config: ExecutionConfig) -> Self {
        Self {
            repository: ExecutionRepository::new(pool),
            authz,
            config,
        }
    }
}

impl ExecutionSchedule for ExecutionScheduleImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn create(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionScheduleCreateRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError> {
        prepare_handler(&ctx, "create");
        let request = request.into_inner();
        let identity = self
            .authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        if request.origin.created_by != identity || request.run_as_identity != identity {
            return Err(TerminalError::new_with_code(
                403,
                "schedule creator and run_as_identity must equal the authorized operator",
            )
            .into());
        }
        if request.origin.source != ExecutionScheduleOriginSource::TenantApi {
            return Err(TerminalError::new_with_code(
                400,
                "ExecutionSchedule create accepts only tenant_api origin; session origins require the session-owned admission path",
            )
            .into());
        }
        let repository = self.repository.clone();
        let config = self.config.clone();
        let write = ctx
            .run(|| async move {
                let first = next_occurrence(&request.policy, Utc::now(), true)?;
                let tenant_id = request.tenant_id;
                match repository
                    .create_schedule(
                        ExecutionScope::Tenant { tenant_id },
                        &config,
                        request,
                        first,
                    )
                    .await
                    .map_err(execution_error_to_handler_error)?
                {
                    ExecutionScheduleCreateOutcome::Created { schedule, trigger } => {
                        Ok(Json::from(JournaledScheduleWrite {
                            record: *schedule,
                            dispatcher_kick_uid: trigger.map(|write| write.dispatch.dispatch_uid),
                        }))
                    }
                    ExecutionScheduleCreateOutcome::Replayed(schedule) => {
                        Ok(Json::from(JournaledScheduleWrite {
                            record: *schedule,
                            dispatcher_kick_uid: None,
                        }))
                    }
                    ExecutionScheduleCreateOutcome::Conflict => Err(TerminalError::new_with_code(
                        409,
                        "schedule_uid is already bound to different immutable inputs",
                    )
                    .into()),
                }
            })
            .name("execution_schedule_create")
            .await?
            .into_inner();
        kick_new_schedule_trigger(&ctx, write.dispatcher_kick_uid).await?;
        Ok(Json::from(write.record))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn status(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionScheduleRequest>,
    ) -> Result<Json<Option<ExecutionScheduleRecord>>, HandlerError> {
        prepare_handler(&ctx, "status");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let repository = self.repository.clone();
        Ok(ctx
            .run(|| async move {
                repository
                    .load_schedule(
                        ExecutionScope::Tenant {
                            tenant_id: request.tenant_id,
                        },
                        request.tenant_id,
                        request.schedule_uid,
                    )
                    .await
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_schedule_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionScheduleListRequest>,
    ) -> Result<Json<ExecutionSchedulePage>, HandlerError> {
        prepare_handler(&ctx, "list");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let repository = self.repository.clone();
        Ok(ctx
            .run(|| async move {
                repository
                    .list_schedules(
                        ExecutionScope::Tenant {
                            tenant_id: request.tenant_id,
                        },
                        request.tenant_id,
                        request.limit,
                        request.cursor,
                    )
                    .await
                    .map(Json::from)
                    .map_err(execution_error_to_handler_error)
            })
            .name("execution_schedule_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn update(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionScheduleUpdateRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError> {
        prepare_handler(&ctx, "update");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let repository = self.repository.clone();
        let config = self.config.clone();
        let write = ctx
            .run(|| async move {
                let next = next_occurrence(&request.policy, Utc::now(), true)?;
                let tenant_id = request.tenant_id;
                mutation_write(
                    repository
                        .update_schedule(
                            ExecutionScope::Tenant { tenant_id },
                            &config,
                            request,
                            next,
                        )
                        .await
                        .map_err(execution_error_to_handler_error)?,
                )
            })
            .name("execution_schedule_update")
            .await?
            .into_inner();
        kick_new_schedule_trigger(&ctx, write.dispatcher_kick_uid).await?;
        Ok(Json::from(write.record))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn pause(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionScheduleRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError> {
        prepare_handler(&ctx, "pause");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let repository = self.repository.clone();
        let config = self.config.clone();
        Ok(ctx
            .run(|| async move {
                mutation_record(
                    repository
                        .pause_schedule(
                            ExecutionScope::Tenant {
                                tenant_id: request.tenant_id,
                            },
                            &config,
                            request.tenant_id,
                            request.schedule_uid,
                        )
                        .await
                        .map_err(execution_error_to_handler_error)?,
                )
            })
            .name("execution_schedule_pause")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn resume(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionScheduleRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError> {
        prepare_handler(&ctx, "resume");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let repository = self.repository.clone();
        let config = self.config.clone();
        let write = ctx
            .run(|| async move {
                let scope = ExecutionScope::Tenant {
                    tenant_id: request.tenant_id,
                };
                let schedule = repository
                    .load_schedule(scope, request.tenant_id, request.schedule_uid)
                    .await
                    .map_err(execution_error_to_handler_error)?
                    .ok_or_else(not_found)?;
                let now = Utc::now();
                let next = resume_occurrence(&schedule, now)?;
                mutation_write(
                    repository
                        .resume_schedule(
                            scope,
                            &config,
                            request.tenant_id,
                            request.schedule_uid,
                            next,
                        )
                        .await
                        .map_err(execution_error_to_handler_error)?,
                )
            })
            .name("execution_schedule_resume")
            .await?
            .into_inner();
        kick_new_schedule_trigger(&ctx, write.dispatcher_kick_uid).await?;
        Ok(Json::from(write.record))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn cancel(
        &self,
        ctx: Context<'_>,
        request: Json<ExecutionScheduleRequest>,
    ) -> Result<Json<ExecutionScheduleRecord>, HandlerError> {
        prepare_handler(&ctx, "cancel");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;
        let repository = self.repository.clone();
        let config = self.config.clone();
        Ok(ctx
            .run(|| async move {
                mutation_record(
                    repository
                        .cancel_schedule(
                            ExecutionScope::Tenant {
                                tenant_id: request.tenant_id,
                            },
                            &config,
                            request.tenant_id,
                            request.schedule_uid,
                        )
                        .await
                        .map_err(execution_error_to_handler_error)?,
                )
            })
            .name("execution_schedule_cancel")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request), fields(
        dispatch_uid = %request.0.dispatch_uid,
        trigger_uid = %request.0.trigger_uid,
    ))]
    // SAFETY: ingress-private delivery reloads the exact trigger, schedule, run-as identity, blueprint, and incarnation under tenant RLS.
    async fn fire_occurrence(
        &self,
        ctx: Context<'_>,
        request: Json<crate::runtime::execution_dispatch::ExecutionTriggerDeliveryRequest>,
    ) -> Result<Json<ExecutionScheduleFireResponse>, HandlerError> {
        prepare_handler(&ctx, "fire_occurrence");
        let request = request.into_inner();
        let trigger_uid = request.trigger_uid;
        let repository = self.repository.clone();
        let config = self.config.clone();
        let response = ctx
            .run(|| async move {
                let scope = ExecutionScope::Tenant {
                    tenant_id: request.tenant_id,
                };
                let trigger = repository
                    .load_trigger(scope, request.trigger_uid)
                    .await
                    .map_err(execution_error_to_handler_error)?
                    .ok_or_else(|| {
                        TerminalError::new_with_code(404, "schedule trigger not found")
                    })?;
                let (Some(schedule_uid), Some(schedule_incarnation), Some(occurrence_sequence)) = (
                    trigger.schedule_uid,
                    trigger.schedule_incarnation,
                    trigger.occurrence_sequence,
                ) else {
                    return Err(TerminalError::new_with_code(
                        409,
                        "trigger is not a schedule occurrence",
                    )
                    .into());
                };
                let Some(schedule) = repository
                    .load_schedule(scope, request.tenant_id, schedule_uid)
                    .await
                    .map_err(execution_error_to_handler_error)?
                else {
                    let _ = repository
                        .fire_trigger(scope, request.trigger_uid)
                        .await
                        .map_err(execution_error_to_handler_error)?;
                    return Ok(Json::from(ExecutionScheduleFireResponse::Stale));
                };
                // The trigger freezes the occurrence. Mutable schedule cursor fields may
                // already be cleared by successful completion, pause, or cancellation
                // when this exact delivery is replayed.
                let local = serde_json::from_value::<NaiveDateTime>(
                    trigger
                        .payload
                        .get("occurrence_local")
                        .cloned()
                        .ok_or_else(|| {
                            TerminalError::new_with_code(
                                500,
                                "schedule trigger is missing its frozen local occurrence",
                            )
                        })?,
                )
                .map_err(|_| {
                    TerminalError::new_with_code(
                        500,
                        "schedule trigger has an invalid frozen local occurrence",
                    )
                })?;
                let occurrence = ExecutionScheduleOccurrence {
                    at: trigger.due_at,
                    local,
                };
                let blueprint = execution_schedule_run_blueprint(&schedule)
                    .map_err(execution_error_to_handler_error)?;
                let run = blueprint
                    .instantiate(
                        &schedule,
                        occurrence,
                        occurrence_sequence,
                        config.maximum_horizon_seconds,
                    )
                    .map_err(execution_error_to_handler_error)?;
                let next =
                    next_occurrence(&schedule.policy, occurrence.at + Duration::seconds(1), true)?;
                let outcome = repository
                    .admit_schedule_occurrence(
                        scope,
                        &config,
                        ExecutionScheduleRunAdmission {
                            tenant_id: request.tenant_id,
                            schedule_uid,
                            schedule_incarnation,
                            occurrence_sequence,
                            trigger_uid: request.trigger_uid,
                            trigger_dispatch_uid: request.dispatch_uid,
                            occurrence,
                            run,
                            next_occurrence: next,
                        },
                    )
                    .await
                    .map_err(execution_error_to_handler_error)?;
                Ok(Json::from(match outcome {
                    ExecutionScheduleRunAdmissionOutcome::Admitted {
                        run, activation, ..
                    } => ExecutionScheduleFireResponse::Admitted {
                        run_uid: run.run_uid,
                        activation_dispatch_uid: activation.dispatch_uid,
                    },
                    ExecutionScheduleRunAdmissionOutcome::Skipped { .. } => {
                        ExecutionScheduleFireResponse::Skipped
                    }
                    ExecutionScheduleRunAdmissionOutcome::Replayed {
                        run_uid,
                        activation_dispatch_uid,
                    } => ExecutionScheduleFireResponse::Replayed {
                        run_uid,
                        activation_dispatch_uid,
                    },
                    ExecutionScheduleRunAdmissionOutcome::Stale => {
                        ExecutionScheduleFireResponse::Stale
                    }
                }))
            })
            .name(format!("execution_schedule_fire_{trigger_uid}"))
            .await?
            .into_inner();
        Ok(Json::from(response))
    }
}

fn prepare_handler(ctx: &Context<'_>, handler: &'static str) {
    crate::ctx::adopt_incoming_trace_parent(ctx);
    annotate_restate_handler_span("ExecutionSchedule", handler);
}

fn mutation_record(
    outcome: ExecutionScheduleMutationOutcome,
) -> Result<Json<ExecutionScheduleRecord>, HandlerError> {
    match outcome {
        ExecutionScheduleMutationOutcome::Updated { schedule, .. } => Ok(Json::from(*schedule)),
        ExecutionScheduleMutationOutcome::NotFound => Err(not_found()),
        ExecutionScheduleMutationOutcome::Stale => Err(TerminalError::new_with_code(
            409,
            "schedule lifecycle state or incarnation is stale",
        )
        .into()),
    }
}

fn mutation_write(
    outcome: ExecutionScheduleMutationOutcome,
) -> Result<Json<JournaledScheduleWrite>, HandlerError> {
    match outcome {
        ExecutionScheduleMutationOutcome::Updated { schedule, trigger } => {
            Ok(Json::from(JournaledScheduleWrite {
                record: *schedule,
                dispatcher_kick_uid: trigger.map(|write| write.dispatch.dispatch_uid),
            }))
        }
        ExecutionScheduleMutationOutcome::NotFound => Err(not_found()),
        ExecutionScheduleMutationOutcome::Stale => Err(TerminalError::new_with_code(
            409,
            "schedule lifecycle state or incarnation is stale",
        )
        .into()),
    }
}

async fn kick_new_schedule_trigger(
    ctx: &Context<'_>,
    dispatch_uid: Option<Uuid>,
) -> Result<(), HandlerError> {
    let Some(dispatch_uid) = dispatch_uid else {
        return Ok(());
    };
    // Schedule mutations occur outside a dispatcher chain. Accept exactly one dispatcher kick
    // only when the transaction armed a new outbox-backed trigger; replay and empty schedules do
    // not create another invocation.
    let handle = crate::restate_identity::replay_safe_request(
        ctx.service_client::<ExecutionDispatcherClient>()
            .dispatch(Json::from(DispatchExecutionsRequest::default()))
            .idempotency_key(format!("execution-schedule-trigger:{dispatch_uid}")),
    )
    .send();
    handle.invocation_id().await?;
    Ok(())
}

fn not_found() -> HandlerError {
    TerminalError::new_with_code(404, "execution schedule not found").into()
}

fn resume_occurrence(
    schedule: &ExecutionScheduleRecord,
    now: DateTime<Utc>,
) -> Result<Option<ExecutionScheduleOccurrence>, HandlerError> {
    let anchor = match schedule.policy.missed_fire_policy {
        ExecutionScheduleMissedFirePolicy::Skip => now,
        ExecutionScheduleMissedFirePolicy::FireOnce => schedule.paused_at.unwrap_or(now),
    };
    let occurrence = next_occurrence(&schedule.policy, anchor, true)?;
    Ok(apply_missed_fire_policy(
        schedule.policy.missed_fire_policy,
        occurrence,
        now,
    ))
}

fn apply_missed_fire_policy(
    policy: ExecutionScheduleMissedFirePolicy,
    occurrence: Option<ExecutionScheduleOccurrence>,
    now: DateTime<Utc>,
) -> Option<ExecutionScheduleOccurrence> {
    match (policy, occurrence) {
        (ExecutionScheduleMissedFirePolicy::FireOnce, Some(missed)) if missed.at <= now => {
            Some(ExecutionScheduleOccurrence {
                at: now,
                local: missed.local,
            })
        }
        (_, occurrence) => occurrence,
    }
}

fn next_occurrence(
    policy: &ExecutionSchedulePolicy,
    anchor: DateTime<Utc>,
    inclusive: bool,
) -> Result<Option<ExecutionScheduleOccurrence>, HandlerError> {
    let cron = Cron::new(&policy.calendar_expression)
        .with_seconds_optional()
        .parse()
        .map_err(|error| TerminalError::new(format!("invalid calendar expression: {error}")))?;
    let timezone: Tz = policy
        .timezone
        .parse()
        .map_err(|_| TerminalError::new(format!("invalid IANA timezone: {}", policy.timezone)))?;
    let mut cursor = anchor.max(policy.start_at);
    for _ in 0..8 {
        let candidate = cron
            .find_next_occurrence(&cursor.with_timezone(&timezone), inclusive)
            .map_err(|error| TerminalError::new(format!("no next schedule occurrence: {error}")))?;
        let local = candidate.naive_local();
        let resolved = resolve_local(timezone, local, policy.dst_policy);
        let Some(at) = resolved else {
            cursor = candidate.with_timezone(&Utc) + Duration::seconds(1);
            continue;
        };
        if policy.end_at.is_some_and(|end_at| at >= end_at) {
            return Ok(None);
        }
        return Ok(Some(ExecutionScheduleOccurrence { at, local }));
    }
    Err(TerminalError::new("schedule DST policy skipped too many consecutive candidates").into())
}

fn resolve_local(
    timezone: Tz,
    local: NaiveDateTime,
    policy: ExecutionScheduleDstPolicy,
) -> Option<DateTime<Utc>> {
    match timezone.from_local_datetime(&local) {
        LocalResult::Single(value) => Some(value.with_timezone(&Utc)),
        LocalResult::Ambiguous(earlier, later) => match policy {
            ExecutionScheduleDstPolicy::Earliest => Some(earlier.with_timezone(&Utc)),
            ExecutionScheduleDstPolicy::Latest => Some(later.with_timezone(&Utc)),
            ExecutionScheduleDstPolicy::Skip => None,
        },
        LocalResult::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dst_fall_back_policy_selects_exact_utc_instant() {
        // Pins: an ambiguous New York 01:30 occurrence does not depend on library default choice.
        let local = NaiveDateTime::parse_from_str("2026-11-01 01:30:00", "%Y-%m-%d %H:%M:%S")
            .expect("valid local timestamp");
        let earlier = resolve_local(
            chrono_tz::America::New_York,
            local,
            ExecutionScheduleDstPolicy::Earliest,
        )
        .expect("earlier ambiguous instant");
        let later = resolve_local(
            chrono_tz::America::New_York,
            local,
            ExecutionScheduleDstPolicy::Latest,
        )
        .expect("later ambiguous instant");

        assert_eq!(later - earlier, Duration::hours(1));
        assert_eq!(
            resolve_local(
                chrono_tz::America::New_York,
                local,
                ExecutionScheduleDstPolicy::Skip
            ),
            None
        );
    }

    #[test]
    fn missed_fire_policy_coalesces_once_without_rewriting_local_identity() {
        // Pins: resume never replays every missed occurrence; FireOnce emits one immediate
        // occurrence with the frozen missed local time, while Skip retains the next future fire.
        let now = DateTime::parse_from_rfc3339("2026-08-11T12:00:00Z")
            .expect("valid UTC test time")
            .with_timezone(&Utc);
        let missed = ExecutionScheduleOccurrence {
            at: now - Duration::hours(3),
            local: (now - Duration::hours(3)).naive_utc(),
        };
        let fired = apply_missed_fire_policy(
            ExecutionScheduleMissedFirePolicy::FireOnce,
            Some(missed),
            now,
        )
        .expect("fire-once keeps one occurrence");
        assert_eq!(fired.at, now);
        assert_eq!(fired.local, missed.local);

        let future = ExecutionScheduleOccurrence {
            at: now + Duration::hours(1),
            local: (now + Duration::hours(1)).naive_utc(),
        };
        assert_eq!(
            apply_missed_fire_policy(ExecutionScheduleMissedFirePolicy::Skip, Some(future), now,),
            Some(future)
        );
    }
}
