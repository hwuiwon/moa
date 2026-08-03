//! Worker turn preparation, response, tool, and security handlers.

use super::*;

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(super) struct JournaledWorkerToolCatalog {
    pub(super) tool_schemas: Vec<serde_json::Value>,
    pub(super) tool_catalog_pin: ToolCatalogPin,
}

impl WorkerImpl {
    pub(super) async fn prepare_turn(
        &self,
        mut ctx: ObjectContext<'_>,
    ) -> Result<Json<WorkerTurnPreparation>, HandlerError> {
        annotate_restate_handler_span("Worker", "prepare_turn");
        let state = Tracked::<WorkerVoState>::load(&ctx).await?;
        let identity = state
            .identity
            .clone()
            .ok_or_else(|| TerminalError::new("worker is missing its admitted caller identity"))?;
        let parent_session = required_parent_session(&state)?;
        let session_store = self.session_store.clone();
        let connector_catalogs = self.connector_catalogs.clone();
        let tool_catalog = ctx
            .run(|| async move {
                let session = session_store
                    .get_session(parent_session)
                    .await
                    .map_err(moa_error_to_handler_error)?;
                let catalog = connector_catalogs
                    .for_session(&identity, &session)
                    .await
                    .map_err(|error| moa_error_to_handler_error(error.into_moa_error()))?;
                Ok::<_, HandlerError>(Json::from(JournaledWorkerToolCatalog {
                    tool_schemas: catalog.schemas().as_ref().clone(),
                    tool_catalog_pin: catalog.pin().clone(),
                }))
            })
            .name(format!("worker_prepare_turn_catalog:{parent_session}"))
            .await?
            .into_inner();
        Ok(Json::from(
            prepare_turn_inner(
                &mut ctx,
                state,
                &self.providers,
                &tool_catalog.tool_schemas,
                tool_catalog.tool_catalog_pin,
                &self.session_store,
            )
            .await?,
        ))
    }

    pub(super) async fn record_response(
        &self,
        ctx: ObjectContext<'_>,
        response: Json<WorkerTurnResponseRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "record_response");
        let record = response.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if !state.active_turn_matches(&record.turn_id) {
            tracing::warn!(
                key = %ctx.key(),
                record_turn_id = %record.turn_id,
                active_turn_id = ?state.active_turn_id,
                "ignored stale worker response"
            );
            return Ok(());
        }
        let response = record.response;
        let token_usage = response.token_usage();
        let token_cost = (token_usage.total_input_tokens() + token_usage.output_tokens) as u64;
        state.record_token_usage(token_cost);
        let parent_session = state.parent_session;
        state.last_turn_summary = summarize_response_text(&response);
        let mut appended = Vec::new();
        apply_response_to_history(&mut appended, &response);
        state
            .history
            .extend(appended.into_iter().map(WorkerHistoryEntry::inline));
        claim_check_worker_history(&ctx, &mut state, &self.session_store).await?;
        state.persist(&ctx);

