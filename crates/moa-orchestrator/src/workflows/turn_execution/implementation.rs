//! Restate handler implementation and dependency construction for root turns.

use std::{collections::HashMap, sync::Arc};

use moa_config::MoaConfig;
use moa_config::SessionLimitsConfig;
use moa_core::{
    events::TurnFailureActor, events::TurnFailureClass, traits::ChannelAdapter,
    traits::LineageHandle, types::channel::Channel,
};
use moa_hands::ToolRouter;
use moa_observability::{
    record_turn_workflow_outcome, restate_observability::annotate_restate_handler_span,
};
use moa_session::PostgresSessionStore;
use moa_wire::turn::{RunTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress};
use restate_sdk::prelude::*;

use super::{
    TurnExecution, execute_turn_inside_workflow, parse_session_id, parse_turn_id,
    reporting::notify_session_of_outcome, run_post_outcome_assessment,
};
use crate::{
    brain_bridge::TurnRequestPreparer,
    services::llm_gateway::{LLMCompletionOwner, cancel_completion_owner},
    turn::util::meaningful_cancel_reason,
    turn_driver::progress as driver_progress,
    workflows::{
        turn_events::{TurnEventAppender, append_turn_failed},
        turn_progress,
    },
};

/// Concrete `TurnExecution` workflow implementation.
#[derive(Clone)]
pub struct TurnExecutionImpl {
    pub(super) session_store: Arc<PostgresSessionStore>,
    pub(super) config: Arc<MoaConfig>,
    pub(super) tool_router: Arc<ToolRouter>,
    pub(super) lineage: Arc<dyn LineageHandle>,
    pub(super) channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
    pub(super) request_preparer: Arc<TurnRequestPreparer>,
    /// Classifier used to sanitize segment transcripts before any learning
    /// artifact is derived from them.
    pub(super) learning_classifier: Arc<dyn moa_memory_pii::PiiClassifier>,
    event_appender: TurnEventAppender,
}

impl TurnExecutionImpl {
    /// Creates a root-turn workflow with its persistence, tool, lineage, event-append, and delivery dependencies.
    #[must_use]
    pub(crate) fn new(
        session_store: Arc<PostgresSessionStore>,
        config: Arc<MoaConfig>,
        tool_router: Arc<ToolRouter>,
        lineage: Arc<dyn LineageHandle>,
        channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
        event_appender: TurnEventAppender,
        request_preparer: Arc<TurnRequestPreparer>,
    ) -> Self {
        Self {
            session_store,
            config,
            tool_router,
            lineage,
            channel_adapters,
            request_preparer,
            // The deterministic local heuristic, the same one lineage capture
            // uses. Learning sanitization runs inside a durable step, so it must
            // stay synchronous and free of network IO.
            learning_classifier: Arc::new(moa_memory_pii::HeuristicPiiClassifier),
            event_appender,
        }
    }

    /// Replaces the learning-evidence classifier.
    ///
    /// Exists so a workflow test can drive the sanitization gate with an
    /// abstaining, failing, or invalid-span classifier and observe that the
    /// learning path refuses rather than proceeding.
    #[must_use]
    pub fn with_learning_classifier(
        mut self,
        classifier: Arc<dyn moa_memory_pii::PiiClassifier>,
    ) -> Self {
        self.learning_classifier = classifier;
        self
    }

    pub(super) fn session_limits(&self) -> &SessionLimitsConfig {
        &self.config.session_limits
    }

    /// Returns the durable event-append dependency this workflow owns.
    pub(super) fn event_appender(&self) -> &TurnEventAppender {
        &self.event_appender
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

        let session_id = parse_session_id(&request.session_id)?;
        let turn_id = parse_turn_id(&request.turn_id)?;
        let (mut outcome, post_outcome_assessment) =
            match execute_turn_inside_workflow(self, &ctx, &request, session_id, turn_id).await {
                Ok(body) => (
                    TurnOutcome {
                        turn_id: request.turn_id.clone(),
                        kind: body.kind,
                        message: body.message,
                    },
                    body.post_outcome_assessment,
                ),
                Err(err) => {
                    // The error is logged for operators and never persisted: it can
                    // carry provider, tool, and prompt material. The one exception
                    // is a hand-authored terminal rejection code from the closed
                    // allowlist, which is a stable caller-facing contract.
                    tracing::error!(
                        session_id = %request.session_id,
                        turn_id = %request.turn_id,
                        error = ?err,
                        "root turn workflow failed at its catch-all boundary"
                    );
                    let message = super::super::turn_events::safe_terminal_rejection_code(&err)
                        .map(str::to_string)
                        .unwrap_or_default();
                    (
                        TurnOutcome {
                            turn_id: request.turn_id.clone(),
                            kind: TurnOutcomeKind::Failed,
                            message,
                        },
                        None,
                    )
                }
            };

        // One canonical fact for every failed root turn, whether it came from the
        // catch-all boundary or from a body that reported a failed outcome. It is
        // appended before the owner callback below, so the failure survives a lost
        // or retried `record_turn_outcome`, and its dedupe key collapses a replay
        // into the same single event. The outcome keeps an authored stable
        // rejection code when one is present; every other failure carries only
        // the fixed class sentence the append returns.
        if matches!(outcome.kind, TurnOutcomeKind::Failed) {
            // Attribute from the phase the turn died in, read before the terminal
            // phase below overwrites it.
            let class = TurnFailureClass::from(driver_progress::current_phase(&ctx).await?);
            let summary = append_turn_failed(
                self.event_appender(),
                &ctx,
                session_id,
                TurnFailureActor::Coordinator,
                &request.turn_id,
                class,
            )
            .await?;
            if super::super::turn_events::safe_terminal_rejection_code(&outcome.message).is_none() {
                outcome.message = summary;
            }
        }

        let phase = match outcome.kind {
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

        record_turn_workflow_outcome(
            "root",
            super::turn_outcome_kind_label(&outcome.kind),
            moa_core::types::provider::ModelTier::Main,
        );
        notify_session_of_outcome(&ctx, &request.session_id, &request.identity, &outcome).await?;
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
        let Some(reason) = meaningful_cancel_reason(Some(reason.into_inner())) else {
            return Ok(());
        };
        cancel_completion_owner(&ctx, LLMCompletionOwner::root_turn(ctx.key())).await?;
        driver_progress::request_cancel(&ctx, reason).await
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
