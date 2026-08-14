//! Execution-task and compensation hand release recovery.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecutionReleaseStep {
    DurableReconciliation,
    ProviderIo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistedLeaseReleaseState {
    Missing,
    Destroyed,
    LiveOrAmbiguous,
}

const fn compensation_release_identity_is_verified(
    persisted_identity_present: bool,
    lease_state: PersistedLeaseReleaseState,
) -> bool {
    matches!(
        (persisted_identity_present, lease_state),
        (true, PersistedLeaseReleaseState::Destroyed)
            | (false, PersistedLeaseReleaseState::Missing)
    )
}

fn admit_execution_release_step(
    scope: ToolCallScope<'_>,
    step: ExecutionReleaseStep,
) -> Result<()> {
    match step {
        ExecutionReleaseStep::DurableReconciliation => Ok(()),
        ExecutionReleaseStep::ProviderIo => scope.admit(),
    }
}

impl ToolRouter {
    /// Checkpoints one execution-task workspace and releases its exact compute lease.
    ///
    /// The returned receipt is the durable proof required before a task may yield to
    /// a timer, external callback, pause, or long backoff. Retries return the same
    /// receipt. Provider teardown errors remain ambiguous and never produce release
    /// proof; a later retry reconciles the checkpoint and repeats exact destruction.
    pub async fn checkpoint_and_release_execution_hand(
        &self,
        request: ExecutionHandReleaseRequest<'_>,
    ) -> Result<ExecutionHandReleaseReceipt> {
        if request.attempt_generation == 0 {
            return Err(MoaError::ValidationError(
                "execution task attempt generation must be positive".to_string(),
            ));
        }
        let (task_id, logical_generation) = match request.owner {
            ExecutionHandReleaseOwner::Task {
                task_id,
                logical_generation,
            } if logical_generation > 0 => (task_id, logical_generation),
            ExecutionHandReleaseOwner::Task { .. } => {
                return Err(MoaError::ValidationError(
                    "execution task logical generation must be positive".to_string(),
                ));
            }
            ExecutionHandReleaseOwner::Compensation {
                compensation_id,
                logical_generation,
            } => {
                return self
                    .release_execution_compensation_hand(
                        request,
                        compensation_id,
                        logical_generation,
                    )
                    .await;
            }
        };
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        let contact_id = request
            .session
            .contact
            .as_ref()
            .map(|contact| contact.contact_id);
        if let Some(receipt) = repository
            .get_task_execution_hand_release_receipt(
                request.session.tenant_id,
                contact_id,
                request.run_id,
                task_id,
                logical_generation,
                request.attempt_generation,
            )
            .await?
        {
            return Ok(receipt);
        }

        let absence_receipt_id = Uuid::new_v5(
            &request.run_id.0,
            format!(
                "execution-task-hand-absence-v1:{task_id}:{logical_generation}:{}",
                request.attempt_generation
            )
            .as_bytes(),
        );
        match repository
            .record_absent_task_execution_hand_release_receipt(AbsentTaskHandReleaseIntent {
                receipt_id: absence_receipt_id,
                tenant_id: request.session.tenant_id,
                contact_id,
                run_id: request.run_id,
                task_id,
                logical_generation,
                attempt_generation: request.attempt_generation,
                verified_at: Utc::now(),
            })
            .await
        {
            Ok(receipt) => return Ok(receipt),
            Err(MoaError::ExternalEffectUnknownOutcome { .. }) => {}
            Err(error) => return Err(error),
        }

        let workspace_scope = SandboxWorkspaceScope::ExecutionTask {
            run_id: request.run_id,
            task_id,
        };
        let initial_workspace = repository
            .get_by_scope(request.session.tenant_id, &workspace_scope)
            .await?
            .ok_or_else(|| {
                MoaError::PermissionDenied(
                    "execution-task workspace disappeared before hand release".to_string(),
                )
            })?;
        let release_key = format!(
            "execution-task-yield-v1:{}:{}:{}",
            request.run_id, task_id, request.attempt_generation
        );
        let tool_call_id = ToolCallId(Uuid::new_v5(
            &initial_workspace.workspace_id.0,
            release_key.as_bytes(),
        ));
        let candidate_receipt_id = Uuid::new_v5(
            &initial_workspace.workspace_id.0,
            format!("release-receipt-v1:{release_key}").as_bytes(),
        );
        let lease_scope = workspace_lease_scope(&workspace_scope);
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let initial_lease = lease_store
            .get(
                request.session.tenant_id,
                request.session.id,
                &lease_scope,
                &initial_workspace.provider,
            )
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key.clone(),
            })?;
        let (receipt_id, release_claim_token, requested_at) = repository
            .begin_task_execution_hand_release(TaskHandReleaseIntent {
                receipt_id: candidate_receipt_id,
                contact_id,
                run_id: request.run_id,
                task_id,
                logical_generation,
                attempt_generation: request.attempt_generation,
                deadline_at: request
                    .scope
                    .budget
                    .deadline
                    .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(5)),
                recovery_claim_expires_at: Utc::now() + ChronoDuration::minutes(5),
                workspace: &initial_workspace,
                lease: &initial_lease,
            })
            .await?;

        admit_execution_release_step(
            request.scope,
            if initial_lease.status == HandLeaseStatus::Active {
                ExecutionReleaseStep::ProviderIo
            } else {
                ExecutionReleaseStep::DurableReconciliation
            },
        )?;

        match initial_lease.status {
            HandLeaseStatus::Active => {
                let hand = initial_lease
                    .handle
                    .as_ref()
                    .map(|handle| handle.handle.clone())
                    .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                        operation_id: release_key.clone(),
                    })?;
                self.commit_workspace_after_tool(WorkspaceCommitExecution {
                    session: request.session,
                    workspace_scope: &workspace_scope,
                    tool_call_id,
                    provider_name: &initial_workspace.provider,
                    hand: &hand,
                    call_scope: request.scope,
                    release_compute: true,
                })
                .await?;
            }
            HandLeaseStatus::Destroyed => {
                if !self
                    .confirmed_workspace_commit_replay(&initial_workspace, tool_call_id)
                    .await?
                {
                    return Err(MoaError::ExternalEffectUnknownOutcome {
                        operation_id: release_key.clone(),
                    });
                }
            }
            HandLeaseStatus::Provisioning
            | HandLeaseStatus::Stale
            | HandLeaseStatus::Failed
            | HandLeaseStatus::Reaping => {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                });
            }
        }

        let mut final_workspace = repository
            .get_by_scope(request.session.tenant_id, &workspace_scope)
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key.clone(),
            })?;
        let mut final_lease = lease_store
            .get(
                request.session.tenant_id,
                request.session.id,
                &lease_scope,
                &initial_workspace.provider,
            )
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key.clone(),
            })?;

        // An unknown provider outcome may reconcile the already-verified bytes
        // while conservatively retaining the attachment. Finish the destroy as a
        // separate exact step, then atomically release lease and capacity ownership.
        if final_lease.status == HandLeaseStatus::Active {
            if !self
                .confirmed_workspace_commit_replay(&final_workspace, tool_call_id)
                .await?
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                });
            }
            let hand = final_lease
                .handle
                .as_ref()
                .map(|handle| handle.handle.clone())
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                })?;
            let provider = self
                .hands
                .providers
                .get(&initial_workspace.provider)
                .ok_or_else(|| {
                    MoaError::ProviderError(format!(
                        "hand provider {} is not registered",
                        initial_workspace.provider
                    ))
                })?;
            self.run_within_scope(request.scope, provider.destroy(&hand))
                .await
                .map_err(|error| {
                    tracing::warn!(
                        operation_id = %release_key,
                        error = %error,
                        "execution-task hand destroy outcome is ambiguous"
                    );
                    MoaError::ExternalEffectUnknownOutcome {
                        operation_id: release_key.clone(),
                    }
                })?;
            if !repository
                .finalize_checkpointed_hand_destroy(&final_workspace.binding()?, &final_lease)
                .await?
            {
                // The compute is gone but the durable release did not commit, so the
                // charge is still held and a reconciler owns it. Recorded as ambiguous
                // rather than succeeded so the two are distinguishable on the dashboard.
                record_workspace_release(
                    &initial_workspace.provider,
                    SandboxWorkspaceMetricResult::Ambiguous,
                );
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                });
            }
            // Counted only after provider destruction is verified AND the release
            // receipt commits, which together are what actually free the capacity.
            record_workspace_release(
                &initial_workspace.provider,
                SandboxWorkspaceMetricResult::Succeeded,
            );
            let key = session_provider_key(
                request.session,
                Some(&lease_scope),
                &initial_workspace.provider,
            );
            self.remove_cached_binding_if_matches(&key, &hand, Some(initial_lease.generation))
                .await;
            self.remove_installed_marker(
                manifest_scope_key(request.session, Some(&lease_scope)),
                &initial_workspace.provider,
            )
            .await;
            final_workspace = repository
                .get_by_scope(request.session.tenant_id, &workspace_scope)
                .await?
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                })?;
            final_lease = lease_store
                .get(
                    request.session.tenant_id,
                    request.session.id,
                    &lease_scope,
                    &initial_workspace.provider,
                )
                .await?
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key.clone(),
                })?;
        }

        let operation_id = WorkspaceOperationId(Uuid::new_v5(
            &initial_workspace.workspace_id.0,
            format!("tool-commit-v1:{tool_call_id}").as_bytes(),
        ));
        let checkpoint_id = WorkspaceCheckpointId(operation_id.0);
        let checkpoint = repository
            .get_checkpoint(
                request.session.tenant_id,
                final_workspace.workspace_id,
                checkpoint_id,
            )
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key.clone(),
            })?;
        if final_workspace.state != SandboxWorkspaceState::Ready
            || final_workspace.writer_epoch != initial_workspace.writer_epoch
            || final_workspace.instance_generation != initial_workspace.instance_generation
            || final_workspace.checkpoint_id != Some(checkpoint_id)
            || final_workspace.checkpoint_generation != checkpoint.generation
            || final_lease.status != HandLeaseStatus::Destroyed
            || final_lease.handle.is_some()
            || final_lease.generation != initial_lease.generation
            || final_lease.provisioning_operation_id != initial_lease.provisioning_operation_id
            || checkpoint.state != WorkspaceCheckpointState::Available
            || checkpoint.manifest_digest.is_none()
            || checkpoint.logical_bytes.is_none()
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key,
            });
        }
        let receipt = ExecutionHandReleaseReceipt {
            receipt_id,
            tenant_id: request.session.tenant_id,
            run_id: request.run_id,
            owner: request.owner,
            attempt_generation: request.attempt_generation,
            workspace_id: Some(final_workspace.workspace_id),
            writer_epoch: Some(u64::try_from(final_workspace.writer_epoch).map_err(|_| {
                MoaError::StorageError("workspace writer epoch is invalid".to_string())
            })?),
            instance_generation: Some(u64::try_from(final_workspace.instance_generation).map_err(
                |_| MoaError::StorageError("workspace instance generation is invalid".to_string()),
            )?),
            hand_provisioning_operation_id: Some(initial_lease.provisioning_operation_id),
            hand_lease_generation: Some(u64::try_from(initial_lease.generation).map_err(|_| {
                MoaError::StorageError("hand lease generation is invalid".to_string())
            })?),
            checkpoint_id: Some(checkpoint_id),
            checkpoint_generation: Some(u64::try_from(checkpoint.generation).map_err(|_| {
                MoaError::StorageError("checkpoint generation is invalid".to_string())
            })?),
            checkpoint_manifest_digest: Some(checkpoint.manifest_digest.ok_or_else(|| {
                MoaError::StorageError("verified checkpoint digest is missing".to_string())
            })?),
            checkpoint_logical_bytes: Some(
                u64::try_from(checkpoint.logical_bytes.ok_or_else(|| {
                    MoaError::StorageError("verified checkpoint bytes are missing".to_string())
                })?)
                .map_err(|_| MoaError::StorageError("checkpoint bytes are negative".to_string()))?,
            ),
            requested_at,
            released_at: Utc::now(),
        };
        repository
            .record_task_execution_hand_release_receipt(&receipt, release_claim_token, contact_id)
            .await
    }

    async fn release_execution_compensation_hand(
        &self,
        request: ExecutionHandReleaseRequest<'_>,
        compensation_id: ExecutionCompensationScopeId,
        logical_generation: u64,
    ) -> Result<ExecutionHandReleaseReceipt> {
        if logical_generation == 0 {
            return Err(MoaError::ValidationError(
                "execution compensation logical generation must be positive".to_string(),
            ));
        }
        let repository =
            self.hands.workspace_repository.as_ref().ok_or_else(|| {
                MoaError::StorageError("workspace repository missing".to_string())
            })?;
        let contact_id = request
            .session
            .contact
            .as_ref()
            .map(|contact| contact.contact_id);
        if let Some(receipt) = repository
            .get_compensation_execution_hand_release_receipt(
                request.session.tenant_id,
                request.run_id,
                compensation_id,
                logical_generation,
                request.attempt_generation,
            )
            .await?
        {
            return Ok(receipt);
        }

        let hand_scope = format!(
            "execution_compensation:{}:{}",
            request.run_id, compensation_id
        );
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        if let Some(claim) = repository
            .claim_pending_compensation_execution_hand_release(CompensationHandReleaseClaimIntent {
                tenant_id: request.session.tenant_id,
                contact_id,
                run_id: request.run_id,
                compensation_id,
                logical_generation,
                attempt_generation: request.attempt_generation,
                recovery_claim_expires_at: Utc::now() + ChronoDuration::minutes(5),
            })
            .await?
        {
            let persisted_identity = match (
                claim.hand_provisioning_operation_id,
                claim.hand_lease_generation,
            ) {
                (Some(operation_id), Some(generation)) => Some((operation_id, generation)),
                (None, None) => None,
                _ => {
                    return Err(MoaError::StorageError(
                        "pending compensation release has a partial hand identity".to_string(),
                    ));
                }
            };
            let exact_lease = match persisted_identity {
                Some((operation_id, generation)) => {
                    lease_store
                        .get_exact_generation(
                            request.session.tenant_id,
                            request.session.id,
                            &hand_scope,
                            operation_id,
                            generation,
                        )
                        .await?
                }
                None => None,
            };
            let provider_io_required = exact_lease
                .as_ref()
                .is_some_and(|lease| lease.status != HandLeaseStatus::Destroyed);
            admit_execution_release_step(
                request.scope,
                if provider_io_required {
                    ExecutionReleaseStep::ProviderIo
                } else {
                    ExecutionReleaseStep::DurableReconciliation
                },
            )?;
            if provider_io_required
                && !self
                    .reclaim_hands(
                        request.session.tenant_id,
                        &request.session.id,
                        Some(&hand_scope),
                    )
                    .await
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: format!(
                        "execution-compensation-hand-release:{}:{compensation_id}:{logical_generation}:{}",
                        request.run_id, request.attempt_generation
                    ),
                });
            }
            let exact_lease = match persisted_identity {
                Some((operation_id, generation)) => {
                    lease_store
                        .get_exact_generation(
                            request.session.tenant_id,
                            request.session.id,
                            &hand_scope,
                            operation_id,
                            generation,
                        )
                        .await?
                }
                None => None,
            };
            let lease_state = match exact_lease.as_ref() {
                None => PersistedLeaseReleaseState::Missing,
                Some(lease)
                    if lease.status == HandLeaseStatus::Destroyed && lease.handle.is_none() =>
                {
                    PersistedLeaseReleaseState::Destroyed
                }
                Some(_) => PersistedLeaseReleaseState::LiveOrAmbiguous,
            };
            let exact_released = compensation_release_identity_is_verified(
                persisted_identity.is_some(),
                lease_state,
            );
            let replacement = lease_store
                .has_live_owner(request.session.tenant_id, request.session.id, &hand_scope)
                .await?;
            if !exact_released || replacement {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: format!(
                        "execution-compensation-hand-release:{}:{compensation_id}:{logical_generation}:{}",
                        request.run_id, request.attempt_generation
                    ),
                });
            }
            let hand_lease_generation = claim
                .hand_lease_generation
                .map(u64::try_from)
                .transpose()
                .map_err(|_| {
                    MoaError::StorageError("hand lease generation is invalid".to_string())
                })?;
            return repository
                .record_compensation_execution_hand_release_receipt(
                    &ExecutionHandReleaseReceipt {
                        receipt_id: claim.receipt_id,
                        tenant_id: request.session.tenant_id,
                        run_id: request.run_id,
                        owner: request.owner,
                        attempt_generation: request.attempt_generation,
                        workspace_id: None,
                        writer_epoch: None,
                        instance_generation: None,
                        hand_provisioning_operation_id: claim.hand_provisioning_operation_id,
                        hand_lease_generation,
                        checkpoint_id: None,
                        checkpoint_generation: None,
                        checkpoint_manifest_digest: None,
                        checkpoint_logical_bytes: None,
                        requested_at: claim.requested_at,
                        released_at: Utc::now(),
                    },
                    request.session.id,
                    &hand_scope,
                    claim.claim_token,
                    contact_id,
                )
                .await;
        }
        let leases = lease_store
            .list_live_owner_candidates(request.session.tenant_id, request.session.id, &hand_scope)
            .await?;
        if leases.len() > 1 {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: format!(
                    "execution-compensation-hand-release:{}:{compensation_id}:{logical_generation}:{}",
                    request.run_id, request.attempt_generation
                ),
            });
        }
        let initial_lease = leases.into_iter().next();

        let release_key = format!(
            "execution-compensation-release-v1:{}:{compensation_id}:{logical_generation}:{}",
            request.run_id, request.attempt_generation
        );
        let receipt_id = Uuid::new_v5(&request.run_id.0, release_key.as_bytes());
        let (receipt_id, claim_token, requested_at) = repository
            .begin_compensation_execution_hand_release(CompensationHandReleaseIntent {
                receipt_id,
                tenant_id: request.session.tenant_id,
                contact_id,
                session_id: request.session.id,
                run_id: request.run_id,
                compensation_id,
                logical_generation,
                attempt_generation: request.attempt_generation,
                hand_scope: &hand_scope,
                lease: initial_lease.as_ref(),
                deadline_at: request
                    .scope
                    .budget
                    .deadline
                    .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(5)),
                recovery_claim_expires_at: Utc::now() + ChronoDuration::minutes(5),
            })
            .await?;
        admit_execution_release_step(
            request.scope,
            if initial_lease.is_some() {
                ExecutionReleaseStep::ProviderIo
            } else {
                ExecutionReleaseStep::DurableReconciliation
            },
        )?;
        if initial_lease.is_some()
            && !self
                .reclaim_hands(
                    request.session.tenant_id,
                    &request.session.id,
                    Some(&hand_scope),
                )
                .await
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: release_key,
            });
        }
        if let Some(initial_lease) = initial_lease.as_ref() {
            let exact_lease = lease_store
                .get(
                    request.session.tenant_id,
                    request.session.id,
                    &hand_scope,
                    &initial_lease.provider,
                )
                .await?;
            let exact_destroyed = exact_lease.as_ref().is_some_and(|lease| {
                lease.worker_id == hand_scope
                    && lease.provisioning_operation_id == initial_lease.provisioning_operation_id
                    && lease.generation == initial_lease.generation
                    && lease.status == HandLeaseStatus::Destroyed
                    && lease.handle.is_none()
            });
            let replacement = lease_store
                .has_live_owner(request.session.tenant_id, request.session.id, &hand_scope)
                .await?;
            if !exact_destroyed || replacement {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: release_key,
                });
            }
        }
        let hand_lease_generation = initial_lease
            .as_ref()
            .map(|lease| {
                u64::try_from(lease.generation).map_err(|_| {
                    MoaError::StorageError("hand lease generation is invalid".to_string())
                })
            })
            .transpose()?;
        repository
            .record_compensation_execution_hand_release_receipt(
                &ExecutionHandReleaseReceipt {
                    receipt_id,
                    tenant_id: request.session.tenant_id,
                    run_id: request.run_id,
                    owner: request.owner,
                    attempt_generation: request.attempt_generation,
                    workspace_id: None,
                    writer_epoch: None,
                    instance_generation: None,
                    hand_provisioning_operation_id: initial_lease
                        .as_ref()
                        .map(|lease| lease.provisioning_operation_id),
                    hand_lease_generation,
                    checkpoint_id: None,
                    checkpoint_generation: None,
                    checkpoint_manifest_digest: None,
                    checkpoint_logical_bytes: None,
                    requested_at,
                    released_at: Utc::now(),
                },
                request.session.id,
                &hand_scope,
                claim_token,
                contact_id,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use moa_core::{error::MoaError, types::resource::ResourceBudget};

    use super::{
        ExecutionReleaseStep, PersistedLeaseReleaseState, ToolCallScope,
        admit_execution_release_step, compensation_release_identity_is_verified,
    };

    #[test]
    fn expired_release_budget_allows_reconciliation_but_rejects_new_provider_io() {
        // Pins: retrying an exact release after its five-minute I/O window may return a durable
        // receipt or finalize verified absence, but it must not start fresh provider operations.
        let release_started_at = Utc::now() - Duration::minutes(6);
        let scope = ToolCallScope::unbounded().with_budget(ResourceBudget::until(
            release_started_at + Duration::minutes(5),
        ));

        assert!(
            admit_execution_release_step(scope, ExecutionReleaseStep::DurableReconciliation)
                .is_ok()
        );
        assert!(matches!(
            admit_execution_release_step(scope, ExecutionReleaseStep::ProviderIo),
            Err(MoaError::BudgetExhausted(_))
        ));
    }

    #[test]
    fn persisted_compensation_lease_identity_requires_the_exact_destroyed_row() {
        // Pins: after provider teardown, a persisted op/generation may finalize only against its
        // exact Destroyed row; a missing row is not interchangeable with an attempt that proved
        // it never acquired a hand.
        assert!(compensation_release_identity_is_verified(
            true,
            PersistedLeaseReleaseState::Destroyed,
        ));
        assert!(!compensation_release_identity_is_verified(
            true,
            PersistedLeaseReleaseState::Missing,
        ));
        assert!(!compensation_release_identity_is_verified(
            true,
            PersistedLeaseReleaseState::LiveOrAmbiguous,
        ));
        assert!(compensation_release_identity_is_verified(
            false,
            PersistedLeaseReleaseState::Missing,
        ));
    }
}
