//! Restate virtual object that owns one durable workspace orchestration key.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::{
    ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, MoaError, TenantId, UserId, WorkspaceId,
};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_scalar};
use crate::workflows::consolidate::{
    ConsolidateClient, ConsolidateReport, ConsolidateRequest, consolidate_workflow_id,
};
use moa_core::restate_observability::annotate_restate_handler_span;

const K_CONFIG: &str = "config";
const K_ACTION_POLICY: &str = "action_policy";
const K_LAST_CONSOLIDATION: &str = "last_consolidation";
const K_NEXT_CONSOLIDATION: &str = "next_consolidation";
const K_CONSOLIDATION_IN_PROGRESS: &str = "consolidation_in_progress";

/// Workspace-scoped action policy snapshot mirrored into Restate object state.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceActionPolicy {
    /// Persisted action-policy rules visible to the workspace.
    #[serde(default)]
    pub rules: Vec<ActionPolicyRule>,
}

/// Input payload used to initialize a workspace object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceConfig {
    /// Workspace identifier.
    pub id: WorkspaceId,
    /// Human-readable workspace name.
    pub name: String,
    /// Hour of day in UTC at which the next consolidation should be scheduled.
    pub consolidation_hour_utc: u8,
    /// Action-policy rules mirrored into Restate state for status and bootstrap flows.
    #[serde(default)]
    pub action_policy: WorkspaceActionPolicy,
}

/// Read-only workspace orchestration status projection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceStatus {
    /// Timestamp of the most recent finished consolidation.
    pub last_consolidation_at: Option<DateTime<Utc>>,
    /// Timestamp of the next scheduled consolidation.
    pub next_consolidation_at: Option<DateTime<Utc>>,
    /// Whether a consolidation workflow is currently in progress.
    pub consolidation_in_progress: bool,
    /// Number of graph memory records currently present in the workspace.
    pub pages_count: u64,
}

/// Request payload for storing a workspace-default action-policy rule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceActionPolicyRuleInput {
    /// Tool name the rule applies to.
    pub tool_name: String,
    /// Persisted normalized pattern.
    pub pattern: String,
    /// Effect applied when the rule matches.
    pub effect: ActionPolicyEffect,
    /// Optional reason stored with the rule.
    pub reason: Option<String>,
}

/// Serializable projection of the Workspace VO's durable keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceVoState {
    /// Workspace configuration payload.
    pub config: Option<WorkspaceConfig>,
    /// Action policy snapshot.
    pub action_policy: WorkspaceActionPolicy,
    /// Most recent completion timestamp.
    pub last_consolidation: Option<DateTime<Utc>>,
    /// Next scheduled consolidation timestamp.
    pub next_consolidation: Option<DateTime<Utc>>,
    /// Whether a workflow is currently running.
    pub consolidation_in_progress: bool,
}

impl WorkspaceVoState {
    /// Ensures the workspace was initialized before mutating scheduling state.
    pub fn ensure_initialized(&self) -> Result<&WorkspaceConfig, HandlerError> {
        self.config.as_ref().ok_or_else(|| {
            TerminalError::new("workspace not initialized; call Workspace/init first").into()
        })
    }

    /// Marks a consolidation workflow as active without requiring scheduler config.
    pub fn mark_consolidation_started(&mut self) {
        self.consolidation_in_progress = true;
    }

    /// Records a completed consolidation and returns whether a daily VO reschedule is configured.
    pub fn record_consolidation_completed(&mut self, ran_at: DateTime<Utc>) -> bool {
        self.last_consolidation = Some(ran_at);
        self.consolidation_in_progress = false;
        self.config.is_some()
    }
}

impl VoState for WorkspaceVoState {
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(Self {
            config: reader.get_json(K_CONFIG).await?,
            action_policy: reader.get_json(K_ACTION_POLICY).await?.unwrap_or_default(),
            last_consolidation: reader.get_json(K_LAST_CONSOLIDATION).await?,
            next_consolidation: reader.get_json(K_NEXT_CONSOLIDATION).await?,
            consolidation_in_progress: reader
                .get_json(K_CONSOLIDATION_IN_PROGRESS)
                .await?
                .unwrap_or_default(),
        })
    }

    fn persist_into(&self, ctx: &ObjectContext<'_>) {
        set_or_clear_opt(ctx, K_CONFIG, self.config.as_ref());
        set_or_clear_opt(
            ctx,
            K_ACTION_POLICY,
            (!self.action_policy.rules.is_empty()).then_some(&self.action_policy),
        );
        set_or_clear_opt(ctx, K_LAST_CONSOLIDATION, self.last_consolidation.as_ref());
        set_or_clear_opt(ctx, K_NEXT_CONSOLIDATION, self.next_consolidation.as_ref());
        set_or_clear_scalar(
            ctx,
            K_CONSOLIDATION_IN_PROGRESS,
            self.consolidation_in_progress,
            false,
        );
    }
}

