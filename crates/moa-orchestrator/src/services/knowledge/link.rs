//! Link-token and public-token exchange logic for the Knowledge service.

use chrono::Utc;
use moa_connectors::{
    domain::{
        ConnectionGeneration, ConnectionStatus as ParentConnectionStatus, ConnectorConnection,
        ManagedParentDefinition,
    },
    service::{
        CredentialGenerationFenceRequest, ManagedParentActivationRequest,
        ManagedParentClaimRequest, ManagedParentDeleteRequest,
    },
};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use moa_knowledge::domain::{
    ApplySourceSelectionRequest, CreateLinkTokenRequest, ExchangePublicTokenRequest,
    KnowledgeConnection, KnowledgeCredentialOwnership, KnowledgeDisconnectReservation,
    KnowledgeDisconnectState, KnowledgeDisconnectTransition, LinkClaim, LinkClaimReservation,
    LinkClaimState, LinkClaimTransition, LinkedAccount, LinkedProviderKind,
    NewKnowledgeConnectionDisconnect, NewLinkClaim, RemoteRevokeRequest,
};
use moa_knowledge::normalize::{normalize_source_selection, redact_provider_metadata};
use moa_wire::knowledge::{
    KnowledgeCreateLinkTokenRequest, KnowledgeCreateLinkTokenResponse,
    KnowledgeDisconnectConnectionRequest, KnowledgeDisconnectConnectionResponse,
    KnowledgeExchangeTokenRequest, KnowledgeExchangeTokenResponse, KnowledgeSyncRequest,
    KnowledgeUpdateConnectionSourceSelectionRequest,
    KnowledgeUpdateConnectionSourceSelectionResponse,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{KnowledgeCaller, KnowledgeService, KnowledgeServiceError};

/// Builds the canonical, secret-free hash fencing one link operation.
///
/// Covers the connection the link claims and the provider account it links, so
/// replaying the same operation reproduces the same hash while reusing the id
/// for a different account or connection is a typed conflict.
fn link_request_hash(tenant_id: TenantId, connection_uid: Uuid, account: &LinkedAccount) -> String {
    let mut hasher = Sha256::new();
    for part in [
        tenant_id.to_string().as_str(),
        connection_uid.to_string().as_str(),
        account.provider.as_str(),
        account.connector.as_str(),
        account.provider_account_id.as_str(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn disconnect_request_hash(tenant_id: TenantId, connection_uid: Uuid) -> String {
    let mut hasher = Sha256::new();
    for part in [
        "moa.knowledge.disconnect.v1",
        tenant_id.to_string().as_str(),
        connection_uid.to_string().as_str(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn disconnect_provider_operation_id(
    tenant_id: TenantId,
    connection_uid: Uuid,
    operation_id: &str,
) -> Uuid {
    let mut hasher = Sha256::new();
    for part in [
        "moa.knowledge.provider-disconnect.v1",
        tenant_id.to_string().as_str(),
        connection_uid.to_string().as_str(),
        operation_id,
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

/// Returns the terminal error a compensated link operation reports.
fn compensated_link_error() -> KnowledgeServiceError {
    KnowledgeServiceError::InvalidRequest(
        "link operation was compensated and cannot be retried under the same id".to_string(),
    )
}

/// Returns the error raised when a concurrent attempt advanced this claim first.
fn lost_link_claim_error(stage: &str) -> KnowledgeServiceError {
    KnowledgeServiceError::InvalidRequest(format!(
        "link claim was advanced concurrently before {stage}"
    ))
}

fn managed_definition_for_parent(
    parent: &ConnectorConnection,
) -> Result<ManagedParentDefinition, KnowledgeServiceError> {
    for definition in [
        ManagedParentDefinition::KnowledgeNango,
        ManagedParentDefinition::KnowledgeMerge,
    ] {
        if parent.definition == definition.definition_ref() {
            return Ok(definition);
        }
    }
    Err(KnowledgeServiceError::InvalidRequest(
        "generic parent is not an exact managed knowledge definition".to_string(),
    ))
}

impl KnowledgeService {
    /// Creates a provider-specific short-lived link token without returning provider secrets.
    pub async fn create_link_token(
        &self,
        request: KnowledgeCreateLinkTokenRequest,
    ) -> Result<KnowledgeCreateLinkTokenResponse, KnowledgeServiceError> {
        let provider_kind = LinkedProviderKind::from_str_exact(&request.provider)
            .ok_or_else(|| KnowledgeServiceError::UnknownProvider(request.provider.clone()))?;
        let provider = self.provider(provider_kind)?;
        let token = provider
            .create_link_token(CreateLinkTokenRequest {
                tenant_id: request.tenant_id,
                connector: request.connector,
                external_account_id: request.external_account_id,
                end_user_email_address: request.end_user_email_address,
                redirect_url: request.redirect_url,
                source_selection: request.source_selection,
            })
            .await?;

        Ok(KnowledgeCreateLinkTokenResponse {
            provider: token.provider.to_string(),
            link_token: token.token,
            link_url: token.link_url,
            expires_at: token.expires_at,
        })
    }

    /// Exchanges a provider public token and links a connection under one claim.
    ///
    /// The whole link — credential write, connection row, and initial provider
    /// sync — runs inside one operation-fenced claim. Replaying the same
    /// operation resumes wherever it stopped and returns the finalized result
    /// once; a failure after the credential exists durably compensates instead
    /// of leaving an unclaimed active version behind.
    pub async fn exchange_public_token(
        &self,
        request: KnowledgeExchangeTokenRequest,
        caller: &KnowledgeCaller,
    ) -> Result<KnowledgeExchangeTokenResponse, KnowledgeServiceError> {
        let tenant_id = request.tenant_id;
        let repository = self.connection_repository(tenant_id);
        let operation_id = caller.step("link");

        // Exchanging the public token is a provider-side read, so replaying it is
        // safe. It has to happen before the claim is consulted at all: the account
        // identity decides which connection this link owns, and therefore the
        // request hash that fences the operation. Short-circuiting on a finalized
        // claim before computing that hash would return a recorded result without
        // ever checking that the inputs still match it.
        let provider_kind = LinkedProviderKind::from_str_exact(&request.provider)
            .ok_or_else(|| KnowledgeServiceError::UnknownProvider(request.provider.clone()))?;
        let account = self
            .provider(provider_kind)?
            .exchange_public_token(ExchangePublicTokenRequest {
                tenant_id,
                connector: request.connector.clone(),
                public_token: request.exchange_token.clone(),
                source_selection: request.source_selection.clone(),
            })
            .await?;

        // Resolve the identifier the upsert will actually own before writing any
        // credential. A compensated first link deletes its projection, so its
        // durable claim remains the replay authority for the original identifier.
        // Otherwise a byte-identical replay would mint a new identifier and
        // conflict with its own request hash before reporting the terminal state.
        let existing_claim = repository.get_link_claim(tenant_id, &operation_id).await?;
        let existing = repository
            .connection_by_provider_account(
                account.provider,
                &account.connector,
                &account.provider_account_id,
            )
            .await?;
        let connection_uid = existing_claim
            .as_ref()
            .map(|claim| claim.connection_uid)
            .or_else(|| {
                existing
                    .as_ref()
                    .map(|connection| connection.connection_uid)
            })
            .unwrap_or_else(Uuid::now_v7);
        let claim = match repository
            .reserve_link_claim(NewLinkClaim {
                tenant_id,
                operation_id: operation_id.clone(),
                request_hash: link_request_hash(tenant_id, connection_uid, &account),
                owner_identity_id: caller.principal().owner_identity(),
                connection_uid,
            })
            .await?
        {
            LinkClaimReservation::Reserved(claim) | LinkClaimReservation::Existing(claim) => claim,
            LinkClaimReservation::Conflict => {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "link operation id was reused with different inputs".to_string(),
                ));
            }
            LinkClaimReservation::ConnectionBusy => {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "another knowledge link is already in progress for this connection".to_string(),
                ));
            }
            LinkClaimReservation::OwnerRequired => {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "an owner identity is required to create a knowledge link".to_string(),
                ));
            }
        };

        match claim.state {
            LinkClaimState::Finalized => {
                return self.finalized_link_response(&claim).await?.ok_or(
                    KnowledgeServiceError::NotFound("finalized knowledge link connection"),
                );
            }
            LinkClaimState::Compensated => return Err(compensated_link_error()),
            LinkClaimState::Compensating => {
                // A previous attempt died mid-undo. Finish it, then report the
                // terminal outcome rather than resurrecting the link.
                self.compensate_link(&claim, caller).await?;
                return Err(compensated_link_error());
            }
            LinkClaimState::Reserved
            | LinkClaimState::ParentClaimed
            | LinkClaimState::CredentialWritten => {}
        }

        let outcome = self.complete_link(&request, &account, &claim, caller).await;
        match outcome {
            Ok(response) => Ok(response),
            Err(error) => {
                // Durably enter compensation before undoing anything, so a crash
                // during the undo resumes as compensation and never as a link.
                self.compensate_link(&claim, caller).await?;
                Err(error)
            }
        }
    }

    /// Drives one reserved claim through credential write, connection, and sync.
    async fn complete_link(
        &self,
        request: &KnowledgeExchangeTokenRequest,
        account: &LinkedAccount,
        claim: &LinkClaim,
        caller: &KnowledgeCaller,
    ) -> Result<KnowledgeExchangeTokenResponse, KnowledgeServiceError> {
        let tenant_id = claim.tenant_id;
        let repository = self.connection_repository(tenant_id);
        let definition =
            ManagedParentDefinition::for_knowledge_provider(account.provider.as_str())?;
        let parent_claim = self
            .connector_connections()?
            .claim_managed_parent(ManagedParentClaimRequest {
                tenant_id,
                operation_id: claim.operation_id.clone(),
                request_hash: claim.request_hash.clone(),
                connection_id: ConnectorConnectionId(claim.connection_uid),
                definition,
                display_name: format!("{} {}", account.provider, account.connector),
                owner_identity_id: claim.owner_identity_id,
            })
            .await?;
        let mut claim = claim.clone();
        if claim.state == LinkClaimState::Reserved {
            claim = repository
                .advance_link_claim(
                    tenant_id,
                    &claim.operation_id,
                    LinkClaimTransition::ParentClaimed {
                        parent_created_by_claim: parent_claim.parent_created_by_claim,
                        credential_expected_generation: parent_claim.connection.generation.get(),
                    },
                )
                .await?
                .ok_or_else(|| lost_link_claim_error("managed parent claim"))?;
        } else if claim.parent_created_by_claim != parent_claim.parent_created_by_claim {
            return Err(KnowledgeServiceError::InvalidRequest(
                "managed parent ownership receipt changed during link replay".to_string(),
            ));
        }
        let expected_generation =
            ConnectionGeneration::new(claim.credential_expected_generation.ok_or_else(|| {
                KnowledgeServiceError::InvalidRequest(
                    "link claim is missing its credential generation fence".to_string(),
                )
            })?)?;

        // The stage call is replay-safe and reconstructs the same host-local
        // receipt after a crash. Persist both its candidate and predecessor
        // before advancing the generic connection generation.
        let staged = self
            .credentials
            .stage_linked_account(tenant_id, claim.connection_uid, caller, account)
            .await?;
        let credential_ownership = staged.credential_ownership();
        let candidate_credential_ref = staged.vault_candidate_reference();
        let previous_vault_credential_ref = staged.previous_vault_reference();
        if claim.state == LinkClaimState::ParentClaimed {
            claim = repository
                .advance_link_claim(
                    tenant_id,
                    &claim.operation_id,
                    LinkClaimTransition::CredentialWritten {
                        credential_ownership,
                        candidate_credential_ref: candidate_credential_ref.clone(),
                        previous_vault_credential_ref: previous_vault_credential_ref.clone(),
                    },
                )
                .await?
                .ok_or_else(|| lost_link_claim_error("credential receipt"))?;
        } else if claim.credential_ownership != Some(credential_ownership)
            || claim.candidate_credential_ref != candidate_credential_ref
            || claim.previous_vault_credential_ref != previous_vault_credential_ref
        {
            return Err(KnowledgeServiceError::InvalidRequest(
                "staged credential receipt changed during link replay".to_string(),
            ));
        }

        let mut parent = self
            .connector_connections()?
            .get(tenant_id, ConnectorConnectionId(claim.connection_uid))
            .await?
            .ok_or(KnowledgeServiceError::NotFound("generic connector parent"))?;
        if staged.is_managed() {
            let fenced_generation = expected_generation.next()?;
            if parent.generation == expected_generation {
                parent = self
                    .connector_connections()?
                    .advance_credential_generation(CredentialGenerationFenceRequest {
                        tenant_id,
                        connection_id: ConnectorConnectionId(claim.connection_uid),
                        expected_generation,
                    })
                    .await?;
            } else if parent.generation != fenced_generation {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "managed parent generation diverged during credential replay".to_string(),
                ));
            }
            self.credentials
                .activate_staged_linked_account(&staged, caller)
                .await?;
        } else if parent.generation != expected_generation {
            return Err(KnowledgeServiceError::InvalidRequest(
                "provider-native parent generation diverged during credential replay".to_string(),
            ));
        }
        if parent.status != ParentConnectionStatus::Active {
            parent = self
                .connector_connections()?
                .activate_managed_knowledge_parent(ManagedParentActivationRequest {
                    tenant_id,
                    connection_id: ConnectorConnectionId(claim.connection_uid),
                    expected_generation: parent.generation,
                    definition,
                })
                .await?;
        }
        if parent.status != ParentConnectionStatus::Active {
            return Err(KnowledgeServiceError::InvalidRequest(
                "managed parent did not activate for knowledge sync".to_string(),
            ));
        }

        let now = Utc::now();
        let connection = repository
            .upsert_connection(KnowledgeConnection {
                connection_uid: claim.connection_uid,
                tenant_id,
                provider: account.provider,
                connector: account.connector.clone(),
                provider_account_id: account.provider_account_id.clone(),
                metadata: redact_provider_metadata(account.metadata.clone()),
                source_selection: normalize_source_selection(request.source_selection.clone()),
                information_barrier: request.information_barrier.clone(),
                created_at: now,
                updated_at: now,
                last_synced_at: None,
            })
            .await?;
        // The upsert resolves its own conflict target, so this is the only proof
        // that the credential is attached to the connection the claim owns.
        if connection.connection_uid != claim.connection_uid {
            return Err(KnowledgeServiceError::InvalidRequest(
                "concurrent link claimed this provider account".to_string(),
            ));
        }

        self.provider(connection.provider)?
            .apply_source_selection(ApplySourceSelectionRequest {
                connection: connection.clone(),
            })
            .await?;

        // The link-owned sync path is what guarantees the durable trigger
        // boundary: it records the run on this claim before dispatching and
        // returns only once the provider's idempotent initial-sync call has been
        // made and recorded for that exact run.
        let claim = repository
            .get_link_claim(tenant_id, &claim.operation_id)
            .await?
            .ok_or_else(|| lost_link_claim_error("initial sync"))?;
        let sync = self
            .sync_connection_for_link(
                KnowledgeSyncRequest {
                    tenant_id,
                    connection_uid: connection.connection_uid,
                    parser: None,
                    max_records: None,
                },
                caller,
                &claim,
            )
            .await?;

        repository
            .advance_link_claim(
                tenant_id,
                &claim.operation_id,
                LinkClaimTransition::Finalized {
                    sync_run_uid: sync.sync_run_uid,
                },
            )
            .await?
            .ok_or_else(|| lost_link_claim_error("finalization"))?;

        Ok(KnowledgeExchangeTokenResponse {
            connection_uid: connection.connection_uid,
            provider: connection.provider.to_string(),
            connector: connection.connector,
            provider_account_id: connection.provider_account_id,
            source_selection: connection.source_selection,
            sync_run_uid: Some(sync.sync_run_uid),
            sync_status: Some(sync.status),
        })
    }

    /// Durably undoes one failed link, revoking only the candidate it wrote.
    ///
    /// Restoration is deliberately conditional: the previous version comes back
    /// only while the connection still points at this claim's candidate. If a
    /// newer link already replaced it, this claim revokes its own candidate and
    /// leaves the newer one alone.
    async fn compensate_link(
        &self,
        claim: &LinkClaim,
        caller: &KnowledgeCaller,
    ) -> Result<(), KnowledgeServiceError> {
        let tenant_id = claim.tenant_id;
        let repository = self.connection_repository(tenant_id);
        let Some(claim) = repository
            .advance_link_claim(
                tenant_id,
                &claim.operation_id,
                LinkClaimTransition::Compensating,
            )
            .await?
        else {
            // Already terminal or already finalized by a concurrent attempt.
            return Ok(());
        };

        if claim.credential_ownership == Some(KnowledgeCredentialOwnership::MoaManaged) {
            let candidate = claim.candidate_credential_ref.as_deref().ok_or_else(|| {
                KnowledgeServiceError::InvalidRequest(
                    "managed link claim is missing its candidate vault receipt".to_string(),
                )
            })?;
            self.credentials
                .rollback_linked_account_activation(
                    tenant_id,
                    claim.connection_uid,
                    candidate,
                    claim.previous_vault_credential_ref.as_deref(),
                    caller,
                )
                .await?;
        } else if claim.credential_ownership == Some(KnowledgeCredentialOwnership::ProviderNative)
            && (claim.candidate_credential_ref.is_some()
                || claim.previous_vault_credential_ref.is_some())
        {
            return Err(KnowledgeServiceError::InvalidRequest(
                "provider-native link claim contains a vault receipt".to_string(),
            ));
        }

        if claim.credential_expected_generation.is_some() && !claim.parent_created_by_claim {
            let connector_id = ConnectorConnectionId(claim.connection_uid);
            let parent = self
                .connector_connections()?
                .get(tenant_id, connector_id)
                .await?
                .ok_or(KnowledgeServiceError::NotFound("generic connector parent"))?;
            let definition = managed_definition_for_parent(&parent)?;
            match parent.status {
                ParentConnectionStatus::Suspended | ParentConnectionStatus::PendingAuth => {
                    self.connector_connections()?
                        .activate_managed_knowledge_parent(ManagedParentActivationRequest {
                            tenant_id,
                            connection_id: connector_id,
                            expected_generation: parent.generation,
                            definition,
                        })
                        .await?;
                }
                ParentConnectionStatus::Active => {}
                ParentConnectionStatus::Disconnecting | ParentConnectionStatus::Deleted => {
                    return Err(KnowledgeServiceError::InvalidRequest(
                        "managed parent entered teardown during link compensation".to_string(),
                    ));
                }
            }
        }

        if claim.parent_created_by_claim {
            repository
                .delete_connection_projection(claim.connection_uid)
                .await?;
            self.connector_connections()?
                .delete_managed_parent_if_unused(ManagedParentDeleteRequest {
                    tenant_id,
                    operation_id: claim.operation_id.clone(),
                    request_hash: claim.request_hash.clone(),
                    connection_id: ConnectorConnectionId(claim.connection_uid),
                })
                .await?;
        }

        repository
            .advance_link_claim(
                tenant_id,
                &claim.operation_id,
                LinkClaimTransition::Compensated,
            )
            .await?;
        Ok(())
    }

    /// Rebuilds the response a finalized claim already produced.
    async fn finalized_link_response(
        &self,
        claim: &LinkClaim,
    ) -> Result<Option<KnowledgeExchangeTokenResponse>, KnowledgeServiceError> {
        if claim.state != LinkClaimState::Finalized {
            return Ok(None);
        }
        let repository = self.connection_repository(claim.tenant_id);
        let Some(connection) = repository.get_connection(claim.connection_uid).await? else {
            return Ok(None);
        };
        let sync_status = match claim.sync_run_uid {
            Some(sync_run_uid) => self
                .sync_repository(claim.tenant_id)
                .get_sync_run(sync_run_uid)
                .await?
                .map(|run| run.status.as_str().to_string()),
            None => None,
        };
        Ok(Some(KnowledgeExchangeTokenResponse {
            connection_uid: connection.connection_uid,
            provider: connection.provider.to_string(),
            connector: connection.connector,
            provider_account_id: connection.provider_account_id,
            source_selection: connection.source_selection,
            sync_run_uid: claim.sync_run_uid,
            sync_status,
        }))
    }

    /// Updates provider-native selected sources for one linked connection.
    pub async fn update_connection_source_selection(
        &self,
        request: KnowledgeUpdateConnectionSourceSelectionRequest,
        caller: &KnowledgeCaller,
    ) -> Result<KnowledgeUpdateConnectionSourceSelectionResponse, KnowledgeServiceError> {
        let repository = self.connection_repository(request.tenant_id);
        let connection = repository
            .get_connection(request.connection_uid)
            .await?
            .ok_or(KnowledgeServiceError::NotFound("knowledge connection"))?;
        if connection.tenant_id != request.tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge connection"));
        }
        let source_selection = normalize_source_selection(request.source_selection);
        let connection = repository
            .update_connection_source_selection(request.connection_uid, source_selection)
            .await?;
        self.provider(connection.provider)?
            .apply_source_selection(ApplySourceSelectionRequest {
                connection: connection.clone(),
            })
            .await?;
        let sync = if request.sync {
            Some(
                self.sync_connection(
                    KnowledgeSyncRequest {
                        tenant_id: request.tenant_id,
                        connection_uid: connection.connection_uid,
                        parser: None,
                        max_records: None,
                    },
                    caller,
                )
                .await?,
            )
        } else {
            None
        };

        Ok(KnowledgeUpdateConnectionSourceSelectionResponse {
            connection_uid: connection.connection_uid,
            source_selection: connection.source_selection,
            sync_run_uid: sync.as_ref().map(|sync| sync.sync_run_uid),
            sync_status: sync.map(|sync| sync.status),
        })
    }

    /// Remotely revokes one linked account and deletes its generic parent lifecycle.
    ///
    /// The durable disconnect row owns the provider send boundary. A replay of
    /// `transmitting` or `unknown_outcome` never calls the provider again and
    /// leaves the generic parent fenced in `disconnecting` for reconciliation.
    pub async fn disconnect_connection(
        &self,
        request: KnowledgeDisconnectConnectionRequest,
        caller: &KnowledgeCaller,
    ) -> Result<KnowledgeDisconnectConnectionResponse, KnowledgeServiceError> {
        let repository = self.connection_repository(request.tenant_id);
        let connection = repository
            .get_connection(request.connection_uid)
            .await?
            .ok_or(KnowledgeServiceError::NotFound("knowledge connection"))?;
        if connection.tenant_id != request.tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge connection"));
        }

        let parent = self.fence_parent_for_disconnect(&connection).await?;
        let request_hash = disconnect_request_hash(request.tenant_id, request.connection_uid);
        let progress = match repository
            .reserve_connection_disconnect(NewKnowledgeConnectionDisconnect {
                tenant_id: request.tenant_id,
                connection_uid: request.connection_uid,
                operation_id: caller.operation_id().to_string(),
                request_hash,
                provider_operation_id: disconnect_provider_operation_id(
                    request.tenant_id,
                    request.connection_uid,
                    caller.operation_id(),
                ),
            })
            .await?
        {
            KnowledgeDisconnectReservation::Reserved(progress)
            | KnowledgeDisconnectReservation::Existing(progress) => progress,
            KnowledgeDisconnectReservation::OperationConflict => {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "disconnect operation id was reused for another connection".to_string(),
                ));
            }
        };

        match progress.state {
            KnowledgeDisconnectState::Deleted | KnowledgeDisconnectState::AlreadyAbsent => {
                return self
                    .finish_confirmed_disconnect(&connection, &parent, caller)
                    .await;
            }
            KnowledgeDisconnectState::Transmitting => {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "knowledge disconnect is awaiting provider outcome reconciliation".to_string(),
                ));
            }
            KnowledgeDisconnectState::UnknownOutcome => {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "knowledge disconnect provider outcome is unknown".to_string(),
                ));
            }
            KnowledgeDisconnectState::FailedBeforeSend => {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "knowledge disconnect failed before provider transmission".to_string(),
                ));
            }
            KnowledgeDisconnectState::Reserved => {}
        }

        let credential = match self
            .resolve_connection_credential(&connection, caller)
            .await
        {
            Ok(credential) => credential,
            Err(error) => {
                repository
                    .advance_connection_disconnect(
                        request.tenant_id,
                        request.connection_uid,
                        KnowledgeDisconnectTransition::FailedBeforeSend {
                            error_code: "credential_resolution_failed".to_string(),
                        },
                    )
                    .await?
                    .ok_or_else(|| {
                        KnowledgeServiceError::InvalidRequest(
                            "disconnect advanced concurrently before credential resolution"
                                .to_string(),
                        )
                    })?;
                return Err(error);
            }
        };
        let provider = match self.provider(connection.provider) {
            Ok(provider) => provider,
            Err(error) => {
                repository
                    .advance_connection_disconnect(
                        request.tenant_id,
                        request.connection_uid,
                        KnowledgeDisconnectTransition::FailedBeforeSend {
                            error_code: "provider_resolution_failed".to_string(),
                        },
                    )
                    .await?
                    .ok_or_else(|| {
                        KnowledgeServiceError::InvalidRequest(
                            "disconnect advanced concurrently before provider resolution"
                                .to_string(),
                        )
                    })?;
                return Err(error);
            }
        };

        let transmitting = repository
            .advance_connection_disconnect(
                request.tenant_id,
                request.connection_uid,
                KnowledgeDisconnectTransition::Transmitting,
            )
            .await?;
        if transmitting.is_none() {
            return Err(KnowledgeServiceError::InvalidRequest(
                "knowledge disconnect crossed the provider send boundary concurrently".to_string(),
            ));
        }

        if let Err(error) = provider
            .revoke_remote_connection(RemoteRevokeRequest {
                connection: connection.clone(),
                credential,
            })
            .await
        {
            repository
                .advance_connection_disconnect(
                    request.tenant_id,
                    request.connection_uid,
                    KnowledgeDisconnectTransition::UnknownOutcome {
                        error_code: "provider_revoke_unknown".to_string(),
                    },
                )
                .await?
                .ok_or_else(|| {
                    KnowledgeServiceError::InvalidRequest(
                        "disconnect outcome advanced concurrently after provider failure"
                            .to_string(),
                    )
                })?;
            return Err(error.into());
        }

        repository
            .advance_connection_disconnect(
                request.tenant_id,
                request.connection_uid,
                KnowledgeDisconnectTransition::Deleted,
            )
            .await?
            .ok_or_else(|| {
                KnowledgeServiceError::InvalidRequest(
                    "disconnect outcome advanced concurrently after provider deletion".to_string(),
                )
            })?;

        self.finish_confirmed_disconnect(&connection, &parent, caller)
            .await
    }

    async fn fence_parent_for_disconnect(
        &self,
        connection: &KnowledgeConnection,
    ) -> Result<ConnectorConnection, KnowledgeServiceError> {
        let connector_id = ConnectorConnectionId(connection.connection_uid);
        let connector_connections = self.connector_connections()?;
        let parent = connector_connections
            .get(connection.tenant_id, connector_id)
            .await?
            .ok_or(KnowledgeServiceError::NotFound("generic connector parent"))?;
        match parent.status {
            ParentConnectionStatus::Active | ParentConnectionStatus::Suspended => {
                Ok(connector_connections
                    .disconnect(connection.tenant_id, connector_id, parent.generation)
                    .await?)
            }
            ParentConnectionStatus::Disconnecting | ParentConnectionStatus::Deleted => Ok(parent),
            ParentConnectionStatus::PendingAuth => Err(KnowledgeServiceError::InvalidRequest(
                "pending connector parent cannot begin knowledge disconnect".to_string(),
            )),
        }
    }

    async fn finish_confirmed_disconnect(
        &self,
        connection: &KnowledgeConnection,
        parent: &ConnectorConnection,
        caller: &KnowledgeCaller,
    ) -> Result<KnowledgeDisconnectConnectionResponse, KnowledgeServiceError> {
        let credential_revoked = self
            .credentials
            .revoke_linked_account(connection.tenant_id, connection, caller)
            .await?;
        let deleted_parent = if parent.status == ParentConnectionStatus::Deleted {
            parent.clone()
        } else {
            self.connector_connections()?
                .delete(
                    connection.tenant_id,
                    ConnectorConnectionId(connection.connection_uid),
                    parent.generation,
                )
                .await?
        };
        Ok(KnowledgeDisconnectConnectionResponse {
            connection_uid: connection.connection_uid,
            status: deleted_parent.status.as_str().to_string(),
            credential_revoked,
        })
    }
}
