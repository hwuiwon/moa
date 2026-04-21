//! Shared plumbing for Restate virtual-object state.

use restate_sdk::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Read-side abstraction over `ObjectContext` and `SharedObjectContext`.
///
/// Handlers frequently need to load durable state from either the exclusive
/// (`ObjectContext`) or the read-only (`SharedObjectContext`) variant of a
/// Restate object context. Both expose the same `get` method with identical
/// semantics for reads; this trait lets VOs write one `load_from` and reuse it
/// from both kinds of handler.
#[allow(async_fn_in_trait)]
pub(crate) trait VoReader {
    /// Loads one JSON-backed value from Restate object state.
    async fn get_json<T>(&self, key: &str) -> Result<Option<T>, HandlerError>
    where
        T: DeserializeOwned + 'static;
}

impl<'a> VoReader for ObjectContext<'a> {
    async fn get_json<T>(&self, key: &str) -> Result<Option<T>, HandlerError>
    where
        T: DeserializeOwned + 'static,
    {
        Ok(self.get::<Json<T>>(key).await?.map(Json::into_inner))
    }
}

impl<'a> VoReader for SharedObjectContext<'a> {
    async fn get_json<T>(&self, key: &str) -> Result<Option<T>, HandlerError>
    where
        T: DeserializeOwned + 'static,
    {
        Ok(self.get::<Json<T>>(key).await?.map(Json::into_inner))
    }
}

/// State that can be loaded from and persisted to a Restate virtual object.
#[allow(async_fn_in_trait)]
pub(crate) trait VoState: Default + Sized {
    /// Loads state from any reader, exclusive or shared.
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError>;