        if let Some(parent_session) = parent_session
            && token_cost > 0
        {
            crate::restate_identity::replay_safe_request(
                ctx.service_client::<RestateSessionStoreClient>()
                    .record_segment_turn_usage(Json(RecordSegmentTurnUsageRequest {
                        session_id: parent_session,
                        token_cost,
                    })),
            )
            .send();
        }
        Ok(())
    }

    pub(super) async fn record_tool_result(
        &self,
        ctx: ObjectContext<'_>,
        record: Json<WorkerToolRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "record_tool_result");
        record_tool_result_inner(
            &ctx,
            record.into_inner(),
            ToolRecordKind::Executed,
            &self.session_store,
        )
        .await
    }

    pub(super) async fn record_denied_tool(
        &self,
        ctx: ObjectContext<'_>,
        record: Json<WorkerToolRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "record_denied_tool");
        record_tool_result_inner(
            &ctx,
            record.into_inner(),
            ToolRecordKind::Denied,
            &self.session_store,
        )
        .await
    }

    // SAFETY: internal workflow delivery of an assessment the router already produced;
    // it reads no caller-owned data back and returns only closed-vocabulary state.
    pub(super) async fn apply_security_assessment(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<moa_wire::turn::ApplySecurityAssessmentRequest>,
    ) -> Result<Json<moa_wire::turn::ApplySecurityAssessmentResponse>, HandlerError> {
        annotate_restate_handler_span("Worker", "apply_security_assessment");
        let request = request.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        // The session that owns this worker also owns the transition key namespace,
        // so a worker's transitions land in its parent session's history.
        let session_id = state.parent_session.ok_or_else(|| {
            HandlerError::from(TerminalError::new(
                "worker has no parent session to scope its security circuit",
            ))
        })?;
        let transition = moa_security::apply_owner_assessment(
            &mut state.security_circuit,
            moa_security::CircuitTarget {
                session_id,
                owner: &request.owner,
                capability: &request.capability,
                tool_call_id: request.tool_call_id,
            },
            &request.assessment,
        );
        let transition = match transition {
            Ok(transition) => transition,
            Err(error)
                if request.allow_superseded_owner_noop
                    && matches!(
                        (error.active.as_ref(), &error.received),
                        (
                            Some(moa_core::types::security::SecurityCircuitOwner::Worker {
                                worker_id: active_worker,
                                generation: active_generation,
                                ..
                            }),
                            moa_core::types::security::SecurityCircuitOwner::Worker {
                                worker_id: received_worker,
                                generation: received_generation,
                                ..
                            }
                        ) if active_worker == received_worker
                            && active_generation > received_generation
                    ) =>
            {
                tracing::info!(
                    worker_id = %ctx.key(),
                    active_owner_generation = error.active.as_ref().map(|owner| owner.generation()),
                    received_owner_generation = error.received.generation(),
                    "discarded superseded reviewed worker security assessment"
                );
                return Ok(Json::from(
                    moa_wire::turn::ApplySecurityAssessmentResponse {
                        transition: None,
                        stage: moa_core::types::security::SecurityCircuitStage::Clear,
                    },
                ));
            }
            Err(error) => {
                tracing::warn!(
                    worker_id = %ctx.key(),
                    active_owner_kind = error.active.as_ref().map(|owner| owner.kind()),
                    active_owner_generation = error.active.as_ref().map(|owner| owner.generation()),
                    received_owner_kind = error.received.kind(),
                    received_owner_generation = error.received.generation(),
                    "rejected stale worker security assessment"
                );
                return Err(TerminalError::new_with_code(
                    409,
                    "security assessment owner is no longer active",
                )
                .into());
            }
        };
        let stage = state
            .security_circuit
            .stage(&request.owner, &request.capability);
        state.persist(&ctx);
        Ok(Json::from(
            moa_wire::turn::ApplySecurityAssessmentResponse { transition, stage },
        ))
    }

    pub(super) async fn apply_turn_outcome(
        &self,
        ctx: ObjectContext<'_>,
        outcome: Json<WorkerTurnOutcomeRecord>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Worker", "apply_turn_outcome");
        let record = outcome.into_inner();
        let mut state = Tracked::<WorkerVoState>::load(&ctx).await?;
        if !state.active_turn_matches(&record.turn_id) {
            tracing::warn!(
                key = %ctx.key(),
                record_turn_id = %record.turn_id,
                active_turn_id = ?state.active_turn_id,
                "ignored stale worker turn outcome"
            );
            return Ok(());
        }
        let outcome = record.outcome;
        if !matches!(
            (state.current_status(), outcome),
            (WorkerState::Failed, TurnOutcome::Idle)
        ) {
            state.apply_turn_outcome(outcome);
        }
        state.persist(&ctx);
        Ok(())
    }
}

