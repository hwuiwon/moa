//! Postgres ownership for OAuth clients, consent, codes, and grants.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_core::error::MoaError;
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use sqlx::PgPool;
use uuid::Uuid;

use super::OAuthError;
use super::client::{OAuthClient, OAuthClientRegistry};
use super::pkce::{self, CodeChallengeMethod};

/// Durable input for one authorization consent transaction.
pub struct NewAuthorizationTransaction<'a> {
    /// Stable transaction identifier rendered into the consent form.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Requesting client.
    pub client_id: &'a str,
    /// Resource-owner identity.
    pub subject_id: Uuid,
    /// Resource-owner type.
    pub subject_type: &'a str,
    /// Exact callback URI.
    pub redirect_uri: &'a str,
    /// Granted scopes.
    pub scopes: &'a [String],
    /// Exact RFC 8707 resource.
    pub resource: &'a str,
    /// Client callback state.
    pub state: Option<&'a str>,
    /// PKCE challenge.
    pub code_challenge: &'a str,
    /// PKCE challenge method.
    pub code_challenge_method: &'a str,
    /// Hash of the CSRF value rendered once.
    pub csrf_hash: &'a str,
    /// Transaction expiry.
    pub expires_at: DateTime<Utc>,
}

/// Exact resource owner completing a consent transaction.
pub struct AuthorizationDecisionSubject<'a> {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Subject identifier.
    pub subject_id: Uuid,
    /// Subject type.
    pub subject_type: &'a str,
}

/// Result of an atomic consent decision.
pub struct AuthorizationDecisionRecord {
    /// Exact callback URI from the durable request.
    pub redirect_uri: String,
    /// Client callback state from the durable request.
    pub state: Option<String>,
}

/// New hashes and lifetimes inserted by an authorization-code exchange.
pub struct ExchangedTokens<'a> {
    /// Access-token hash.
    pub access_token_hash: &'a str,
    /// Access-token expiry.
    pub access_token_expires_at: DateTime<Utc>,
    /// Refresh-token hash.
    pub refresh_token_hash: &'a str,
    /// Refresh-token expiry.
    pub refresh_token_expires_at: DateTime<Utc>,
}

/// Authorization values recovered during atomic exchange.
#[derive(Debug, Clone)]
pub struct ExchangedGrant {
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Exact protected resource.
    pub resource: String,
}

/// New token material to swap in during refresh rotation.
pub struct RotatedTokens<'a> {
    /// SHA-256 digest of the new access token.
    pub access_token_hash: &'a str,
    /// New access-token expiry.
    pub access_token_expires_at: DateTime<Utc>,
    /// SHA-256 digest of the new refresh token.
    pub refresh_token_hash: &'a str,
    /// New refresh-token expiry.
    pub refresh_token_expires_at: DateTime<Utc>,
}

/// Authorization values carried forward by refresh rotation.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RotatedGrantIdentity {
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Exact protected resource.
    pub resource: String,
}

/// One active access-token principal resolved for edge authentication.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ResolvedAccessToken {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Issuing client.
    pub client_id: String,
    /// Resource-owner id.
    pub subject_id: Uuid,
    /// Resource-owner type.
    pub subject_type: String,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Exact protected resource.
    pub resource: String,
    /// Access-token expiry.
    pub access_token_expires_at: DateTime<Utc>,
}

/// A row matched by client-scoped token introspection.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IntrospectionRow {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Issuing client.
    pub client_id: String,
    /// Resource-owner id.
    pub subject_id: Uuid,
    /// Resource-owner type.
    pub subject_type: String,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Exact protected resource.
    pub resource: String,
    /// Access-token digest.
    pub access_token_hash: String,
    /// Access-token expiry.
    pub access_token_expires_at: DateTime<Utc>,
    /// Refresh-token digest.
    pub refresh_token_hash: String,
    /// Refresh-token expiry.
    pub refresh_token_expires_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct ClientRow {
    client_id: String,
    client_type: String,
    redirect_uris: Vec<String>,
    scopes: Vec<String>,
    client_secret_hash: Option<String>,
    config_hash: String,
}

