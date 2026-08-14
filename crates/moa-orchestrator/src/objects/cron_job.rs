//! Reusable wall-clock-anchored cron virtual object.
//!
//! One object instance is keyed by job name. Its state stores the cron
//! expression, timezone, target service handler, and a monotonic version that
//! invalidates ticks scheduled before a reconfiguration.

use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;
use restate_sdk::context::RequestTarget;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use moa_observability::restate_observability::annotate_restate_handler_span;

use crate::vo::VoReader;

const K_STATE: &str = "state";

/// Cron schedule and target dispatch payload for one CronJob key.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CronJobConfig {
    /// 5- or 6-field cron expression, for example `"0 0 * * * *"`.
    pub schedule: String,
    /// IANA timezone name used to anchor wall-clock schedule evaluation.
    pub timezone: String,
    /// Target Restate service to invoke at each fire.
    pub target_service: String,
    /// Target handler on the service.
    pub target_handler: String,
    /// JSON payload sent to the target handler.
    pub payload: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct CronJobState {
    config: Option<CronJobConfig>,
    last_scheduled_fire: Option<DateTime<Utc>>,
    version: u64,
    paused: bool,
}

/// Internal tick payload captured when a delayed tick is scheduled.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TickPayload {
    /// Version observed when this tick was scheduled.
    pub version: u64,
    /// Wall-clock instant this tick was intended to fire at.
    pub scheduled_for: DateTime<Utc>,
}

/// Read-only status projection for one CronJob key.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CronJobStatus {
    /// Whether a config is currently installed.
    pub configured: bool,
    /// Whether firing is paused without clearing the config.
    pub paused: bool,
    /// Current cron expression, if configured.
    pub schedule: Option<String>,
    /// Current IANA timezone, if configured.
    pub timezone: Option<String>,
    /// Most recent delayed tick scheduled by this object.
    pub last_scheduled_fire: Option<DateTime<Utc>>,
    /// Next wall-clock fire instant relative to the status query time.
    pub next_fire: Option<DateTime<Utc>>,
    /// Monotonic version that invalidates stale delayed ticks.
    pub version: u64,
}

/// Restate virtual object surface for one named cron job.
#[restate_sdk::object]
pub trait CronJob {
    /// Install or replace the schedule. Identical active configs are idempotent.
    async fn configure(config: Json<CronJobConfig>) -> Result<(), HandlerError>;

    /// Pause firing without losing config.
    async fn pause() -> Result<(), HandlerError>;

    /// Resume firing and reschedule from the current wall clock.
    async fn resume() -> Result<(), HandlerError>;

    /// Stop and clear the schedule while retaining its version tombstone.
    async fn stop() -> Result<(), HandlerError>;

    /// Internal handler fired by delayed sends.
    async fn tick(payload: Json<TickPayload>) -> Result<(), HandlerError>;

    /// Return the current job status without entering the writer queue.
    #[shared]
    async fn status() -> Result<Json<CronJobStatus>, HandlerError>;
}

/// Concrete `CronJob` virtual object implementation.
pub struct CronJobImpl;

