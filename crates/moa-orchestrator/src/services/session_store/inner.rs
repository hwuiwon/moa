//! Backend calls used by Restate session-store handlers.

use super::*;
use moa_authz::{enqueue, enqueue_raw};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use moa_core::traits::{Identity, IdentityType};

impl SessionStoreImpl {
    #[cfg(test)]
    pub(super) async fn create_session_inner(
        &self,
        meta: SessionMeta,
    ) -> Result<SessionId, HandlerError> {
        self.store
            .create_session(meta)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn create_session_authorized_inner(
        &self,
        meta: SessionMeta,
        identity: Identity,
    ) -> Result<SessionId, HandlerError> {
        let (owner_user_type, owner_id) = owner_tuple_subject(&identity)?;
        let tenant_id = identity.tenant_id;
        let workspace_id = meta.workspace_id.clone();
        let mut transaction = self
            .store
            .pool()
            .begin()
            .await
            .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;

        let session_id = self
            .store
            .create_session_in_tx(&mut transaction, meta)
            .await
            .map_err(HandlerError::from)?;

        let owner_tuple = TupleKey::new(
            owner_user_type,
            owner_id,
            Relation::Owner,
            ObjectType::Session,
            session_id.0,
        );
        enqueue(
            &mut *transaction,
            TupleOp::Write,
            &owner_tuple,
            Some(tenant_id),
        )
        .await
        .map_err(|error| TerminalError::new(format!("authz outbox owner tuple: {error}")))?;

        enqueue_raw(
            &mut *transaction,
            TupleOp::Write,
            &format!("workspace:{workspace_id}"),
            "workspace",
            &format!("session:{session_id}"),
            Some(tenant_id),
        )
        .await
        .map_err(|error| TerminalError::new(format!("authz outbox parent tuple: {error}")))?;

        transaction
            .commit()
            .await
            .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
        self.store
            .refresh_active_session_metric()
            .await
            .map_err(HandlerError::from)?;

        Ok(session_id)
    }

    pub(super) async fn append_event_inner(
        &self,
        request: AppendEventRequest,
    ) -> Result<u64, HandlerError> {
        if matches!(&request.event, Event::Error { .. }) {
            record_session_error("event_log");
        }
        self.store
            .emit_event(request.session_id, request.event)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn get_events_inner(
        &self,
        request: GetEventsRequest,
    ) -> Result<Vec<EventRecord>, HandlerError> {
        self.store
            .get_events(request.session_id, request.range)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn get_session_inner(
        &self,
        session_id: SessionId,
    ) -> Result<SessionMeta, HandlerError> {
        self.store
            .get_session(session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn update_status_inner(
        &self,
        request: UpdateStatusRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .update_status(request.session_id, request.status)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn search_events_inner(
        &self,
        request: SearchEventsRequest,
    ) -> Result<Vec<EventRecord>, HandlerError> {
        self.store
            .search_events(&request.query, request.filter)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn list_sessions_inner(
        &self,
        request: ListSessionsRequest,
    ) -> Result<Vec<SessionSummary>, HandlerError> {
        self.store
            .list_sessions(request.filter)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn workspace_cost_since_inner(
        &self,
        request: WorkspaceCostSinceRequest,
    ) -> Result<u32, HandlerError> {
        self.store
            .workspace_cost_since(&request.workspace_id, request.since)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn create_segment_inner(
        &self,
        request: CreateSegmentRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .create_segment(&request.segment)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn complete_segment_inner(
        &self,
        request: CompleteSegmentRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .complete_segment(request.segment_id, request.update)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn get_active_segment_inner(
        &self,
        session_id: SessionId,
    ) -> Result<Option<TaskSegment>, HandlerError> {
        self.store
            .get_active_segment(session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn list_segments_inner(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<TaskSegment>, HandlerError> {
        self.store
            .list_segments(session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn update_segment_resolution_inner(
        &self,
        request: UpdateSegmentResolutionRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .update_segment_resolution(request.segment_id, &request.resolution, request.confidence)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn update_segment_resolution_score_inner(
        &self,
        request: UpdateSegmentResolutionScoreRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .update_segment_resolution_score(request.segment_id, &request.score)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn get_segment_baseline_inner(
        &self,
        request: GetSegmentBaselineRequest,
    ) -> Result<Option<SegmentBaseline>, HandlerError> {
        self.store
            .get_segment_baseline(&request.tenant_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn list_skill_resolution_rates_inner(
        &self,
        request: ListSkillResolutionRatesRequest,
    ) -> Result<Vec<SkillResolutionRate>, HandlerError> {
        self.store
            .list_skill_resolution_rates(&request.tenant_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn refresh_segment_materialized_views_inner(
        &self,
    ) -> Result<(), HandlerError> {
        self.store
            .refresh_segment_materialized_views()
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn record_segment_tool_use_inner(
        &self,
        request: RecordSegmentToolUseRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .record_active_segment_tool_use(request.session_id, &request.tool_name)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn record_segment_skill_activation_inner(
        &self,
        request: RecordSegmentSkillActivationRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .record_active_segment_skill_activation(request.session_id, &request.skill_name)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn record_segment_turn_usage_inner(
        &self,
        request: RecordSegmentTurnUsageRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .record_active_segment_turn_usage(request.session_id, request.token_cost)
            .await
            .map_err(HandlerError::from)
    }
}

fn owner_tuple_subject(identity: &Identity) -> Result<(UserType, uuid::Uuid), HandlerError> {
    if let Some(api_key_id) = identity.api_key_id {
        return Ok((UserType::ApiKey, api_key_id));
    }

    match identity.identity_type {
        IdentityType::User => Ok((UserType::User, identity.id)),
        IdentityType::Agent => Ok((UserType::Agent, identity.id)),
        IdentityType::Service => {
            Err(TerminalError::new_with_code(403, "service identities cannot own sessions").into())
        }
    }
}