#[derive(sqlx::FromRow)]
struct ConsentRow {
    tenant_id: Uuid,
    subject_id: Uuid,
    subject_type: String,
    redirect_uri: String,
    state: Option<String>,
    csrf_hash: String,
    expires_at: DateTime<Utc>,
    decision: Option<String>,
}

#[derive(sqlx::FromRow)]
struct CodeRow {
    tenant_id: Uuid,
    client_id: String,
    subject_id: Uuid,
    subject_type: String,
    redirect_uri: String,
    scopes: Vec<String>,
    resource: String,
    code_challenge: String,
    code_challenge_method: String,
}

/// Postgres-backed OAuth protocol store.
pub struct OAuthStore {
    pool: Arc<PgPool>,
}

impl OAuthStore {
    /// Construct a store over a shared pool.
    #[must_use]
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Idempotently bootstrap clients and reject any conflicting declaration.
    pub async fn bootstrap_clients(
        &self,
        registry: &OAuthClientRegistry,
    ) -> Result<(), OAuthError> {
        let mut conn = self.control_plane_conn().await?;
        for client in registry.clients() {
            let client_type = match client.client_type {
                moa_core::config::OAuthClientType::Public => "public",
                moa_core::config::OAuthClientType::Confidential => "confidential",
            };
            sqlx::query(
                r#"
                INSERT INTO oauth_clients (
                    client_id, client_type, redirect_uris, scopes,
                    client_secret_hash, config_hash
                )
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (client_id) DO NOTHING
                "#,
            )
            .bind(&client.client_id)
            .bind(client_type)
            .bind(&client.redirect_uris)
            .bind(&client.scopes)
            .bind(&client.client_secret_hash)
            .bind(&client.config_hash)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;

            let stored_hash: String =
                sqlx::query_scalar("SELECT config_hash FROM oauth_clients WHERE client_id = $1")
                    .bind(&client.client_id)
                    .fetch_one(conn.as_mut())
                    .await
                    .map_err(map_sqlx_error)?;
            if stored_hash != client.config_hash {
                conn.rollback().await.map_err(map_db_error)?;
                return Err(OAuthError::ClientBootstrapConflict(
                    client.client_id.clone(),
                ));
            }
        }
        conn.commit().await.map_err(map_db_error)
    }

    /// Resolve one client from Postgres.
    pub async fn client(&self, client_id: &str) -> Result<Option<OAuthClient>, OAuthError> {
        let mut conn = self.control_plane_conn().await?;
        let row: Option<ClientRow> = sqlx::query_as(
            r#"
            SELECT client_id, client_type, redirect_uris, scopes,
                   client_secret_hash, config_hash
            FROM oauth_clients
            WHERE client_id = $1
            "#,
        )
        .bind(client_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        row.map(|row| {
            OAuthClient::from_storage(
                row.client_id,
                &row.client_type,
                row.redirect_uris,
                row.scopes,
                row.client_secret_hash,
                row.config_hash,
            )
        })
        .transpose()
    }

    /// Persist a tenant-scoped authorization transaction without issuing a code.
    pub async fn insert_authorization_transaction(
        &self,
        new: NewAuthorizationTransaction<'_>,
    ) -> Result<(), OAuthError> {
        let mut conn =
            ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(new.tenant_id), true)
                .await
                .map_err(map_db_error)?;
        sqlx::query(
            r#"
            INSERT INTO oauth_authorization_transactions (
                id, tenant_id, client_id, subject_id, subject_type, redirect_uri,
                scopes, resource, state, code_challenge, code_challenge_method,
                csrf_hash, expires_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            "#,
        )
        .bind(new.id)
        .bind(new.tenant_id.0)
        .bind(new.client_id)
        .bind(new.subject_id)
        .bind(new.subject_type)
        .bind(new.redirect_uri)
        .bind(new.scopes)
        .bind(new.resource)
        .bind(new.state)
        .bind(new.code_challenge)
        .bind(new.code_challenge_method)
        .bind(new.csrf_hash)
        .bind(new.expires_at)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)
    }