impl CronJob for CronJobImpl {
    #[tracing::instrument(skip(self, ctx, config))]
    async fn configure(
        &self,
        ctx: ObjectContext<'_>,
        config: Json<CronJobConfig>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("CronJob", "configure");
        let config = config.into_inner();
        validate(&config)?;

        let mut state = load_state(&ctx).await?;
        if !install_config(&mut state, config)? {
            return Ok(());
        }
        persist_state(&ctx, &state);

        schedule_next_tick(&ctx, &mut state).await?;
        persist_state(&ctx, &state);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn pause(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("CronJob", "pause");
        let mut state = load_state(&ctx).await?;
        state.paused = true;
        advance_version(&mut state)?;
        persist_state(&ctx, &state);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn resume(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("CronJob", "resume");
        let mut state = load_state(&ctx).await?;
        if state.config.is_none() {
            return Err(TerminalError::new("cron job not configured").into());
        }

        state.paused = false;
        advance_version(&mut state)?;
        persist_state(&ctx, &state);

        schedule_next_tick(&ctx, &mut state).await?;
        persist_state(&ctx, &state);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn stop(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("CronJob", "stop");
        let mut state = load_state(&ctx).await?;
        stop_schedule(&mut state);
        persist_state(&ctx, &state);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, payload))]
    async fn tick(
        &self,
        ctx: ObjectContext<'_>,
        payload: Json<TickPayload>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("CronJob", "tick");
        let payload = payload.into_inner();
        let state = load_state(&ctx).await?;

        let Some(config) = config_for_tick(&state, &payload).cloned() else {
            return Ok(());
        };

        let mut next_state = state.clone();
        schedule_next_tick(&ctx, &mut next_state).await?;
        persist_state(&ctx, &next_state);

        let idempotency_key = format!(
            "cron-{}-{}-{}",
            ctx.key(),
            payload.version,
            payload.scheduled_for.timestamp()
        );
        crate::restate_identity::replay_safe_request(
            ctx.request::<Json<serde_json::Value>, ()>(
                RequestTarget::service(config.target_service, config.target_handler),
                Json::from(config.payload),
            )
            .idempotency_key(idempotency_key),
        )
        .send();

        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<CronJobStatus>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("CronJob", "status");
        let state = load_state(&ctx).await?;
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
            .await?
            .into_inner();
        let next_fire = state
            .config
            .as_ref()
            .map(|config| compute_next_fire_at(config, now))
            .transpose()?;

        Ok(Json::from(CronJobStatus {
            configured: state.config.is_some(),
            paused: state.paused,
            schedule: state.config.as_ref().map(|config| config.schedule.clone()),
            timezone: state.config.as_ref().map(|config| config.timezone.clone()),
            last_scheduled_fire: state.last_scheduled_fire,
            next_fire,
            version: state.version,
        }))
    }
}

async fn load_state<R>(ctx: &R) -> Result<CronJobState, HandlerError>
where
    R: VoReader,
{
    Ok(ctx.get_json(K_STATE).await?.unwrap_or_default())
}

fn persist_state(ctx: &ObjectContext<'_>, state: &CronJobState) {
    ctx.set(K_STATE, Json::from(state.clone()));
}

fn advance_version(state: &mut CronJobState) -> Result<(), TerminalError> {
    state.version = state
        .version
        .checked_add(1)
        .ok_or_else(|| TerminalError::new("cron job version exhausted"))?;
    Ok(())
}

fn install_config(state: &mut CronJobState, config: CronJobConfig) -> Result<bool, TerminalError> {
    if state.config.as_ref() == Some(&config) && !state.paused {
        return Ok(false);
    }

    advance_version(state)?;
    state.config = Some(config);
    state.last_scheduled_fire = None;
    state.paused = false;
    Ok(true)
}

fn stop_schedule(state: &mut CronJobState) {
    state.config = None;
    state.last_scheduled_fire = None;
    state.paused = false;
}

fn config_for_tick<'a>(
    state: &'a CronJobState,
    payload: &TickPayload,
) -> Option<&'a CronJobConfig> {
    if state.paused || payload.version != state.version {
        return None;
    }
    state.config.as_ref()
}

fn validate(config: &CronJobConfig) -> Result<(), HandlerError> {
    parse_cron(&config.schedule)
        .map_err(|error| TerminalError::new(format!("invalid cron schedule: {error}")))?;
    parse_timezone(&config.timezone)?;
    if config.target_service.trim().is_empty() {
        return Err(TerminalError::new("target_service must be non-empty").into());
    }
    if config.target_handler.trim().is_empty() {
        return Err(TerminalError::new("target_handler must be non-empty").into());
    }
    Ok(())
}

fn parse_cron(expr: &str) -> Result<Cron, croner::errors::CronError> {
    Cron::new(expr).with_seconds_optional().parse()
}

fn parse_timezone(timezone: &str) -> Result<Tz, HandlerError> {
    timezone
        .parse()
        .map_err(|_| TerminalError::new(format!("invalid IANA timezone: {timezone}")).into())
}

#[cfg(test)]
fn compute_next_fire(config: &CronJobConfig) -> Result<DateTime<Utc>, HandlerError> {
    compute_next_fire_at(config, Utc::now())
}

fn compute_next_fire_at(
    config: &CronJobConfig,
    now_utc: DateTime<Utc>,
) -> Result<DateTime<Utc>, HandlerError> {
    let cron = parse_cron(&config.schedule)
        .map_err(|error| TerminalError::new(format!("invalid cron schedule: {error}")))?;
    let timezone = parse_timezone(&config.timezone)?;
    let anchor_utc = truncate_to_second(now_utc);
    let next_local = cron
        .find_next_occurrence(&anchor_utc.with_timezone(&timezone), false)
        .map_err(|error| TerminalError::new(format!("no next occurrence: {error}")))?;
    Ok(next_local.with_timezone(&Utc))
}

fn truncate_to_second(value: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(value.timestamp(), 0).unwrap_or(value)
}

