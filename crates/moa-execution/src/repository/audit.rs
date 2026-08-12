//! Execution-template admission and normalized planning audit persistence.

use super::*;

/// Input used to insert one immutable origin-bound planning-context snapshot.
#[derive(Clone, Debug)]
pub struct NewExecutionPlanningContext {
    /// Exact immutable snapshot whose canonical bytes are hashed.
    pub snapshot: ExecutionPlanningContextSnapshot,
    /// Domain-separated hash of the canonical snapshot bytes.
    pub planning_context_hash: ExecutionHash,
}

/// Persisted immutable planning-context projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionPlanningContextRecord {
    /// Durable planning-context identifier.
    pub planning_context_uid: Uuid,
    /// Exact immutable snapshot.
    pub snapshot: ExecutionPlanningContextSnapshot,
    /// Domain-separated hash of the canonical snapshot bytes.
    pub planning_context_hash: ExecutionHash,
    /// Database-owned creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Result of inserting or replaying one unique origin-bound planning context.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanningContextWriteOutcome {
    /// The immutable snapshot was inserted.
    Created(ExecutionPlanningContextRecord),
    /// The exact immutable snapshot already existed for the origin.
    Replayed(ExecutionPlanningContextRecord),
    /// The unique origin already exists with different immutable bytes or scope.
    Conflict,
}

/// Persisted low-cardinality evidence for one route-audit insertion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteAuditEvidence {
    /// Deterministic UUIDv5 audit identifier.
    pub audit_uid: Uuid,
    /// Respond, Execute, or NeedsInput decision.
    pub decision: ExecutionRouteKind,
    /// Selected strategy, present exactly for Execute.
    pub strategy: Option<ExecutionStrategy>,
    /// Redacted trusted-bypass or classifier provenance.
    pub provenance: ExecutionRouteProvenance,
    /// First durable acceptance timestamp.
    pub accepted_at: DateTime<Utc>,
}

/// Durable result of inserting one normalized route audit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RouteAuditWriteOutcome {
    /// This transaction inserted the first route row.
    Applied(RouteAuditEvidence),
    /// The exact semantic route row already existed.
    Replayed(RouteAuditEvidence),
    /// The logical key already carries different route semantics.
    Conflict {
        /// Deterministic audit identifier for the conflicting logical key.
        audit_uid: Uuid,
    },
}

/// Persisted low-cardinality evidence for one planner-call audit insertion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlannerCallAuditEvidence {
    /// Deterministic UUIDv5 audit identifier.
    pub audit_uid: Uuid,
    /// Exact closed planner call kind.
    pub call: ExecutionPlannerCallKind,
    /// Exact closed planner outcome.
    pub outcome: ExecutionPlannerOutcome,
    /// First persisted measured duration.
    pub duration_micros: u64,
    /// Candidate hash when required by the outcome.
    pub candidate_hash: Option<String>,
}

/// Durable result of inserting one normalized planner-call audit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PlannerCallAuditWriteOutcome {
    /// This transaction inserted the first planner-call row.
    Applied(PlannerCallAuditEvidence),
    /// The exact semantic planner-call row already existed.
    Replayed(PlannerCallAuditEvidence),
    /// The logical key already carries different planner-call semantics.
    Conflict {
        /// Deterministic audit identifier for the conflicting logical key.
        audit_uid: Uuid,
    },
}

/// Persisted low-cardinality evidence for one compiler-audit insertion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompileAuditEvidence {
    /// Deterministic UUIDv5 audit identifier.
    pub audit_uid: Uuid,
    /// Exact closed compiler source.
    pub source: ExecutionCompileSource,
    /// Exact closed compiler outcome.
    pub outcome: ExecutionCompileOutcome,
    /// First persisted measured duration.
    pub duration_micros: u64,
    /// Hash of the strict compile candidate.
    pub candidate_hash: String,
    /// Accepted final plan hash, when compilation succeeded.
    pub final_plan_hash: Option<String>,
}

/// Durable result of inserting one normalized compiler audit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CompileAuditWriteOutcome {
    /// This transaction inserted the first compiler row.
    Applied(CompileAuditEvidence),
    /// The exact semantic compiler row already existed.
    Replayed(CompileAuditEvidence),
    /// The logical key already carries different compiler semantics.
    Conflict {
        /// Deterministic audit identifier for the conflicting logical key.
        audit_uid: Uuid,
    },
}

