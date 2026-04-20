//! Restate virtual object that owns one durable workspace orchestration key.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::{ApprovalRule, MoaError, PolicyAction, PolicyScope, UserId, WorkspaceId};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::observability::annotate_restate_handler_span;
use crate::services::memory_store::{ListPagesRequest, MemoryStoreClient};
use crate::workflows::consolidate::{ConsolidateClient, ConsolidateReport, ConsolidateRequest};

const K_CONFIG: &str = "config";
const K_APPROVAL_POLICY: &str = "approval_policy";
const K_LAST_CONSOLIDATION: &str = "last_consolidation";
const K_NEXT_CONSOLIDATION: &str = "next_consolidation";
const K_CONSOLIDATION_IN_PROGRESS: &str = "consolidation_in_progress";

/// Workspace-scoped approval policy snapshot mirrored into Restate object state.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceApprovalPolicy {
    /// Persisted approval rules visible to the workspace.
    #[serde(default)]
    pub rules: Vec<ApprovalRule>,
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
    /// Approval rules mirrored into Restate state for status and bootstrap flows.
    #[serde(default)]
    pub approval_policy: WorkspaceApprovalPolicy,
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
    /// Number of pages currently present in the workspace wiki.
    pub pages_count: u64,
}

/// Request payload for storing a workspace-scoped allow rule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AlwaysAllowPattern {
    /// Tool name the rule applies to.
    pub tool_name: String,
    /// Persisted normalized pattern.
    pub pattern: String,
    /// User who approved the rule.
    pub created_by: UserId,
    /// Approval timestamp.
    pub created_at: DateTime<Utc>,
}

/// Serializable projection of the Workspace VO's durable keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceVoState {
    /// Workspace configuration payload.
    pub config: Option<WorkspaceConfig>,
    /// Approval policy snapshot.
    pub approval_policy: WorkspaceApprovalPolicy,
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
pub trait Workspace {
    /// Initializes the workspace object with its persisted config and schedules the first run.
    async fn init(config: Json<WorkspaceConfig>) -> Result<(), HandlerError>;

    /// Returns the current workspace-scoped approval rules mirrored into Restate state.
    #[shared]
    async fn get_approval_policy() -> Result<Json<WorkspaceApprovalPolicy>, HandlerError>;

    /// Persists one always-allow rule and updates the VO snapshot.
    async fn add_always_allow(pattern: Json<AlwaysAllowPattern>) -> Result<(), HandlerError>;

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

impl Workspace for WorkspaceImpl {
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

        let mut state = load_object_state(&ctx).await?;
        state.config = Some(config.clone());
        state.approval_policy = config.approval_policy.clone();
        persist_state(&ctx, &state);