async fn schedule_next_tick(
    ctx: &ObjectContext<'_>,
    state: &mut CronJobState,
) -> Result<(), HandlerError> {
    let Some(config) = state.config.as_ref().filter(|_| !state.paused) else {
        return Ok(());
    };
    let now_utc = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .await?
        .into_inner();
    let next_utc = compute_next_fire_at(config, now_utc)?;
    let delay = next_utc
        .signed_duration_since(now_utc)
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(1));

    state.last_scheduled_fire = Some(next_utc);
    let payload = TickPayload {
        version: state.version,
        scheduled_for: next_utc,
    };
    let idempotency_key = format!(
        "cron-sched-{}-{}-{}",
        ctx.key(),
        state.version,
        next_utc.timestamp()
    );
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<CronJobClient>(ctx.key().to_string())
            .tick(Json::from(payload))
            .idempotency_key(idempotency_key),
    )
    .send_after(delay);
    tracing::info!(
        key = %ctx.key(),
        scheduled_for = %next_utc,
        version = state.version,
        "scheduled cron job tick"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Timelike;

    use super::*;

    fn valid_config() -> CronJobConfig {
        CronJobConfig {
            schedule: "0 0 * * * *".to_string(),
            timezone: "UTC".to_string(),
            target_service: "Health".to_string(),
            target_handler: "check".to_string(),
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn parses_six_field_expression() {
        // Pins: cron parser accepts second-precision schedules.
        assert!(parse_cron("0 0 * * * *").is_ok());
    }

    #[test]
    fn parses_five_field_expression() {
        // Pins: cron parser accepts Vixie five-field schedules.
        assert!(parse_cron("0 * * * *").is_ok());
    }

    #[test]
    fn rejects_garbage() {
        // Pins: invalid cron text is rejected before state is stored.
        assert!(parse_cron("not a cron").is_err());
    }

    #[test]
    fn validates_config_rejects_bad_timezone() {
        // Pins: CronJob validation rejects non-IANA timezone names.
        let config = CronJobConfig {
            timezone: "Mars/Olympus".to_string(),
            ..valid_config()
        };

        assert!(validate(&config).is_err());
    }

    #[test]
    fn computes_next_top_of_hour_in_utc() {
        // Pins: top-of-hour UTC schedules resolve to a future hour boundary.
        let next = compute_next_fire(&valid_config()).expect("next fire");

        assert_eq!(next.minute(), 0);
        assert_eq!(next.second(), 0);
        assert_eq!(next.nanosecond(), 0);
        assert!(next > Utc::now());
    }

    #[test]
    fn stop_reconfigure_rejects_late_tick_from_old_incarnation() {
        // Pins: stopping and reconfiguring a CronJob never lets a delayed tick from the
        // previous configuration dispatch against the new target.
        let original_config = valid_config();
        let mut state = CronJobState::default();
        assert!(
            install_config(&mut state, original_config)
                .expect("initial configuration should advance the incarnation")
        );
        let stale_tick = TickPayload {
            version: state.version,
            scheduled_for: DateTime::<Utc>::from_timestamp(1_700_000_000, 0)
                .expect("fixture timestamp should be valid"),
        };
        assert_eq!(state.version, 1);
        assert_eq!(config_for_tick(&state, &stale_tick), state.config.as_ref());

        stop_schedule(&mut state);
        assert_eq!(state.version, 1);
        assert_eq!(state.config, None);
        assert_eq!(state.last_scheduled_fire, None);
        assert!(!state.paused);
        assert_eq!(config_for_tick(&state, &stale_tick), None);

        let replacement_config = CronJobConfig {
            target_handler: "replacement".to_string(),
            ..valid_config()
        };
        assert!(
            install_config(&mut state, replacement_config.clone())
                .expect("replacement configuration should advance the incarnation")
        );
        assert_eq!(state.version, 2);
        assert_eq!(state.config, Some(replacement_config));
        assert_eq!(config_for_tick(&state, &stale_tick), None);

        let replacement_tick = TickPayload {
            version: state.version,
            scheduled_for: stale_tick.scheduled_for,
        };
        assert_eq!(
            config_for_tick(&state, &replacement_tick),
            state.config.as_ref()
        );
    }

    #[test]
    fn version_exhaustion_preserves_the_existing_incarnation() {
        // Pins: version rollover cannot make an ancient delayed tick current again.
        let mut state = CronJobState {
            config: Some(valid_config()),
            version: u64::MAX,
            ..CronJobState::default()
        };

        let replacement_config = CronJobConfig {
            target_handler: "replacement".to_string(),
            ..valid_config()
        };
        let error = install_config(&mut state, replacement_config)
            .expect_err("an exhausted version must fail closed");

        assert_eq!(state.version, u64::MAX);
        assert_eq!(state.config, Some(valid_config()));
        assert_eq!(
            error.message(),
            "cron job version exhausted",
            "exhaustion should return the exact terminal reason"
        );
    }
}