    /// Approve or deny one consent transaction exactly once.
    pub async fn decide_authorization_transaction(
        &self,
        request_id: Uuid,
        subject: AuthorizationDecisionSubject<'_>,
        csrf_hash: &str,
        approved: bool,
        code_hash: Option<&str>,
        code_expires_at: DateTime<Utc>,
    ) -> Result<AuthorizationDecisionRecord, OAuthError> {
        let mut conn =
            ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(subject.tenant_id), true)
                .await
                .map_err(map_db_error)?;
        let row: Option<ConsentRow> = sqlx::query_as(
            r#"
            SELECT tenant_id, subject_id, subject_type, redirect_uri, state,
                   csrf_hash, expires_at, decision
            FROM oauth_authorization_transactions
            WHERE id = $1
            FOR UPDATE
            "#,
        )
        .bind(request_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(map_db_error)?;
            return Err(OAuthError::InvalidGrant);
        };
        if row.decision.is_some() {
            conn.rollback().await.map_err(map_db_error)?;
            return Err(OAuthError::AuthorizationAlreadyDecided);
        }
        if row.expires_at <= Utc::now()
            || row.tenant_id != subject.tenant_id.0
            || row.subject_id != subject.subject_id
            || row.subject_type != subject.subject_type
            || row.csrf_hash != csrf_hash
        {
            conn.rollback().await.map_err(map_db_error)?;
            return Err(OAuthError::InvalidGrant);
        }

        if approved {
            let code_hash = code_hash.ok_or(OAuthError::InvalidGrant)?;
            sqlx::query(
                r#"
                INSERT INTO oauth_authorization_codes (
                    code_hash, authorization_request_id, tenant_id, client_id,
                    subject_id, subject_type, redirect_uri, scopes, resource,
                    code_challenge, code_challenge_method, expires_at
                )
                SELECT $2, id, tenant_id, client_id, subject_id, subject_type,
                       redirect_uri, scopes, resource, code_challenge,
                       code_challenge_method, $3
                FROM oauth_authorization_transactions
                WHERE id = $1
                "#,
            )
            .bind(request_id)
            .bind(code_hash)
            .bind(code_expires_at)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        }

