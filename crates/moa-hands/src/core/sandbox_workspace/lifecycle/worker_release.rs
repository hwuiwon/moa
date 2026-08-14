//! Worker sandbox checkpoint and compute-release boundary.

use super::*;
use moa_core::types::worker::state::WorkerInputTarget;

use crate::core::{
    WorkerHandReleaseFence, WorkerHandReleaseRequest, lifecycle::workspace_lease_scope,
};

impl ToolRouter {
    /// Captures the exact live worker attachment allowed to enter an input wait.
    ///
    /// Callers must journal this result before asking to park the hand. `None`
    /// proves there was no live hand at capture time; a later hand therefore cannot
    /// be released by replaying that request.
    pub async fn capture_worker_hand_release_fence(
        &self,
        session: &SessionMeta,
        worker_id: &str,
    ) -> Result<Option<WorkerHandReleaseFence>> {
        if worker_id.trim().is_empty() {
            return Err(MoaError::ValidationError(
                "worker hand release fence requires a worker".to_string(),
            ));
        }
        let workspace_scope = SandboxWorkspaceScope::Worker {
            session_id: session.id,
            worker_id: worker_id.to_string(),
        };
        let lease_scope = workspace_lease_scope(&workspace_scope);
        let scope_key = crate::core::HandScopeKey::new(session.tenant_id, session.id, &lease_scope);
        let Some(repository) = self.hands.workspace_repository.as_ref() else {
            self.verify_worker_hand_absent(
                session,
                &lease_scope,
                &scope_key,
                "worker-hand-release-fence",
            )
            .await?;
            return Ok(None);
        };
        let Some(workspace) = repository
            .get_by_scope(session.tenant_id, &workspace_scope)
            .await?
        else {
            self.verify_worker_hand_absent(
                session,
                &lease_scope,
                &scope_key,
                "worker-hand-release-fence",
            )
            .await?;
            return Ok(None);
        };
        let lease_store = self.hands.hand_leases.as_ref().ok_or_else(|| {
            MoaError::StorageError("durable hand lease store missing".to_string())
        })?;
        let lease = lease_store
            .get(
                session.tenant_id,
                session.id,
                &lease_scope,
                &workspace.provider,
            )
            .await?;
        if workspace.state == SandboxWorkspaceState::Ready {
            self.verify_worker_hand_absent(
                session,
                &lease_scope,
                &scope_key,
                "worker-hand-release-fence",
            )
            .await?;
            return Ok(None);
        }
        let lease = lease.ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
            operation_id: "worker-hand-release-fence".to_string(),
        })?;
        let binding = workspace.binding()?;
        if workspace.state != SandboxWorkspaceState::Active
            || lease.status != HandLeaseStatus::Active
            || lease.handle.is_none()
            || lease.attachment != Some(lease_attachment(&binding)?)
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: "worker-hand-release-fence".to_string(),
            });
        }
        Ok(Some(WorkerHandReleaseFence {
            workspace_id: workspace.workspace_id,
            writer_epoch: binding.writer_epoch,
            instance_generation: binding.instance_generation,
            provider: workspace.provider,
            provisioning_operation_id: lease.provisioning_operation_id,
            hand_lease_generation: u64::try_from(lease.generation).map_err(|_| {
                MoaError::StorageError("hand lease generation is invalid".to_string())
            })?,
        }))
    }

    /// Checkpoints and releases one worker's sandbox before a durable human-input wait.
    ///
    /// The wait target supplies the replay identity. A successful return proves that
    /// the worker has no active durable hand lease. Its retained workspace remains
    /// `Ready`; the next sandbox dispatch provisions fresh compute and restores the
    /// exact committed portable checkpoint.
    pub async fn checkpoint_and_release_worker_hand(
        &self,
        request: WorkerHandReleaseRequest<'_>,
    ) -> Result<()> {
        validate_worker_input_target(request.worker_id, request.input_target)?;
        let operation_key = worker_wait_operation_key(request.input_target);
        let workspace_scope = SandboxWorkspaceScope::Worker {
            session_id: request.session.id,
            worker_id: request.worker_id.to_string(),
        };
        let lease_scope = workspace_lease_scope(&workspace_scope);
        let scope_key = crate::core::HandScopeKey::new(
            request.session.tenant_id,
            request.session.id,
            &lease_scope,
        );
        let repository = match self.hands.workspace_repository.as_ref() {
            Some(repository) => repository,
            None => {
                if request.expected.is_some() {
                    return Err(MoaError::ExternalEffectUnknownOutcome {
                        operation_id: operation_key,
                    });
                }
                self.verify_worker_hand_absent(
                    request.session,
                    &lease_scope,
                    &scope_key,
                    &operation_key,
                )
                .await?;
                return Ok(());
            }
        };
        let Some(initial_workspace) = repository
            .get_by_scope(request.session.tenant_id, &workspace_scope)
            .await?
        else {
            if request.expected.is_some() {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_key,
                });
            }
            self.verify_worker_hand_absent(
                request.session,
                &lease_scope,
                &scope_key,
                &operation_key,
            )
            .await?;
            return Ok(());
        };
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
            .await?;
        if initial_workspace.state == SandboxWorkspaceState::Ready {
            verify_released_worker_fence(
                request.expected,
                &initial_workspace,
                initial_lease.as_ref(),
                &operation_key,
            )?;
            self.verify_worker_hand_absent(
                request.session,
                &lease_scope,
                &scope_key,
                &operation_key,
            )
            .await?;
            return Ok(());
        }
        let initial_lease =
            initial_lease.ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_key.clone(),
            })?;
        if !worker_fence_matches(request.expected, &initial_workspace, &initial_lease)?
            || initial_lease.status != HandLeaseStatus::Active
            || initial_lease.handle.is_none()
            || initial_lease.attachment != Some(lease_attachment(&initial_workspace.binding()?)?)
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_key,
            });
        }
        let hand = initial_lease
            .handle
            .as_ref()
            .map(|handle| handle.handle.clone())
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_key.clone(),
            })?;
        let tool_call_id =
            worker_wait_tool_call_id(initial_workspace.workspace_id, request.input_target);
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

        let mut workspace = repository
            .get_by_scope(request.session.tenant_id, &workspace_scope)
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_key.clone(),
            })?;
        let mut lease = lease_store
            .get(
                request.session.tenant_id,
                request.session.id,
                &lease_scope,
                &initial_workspace.provider,
            )
            .await?
            .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_key.clone(),
            })?;

        // Reconciliation can prove the checkpoint bytes while retaining compute.
        // Finish that exact attachment as a separate fenced destroy step.
        if lease.status == HandLeaseStatus::Active {
            if !self
                .confirmed_workspace_commit_replay(&workspace, tool_call_id)
                .await?
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_key,
                });
            }
            let current_hand = lease
                .handle
                .as_ref()
                .map(|handle| handle.handle.clone())
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_key.clone(),
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
            self.run_within_scope(request.scope, provider.destroy(&current_hand))
                .await
                .map_err(|error| {
                    tracing::warn!(
                        operation_id = %operation_key,
                        error = %error,
                        "worker hand destroy outcome is ambiguous"
                    );
                    MoaError::ExternalEffectUnknownOutcome {
                        operation_id: operation_key.clone(),
                    }
                })?;
            if !repository
                .finalize_checkpointed_hand_destroy(&workspace.binding()?, &lease)
                .await?
            {
                return Err(MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_key,
                });
            }
            let key = crate::core::lifecycle::session_provider_key(
                request.session,
                Some(&lease_scope),
                &initial_workspace.provider,
            );
            self.remove_cached_binding_if_matches(
                &key,
                &current_hand,
                Some(initial_lease.generation),
            )
            .await;
            self.remove_installed_marker(
                crate::core::lifecycle::manifest_scope_key(request.session, Some(&lease_scope)),
                &initial_workspace.provider,
            )
            .await;
            workspace = repository
                .get_by_scope(request.session.tenant_id, &workspace_scope)
                .await?
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_key.clone(),
                })?;
            lease = lease_store
                .get(
                    request.session.tenant_id,
                    request.session.id,
                    &lease_scope,
                    &initial_workspace.provider,
                )
                .await?
                .ok_or_else(|| MoaError::ExternalEffectUnknownOutcome {
                    operation_id: operation_key.clone(),
                })?;
        }

        let exact_generation = lease.generation == initial_lease.generation
            && lease.provisioning_operation_id == initial_lease.provisioning_operation_id;
        if workspace.state != SandboxWorkspaceState::Ready
            || workspace.checkpoint_id.is_none()
            || workspace.checkpoint_generation <= initial_workspace.checkpoint_generation
            || !exact_generation
            || lease.status != HandLeaseStatus::Destroyed
            || lease.handle.is_some()
            || lease.attachment.is_some()
        {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_key,
            });
        }
        self.verify_worker_hand_absent(request.session, &lease_scope, &scope_key, &operation_key)
            .await
    }

    async fn verify_worker_hand_absent(
        &self,
        session: &SessionMeta,
        lease_scope: &str,
        scope_key: &crate::core::HandScopeKey,
        operation_key: &str,
    ) -> Result<()> {
        let durably_live = match self.hands.hand_leases.as_ref() {
            Some(leases) => {
                leases
                    .has_live_owner(session.tenant_id, session.id, lease_scope)
                    .await?
            }
            None => false,
        };
        let cached = self
            .hands
            .active_hands
            .read()
            .await
            .keys()
            .any(|key| &key.scope == scope_key);
        if durably_live || cached {
            return Err(MoaError::ExternalEffectUnknownOutcome {
                operation_id: operation_key.to_string(),
            });
        }
        Ok(())
    }
}