    /// Persists state to an exclusive context.
    fn persist_into(&self, ctx: &ObjectContext<'_>);
}

/// Sets `key` when `value` is `Some`, clears it otherwise.
pub(crate) fn set_or_clear_opt<T>(ctx: &ObjectContext<'_>, key: &str, value: Option<&T>)
where
    T: Clone + Serialize + 'static,
{
    match value {
        Some(value) => ctx.set(key, Json::from(value.clone())),
        None => ctx.clear(key),
    }
}

/// Sets `key` when `values` is non-empty, clears it otherwise.
pub(crate) fn set_or_clear_vec<T>(ctx: &ObjectContext<'_>, key: &str, values: &[T])
where
    T: Clone + Serialize + 'static,
{
    if values.is_empty() {
        ctx.clear(key);
    } else {
        ctx.set(key, Json::from(values.to_vec()));
    }
}

/// Sets `key` to `value` unless it equals `empty_sentinel`, in which case clears.
pub(crate) fn set_or_clear_scalar<T>(
    ctx: &ObjectContext<'_>,
    key: &str,
    value: T,
    empty_sentinel: T,
) where
    T: PartialEq + Serialize + 'static,
{
    if value == empty_sentinel {
        ctx.clear(key);
    } else {
        ctx.set(key, Json::from(value));
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;

    use restate_sdk::prelude::{HandlerError, TerminalError};
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use super::VoReader;

    /// In-memory `VoReader` used by VO round-trip unit tests.
    #[derive(Default)]
    pub struct FakeReader {
        values: HashMap<String, serde_json::Value>,
    }

    impl FakeReader {
        /// Stores one JSON-serializable value under `key`.
        #[must_use]
        pub fn insert<T>(mut self, key: &str, value: T) -> Self
        where
            T: Serialize,
        {
            self.values.insert(
                key.to_string(),
                serde_json::to_value(value).expect("serialize fake reader value"),
            );
            self
        }
    }

    impl VoReader for FakeReader {
        async fn get_json<T>(&self, key: &str) -> Result<Option<T>, HandlerError>
        where
            T: DeserializeOwned + 'static,
        {
            self.values
                .get(key)
                .cloned()
                .map(|value| {
                    serde_json::from_value(value).map_err(|error| {
                        TerminalError::new(format!(
                            "decode fake reader value for `{key}` failed: {error}"
                        ))
                        .into()
                    })
                })
                .transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        ApprovalRule, Attachment, ContextMessage, ModelId, Platform, PolicyAction, PolicyScope,
        SubAgentState, UserId, WorkspaceId,
    };
    use uuid::Uuid;

    use super::VoState;
    use super::test_support::FakeReader;
    use crate::objects::session::SessionVoState;
    use crate::objects::sub_agent::SubAgentVoState;
    use crate::objects::workspace::{WorkspaceApprovalPolicy, WorkspaceConfig, WorkspaceVoState};

    fn test_message(text: &str) -> moa_core::UserMessage {
        moa_core::UserMessage {
            text: text.to_string(),
            attachments: vec![Attachment {
                name: "a.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                url: None,
                path: None,
                size_bytes: Some(3),
            }],
        }
    }

    fn test_meta() -> moa_core::SessionMeta {
        moa_core::SessionMeta {
            workspace_id: WorkspaceId::new("workspace-1"),
            user_id: UserId::new("user-1"),
            platform: Platform::Desktop,
            model: ModelId::new("test-model"),
            ..moa_core::SessionMeta::default()
        }
    }

    fn initial_task() -> moa_core::SubAgentMessage {
        moa_core::SubAgentMessage::InitialTask {
            task: "summarize repo status".to_string(),
            tool_subset: vec!["web_fetch".to_string()],
            budget_tokens: 512,
            parent_session: moa_core::SessionId::new(),
            parent_sub_agent: None,
            depth: 1,
            result_awakeable_id: "awake-1".to_string(),
            workspace_id: WorkspaceId::new("workspace-1"),
            user_id: UserId::new("user-1"),
            model: ModelId::new("test-model"),
        }
    }

    fn test_workspace_config() -> WorkspaceConfig {
        WorkspaceConfig {
            id: WorkspaceId::new("workspace-1"),
            name: "Workspace One".to_string(),
            consolidation_hour_utc: 4,
            approval_policy: WorkspaceApprovalPolicy::default(),
        }
    }

    fn test_rule() -> ApprovalRule {
        ApprovalRule {
            id: Uuid::now_v7(),
            workspace_id: WorkspaceId::new("workspace-1"),
            tool: "bash".to_string(),
            pattern: "bash printf*".to_string(),
            action: PolicyAction::Allow,
            scope: PolicyScope::Workspace,
            created_by: UserId::new("user-1"),
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn session_vo_state_round_trips_via_fake_reader() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        state
            .enqueue_message(test_message("hello"))
            .expect("enqueue should succeed");
        state.pending_approval = Some("approval-1".to_string());
        state.children.push(moa_core::SubAgentChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
        });
        state.last_turn_summary = Some("summary".to_string());
        state.set_cancel_flag(moa_core::CancelMode::Soft);

        let reader = FakeReader::default()
            .insert("meta", state.meta.clone().expect("meta should exist"))
            .insert("status", state.status.clone().expect("status should exist"))
            .insert("pending", state.pending.clone())
            .insert(
                "pending_approval",
                state
                    .pending_approval
                    .clone()
                    .expect("pending approval should exist"),
            )
            .insert("children", state.children.clone())
            .insert(
                "last_turn_summary",
                state
                    .last_turn_summary
                    .clone()
                    .expect("summary should exist"),
            )
            .insert(
                "cancel_flag",
                state.cancel_flag.expect("cancel flag should exist"),
            );

        let loaded = SessionVoState::load_from(&reader)
            .await
            .expect("load from fake reader should succeed");
        assert_eq!(loaded, state);
    }

    #[tokio::test]
    async fn sub_agent_vo_state_round_trips_via_fake_reader() {
        let mut state = SubAgentVoState::default();
        state
            .initialize(&initial_task())
            .expect("initial task should seed state");
        state.status = Some(SubAgentState::WaitingApproval);
        state.record_token_usage(200);
        state.history.push(ContextMessage::assistant("done"));
        state.pending_approval = Some("approval-1".to_string());
        state.children.push(moa_core::SubAgentChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
        });
        state.last_turn_summary = Some("summary".to_string());
        state.tools_invoked = 2;
        state.cancel_reason = Some("cancelled".to_string());

        let reader = FakeReader::default()
            .insert("status", state.status.expect("status should exist"))
            .insert(
                "parent_session",
                state.parent_session.expect("parent session should exist"),
            )
            .insert("depth", state.depth)
            .insert("budget_remaining", state.budget_remaining)
            .insert("tokens_used", state.tokens_used)
            .insert(
                "result_awakeable_id",
                state
                    .result_awakeable_id
                    .clone()
                    .expect("awakeable id should exist"),
            )
            .insert("task", state.task.clone().expect("task should exist"))
            .insert("tool_subset", state.tool_subset.clone())
            .insert(
                "workspace_id",
                state.workspace_id.clone().expect("workspace should exist"),
            )
            .insert("user_id", state.user_id.clone().expect("user should exist"))
            .insert("model", state.model.clone().expect("model should exist"))
            .insert("pending", state.pending.clone())
            .insert("history", state.history.clone())
            .insert(
                "pending_approval",
                state
                    .pending_approval
                    .clone()
                    .expect("pending approval should exist"),
            )
            .insert("children", state.children.clone())
            .insert(
                "last_turn_summary",
                state
                    .last_turn_summary
                    .clone()
                    .expect("summary should exist"),
            )
            .insert("tools_invoked", state.tools_invoked)
            .insert(
                "cancel_reason",
                state
                    .cancel_reason
                    .clone()
                    .expect("cancel reason should exist"),
            );

        let loaded = SubAgentVoState::load_from(&reader)
            .await
            .expect("load from fake reader should succeed");
        assert_eq!(loaded, state);
    }

    #[tokio::test]
    async fn workspace_vo_state_round_trips_via_fake_reader() {
        let state = WorkspaceVoState {
            config: Some(test_workspace_config()),
            approval_policy: WorkspaceApprovalPolicy {
                rules: vec![test_rule()],
            },
            last_consolidation: Some(Utc::now()),
            next_consolidation: Some(Utc::now() + chrono::Duration::days(1)),
            consolidation_in_progress: true,
        };

        let reader = FakeReader::default()
            .insert("config", state.config.clone().expect("config should exist"))
            .insert("approval_policy", state.approval_policy.clone())
            .insert(
                "last_consolidation",
                state
                    .last_consolidation
                    .expect("last consolidation should exist"),
            )
            .insert(
                "next_consolidation",
                state
                    .next_consolidation
                    .expect("next consolidation should exist"),
            )
            .insert("consolidation_in_progress", state.consolidation_in_progress);

        let loaded = WorkspaceVoState::load_from(&reader)
            .await
            .expect("load from fake reader should succeed");
        assert_eq!(loaded, state);
    }
}
