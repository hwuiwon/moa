//! Link-token and public-token exchange logic for the Knowledge service.

use chrono::Utc;
use moa_core::wire::knowledge::{
    KnowledgeCreateLinkTokenRequest, KnowledgeCreateLinkTokenResponse,
    KnowledgeExchangeTokenRequest, KnowledgeExchangeTokenResponse,
};
use moa_knowledge::domain::{
    ConnectionStatus, CreateLinkTokenRequest, ExchangePublicTokenRequest, KnowledgeConnection,
};
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
                redirect_url: None,
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
            created_at: now,
            updated_at: now,
            last_synced_at: None,
        };

        self.repository(request.tenant_id)
            .upsert_connection(connection.clone())
            .await?;

        Ok(KnowledgeExchangeTokenResponse {
            connection_uid: connection.connection_uid,
            provider: connection.provider,
            connector: connection.connector,
            provider_account_id: connection.provider_account_id,
        })
    }
}