fn validate_worker_input_target(worker_id: &str, target: &WorkerInputTarget) -> Result<()> {
    if worker_id.trim().is_empty()
        || target.turn_id.trim().is_empty()
        || target.generation == 0
        || target.input_request_id.trim().is_empty()
    {
        return Err(MoaError::ValidationError(
            "worker hand park requires a worker, turn, positive generation, and input request"
                .to_string(),
        ));
    }
    Ok(())
}

fn worker_fence_matches(
    expected: Option<&WorkerHandReleaseFence>,
    workspace: &SandboxWorkspace,
    lease: &HandLease,
) -> Result<bool> {
    let Some(expected) = expected else {
        return Ok(false);
    };
    Ok(expected.workspace_id == workspace.workspace_id
        && expected.writer_epoch
            == u64::try_from(workspace.writer_epoch).map_err(|_| {
                MoaError::StorageError("workspace writer epoch is invalid".to_string())
            })?
        && expected.instance_generation
            == u64::try_from(workspace.instance_generation).map_err(|_| {
                MoaError::StorageError("workspace instance generation is invalid".to_string())
            })?
        && expected.provider == workspace.provider
        && expected.provisioning_operation_id == lease.provisioning_operation_id
        && expected.hand_lease_generation
            == u64::try_from(lease.generation).map_err(|_| {
                MoaError::StorageError("hand lease generation is invalid".to_string())
            })?)
}

