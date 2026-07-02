//! Link-token and public-token exchange logic for the Knowledge service.

use chrono::Utc;
use moa_core::wire::knowledge::{
    KnowledgeCreateLinkTokenRequest, KnowledgeCreateLinkTokenResponse,
    KnowledgeExchangeTokenRequest, KnowledgeExchangeTokenResponse, KnowledgeSyncRequest,
    KnowledgeUpdateConnectionSourceSelectionRequest,
    KnowledgeUpdateConnectionSourceSelectionResponse,
};
use moa_knowledge::domain::{
    ApplySourceSelectionRequest, ConnectionStatus, CreateLinkTokenRequest,
    ExchangePublicTokenRequest, KnowledgeConnection,
};
use moa_knowledge::normalize::normalize_source_selection;
use uuid::Uuid;

use super::{KnowledgeService, KnowledgeServiceError};

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

    /// Exchanges a provider public token and persists a connection with only a credential ref.
    pub async fn exchange_public_token(
        &self,
        request: KnowledgeExchangeTokenRequest,
    ) -> Result<KnowledgeExchangeTokenResponse, KnowledgeServiceError> {
        let provider = self.provider(&request.provider)?;
        let account = provider
            .exchange_public_token(ExchangePublicTokenRequest {
                tenant_id: request.tenant_id,
                public_token: request.exchange_token,
                source_selection: request.source_selection.clone(),
            })
            .await?;
        let credential_ref = self
            .credentials
            .store_linked_account(request.tenant_id, &account)
            .await?;
        let now = Utc::now();
        let connection = KnowledgeConnection {
            connection_uid: Uuid::now_v7(),
            tenant_id: request.tenant_id,
            provider: account.provider.clone(),
            connector: account.connector.clone(),
            provider_account_id: account.provider_account_id.clone(),
            credential_ref,
            status: ConnectionStatus::Active,
            metadata: account.metadata,
            source_selection: normalize_source_selection(request.source_selection),
            created_at: now,
            updated_at: now,
            last_synced_at: None,
        };

        let connection = self
            .repository(request.tenant_id)
            .upsert_connection(connection)
            .await?;
        self.provider(&connection.provider)?
            .apply_source_selection(ApplySourceSelectionRequest {
                connection: connection.clone(),
            })
            .await?;
        let sync = self
            .sync_connection(KnowledgeSyncRequest {
                tenant_id: request.tenant_id,
                connection_uid: connection.connection_uid,
                parser: None,
                max_records: None,
            })
            .await?;

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

    /// Updates provider-native selected sources for one linked connection.
    pub async fn update_connection_source_selection(
        &self,
        request: KnowledgeUpdateConnectionSourceSelectionRequest,
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
                self.sync_connection(KnowledgeSyncRequest {
                    tenant_id: request.tenant_id,
                    connection_uid: connection.connection_uid,
                    parser: None,
                    max_records: None,
                })
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
}