pub(super) async fn prepare_turn_inner(
    ctx: &mut ObjectContext<'_>,
    mut state: Tracked<WorkerVoState>,
    providers: &ProviderRegistry,
    tool_schemas: &[serde_json::Value],
    tool_catalog_pin: ToolCatalogPin,
    session_store: &Arc<dyn SessionStore>,
) -> Result<WorkerTurnPreparation, HandlerError> {
    if state.cancel_reason.is_some() {
        state.apply_turn_outcome(TurnOutcome::Cancelled);
        state.persist(ctx);
        return Ok(WorkerTurnPreparation::Outcome {
            outcome: TurnOutcome::Cancelled,
        });
    }
    if state.depth > MAX_WORKER_DEPTH {
        return Err(TerminalError::new(format!(
            "worker depth exceeds maximum ({MAX_WORKER_DEPTH})"
        ))
        .into());
    }
    state
        .ensure_initialized()
        .map_err(moa_error_to_handler_error)?;

    let pending = std::mem::take(&mut state.pending);
    for message in &pending {
        state
            .history
            .push(WorkerHistoryEntry::inline(ContextMessage::user(
                render_user_message(message),
            )));
    }

    if state.budget_exhausted() {
        state.complete_after_budget_exhausted();
        state.persist(ctx);
        return Ok(WorkerTurnPreparation::Outcome {
            outcome: TurnOutcome::Idle,
        });
    }

    let parent_session = state
        .parent_session
        .ok_or_else(|| TerminalError::new("worker parent session missing"))?;
    let tenant_id = state
        .tenant_id
        .ok_or_else(|| TerminalError::new("worker tenant_id missing"))?;
    let user_id = state
        .user_id
        .clone()
        .ok_or_else(|| TerminalError::new("worker user_id missing"))?;
    let model = state
        .model
        .clone()
        .ok_or_else(|| TerminalError::new("worker model missing"))?;

    let mut request = build_completion_request(&state, providers, tool_schemas)?;
    extend_request_with_history(
        &*ctx,
        parent_session,
        &state.history,
        &mut request.messages,
        session_store,
    )
    .await?;
    let active_canary = if request.tools.is_empty() {
        None
    } else {
        let canary = new_canary_token();
        request
            .messages
            .push(ContextMessage::system(canary_system_message(&canary)));
        Some(canary)
    };
    request.metadata.insert(
        "_moa.session_id".to_string(),
        json!(parent_session.to_string()),
    );
    request
        .metadata
        .insert("_moa.tenant_id".to_string(), json!(tenant_id.to_string()));
    request
        .metadata
        .insert("_moa.contact_id".to_string(), json!(user_id.to_string()));
    request
        .metadata
        .insert("_moa.model".to_string(), json!(model.as_str()));
    request
        .metadata
        .insert("_moa.worker_id".to_string(), json!(ctx.key().to_string()));
    request.metadata.insert(
        crate::tool_invocation::governed::TOOL_CATALOG_PIN_METADATA_KEY.to_string(),
        serde_json::to_value(tool_catalog_pin)
            .map_err(|error| TerminalError::new(format!("serialize tool catalog pin: {error}")))?,
    );
    let session_meta = synthetic_session_meta(&state)?;
    state.persist(ctx);

    Ok(WorkerTurnPreparation::Request {
        request: Box::new(request),
        active_canary,
        session_meta: Box::new(session_meta),
        parent_session,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolRecordKind {
    Executed,
    Denied,
}

impl ToolRecordKind {
    fn counts_invocation(self) -> bool {
        matches!(self, Self::Executed)
    }
}

async fn record_tool_result_inner(
    ctx: &ObjectContext<'_>,
    record: WorkerToolRecord,
    kind: ToolRecordKind,
    session_store: &Arc<dyn SessionStore>,
) -> Result<(), HandlerError> {
    let mut state = Tracked::<WorkerVoState>::load(ctx).await?;
    if let Some(turn_id) = record.turn_id.as_deref()
        && !state.active_turn_matches(turn_id)
    {
        tracing::warn!(
            key = %ctx.key(),
            record_turn_id = %turn_id,
            active_turn_id = ?state.active_turn_id,
            "ignored stale worker tool result"
        );
        return Ok(());
    }
    state
        .history
        .push(WorkerHistoryEntry::inline(ContextMessage::tool_result(
            record
                .invocation
                .id
                .clone()
                .unwrap_or_else(|| record.tool_id.0.to_string()),
            record.output.safe_output.to_text(),
            Some(record.output.safe_output.content.clone()),
        )));
    if kind.counts_invocation() {
        state.tools_invoked = state.tools_invoked.saturating_add(1);
    }
    claim_check_worker_history(ctx, &mut state, session_store).await?;
    state.persist(ctx);
    Ok(())
}

/// Offloads aged-out, over-threshold inline history entries to content-addressed blobs.
///
/// Runs after any append to `state.history`. The pure candidate selection keeps the
/// most-recent inline tail resident (no hydration on the hot path) and only offloads older
/// entries whose serialized body crosses the threshold. Each body is stored via
/// `store_text_artifact` inside a journaled `ctx.run`: the blob store is content-addressed,
/// so the recorded blob id is a deterministic function of the body and is reused verbatim on
/// replay. A worker without an owning session has no blob namespace, so its history stays
/// inline.
pub(super) async fn claim_check_worker_history(
    ctx: &ObjectContext<'_>,
    state: &mut WorkerVoState,
    session_store: &Arc<dyn SessionStore>,
) -> Result<(), HandlerError> {
    let Some(session_id) = state.parent_session else {
        return Ok(());
    };
    for (idx, body) in state.history_entries_to_claim_check()? {
        let store = session_store.clone();
        let claim = ctx
            .run(|| async move {
                store
                    .store_text_artifact(session_id, &body)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            })
            .name(format!("worker_history_claim_check_{idx}"))
            .await?
            .into_inner();
        state.claim_history_entry(idx, claim);
    }
    Ok(())
}

/// Appends the worker's buffered history to `out`, hydrating any claim-checked entries.
///
/// Inline entries are cloned directly; a `Claimed` entry's full body is read back from its
/// content-addressed blob and decoded into the original `ContextMessage`. The read is
/// journaled (rather than a bare content-addressed read) so the compiled turn request is
/// byte-identical on replay without re-touching the blob store.
pub(super) async fn extend_request_with_history(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    history: &[WorkerHistoryEntry],
    out: &mut Vec<ContextMessage>,
    session_store: &Arc<dyn SessionStore>,
) -> Result<(), HandlerError> {
    for (idx, entry) in history.iter().enumerate() {
        match entry {
            WorkerHistoryEntry::Inline(message) => out.push(message.clone()),
            WorkerHistoryEntry::Claimed(claimed) => {
                out.push(
                    hydrate_claimed_history_entry(ctx, session_id, idx, claimed, session_store)
                        .await?,
                );
            }
        }
    }
    Ok(())
}

/// Reads one claim-checked history entry's full body back from its blob and decodes it.
pub(super) async fn hydrate_claimed_history_entry(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    idx: usize,
    claimed: &ClaimedHistoryEntry,
    session_store: &Arc<dyn SessionStore>,
) -> Result<ContextMessage, HandlerError> {
    let claim_check = ClaimCheck {
        blob_id: claimed.blob_id.clone(),
        size: claimed.size,
        preview: claimed.preview.clone(),
    };
    let store = session_store.clone();
    let body = ctx
        .run(|| async move {
            store
                .load_text_artifact(session_id, &claim_check)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name(format!("worker_history_hydrate_{idx}"))
        .await?
        .into_inner();
    serde_json::from_str(&body).map_err(|error| {
        HandlerError::from(TerminalError::new(format!(
            "failed to decode claimed worker history entry {}: {error}",
            claimed.blob_id
        )))
    })
}