        sqlx::query(
            r#"
            UPDATE oauth_authorization_transactions
            SET decision = $2, decided_at = NOW()
            WHERE id = $1 AND decision IS NULL
            "#,
        )
        .bind(request_id)
        .bind(if approved { "approved" } else { "denied" })
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(AuthorizationDecisionRecord {
            redirect_uri: row.redirect_uri,
            state: row.state,
        })
    }

    /// Validate, consume, and issue a grant in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub async fn exchange_authorization_code(
        &self,
        code_hash: &str,
        client_id: &str,
        redirect_uri: &str,
        resource: &str,
        code_verifier: &str,
        tokens: ExchangedTokens<'_>,
    ) -> Result<Option<ExchangedGrant>, OAuthError> {
        let mut conn = self.control_plane_conn().await?;
        let row: Option<CodeRow> = sqlx::query_as(
            r#"
            SELECT tenant_id, client_id, subject_id, subject_type, redirect_uri,
                   scopes, resource, code_challenge, code_challenge_method
            FROM oauth_authorization_codes
            WHERE code_hash = $1
              AND consumed_at IS NULL
              AND expires_at > NOW()
            FOR UPDATE
            "#,
        )
        .bind(code_hash)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(map_db_error)?;
            return Ok(None);
        };
        let method = CodeChallengeMethod::parse(&row.code_challenge_method);
        let valid = row.client_id == client_id
            && row.redirect_uri == redirect_uri
            && row.resource == resource
            && method.is_some_and(|method| {
                pkce::verify_code_challenge(&row.code_challenge, method, code_verifier)
            });
        if !valid {
            conn.rollback().await.map_err(map_db_error)?;
            return Ok(None);
        }

        sqlx::query(
            r#"
            INSERT INTO oauth_tokens (
                tenant_id, client_id, subject_id, subject_type, scopes, resource,
                access_token_hash, access_token_expires_at,
                refresh_token_hash, refresh_token_expires_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(row.tenant_id)
        .bind(&row.client_id)
        .bind(row.subject_id)
        .bind(&row.subject_type)
        .bind(&row.scopes)
        .bind(&row.resource)
        .bind(tokens.access_token_hash)
        .bind(tokens.access_token_expires_at)
        .bind(tokens.refresh_token_hash)
        .bind(tokens.refresh_token_expires_at)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        sqlx::query(
            "UPDATE oauth_authorization_codes SET consumed_at = NOW() WHERE code_hash = $1",
        )
        .bind(code_hash)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(Some(ExchangedGrant {
            scopes: row.scopes,
            resource: row.resource,
        }))
    }

    /// Atomically rotate a refresh token without changing scopes or resource.
    pub async fn rotate_refresh_token(
        &self,
        refresh_token_hash: &str,
        client_id: &str,
        rotated: RotatedTokens<'_>,
    ) -> Result<Option<RotatedGrantIdentity>, OAuthError> {
        let mut conn = self.control_plane_conn().await?;
        let row: Option<RotatedGrantIdentity> = sqlx::query_as(
            r#"
            UPDATE oauth_tokens
            SET access_token_hash = $1,
                access_token_expires_at = $2,
                refresh_token_hash = $3,
                refresh_token_expires_at = $4,
                updated_at = NOW()
            WHERE refresh_token_hash = $5
              AND client_id = $6
              AND revoked_at IS NULL
              AND refresh_token_expires_at > NOW()
            RETURNING scopes, resource
            "#,
        )
        .bind(rotated.access_token_hash)
        .bind(rotated.access_token_expires_at)
        .bind(rotated.refresh_token_hash)
        .bind(rotated.refresh_token_expires_at)
        .bind(refresh_token_hash)
        .bind(client_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(row)
    }

    /// Find one active token belonging to the authenticated issuing client.
    pub async fn find_active_for_introspection(
        &self,
        token_hash: &str,
        client_id: &str,
    ) -> Result<Option<IntrospectionRow>, OAuthError> {
        let mut conn = self.control_plane_conn().await?;
        let row: Option<IntrospectionRow> = sqlx::query_as(
            r#"
            SELECT tenant_id, client_id, subject_id, subject_type, scopes, resource,
                   access_token_hash, access_token_expires_at,
                   refresh_token_hash, refresh_token_expires_at
            FROM oauth_tokens
            WHERE (access_token_hash = $1 OR refresh_token_hash = $1)
              AND client_id = $2
              AND revoked_at IS NULL
            "#,
        )
        .bind(token_hash)
        .bind(client_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(row)
    }

    /// Resolve a live access token to identity, client, scopes, and resource.
    pub async fn resolve_active_access_token(
        &self,
        access_token_hash: &str,
    ) -> Result<Option<ResolvedAccessToken>, OAuthError> {
        let mut conn = self.control_plane_conn().await?;
        let row: Option<ResolvedAccessToken> = sqlx::query_as(
            r#"
            SELECT tenant_id, client_id, subject_id, subject_type, scopes, resource,
                   access_token_expires_at
            FROM oauth_tokens
            WHERE access_token_hash = $1
              AND revoked_at IS NULL
            "#,
        )
        .bind(access_token_hash)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(row)
    }

    /// Revoke only a token belonging to `client_id`.
    pub async fn revoke_token(&self, token_hash: &str, client_id: &str) -> Result<(), OAuthError> {
        let mut conn = self.control_plane_conn().await?;
        sqlx::query(
            r#"
            UPDATE oauth_tokens
            SET revoked_at = COALESCE(revoked_at, NOW()), updated_at = NOW()
            WHERE (access_token_hash = $1 OR refresh_token_hash = $1)
              AND client_id = $2
            "#,
        )
        .bind(token_hash)
        .bind(client_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)
    }

    async fn control_plane_conn(&self) -> Result<ScopedConn<'_>, OAuthError> {
        let mut conn = ScopedConn::begin_control_plane(&self.pool)
            .await
            .map_err(map_db_error)?;
        conn.assume_app_role().await.map_err(map_db_error)?;
        Ok(conn)
    }
}

fn map_db_error(error: MoaError) -> OAuthError {
    OAuthError::Storage(format!("db: {error}"))
}

fn map_sqlx_error(error: sqlx::Error) -> OAuthError {
    OAuthError::Storage(format!("db: {error}"))
}
