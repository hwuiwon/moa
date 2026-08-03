//! Execution-run creation, lookup, pagination, and scheduling snapshots.

use super::*;
use super::{
    projection::{budget_ledger, scheduling_projection},
    rows::*,
    sql::*,
};

impl ExecutionRepository {
    /// Creates a run or returns the existing row for the same scoped idempotency key.
    pub async fn create_run(
        &self,
        scope: ExecutionScope,
        new_run: NewExecutionRun,
    ) -> Result<ExecutionRunRecord> {
        validate_new_run(scope, &new_run)?;
        let budget = DbBudgetLimit::try_from(&new_run.approved_budget)?;
        let run_uid = Uuid::now_v7();
        let plan_value = serde_json::to_value(&new_run.plan)?;
        let goal_value = serde_json::to_value(&new_run.goal)?;
        let catalog_value = serde_json::to_value(&new_run.catalog)?;
        let authorization_value = serde_json::to_value(&new_run.authorization)?;
        let pinned_skills_value = serde_json::to_value(&new_run.pinned_instruction_skills)?;
        let source_provenance_value = serde_json::to_value(&new_run.source_provenance)?;
        let source_fields = normalized_source_fields(&new_run.source_provenance);
        let originating_user_sequence_num = to_i64(
            new_run.originating_user_sequence_num,
            "originating user sequence",
        )?;
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(CREATE_RUN_SQL)
            .bind(run_uid)
            .bind(new_run.tenant_id.0)
            .bind(new_run.contact_id.map(|value| value.0))
            .bind(new_run.session_id.0)
            .bind(originating_user_sequence_num)
            .bind(new_run.planning_context_uid)
            .bind(new_run.planning_context_hash.to_string())
            .bind(new_run.owner_user_id.as_str())
            .bind(goal_value)
            .bind(&plan_value)
            .bind(&plan_value)
            .bind(new_run.plan.plan_hash.to_string())
            .bind(new_run.plan.plan_hash.to_string())
            .bind(catalog_value)
            .bind(authorization_value)
            .bind(pinned_skills_value)
            .bind(source_provenance_value)
            .bind(source_fields.kind.as_str())
            .bind(source_fields.skill_template_ref)
            .bind(source_fields.skill_template_revision_uid)
            .bind(new_run.input)
            .bind(new_run.status.as_str())
            .bind(budget.max_cost_microusd)
            .bind(budget.max_tokens)
            .bind(budget.max_tasks)
            .bind(budget.max_tool_calls)
            .bind(budget.max_retrieved_bytes)
            .bind(budget.deadline_at)
            .bind(0_i64)
            .bind(new_run.idempotency_key.as_deref())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;

        let record = if let Some(row) = row {
            run_from_row(&row)?
        } else if let Some(idempotency_key) = new_run.idempotency_key.as_deref() {
            let row = sqlx::query(LOAD_RUN_BY_IDEMPOTENCY_SQL)
                .bind(new_run.tenant_id.0)
                .bind(new_run.contact_id.map(|value| value.0))
                .bind(idempotency_key)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?
                .ok_or_else(|| Error::Storage {
                    message: "idempotent run insert conflicted without a visible existing row"
                        .to_string(),
                })?;
            run_from_row(&row)?
        } else {
            return Err(Error::Storage {
                message: "execution run insert conflicted without an idempotency key".to_string(),
            });
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// Loads one visible execution run.
    pub async fn load_run(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionRunRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Loads one visible task under its owning run and stable task ID.
    pub async fn load_task(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
    ) -> Result<Option<ExecutionTaskRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_TASK_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(task_from_row).transpose()
    }

    /// Loads a visible run for one scope-local idempotency key.
    pub async fn load_run_by_idempotency_key(
        &self,
        scope: ExecutionScope,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        idempotency_key: &str,
    ) -> Result<Option<ExecutionRunRecord>> {
        if !scope.permits_owner(tenant_id, contact_id) {
            return Err(Error::InvalidRepositoryInput {
                message: "idempotency lookup owner does not match repository scope".to_string(),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_RUN_BY_IDEMPOTENCY_SQL)
            .bind(tenant_id.0)
            .bind(contact_id.map(|value| value.0))
            .bind(idempotency_key)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Lists one bounded, stable page of visible execution runs.
    pub async fn list_runs(
        &self,
        scope: ExecutionScope,
        page: ExecutionRunPageRequest,
    ) -> Result<ExecutionRunPage> {
        let limit = if page.limit == 0 {
            DEFAULT_RUN_PAGE_LIMIT
        } else {
            page.limit.min(MAX_RUN_PAGE_LIMIT)
        };
        let mut conn = scope.begin(&self.pool).await?;
        let rows = sqlx::query(LIST_RUNS_SQL)
            .bind(page.cursor.map(|cursor| cursor.created_at))
            .bind(page.cursor.map(|cursor| cursor.run_uid))
            .bind(i64::from(limit) + 1)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        let mut runs = rows.iter().map(run_from_row).collect::<Result<Vec<_>>>()?;
        let has_more = runs.len() > limit as usize;
        if has_more {
            let _ = runs.pop();
        }
        let next_cursor = if has_more {
            runs.last().map(|run| ExecutionRunCursor {
                created_at: run.created_at,
                run_uid: run.run_uid,
            })
        } else {
            None
        };
        Ok(ExecutionRunPage { runs, next_cursor })
    }

    /// Loads one repeatable-read scheduling snapshot with its complete ordered task projection.
    pub async fn load_scheduling_snapshot(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionSchedulingSnapshot>> {
        let mut conn = self.pool.begin().await.map_err(sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *conn)
            .await
            .map_err(sqlx_error)?;
        install_execution_scope(&mut conn, scope).await?;
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *conn)
            .await
            .map_err(sqlx_error)?;
        let Some(row) = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(sqlx_error)?;
            return Ok(None);
        };
        let run = run_from_row(&row)?;
        let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
            .bind(run_uid)
            .fetch_all(&mut *conn)
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(sqlx_error)?;
        let tasks = task_rows
            .iter()
            .map(task_from_row)
            .collect::<Result<Vec<_>>>()?;
        let projection = scheduling_projection(&run, &tasks);
        Ok(Some(ExecutionSchedulingSnapshot {
            catalog: run.catalog.clone(),
            authorization: run.authorization.clone(),
            pinned_instruction_skills: run.pinned_instruction_skills.clone(),
            budget_ledger: budget_ledger(&run),
            run,
            projection,
        }))
    }

    /// Loads one terminal run and derives its compact session delivery from the same snapshot.
    pub async fn load_terminal_delivery(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionTerminalDelivery>> {
        let Some(snapshot) = self.load_scheduling_snapshot(scope, run_uid).await? else {
            return Ok(None);
        };
        execution_terminal_delivery_from_state(&snapshot.run, &snapshot.projection).map(Some)
    }

    /// Acknowledges only the exact current wake epoch, preserving any later wake.
    pub async fn ack_run_wake(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_wake_epoch: u64,
    ) -> Result<WakeAckOutcome> {
        let expected = to_i64(expected_wake_epoch, "wake epoch")?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(WakeAckOutcome::NotFound);
        };
        let run = run_from_row(&row)?;
        let outcome = if run.wake_epoch != expected_wake_epoch {
            if expected_wake_epoch <= run.processed_wake_epoch {
                WakeAckOutcome::Replayed {
                    processed_wake_epoch: run.processed_wake_epoch,
                }
            } else {
                WakeAckOutcome::Changed {
                    current_wake_epoch: run.wake_epoch,
                }
            }
        } else if run.processed_wake_epoch >= expected_wake_epoch {
            WakeAckOutcome::Replayed {
                processed_wake_epoch: run.processed_wake_epoch,
            }
        } else {
            let updated = sqlx::query(
                "UPDATE moa.execution_run SET processed_wake_epoch = $2, updated_at = NOW() \
                 WHERE run_uid = $1 AND wake_epoch = $2 AND processed_wake_epoch < $2",
            )
            .bind(run_uid)
            .bind(expected)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if updated.rows_affected() == 1 {
                WakeAckOutcome::Acknowledged {
                    processed_wake_epoch: expected_wake_epoch,
                }
            } else {
                return Err(Error::Storage {
                    message: "wake acknowledgement lost its locked compare-and-set".to_string(),
                });
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }
}

pub(super) fn validate_new_run(scope: ExecutionScope, new_run: &NewExecutionRun) -> Result<()> {
    if new_run
        .contact_id
        .is_some_and(|contact_id| contact_id.0.is_nil())
    {
        return Err(Error::InvalidRepositoryInput {
            message: "execution run contact_id must not be nil".to_string(),
        });
    }
    if !scope.permits_owner(new_run.tenant_id, new_run.contact_id) {
        return Err(Error::InvalidRepositoryInput {
            message: "run owner does not match the repository scope".to_string(),
        });
    }
    if !matches!(
        new_run.status,
        ExecutionRunStatus::AwaitingConfirmation | ExecutionRunStatus::Queued
    ) {
        return Err(Error::InvalidRepositoryInput {
            message: "new runs must start awaiting_confirmation or queued".to_string(),
        });
    }
    if new_run.plan.estimate.tasks == 0 {
        return Err(Error::InvalidRepositoryInput {
            message: "a canonical run plan must estimate at least one logical task".to_string(),
        });
    }
    if new_run.catalog.catalog_hash != new_run.plan.catalog_hash {
        return Err(Error::InvalidRepositoryInput {
            message: "persisted catalog hash does not match the canonical plan".to_string(),
        });
    }
    new_run
        .source_provenance
        .validate(&new_run.plan.plan_hash.to_string())
        .map_err(|error| Error::InvalidRepositoryInput {
            message: format!("invalid execution source provenance: {error}"),
        })?;
    let mut pinned = new_run.pinned_instruction_skills.clone();
    pinned.sort_by(|left, right| {
        left.skill_ref
            .to_string()
            .cmp(&right.skill_ref.to_string())
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });
    if pinned != new_run.pinned_instruction_skills
        || pinned.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(Error::InvalidRepositoryInput {
            message: "pinned instruction skills must be sorted and duplicate-free".to_string(),
        });
    }
    if new_run
        .pinned_instruction_skills
        .iter()
        .any(|pinned| !new_run.authorization.skill_refs.contains(&pinned.skill_ref))
    {
        return Err(Error::InvalidRepositoryInput {
            message: "pinned instruction skills must be present in the authorization envelope"
                .to_string(),
        });
    }
    Ok(())
}

pub(super) struct NormalizedSourceFields<'a> {
    kind: ExecutionSourceKind,
    skill_template_ref: Option<&'a str>,
    skill_template_revision_uid: Option<Uuid>,
}

pub(super) fn normalized_source_fields(
    provenance: &ExecutionSourceProvenance,
) -> NormalizedSourceFields<'_> {
    match provenance {
        ExecutionSourceProvenance::GeneratedPlan { .. } => NormalizedSourceFields {
            kind: ExecutionSourceKind::GeneratedPlan,
            skill_template_ref: None,
            skill_template_revision_uid: None,
        },
        ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref,
            skill_template_revision_uid,
        } => NormalizedSourceFields {
            kind: ExecutionSourceKind::SkillTemplate,
            skill_template_ref: Some(skill_template_ref.as_str()),
            skill_template_revision_uid: Some(*skill_template_revision_uid),
        },
        ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        } => NormalizedSourceFields {
            kind: ExecutionSourceKind::ExperimentTemplate,
            skill_template_ref: Some(skill_template_ref.as_str()),
            skill_template_revision_uid: Some(*skill_template_revision_uid),
        },
    }
}
