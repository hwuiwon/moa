//! Auth0 Token Vault provider for third-party user tokens.
//!
//! MOA stores only linkage metadata in `linked_connections`. Access tokens for
//! external providers are retrieved just-in-time from Auth0 and returned to the
//! caller without being persisted by MOA.

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use moa_core::traits::{TokenVaultError, TokenVaultProvider, VaultToken};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use uuid::Uuid;

type CachedM2mToken = Option<(SecretString, DateTime<Utc>)>;

/// Auth0-backed implementation of [`TokenVaultProvider`].
pub struct Auth0TokenVaultProvider {
    http: reqwest::Client,
    base_url: String,
    client_id: String,
    client_secret: SecretString,
    management_audience: String,
    pool: Arc<PgPool>,
    m2m_token: Arc<RwLock<CachedM2mToken>>,
}

impl Auth0TokenVaultProvider {
    /// Construct a provider for an Auth0 tenant domain.
    pub fn new(
        domain: String,
        client_id: String,
        client_secret: SecretString,
        management_audience: String,
        pool: Arc<PgPool>,
    ) -> Result<Self, TokenVaultError> {
        Self::new_with_base_url(
            format!("https://{}", domain.trim_end_matches('/')),
            client_id,
            client_secret,
            management_audience,
            pool,
        )
    }

    /// Construct a provider with an explicit Auth0 base URL.
    pub fn new_with_base_url(
        base_url: String,
        client_id: String,
        client_secret: SecretString,
        management_audience: String,
        pool: Arc<PgPool>,
    ) -> Result<Self, TokenVaultError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| TokenVaultError::Unavailable(format!("http client: {error}")))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            client_id,
            client_secret,
            management_audience,
            pool,
            m2m_token: Arc::new(RwLock::new(None)),
        })
    }

    async fn m2m_token(&self) -> Result<SecretString, TokenVaultError> {
        {
            let token = self.m2m_token.read().await;
            if let Some((value, expires_at)) = &*token
                && *expires_at > Utc::now() + ChronoDuration::seconds(60)
            {
                return Ok(value.clone());
            }
        }

        #[derive(Debug, Deserialize)]
        struct M2MResponse {
            access_token: String,
            expires_in: i64,
        }

        let response: M2MResponse = self
            .http
            .post(format!("{}/oauth/token", self.base_url))
            .json(&serde_json::json!({
                "grant_type": "client_credentials",
                "client_id": self.client_id,
                "client_secret": self.client_secret.expose_secret(),
                "audience": self.management_audience,
            }))
            .send()
            .await
            .map_err(|error| TokenVaultError::Unavailable(format!("m2m: {error}")))?
            .error_for_status()
            .map_err(|error| TokenVaultError::Unavailable(format!("m2m status: {error}")))?
            .json()
            .await
            .map_err(|error| TokenVaultError::Unavailable(format!("m2m parse: {error}")))?;

        let token = SecretString::new(response.access_token.into_boxed_str());
        let expires_at = Utc::now() + ChronoDuration::seconds(response.expires_in);
        let mut cached = self.m2m_token.write().await;
        *cached = Some((token.clone(), expires_at));
        Ok(token)
    }
}

#[async_trait]
impl TokenVaultProvider for Auth0TokenVaultProvider {
    async fn get_token(
        &self,
        user_id: Uuid,
        connection_name: &str,
    ) -> Result<VaultToken, TokenVaultError> {
        let sub: Option<(String,)> =
            sqlx::query_as("SELECT sub FROM auth0_user_map WHERE user_id = $1 LIMIT 1")
                .bind(user_id)
                .fetch_optional(&*self.pool)
                .await
                .map_err(|error| TokenVaultError::Internal(format!("db: {error}")))?;
        let Some((sub,)) = sub else {
            return Err(TokenVaultError::NotLinked);
        };

        let linked: Option<(Option<String>,)> = sqlx::query_as(
            r#"
            SELECT external_sub
            FROM linked_connections
            WHERE user_id = $1 AND connection_name = $2
            "#,
        )
        .bind(user_id)
        .bind(connection_name)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| TokenVaultError::Internal(format!("db: {error}")))?;
        if linked.is_none() {
            return Err(TokenVaultError::NotLinked);
        }

        #[derive(Debug, Deserialize)]
        struct VaultResponse {
            access_token: String,
            expires_in: Option<i64>,
            #[serde(default)]
            scope: String,
        }

        let m2m = self.m2m_token().await?;
        let response: VaultResponse = self
            .http
            .post(format!("{}/oauth/token", self.base_url))
            .bearer_auth(m2m.expose_secret())
            .json(&serde_json::json!({
                "grant_type": "urn:auth0:params:oauth:grant-type:token-vault",
                "subject_token": sub,
                "subject_token_type": "urn:auth0:params:oauth:token-type:auth0-user",
                "connection": connection_name,
            }))
            .send()
            .await
            .map_err(|error| TokenVaultError::Unavailable(format!("exchange: {error}")))?
            .error_for_status()
            .map_err(|error| TokenVaultError::Unavailable(format!("exchange status: {error}")))?
            .json()
            .await
            .map_err(|error| TokenVaultError::Unavailable(format!("exchange parse: {error}")))?;

        let scopes = response
            .scope
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect();
        let expires_at = response
            .expires_in
            .map(|seconds| Utc::now() + ChronoDuration::seconds(seconds));

        Ok(VaultToken {
            access_token: SecretString::new(response.access_token.into_boxed_str()),
            expires_at,
            scopes,
        })
    }

    async fn list_connections(&self, user_id: Uuid) -> Result<Vec<String>, TokenVaultError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT connection_name FROM linked_connections WHERE user_id = $1 ORDER BY connection_name",
        )
        .bind(user_id)
        .fetch_all(&*self.pool)
        .await
        .map_err(|error| TokenVaultError::Internal(format!("db: {error}")))?;
        Ok(rows.into_iter().map(|(connection,)| connection).collect())
    }

    fn name(&self) -> &'static str {
        "auth0"
    }
}