const LOAD_PLANNING_CONTEXT_FOR_SESSION_SQL: &str = r#"
    SELECT planning_context_uid, snapshot, planning_context_hash, created_at
    FROM moa.execution_planning_context
    WHERE planning_context_uid = $1
      AND session_id = $2
"#;
use super::{audit_codec::*, rows::*, sql::*};

impl ExecutionRepository {
    /// Reserves or loads one permanent external execution-template admission.
    ///
    /// This intentionally uses control-plane RLS after the Session handler has authorized the
    /// exact parent Session. A tenant-scoped idempotency-key replay with changed contact scope must
    /// load the first row so the caller receives a semantic conflict instead of a hidden row.
    pub async fn reserve_execution_template_admission(
        &self,
        request: &ExecutionTemplateAdmissionRequest,
        operation_uid: Uuid,
        request_fingerprint: &str,
    ) -> Result<ExecutionTemplateAdmissionRecord> {
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        sqlx::query(RESERVE_EXECUTION_TEMPLATE_ADMISSION_SQL)
            .bind(operation_uid)
            .bind(request.tenant_id.0)
            .bind(request.contact_id.map(|contact_id| contact_id.0))
            .bind(request.session_id.0)
            .bind(request.idempotency_key.as_deref())
            .bind(request_fingerprint)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let record =
            load_execution_template_admission(&mut conn, request.tenant_id, operation_uid).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// CAS-records the exact first persisted objective sequence for one admission.
    pub async fn record_execution_template_admission_origin(
        &self,
        tenant_id: TenantId,
        operation_uid: Uuid,
        request_fingerprint: &str,
        originating_user_sequence_num: u64,
    ) -> Result<ExecutionTemplateAdmissionRecord> {
        let sequence = to_i64(
            originating_user_sequence_num,
            "execution-template admission objective sequence",
        )?;
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        sqlx::query(RECORD_EXECUTION_TEMPLATE_ADMISSION_ORIGIN_SQL)
            .bind(operation_uid)
            .bind(tenant_id.0)
            .bind(request_fingerprint)
            .bind(sequence)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let record = load_execution_template_admission(&mut conn, tenant_id, operation_uid).await?;
        if record.request_fingerprint != request_fingerprint
            || record.originating_user_sequence_num != Some(originating_user_sequence_num)
        {
            return Err(Error::InvalidRepositoryInput {
                message:
                    "execution-template admission objective CAS conflicts with first persisted evidence"
                        .to_string(),
            });
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// CAS-records the exact durable execution run created for one admission.
    pub async fn record_execution_template_admission_run(
        &self,
        tenant_id: TenantId,
        operation_uid: Uuid,
        request_fingerprint: &str,
        execution_run_uid: Uuid,
    ) -> Result<ExecutionTemplateAdmissionRecord> {
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        sqlx::query(RECORD_EXECUTION_TEMPLATE_ADMISSION_RUN_SQL)
            .bind(operation_uid)
            .bind(tenant_id.0)
            .bind(request_fingerprint)
            .bind(execution_run_uid)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let record = load_execution_template_admission(&mut conn, tenant_id, operation_uid).await?;
        if record.request_fingerprint != request_fingerprint
            || record.execution_run_uid != Some(execution_run_uid)
        {
            return Err(Error::InvalidRepositoryInput {
                message:
                    "execution-template admission run CAS conflicts with first persisted evidence"
                        .to_string(),
            });
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// Inserts or exactly replays one immutable origin-bound planning context.
    pub async fn create_planning_context(
        &self,
        scope: ExecutionScope,
        new_context: NewExecutionPlanningContext,
    ) -> Result<PlanningContextWriteOutcome> {
        let snapshot = &new_context.snapshot;
        if snapshot.schema_version != 1
            || !scope.permits_owner(snapshot.tenant_id, snapshot.contact_id)
            || snapshot
                .contact_id
                .is_some_and(|contact_id| contact_id.0.is_nil())
        {
            return Err(Error::InvalidRepositoryInput {
                message: "planning context scope or schema version is invalid".to_string(),
            });
        }
        let sequence = to_i64(
            snapshot.originating_user_sequence_num,
            "originating user sequence",
        )?;
        let snapshot_value = serde_json::to_value(snapshot)?;
        let planning_context_uid = Uuid::now_v7();
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(CREATE_PLANNING_CONTEXT_SQL)
            .bind(planning_context_uid)
            .bind(snapshot.tenant_id.0)
            .bind(snapshot.contact_id.map(|value| value.0))
            .bind(snapshot.session_id.0)
            .bind(sequence)
            .bind(&snapshot.originating_user_event_hash)
            .bind(snapshot.owner_user_id.as_str())
            .bind(new_context.planning_context_hash.to_string())
            .bind(snapshot_value)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let outcome = if let Some(row) = row {
            PlanningContextWriteOutcome::Created(planning_context_from_row(&row)?)
        } else {
            let row = sqlx::query(LOAD_PLANNING_CONTEXT_BY_ORIGIN_SQL)
                .bind(snapshot.tenant_id.0)
                .bind(snapshot.session_id.0)
                .bind(sequence)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?
                .ok_or_else(|| Error::Storage {
                    message: "planning-context origin conflict had no visible row".to_string(),
                })?;
            let existing = planning_context_from_row(&row)?;
            if existing.snapshot == new_context.snapshot
                && existing.planning_context_hash == new_context.planning_context_hash
            {
                PlanningContextWriteOutcome::Replayed(existing)
            } else {
                PlanningContextWriteOutcome::Conflict
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Loads one visible immutable planning context by durable identifier.
    pub async fn load_planning_context(
        &self,
        scope: ExecutionScope,
        planning_context_uid: Uuid,
    ) -> Result<Option<ExecutionPlanningContextRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_PLANNING_CONTEXT_SQL)
            .bind(planning_context_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(planning_context_from_row).transpose()
    }

    /// Loads one immutable planning context only when it belongs to the expected session.
    pub async fn load_planning_context_for_session(
        &self,
        scope: ExecutionScope,
        planning_context_uid: Uuid,
        expected_session_id: SessionId,
    ) -> Result<Option<ExecutionPlanningContextRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_PLANNING_CONTEXT_FOR_SESSION_SQL)
            .bind(planning_context_uid)
            .bind(expected_session_id.0)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(planning_context_from_row).transpose()
    }

    /// Inserts or exactly replays one normalized route-audit row.
    pub async fn write_route_audit(
        &self,
        scope: ExecutionScope,
        envelope: &ExecutionPlanningAuditEnvelope,
    ) -> Result<RouteAuditWriteOutcome> {
        validate_audit_scope(scope, envelope)?;
        let (
            ExecutionPlanningAuditPayload::Route {
                stage,
                decision,
                strategy,
                provenance,
                accepted_at,
            },
            Some(session_id),
            Some(originating_sequence),
        ) = (
            &envelope.payload,
            envelope.session_id,
            envelope.originating_sequence,
        )
        else {
            return Err(Error::InvalidRepositoryInput {
                message: "route audit requires a session-bound route payload".to_string(),
            });
        };
        let audit_uid = route_audit_uid(
            envelope.tenant_id,
            envelope.contact_id,
            session_id,
            originating_sequence,
            *stage,
        )?;
        let originating_sequence_db = to_i64(originating_sequence, "originating sequence")?;
        let confidence_bps = provenance
            .confidence_bps
            .map(i16::try_from)
            .transpose()
            .map_err(|_| Error::InvalidRepositoryInput {
                message: "route confidence exceeds SMALLINT".to_string(),
            })?;
        let missing_input_count = i16::from(provenance.missing_input_count);
        let mut conn = scope.begin(&self.pool).await?;
        let inserted = sqlx::query(INSERT_ROUTE_AUDIT_SQL)
            .bind(audit_uid)
            .bind(envelope.tenant_id.0)
            .bind(envelope.contact_id.map(|value| value.0))
            .bind(session_id.0)
            .bind(originating_sequence_db)
            .bind(route_stage_label(*stage))
            .bind(route_decision_label(*decision))
            .bind(strategy.map(execution_strategy_label))
            .bind(route_source_label(provenance.source))
            .bind(route_classifier_outcome_label(
                provenance.classifier_outcome,
            ))
            .bind(provenance.provider_model.as_deref())
            .bind(provenance.prompt_version.as_deref())
            .bind(provenance.objective_hash.as_str())
            .bind(provenance.response_hash.as_deref())
            .bind(confidence_bps)
            .bind(missing_input_count)
            .bind(to_i64(
                provenance.usage.input_tokens_uncached,
                "route uncached input tokens",
            )?)
            .bind(to_i64(
                provenance.usage.input_tokens_cache_write,
                "route cache-write input tokens",
            )?)
            .bind(to_i64(
                provenance.usage.input_tokens_cache_read,
                "route cache-read input tokens",
            )?)
            .bind(to_i64(
                provenance.usage.output_tokens,
                "route output tokens",
            )?)
            .bind(to_i64(provenance.cost_microusd, "route cost")?)
            .bind(to_i64(provenance.duration_micros, "route duration")?)
            .bind(*accepted_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let outcome = if let Some(row) = inserted {
            RouteAuditWriteOutcome::Applied(route_audit_from_row(&row)?.evidence)
        } else {
            let row = sqlx::query(LOAD_ROUTE_AUDIT_SQL)
                .bind(envelope.tenant_id.0)
                .bind(envelope.contact_id.map(|value| value.0))
                .bind(session_id.0)
                .bind(originating_sequence_db)
                .bind(route_stage_label(*stage))
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let Some(row) = row else {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(RouteAuditWriteOutcome::Conflict { audit_uid });
            };
            let persisted = route_audit_from_row(&row)?;
            if persisted.audit_uid == audit_uid
                && persisted.stage == *stage
                && persisted.evidence.decision == *decision
                && persisted.evidence.strategy == *strategy
                && route_provenance_semantically_equal(&persisted.evidence.provenance, provenance)
            {
                RouteAuditWriteOutcome::Replayed(persisted.evidence)
            } else {
                RouteAuditWriteOutcome::Conflict { audit_uid }
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Inserts or exactly replays one normalized planner-call audit row.
    pub async fn write_planner_call_audit(
        &self,
        scope: ExecutionScope,
        envelope: &ExecutionPlanningAuditEnvelope,
    ) -> Result<PlannerCallAuditWriteOutcome> {
        validate_audit_scope(scope, envelope)?;
        let (
            ExecutionPlanningAuditPayload::PlannerCall {
                call_kind,
                call_ordinal,
                run_uid,
                plan_revision,
                outcome,
                provider_model,
                prompt_version,
                candidate_hash,
                candidate_json,
                compiler_report,
                duration_micros,
                created_at,
            },
            Some(session_id),
            Some(originating_sequence),
        ) = (
            &envelope.payload,
            envelope.session_id,
            envelope.originating_sequence,
        )
        else {
            return Err(Error::InvalidRepositoryInput {
                message: "planner audit requires a session-bound planner-call payload".to_string(),
            });
        };
        let audit_uid = planner_audit_uid(
            envelope.tenant_id,
            envelope.contact_id,
            session_id,
            originating_sequence,
            *run_uid,
            *plan_revision,
            *call_kind,
            *call_ordinal,
        )?;
        let originating_sequence_db = to_i64(originating_sequence, "originating sequence")?;
        let plan_revision_db = plan_revision
            .map(|value| to_i64(value, "plan revision"))
            .transpose()?;
        let duration_micros_db = to_i64(*duration_micros, "planner duration")?;
        let call_ordinal_db = i16::from(*call_ordinal);
        let mut conn = scope.begin(&self.pool).await?;
        let inserted = sqlx::query(INSERT_PLANNER_AUDIT_SQL)
            .bind(audit_uid)
            .bind(envelope.tenant_id.0)
            .bind(envelope.contact_id.map(|value| value.0))
            .bind(session_id.0)
            .bind(originating_sequence_db)
            .bind(*run_uid)
            .bind(plan_revision_db)
            .bind(planner_call_label(*call_kind))
            .bind(call_ordinal_db)
            .bind(planner_outcome_label(*outcome))
            .bind(provider_model)
            .bind(prompt_version)
            .bind(candidate_hash)
            .bind(candidate_json)
            .bind(compiler_report)
            .bind(duration_micros_db)
            .bind(*created_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let outcome = if let Some(row) = inserted {
            PlannerCallAuditWriteOutcome::Applied(planner_audit_from_row(&row)?.evidence)
        } else {
            let row = sqlx::query(LOAD_PLANNER_AUDIT_SQL)
                .bind(envelope.tenant_id.0)
                .bind(envelope.contact_id.map(|value| value.0))
                .bind(session_id.0)
                .bind(originating_sequence_db)
                .bind(*run_uid)
                .bind(plan_revision_db)
                .bind(planner_call_label(*call_kind))
                .bind(call_ordinal_db)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let Some(row) = row else {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(PlannerCallAuditWriteOutcome::Conflict { audit_uid });
            };
            let persisted = planner_audit_from_row(&row)?;
            if persisted.semantically_matches(audit_uid, envelope) {
                PlannerCallAuditWriteOutcome::Replayed(persisted.evidence)
            } else {
                PlannerCallAuditWriteOutcome::Conflict { audit_uid }
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Inserts or exactly replays one normalized compiler-audit row.
    pub async fn write_compile_audit(
        &self,
        scope: ExecutionScope,
        envelope: &ExecutionPlanningAuditEnvelope,
    ) -> Result<CompileAuditWriteOutcome> {
        validate_audit_scope(scope, envelope)?;
        let ExecutionPlanningAuditPayload::Compile {
            source,
            operation_key,
            run_uid,
            plan_revision,
            outcome,
            candidate_hash,
            final_plan_hash,
            validation_report,
            duration_micros,
            created_at,
        } = &envelope.payload
        else {
            return Err(Error::InvalidRepositoryInput {
                message: "compiler audit requires a compile payload".to_string(),
            });
        };
        let audit_uid = compile_audit_uid(
            envelope.tenant_id,
            envelope.contact_id,
            *source,
            operation_key,
        )?;
        let originating_sequence_db = envelope
            .originating_sequence
            .map(|value| to_i64(value, "originating sequence"))
            .transpose()?;
        let plan_revision_db = plan_revision
            .map(|value| to_i64(value, "plan revision"))
            .transpose()?;
        let duration_micros_db = to_i64(*duration_micros, "compile duration")?;
        let mut conn = scope.begin(&self.pool).await?;
        let inserted = sqlx::query(INSERT_COMPILE_AUDIT_SQL)
            .bind(audit_uid)
            .bind(envelope.tenant_id.0)
            .bind(envelope.contact_id.map(|value| value.0))
            .bind(envelope.session_id.map(|value| value.0))
            .bind(originating_sequence_db)
            .bind(*run_uid)
            .bind(plan_revision_db)
            .bind(compile_source_label(*source))
            .bind(operation_key)
            .bind(compile_outcome_label(*outcome))
            .bind(candidate_hash)
            .bind(final_plan_hash)
            .bind(validation_report)
            .bind(duration_micros_db)
            .bind(*created_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let outcome = if let Some(row) = inserted {
            CompileAuditWriteOutcome::Applied(compile_audit_from_row(&row)?.evidence)
        } else {
            let row = sqlx::query(LOAD_COMPILE_AUDIT_SQL)
                .bind(envelope.tenant_id.0)
                .bind(envelope.contact_id.map(|value| value.0))
                .bind(compile_source_label(*source))
                .bind(operation_key)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let Some(row) = row else {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(CompileAuditWriteOutcome::Conflict { audit_uid });
            };
            let persisted = compile_audit_from_row(&row)?;
            if persisted.semantically_matches(audit_uid, envelope) {
                CompileAuditWriteOutcome::Replayed(persisted.evidence)
            } else {
                CompileAuditWriteOutcome::Conflict { audit_uid }
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }
}

pub(super) async fn load_execution_template_admission(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    operation_uid: Uuid,
) -> Result<ExecutionTemplateAdmissionRecord> {
    let row = sqlx::query(LOAD_EXECUTION_TEMPLATE_ADMISSION_SQL)
        .bind(operation_uid)
        .bind(tenant_id.0)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        .ok_or_else(|| Error::Storage {
            message: "execution-template admission reservation was not visible after insert"
                .to_string(),
        })?;
    let sequence: Option<i64> = row
        .try_get("originating_user_sequence_num")
        .map_err(sqlx_error)?;
    let originating_user_sequence_num =
        sequence
            .map(u64::try_from)
            .transpose()
            .map_err(|_| Error::Storage {
                message: "execution-template admission stored a negative objective sequence"
                    .to_string(),
            })?;
    Ok(ExecutionTemplateAdmissionRecord {
        operation_uid: row.try_get("operation_uid").map_err(sqlx_error)?,
        request_fingerprint: row.try_get("request_fingerprint").map_err(sqlx_error)?,
        originating_user_sequence_num,
        execution_run_uid: row.try_get("execution_run_uid").map_err(sqlx_error)?,
    })
}