/// Returns the next scheduled consolidation time for the given UTC hour.
#[must_use]
pub fn compute_next_consolidation_utc(now: DateTime<Utc>, hour: u8) -> DateTime<Utc> {
    let hour = hour.min(23) as u32;
    let Some(today_target) = now.date_naive().and_hms_opt(hour, 0, 0) else {
        return now;
    };
    let today_target = today_target.and_utc();

    if today_target > now {
        today_target
    } else {
        today_target + chrono::Duration::days(1)
    }
}

/// Returns a stable per-workspace schedule jitter in seconds.
#[must_use]
pub fn deterministic_consolidation_jitter_secs(workspace_id: &WorkspaceId) -> u64 {
    let mut hasher = DefaultHasher::new();
    workspace_id.hash(&mut hasher);
    hasher.finish() % 600
}

/// Restate virtual object surface for one workspace orchestration key.
#[restate_sdk::object]
#[name = "Workspace"]
pub trait WorkspaceObject {
    /// Initializes the workspace object with its persisted config and schedules the first run.
    async fn init(config: Json<WorkspaceConfig>) -> Result<(), HandlerError>;

    /// Returns the current workspace-default action-policy rules mirrored into Restate state.
    #[shared]
    async fn get_action_policy() -> Result<Json<WorkspaceActionPolicy>, HandlerError>;

    /// Persists one action-policy rule and updates the VO snapshot.
    async fn add_action_policy_rule(
        pattern: Json<WorkspaceActionPolicyRuleInput>,
    ) -> Result<(), HandlerError>;

    /// Schedules the next daily consolidation workflow.
    async fn schedule_consolidation() -> Result<(), HandlerError>;

    /// Marks the workspace as actively consolidating.
    async fn mark_consolidation_started(
        target_date: Json<chrono::NaiveDate>,
    ) -> Result<(), HandlerError>;

    /// Records one completed workflow run and schedules the next run.
    async fn consolidation_completed(report: Json<ConsolidateReport>) -> Result<(), HandlerError>;

    /// Returns read-only scheduling status for the workspace.
    #[shared]
    async fn status() -> Result<Json<WorkspaceStatus>, HandlerError>;
}

/// Concrete `Workspace` virtual object implementation.
pub struct WorkspaceImpl;