fn verify_released_worker_fence(
    expected: Option<&WorkerHandReleaseFence>,
    workspace: &SandboxWorkspace,
    lease: Option<&HandLease>,
    operation_key: &str,
) -> Result<()> {
    match (expected, lease) {
        (None, None) => Ok(()),
        (None, Some(lease))
            if lease.status == HandLeaseStatus::Destroyed && lease.handle.is_none() =>
        {
            Ok(())
        }
        (Some(_), Some(lease))
            if worker_fence_matches(expected, workspace, lease)?
                && lease.status == HandLeaseStatus::Destroyed
                && lease.handle.is_none()
                && lease.attachment.is_none() =>
        {
            Ok(())
        }
        _ => Err(MoaError::ExternalEffectUnknownOutcome {
            operation_id: operation_key.to_string(),
        }),
    }
}

fn worker_wait_tool_call_id(
    workspace_id: SandboxWorkspaceId,
    target: &WorkerInputTarget,
) -> ToolCallId {
    ToolCallId(Uuid::new_v5(
        &workspace_id.0,
        worker_wait_operation_key(target).as_bytes(),
    ))
}

fn worker_wait_operation_key(target: &WorkerInputTarget) -> String {
    format!(
        "worker-input-wait-v1:{}:{}:{}:{}:{}",
        target.turn_id.len(),
        target.turn_id,
        target.generation,
        target.input_request_id.len(),
        target.input_request_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_wait_identity_is_generation_fenced_offline() {
        // Pins: reusing one input request id from a different worker generation cannot
        // replay the prior checkpoint-and-release operation.
        let workspace_id = SandboxWorkspaceId::new();
        let first = WorkerInputTarget {
            turn_id: "turn-1".to_string(),
            generation: 3,
            input_request_id: "input-7".to_string(),
        };
        let superseding = WorkerInputTarget {
            generation: 4,
            ..first.clone()
        };
        assert_ne!(
            worker_wait_tool_call_id(workspace_id, &first),
            worker_wait_tool_call_id(workspace_id, &superseding)
        );
    }

    #[test]
    fn worker_wait_identity_rejects_unfenced_targets_offline() {
        // Pins: an unversioned input request cannot own sandbox release.
        let target = WorkerInputTarget {
            turn_id: "turn-1".to_string(),
            generation: 0,
            input_request_id: "input-7".to_string(),
        };
        assert!(validate_worker_input_target("worker-1", &target).is_err());
    }
}