        persist_policy_rules(config.id.clone(), &state.approval_policy.rules).await?;
        schedule_consolidation_inner(&ctx, &mut state)?;
        persist_state(&ctx, &state);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn get_approval_policy(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<WorkspaceApprovalPolicy>, HandlerError> {
        annotate_restate_handler_span("Workspace", "get_approval_policy");
        Ok(Json::from(load_shared_state(&ctx).await?.approval_policy))
    }

    #[tracing::instrument(skip(self, ctx, pattern))]
    async fn add_always_allow(
        &self,
        ctx: ObjectContext<'_>,
        pattern: Json<AlwaysAllowPattern>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Workspace", "add_always_allow");
        let pattern = pattern.into_inner();
        let workspace_id = parse_workspace_key(ctx.key());
        let mut state = load_object_state(&ctx).await?;
        let _ = state.ensure_initialized()?;

        let rule = ApprovalRule {
            id: Uuid::now_v7(),
            workspace_id: workspace_id.clone(),
            tool: pattern.tool_name.clone(),
            pattern: pattern.pattern.clone(),
            action: PolicyAction::Allow,
            scope: PolicyScope::Workspace,
            created_by: pattern.created_by.clone(),
            created_at: pattern.created_at,
        };

        if let Some(existing) = state
            .approval_policy
            .rules
            .iter_mut()
            .find(|existing| existing.tool == rule.tool && existing.pattern == rule.pattern)
        {
            *existing = rule.clone();
        } else {
            state.approval_policy.rules.push(rule.clone());
        }
        persist_state(&ctx, &state);
        persist_policy_rules(workspace_id, &[rule]).await?;
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn schedule_consolidation(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Workspace", "schedule_consolidation");
        let mut state = load_object_state(&ctx).await?;
        schedule_consolidation_inner(&ctx, &mut state)?;
        persist_state(&ctx, &state);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, _target_date))]
    async fn mark_consolidation_started(
        &self,
        ctx: ObjectContext<'_>,
        _target_date: Json<chrono::NaiveDate>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Workspace", "mark_consolidation_started");
        let mut state = load_object_state(&ctx).await?;
        let _ = state.ensure_initialized()?;
        state.consolidation_in_progress = true;
        persist_state(&ctx, &state);
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
        validate_workspace_key(ctx.key(), &report.workspace_id)?;

        let mut state = load_object_state(&ctx).await?;
        let _ = state.ensure_initialized()?;
        state.last_consolidation = Some(report.ran_at);
        state.consolidation_in_progress = false;
        tracing::info!(
            workspace_id = %report.workspace_id,
            target_date = %report.target_date,
            pages_updated = report.pages_updated,
            pages_deleted = report.pages_deleted,
            duration_ms = report.duration_ms,
            errors = ?report.errors,
            "workspace consolidation completed"
        );
        schedule_consolidation_inner(&ctx, &mut state)?;
        persist_state(&ctx, &state);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<WorkspaceStatus>, HandlerError> {
        annotate_restate_handler_span("Workspace", "status");
        let state = load_shared_state(&ctx).await?;
        let workspace_id = parse_workspace_key(ctx.key());
        let pages_count = ctx
            .service_client::<MemoryStoreClient>()
            .list_pages(Json(ListPagesRequest {
                scope: moa_core::MemoryScope::Workspace(workspace_id),
                page_type: None,
            }))
            .call()
            .await?
            .into_inner()
            .len() as u64;

        Ok(Json::from(WorkspaceStatus {
            last_consolidation_at: state.last_consolidation,
            next_consolidation_at: state.next_consolidation,
            consolidation_in_progress: state.consolidation_in_progress,
            pages_count,
        }))
    }
}

async fn load_object_state(ctx: &ObjectContext<'_>) -> Result<WorkspaceVoState, HandlerError> {
    Ok(WorkspaceVoState {
        config: ctx
            .get::<Json<WorkspaceConfig>>(K_CONFIG)
            .await?
            .map(Json::into_inner),
        approval_policy: ctx
            .get::<Json<WorkspaceApprovalPolicy>>(K_APPROVAL_POLICY)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        last_consolidation: ctx
            .get::<Json<DateTime<Utc>>>(K_LAST_CONSOLIDATION)
            .await?
            .map(Json::into_inner),
        next_consolidation: ctx
            .get::<Json<DateTime<Utc>>>(K_NEXT_CONSOLIDATION)
            .await?
            .map(Json::into_inner),
        consolidation_in_progress: ctx
            .get::<Json<bool>>(K_CONSOLIDATION_IN_PROGRESS)
            .await?
            .map(Json::into_inner)
            .unwrap_or(false),
    })
}

async fn load_shared_state(
    ctx: &SharedObjectContext<'_>,
) -> Result<WorkspaceVoState, HandlerError> {
    Ok(WorkspaceVoState {
        config: ctx
            .get::<Json<WorkspaceConfig>>(K_CONFIG)
            .await?
            .map(Json::into_inner),
        approval_policy: ctx
            .get::<Json<WorkspaceApprovalPolicy>>(K_APPROVAL_POLICY)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        last_consolidation: ctx
            .get::<Json<DateTime<Utc>>>(K_LAST_CONSOLIDATION)
            .await?
            .map(Json::into_inner),
        next_consolidation: ctx
            .get::<Json<DateTime<Utc>>>(K_NEXT_CONSOLIDATION)
            .await?
            .map(Json::into_inner),
        consolidation_in_progress: ctx
            .get::<Json<bool>>(K_CONSOLIDATION_IN_PROGRESS)
            .await?
            .map(Json::into_inner)
            .unwrap_or(false),
    })
}

fn persist_state(ctx: &ObjectContext<'_>, state: &WorkspaceVoState) {
    match &state.config {
        Some(config) => ctx.set(K_CONFIG, Json::from(config.clone())),
        None => ctx.clear(K_CONFIG),
    }
    if state.approval_policy.rules.is_empty() {
        ctx.clear(K_APPROVAL_POLICY);
    } else {
        ctx.set(K_APPROVAL_POLICY, Json::from(state.approval_policy.clone()));
    }
    match state.last_consolidation {
        Some(timestamp) => ctx.set(K_LAST_CONSOLIDATION, Json::from(timestamp)),
        None => ctx.clear(K_LAST_CONSOLIDATION),
    }
    match state.next_consolidation {
        Some(timestamp) => ctx.set(K_NEXT_CONSOLIDATION, Json::from(timestamp)),
        None => ctx.clear(K_NEXT_CONSOLIDATION),
    }
    if state.consolidation_in_progress {
        ctx.set(K_CONSOLIDATION_IN_PROGRESS, Json::from(true));
    } else {
        ctx.clear(K_CONSOLIDATION_IN_PROGRESS);
    }
}

fn schedule_consolidation_inner(
    ctx: &ObjectContext<'_>,
    state: &mut WorkspaceVoState,
) -> Result<(), HandlerError> {
    let config = state.ensure_initialized()?.clone();
    let now = Utc::now();
    let next = compute_next_consolidation_utc(now, config.consolidation_hour_utc);
    let jitter_secs = deterministic_consolidation_jitter_secs(&config.id);
    let scheduled_at = next + chrono::Duration::seconds(jitter_secs as i64);
    let delay = scheduled_at.signed_duration_since(now);
    let delay = duration_from_chrono(delay);
    let workflow_id = format!("{}:{}", config.id, next.date_naive());

    state.next_consolidation = Some(scheduled_at);
    ctx.workflow_client::<ConsolidateClient>(workflow_id)
        .run(Json(ConsolidateRequest {
            workspace_id: config.id.clone(),
            target_date: next.date_naive(),
        }))
        .send_after(delay);
    tracing::info!(
        workspace_id = %config.id,
        scheduled_at = %scheduled_at,
        hour_utc = config.consolidation_hour_utc,
        "scheduled next workspace consolidation"
    );
    Ok(())
}

async fn persist_policy_rules(
    workspace_id: WorkspaceId,
    rules: &[ApprovalRule],
) -> Result<(), HandlerError> {
    if rules.is_empty() {
        return Ok(());
    }

    let store = crate::runtime::SESSION_STORE
        .get()
        .cloned()
        .ok_or_else(|| TerminalError::new("session store runtime is not initialized"))?;
    let mut normalized_rules = rules.to_vec();
    for rule in &mut normalized_rules {
        rule.workspace_id = workspace_id.clone();
    }

    let result: Result<(), MoaError> = async {
        for rule in normalized_rules {
            store.upsert_approval_rule(rule).await?;
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