impl WorkspaceObject for WorkspaceImpl {
    #[tracing::instrument(skip(self, ctx, config))]
    async fn init(
        &self,
        ctx: ObjectContext<'_>,
        config: Json<WorkspaceConfig>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Workspace", "init");
        let config = config.into_inner();
        validate_workspace_key(ctx.key(), &config.id)?;
        validate_consolidation_hour(config.consolidation_hour_utc)?;
        moa_security::validate_action_policy_rules(&config.action_policy.rules)
            .map_err(to_handler_error)?;

        let mut state = WorkspaceVoState::load_from(&ctx).await?;
        state.config = Some(config.clone());
        state.action_policy = config.action_policy.clone();
        state.persist_into(&ctx);

        persist_policy_rules(config.id.clone(), &state.action_policy.rules).await?;
        schedule_consolidation_inner(&ctx, &mut state).await?;
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn get_action_policy(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<WorkspaceActionPolicy>, HandlerError> {
        annotate_restate_handler_span("Workspace", "get_action_policy");
        Ok(Json::from(
            WorkspaceVoState::load_from(&ctx).await?.action_policy,
        ))
    }

    #[tracing::instrument(skip(self, ctx, pattern))]
    async fn add_action_policy_rule(
        &self,
        ctx: ObjectContext<'_>,
        pattern: Json<WorkspaceActionPolicyRuleInput>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Workspace", "add_action_policy_rule");
        let pattern = pattern.into_inner();
        let workspace_id = parse_workspace_key(ctx.key());
        let tenant_id = tenant_id_from_workspace_id(&workspace_id)?;
        let identity = require_tenant_admin(&ctx, tenant_id).await?;
        let created_at = durable_utc_now(&ctx).await?;
        let mut state = WorkspaceVoState::load_from(&ctx).await?;
        let _ = state.ensure_initialized()?;

        let rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            tool: pattern.tool_name.clone(),
            pattern: pattern.pattern.clone(),
            effect: pattern.effect,
            scope: ActionRuleScope::Tenant { tenant_id },
            reason: pattern.reason,
            created_by: UserId::new(identity.id.to_string()),
            created_at,
        };
        moa_security::validate_action_policy_rule(&rule).map_err(to_handler_error)?;

        if let Some(existing) = state
            .action_policy
            .rules
            .iter_mut()
            .find(|existing| existing.tool == rule.tool && existing.pattern == rule.pattern)
        {
            *existing = rule.clone();
        } else {
            state.action_policy.rules.push(rule.clone());
        }
        state.persist_into(&ctx);
        persist_policy_rules(workspace_id, &[rule]).await?;
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn schedule_consolidation(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Workspace", "schedule_consolidation");
        let mut state = WorkspaceVoState::load_from(&ctx).await?;
        schedule_consolidation_inner(&ctx, &mut state).await?;
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, _target_date))]
    async fn mark_consolidation_started(
        &self,
        ctx: ObjectContext<'_>,
        _target_date: Json<chrono::NaiveDate>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Workspace", "mark_consolidation_started");
        let mut state = WorkspaceVoState::load_from(&ctx).await?;
        state.mark_consolidation_started();
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, report))]
    async fn consolidation_completed(
        &self,
        ctx: ObjectContext<'_>,
        report: Json<ConsolidateReport>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Workspace", "consolidation_completed");
        let report = report.into_inner();
        validate_tenant_key(ctx.key(), report.tenant_id)?;

        let mut state = WorkspaceVoState::load_from(&ctx).await?;
        let should_reschedule = state.record_consolidation_completed(report.ran_at);
        tracing::info!(
            tenant_id = %report.tenant_id,
            target_date = %report.target_date,
            records_updated = report.records_updated,
            records_deleted = report.records_deleted,
            duration_ms = report.duration_ms,
            errors = ?report.errors,
            "tenant consolidation completed"
        );
        if should_reschedule {
            schedule_consolidation_inner(&ctx, &mut state).await?;
        }
        state.persist_into(&ctx);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<WorkspaceStatus>, HandlerError> {
        annotate_restate_handler_span("Workspace", "status");
        let state = WorkspaceVoState::load_from(&ctx).await?;
        let workspace_id = parse_workspace_key(ctx.key());
        let pages_count = count_graph_nodes(&workspace_id).await?;

        Ok(Json::from(WorkspaceStatus {
            last_consolidation_at: state.last_consolidation,
            next_consolidation_at: state.next_consolidation,
            consolidation_in_progress: state.consolidation_in_progress,
            pages_count,
        }))
    }
}

async fn require_tenant_admin(
    ctx: &ObjectContext<'_>,
    tenant_id: TenantId,
) -> Result<moa_core::traits::Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
}

async fn count_graph_nodes(workspace_id: &WorkspaceId) -> Result<u64, HandlerError> {
    let ctx = OrchestratorCtx::current();
    let pool = ctx.graph_pool();
    let count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM moa.node_index
        WHERE workspace_id = $1
          AND valid_to IS NULL
        "#,
    )
    .bind(workspace_id.as_str())
    .fetch_one(&pool)
    .await
    .map_err(HandlerError::from)?;
    Ok(count.max(0) as u64)
}

async fn schedule_consolidation_inner(
    ctx: &ObjectContext<'_>,
    state: &mut WorkspaceVoState,
) -> Result<(), HandlerError> {
    let config = state.ensure_initialized()?.clone();
    let now = durable_utc_now(ctx).await?;
    let next = compute_next_consolidation_utc(now, config.consolidation_hour_utc);
    let jitter_secs = deterministic_consolidation_jitter_secs(&config.id);
    let scheduled_at = next + chrono::Duration::seconds(jitter_secs as i64);
    let delay = scheduled_at.signed_duration_since(now);
    let delay = duration_from_chrono(delay);
    let tenant_id = tenant_id_from_workspace_id(&config.id)?;
    let workflow_id = consolidate_workflow_id(&tenant_id, next.date_naive());

    state.next_consolidation = Some(scheduled_at);
    ctx.workflow_client::<ConsolidateClient>(workflow_id)
        .run(Json(ConsolidateRequest {
            tenant_id,
            target_date: next.date_naive(),
        }))
        .send_after(delay);
    tracing::info!(
        tenant_id = %tenant_id,
        scheduled_at = %scheduled_at,
        hour_utc = config.consolidation_hour_utc,
        "scheduled next tenant consolidation"
    );
    Ok(())
}

