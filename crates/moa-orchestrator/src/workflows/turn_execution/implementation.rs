//! Restate handler implementation and dependency construction for root turns.

use std::{collections::HashMap, sync::Arc};

use moa_core::{
    config::MoaConfig,
    config::SessionLimitsConfig,
    traits::ChannelAdapter,
    traits::LineageHandle,
    types::channel::Channel,
    wire::turn::{RunTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress},
};
use moa_hands::ToolRouter;
use moa_observability::{
    record_turn_workflow_outcome, restate_observability::annotate_restate_handler_span,
};
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use serde_json::Value;

use super::{
    TurnExecution, execute_turn_inside_workflow, notify_session_of_outcome, parse_session_id,
    parse_turn_id, run_post_outcome_assessment,
};
use crate::{turn_driver::progress as driver_progress, workflows::turn_progress};

/// Concrete `TurnExecution` workflow implementation.
#[derive(Clone)]
pub struct TurnExecutionImpl {
    pub(super) session_store: Arc<PostgresSessionStore>,
    pub(super) config: Arc<MoaConfig>,
    pub(super) tool_router: Arc<ToolRouter>,
    pub(super) lineage: Arc<dyn LineageHandle>,
    pub(super) channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
}

impl TurnExecutionImpl {
    /// Creates a root-turn workflow with its persistence, tool, lineage, and delivery dependencies.
    #[must_use]
    pub fn new(
        session_store: Arc<PostgresSessionStore>,
        config: Arc<MoaConfig>,
        tool_router: Arc<ToolRouter>,
        _tool_schemas: Arc<Vec<Value>>,
        lineage: Arc<dyn LineageHandle>,
        channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
    ) -> Self {
        Self {
            session_store,
            config,
            tool_router,
            lineage,
            channel_adapters,
        }
    }

    pub(super) fn session_limits(&self) -> &SessionLimitsConfig {
        &self.config.session_limits
    }
}

impl TurnExecution for TurnExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunTurnRequest>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("TurnExecution", "run");
        let request = request.into_inner();

        driver_progress::set_phase(&ctx, TurnPhase::Compiling);
        tracing::info!(
            session_id = %request.session_id,
            turn_id = %request.turn_id,
            "TurnExecution workflow started"
        );

        let session_id = parse_session_id(&request.session_id)?;
        let turn_id = parse_turn_id(&request.turn_id)?;
        let (outcome, post_outcome_assessment) =
            match execute_turn_inside_workflow(self, &ctx, &request, session_id, turn_id).await {
                Ok(body) => {
                    let phase = match body.kind {
                        TurnOutcomeKind::Completed => TurnPhase::Completed,
                        TurnOutcomeKind::Accepted { .. } => TurnPhase::Accepted,
                        TurnOutcomeKind::Cancelled => TurnPhase::Cancelled,
                        TurnOutcomeKind::Failed => TurnPhase::Failed,
                    };
                    turn_progress::finish_with_live_delivery(
                        &ctx,
                        session_id,
                        phase.clone(),
                        self.session_store.clone(),
                        self.channel_adapters.as_ref(),
                    )
                    .await?;
                    driver_progress::set_phase(&ctx, phase);
                    (
                        TurnOutcome {
                            turn_id: request.turn_id.clone(),
                            kind: body.kind,
                            message: body.message,
                        },
                        body.post_outcome_assessment,
                    )
                }
                Err(err) => {
                    turn_progress::finish_with_live_delivery(
                        &ctx,
                        session_id,
                        TurnPhase::Failed,
                        self.session_store.clone(),
                        self.channel_adapters.as_ref(),
                    )
                    .await?;
                    driver_progress::set_phase(&ctx, TurnPhase::Failed);
                    (
                        TurnOutcome {
                            turn_id: request.turn_id.clone(),
                            kind: TurnOutcomeKind::Failed,
                            message: format!("{err:?}"),
                        },
                        None,
                    )
                }
            };

        record_turn_workflow_outcome(
            "root",
            super::turn_outcome_kind_label(&outcome.kind),
            moa_core::types::provider::ModelTier::Main,
        );
        notify_session_of_outcome(&ctx, &request.session_id, &request.identity, &outcome);
        if let Some(assessment) = post_outcome_assessment {
            run_post_outcome_assessment(self, &ctx, assessment).await;
        }
        Ok(Json::from(outcome))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("TurnExecution", "request_cancel");
        driver_progress::request_cancel(&ctx, reason.into_inner()).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<TurnProgress>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("TurnExecution", "progress");
        driver_progress::snapshot(&ctx).await
    }
}
