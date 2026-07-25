//! Link-token and public-token exchange logic for the Knowledge service.

use chrono::Utc;
use moa_core::types::identifiers::TenantId;
use moa_knowledge::domain::{
    ApplySourceSelectionRequest, ConnectionStatus, CreateLinkTokenRequest,
    ExchangePublicTokenRequest, KnowledgeConnection, LinkClaim, LinkClaimReservation,
    LinkClaimState, LinkClaimTransition, LinkedAccount, NewLinkClaim,
};
use moa_knowledge::normalize::normalize_source_selection;
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

impl KnowledgeService {
    /// Creates a provider-specific short-lived link token without returning provider secrets.
    pub async fn create_link_token(
        &self,
        request: KnowledgeCreateLinkTokenRequest,
    ) -> Result<KnowledgeCreateLinkTokenResponse, KnowledgeServiceError> {
        let provider = self.provider(&request.provider)?;
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
            provider: token.provider,
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
        let repository = self.repository(tenant_id);
        let operation_id = caller.step("link");

        // Exchanging the public token is a provider-side read, so replaying it is
        // safe. It has to happen before the claim is consulted at all: the account
        // identity decides which connection this link owns, and therefore the
        // request hash that fences the operation. Short-circuiting on a finalized
        // claim before computing that hash would return a recorded result without
        // ever checking that the inputs still match it.
        let account = self
            .provider(&request.provider)?
            .exchange_public_token(ExchangePublicTokenRequest {
                tenant_id,
                connector: request.connector.clone(),
                public_token: request.exchange_token.clone(),
                source_selection: request.source_selection.clone(),
            })
            .await?;

        // Resolve the identifier the upsert will actually own before writing any
        // credential. A re-link keeps the existing connection, so minting a fresh
        // identifier here would bind the credential to a connection that never
        // materializes.
        let existing = repository
            .connection_by_provider_account(
                &account.provider,
                &account.connector,
                &account.provider_account_id,
            )
            .await?;
        let connection_uid = existing
            .as_ref()
            .map_or_else(Uuid::now_v7, |connection| connection.connection_uid);
        let previous_credential_ref = existing
            .as_ref()
            .map(|connection| connection.credential_ref.clone());

        let claim = match repository
            .reserve_link_claim(NewLinkClaim {
                tenant_id,
                operation_id: operation_id.clone(),
                request_hash: link_request_hash(tenant_id, connection_uid, &account),
                owner_identity_id: caller.principal().owner_identity(),
                connection_uid,
                previous_credential_ref,
            })
            .await?
        {
            LinkClaimReservation::Reserved(claim) | LinkClaimReservation::Existing(claim) => claim,
            LinkClaimReservation::Conflict => {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "link operation id was reused with different inputs".to_string(),
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
            LinkClaimState::Reserved | LinkClaimState::CredentialWritten => {}
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
        let repository = self.repository(tenant_id);

        // Writing the credential is replay-safe on its own: the vault keys the
        // create on this caller's operation id, so a crash between the vault
        // insert and this transition replays one version rather than minting a
        // second orphan.
        let candidate_credential_ref = match &claim.candidate_credential_ref {
            Some(candidate) => candidate.clone(),
            None => {
                let candidate = self
                    .credentials
                    .store_linked_account(tenant_id, claim.connection_uid, caller, account)
                    .await?;
                repository
                    .advance_link_claim(
                        tenant_id,
                        &claim.operation_id,
                        LinkClaimTransition::CredentialWritten {
                            candidate_credential_ref: candidate.clone(),
                        },
                    )
                    .await?
                    .ok_or_else(|| lost_link_claim_error("credential write"))?;
                candidate
            }
        };

        let now = Utc::now();
        let connection = repository
            .upsert_connection(KnowledgeConnection {
                connection_uid: claim.connection_uid,
                tenant_id,
                provider: account.provider.clone(),
                connector: account.connector.clone(),
                provider_account_id: account.provider_account_id.clone(),
                credential_ref: candidate_credential_ref,
                status: ConnectionStatus::Active,
                metadata: account.metadata.clone(),
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

        self.provider(&connection.provider)?
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
            provider: connection.provider,
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
        let repository = self.repository(tenant_id);
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

        if let Some(candidate) = claim.candidate_credential_ref.as_deref() {
            let current = repository.get_connection(claim.connection_uid).await?;
            let candidate_is_current = current
                .as_ref()
                .is_some_and(|connection| connection.credential_ref == candidate);
            self.credentials
                .revoke_credential(tenant_id, candidate, caller)
                .await?;
            if candidate_is_current {
                match claim.previous_credential_ref.as_deref() {
                    // A re-link: put the exact superseded reference back.
                    Some(previous) => {
                        repository
                            .restore_connection_credential(claim.connection_uid, previous)
                            .await?;
                    }
                    // A first link: there is nothing to restore, and leaving the
                    // connection active would advertise credentials that were
                    // just revoked.
                    None => {
                        repository
                            .disable_connection(tenant_id, claim.connection_uid)
                            .await?;
                    }
                }
            }
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
        let repository = self.repository(claim.tenant_id);
        let Some(connection) = repository.get_connection(claim.connection_uid).await? else {
            return Ok(None);
        };
        let sync_status = match claim.sync_run_uid {
            Some(sync_run_uid) => repository
                .get_sync_run(sync_run_uid)
                .await?
                .map(|run| run.status.as_str().to_string()),
            None => None,
        };
        Ok(Some(KnowledgeExchangeTokenResponse {
            connection_uid: connection.connection_uid,
            provider: connection.provider,
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
        let repository = self.repository(request.tenant_id);
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
        self.provider(&connection.provider)?
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

    /// Disables one linked connection and revokes MOA-managed credential material.
    pub async fn disconnect_connection(
        &self,
        request: KnowledgeDisconnectConnectionRequest,
        caller: &KnowledgeCaller,
    ) -> Result<KnowledgeDisconnectConnectionResponse, KnowledgeServiceError> {
        let repository = self.repository(request.tenant_id);
        let connection = repository
            .get_connection(request.connection_uid)
            .await?
            .ok_or(KnowledgeServiceError::NotFound("knowledge connection"))?;
        if connection.tenant_id != request.tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge connection"));
        }

        let credential_revoked = self
            .credentials
            .delete_linked_account(request.tenant_id, &connection, caller)
            .await?;
        let connection = repository
            .disable_connection(request.tenant_id, request.connection_uid)
            .await?;

        Ok(KnowledgeDisconnectConnectionResponse {
            connection_uid: connection.connection_uid,
            status: connection.status.as_str().to_string(),
            credential_revoked,
        })
    }
}