async fn durable_utc_now(ctx: &ObjectContext<'_>) -> Result<DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .await?
        .into_inner())
}

async fn persist_policy_rules(
    workspace_id: WorkspaceId,
    rules: &[ActionPolicyRule],
) -> Result<(), HandlerError> {
    if rules.is_empty() {
        return Ok(());
    }

    let store = OrchestratorCtx::current_session_store();
    let _ = workspace_id;

    let result: Result<(), MoaError> = async {
        for rule in rules.iter().cloned() {
            store.upsert_action_policy_rule(rule).await?;
        }
        Ok(())
    }
    .await;

    result.map_err(to_handler_error)
}

fn duration_from_chrono(duration: chrono::Duration) -> Duration {
    duration
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(24 * 60 * 60))
}

fn parse_workspace_key(key: &str) -> WorkspaceId {
    WorkspaceId::new(key)
}

fn validate_workspace_key(key: &str, workspace_id: &WorkspaceId) -> Result<(), HandlerError> {
    if key == workspace_id.as_str() {
        return Ok(());
    }

    Err(TerminalError::new(format!(
        "workspace key `{key}` does not match config/report id `{workspace_id}`"
    ))
    .into())
}

fn validate_tenant_key(key: &str, tenant_id: TenantId) -> Result<(), HandlerError> {
    if key == tenant_id.to_string() {
        return Ok(());
    }

    Err(TerminalError::new(format!(
        "workspace key `{key}` does not match tenant report id `{tenant_id}`"
    ))
    .into())
}

fn tenant_id_from_workspace_id(workspace_id: &WorkspaceId) -> Result<TenantId, HandlerError> {
    Uuid::parse_str(workspace_id.as_str())
        .map(TenantId::from)
        .map_err(|error| {
            TerminalError::new_with_code(
                400,
                format!("workspace object id must be a tenant UUID for consolidation: {error}"),
            )
            .into()
        })
}

fn validate_consolidation_hour(hour: u8) -> Result<(), HandlerError> {
    if hour <= 23 {
        return Ok(());
    }

    Err(TerminalError::new(format!(
        "consolidation hour must be within 0..=23, got {hour}"
    ))
    .into())
}

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-15T12:00:00Z")
            .expect("fixed timestamp parses")
            .with_timezone(&Utc)
    }

    #[test]
    fn consolidation_started_accepts_uninitialized_workspace() {
        // Pins: graph-memory cron can mark an active workspace even when Workspace/init never ran.
        let mut state = WorkspaceVoState::default();

        state.mark_consolidation_started();

        assert!(state.consolidation_in_progress);
        assert!(state.config.is_none());
    }

    #[test]
    fn consolidation_completed_uninitialized_workspace_skips_vo_reschedule() {
        // Pins: cron-dispatched consolidation completion does not require daily scheduler config.
        let mut state = WorkspaceVoState {
            consolidation_in_progress: true,
            ..WorkspaceVoState::default()
        };
        let ran_at = fixed_time();

        let should_reschedule = state.record_consolidation_completed(ran_at);

        assert!(!should_reschedule);
        assert_eq!(state.last_consolidation, Some(ran_at));
        assert!(!state.consolidation_in_progress);
    }

    #[test]
    fn consolidation_completed_initialized_workspace_keeps_daily_reschedule() {
        // Pins: initialized Workspace VOs still use their daily self-scheduler after completion.
        let mut state = WorkspaceVoState {
            config: Some(WorkspaceConfig {
                id: WorkspaceId::new("workspace"),
                name: "workspace".to_string(),
                consolidation_hour_utc: 3,
                action_policy: WorkspaceActionPolicy::default(),
            }),
            consolidation_in_progress: true,
            ..WorkspaceVoState::default()
        };

        assert!(state.record_consolidation_completed(fixed_time()));
    }
}
