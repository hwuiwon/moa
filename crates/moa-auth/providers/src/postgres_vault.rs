//! Self-hosted, Postgres-backed [`TokenVaultProvider`] for third-party tokens.
//!
//! Unlike the Auth0 Token Vault (which holds token material externally and
//! persists only linkage metadata), the self-hosted vault stores the sealed
//! access and refresh tokens in `token_vault_connections` so MOA can broker
//! delegated third-party access without an external identity provider. Token
//! secrets are envelope-encrypted through the explicitly supplied KMS before
//! insertion and opened on retrieval.
//!
//! All storage access flows through [`ScopedConn`] under the `moa_app` role so
//! the row-level-security policy on the table is enforced. Writes are
//! tenant-scoped; reads are keyed by the globally-unique `user_id` the
//! [`TokenVaultProvider`] contract carries.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use moa_core::error::MoaError;
use moa_core::traits::{TokenVaultError, TokenVaultProvider, VaultToken};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_crypto::{Ciphertext, EncryptionContext, KeyManagementProvider};
use moa_db::ScopedConn;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

/// Tokens expiring within this window are treated as already expired so a
/// refresh can run before the credential goes stale mid-request.
const EXPIRY_SKEW_SECONDS: i64 = 60;

/// A refresh winner has this long to finish remote I/O and persist its result.
/// Expiry never elects another winner: it moves the connection to
/// `relink_required` because the provider may already have rotated the token.
const REFRESH_LEASE_SECONDS: i64 = 30;

/// Delay between durable state reads while another replica owns the lease.
const REFRESH_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Classification bound into every token-vault ciphertext.
const CREDENTIAL_PII_CLASS: &str = "credential";

/// Parameters for persisting (or refreshing) one user's linked connection token.
///
/// Borrowed string slices avoid cloning caller-owned identifiers; the secret
/// token material is moved in so it is not duplicated in memory.
pub struct StoreTokenRequest<'a> {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning user (globally unique across tenants).
    pub user_id: Uuid,
    /// Connection name presented to [`TokenVaultProvider::get_token`].
    pub connection_name: &'a str,
    /// Upstream provider slug, e.g. `"google"` or `"github"`.
    pub provider: &'a str,
    /// Optional external account identifier reported by the provider.
    pub external_account_id: Option<&'a str>,
    /// Access token to seal and store.
    pub access_token: SecretString,
    /// Optional refresh token to seal and store.
    pub refresh_token: Option<SecretString>,
    /// Optional OAuth token type, e.g. `"Bearer"`.
    pub token_type: Option<&'a str>,
    /// Optional access-token expiry.
    pub expires_at: Option<DateTime<Utc>>,
    /// Scopes granted for this connection.
    pub scopes: &'a [String],
}

/// Postgres-backed implementation of [`TokenVaultProvider`].
pub struct PostgresTokenVaultProvider {
    pool: Arc<PgPool>,
    kms: Arc<dyn KeyManagementProvider>,
    refresher: Option<Arc<TokenRefresher>>,
}

impl PostgresTokenVaultProvider {
    /// Construct a provider with its required key-management provider.
    #[must_use]
    pub fn new(pool: Arc<PgPool>, kms: Arc<dyn KeyManagementProvider>) -> Self {
        Self {
            pool,
            kms,
            refresher: None,
        }
    }

    /// Attach an OAuth [`TokenRefresher`] so expired access tokens are refreshed
    /// via the `refresh_token` grant instead of surfacing as unavailable.
    ///
    /// Additive: without a refresher an expired token still fails closed with
    /// [`TokenVaultError::Unavailable`].
    #[must_use]
    pub fn with_refresher(mut self, refresher: Arc<TokenRefresher>) -> Self {
        self.refresher = Some(refresher);
        self
    }

    /// Persist or explicitly relink a user's connection token, tenant-scoped.
    ///
    /// Upserts on `(tenant_id, user_id, connection_name)`. Re-linking increments
    /// the durable generation and clears any refresh lease, fencing an older
    /// refresh winner from overwriting the newly supplied credential. The write
    /// runs under a tenant-scoped `moa_app` transaction, so the row-level-security
    /// policy rejects any attempt to store a token under a mismatched tenant.
    pub async fn store_token(&self, request: StoreTokenRequest<'_>) -> Result<(), TokenVaultError> {
        let ctx = token_encryption_context(
            request.tenant_id.0,
            request.user_id,
            request.connection_name,
        );
        let access_sealed = seal_token(
            self.kms.as_ref(),
            request.access_token.expose_secret().as_bytes(),
            &ctx,
        )
        .await
        .map_err(|error| TokenVaultError::Internal(format!("seal access token: {error}")))?;
        let refresh_sealed = match request.refresh_token.as_ref() {
            Some(refresh) => Some(
                seal_token(self.kms.as_ref(), refresh.expose_secret().as_bytes(), &ctx)
                    .await
                    .map_err(|error| {
                        TokenVaultError::Internal(format!("seal refresh token: {error}"))
                    })?,
            ),
            None => None,
        };

        let mut conn =
            ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(request.tenant_id), true)
                .await
                .map_err(map_db_error)?;
        sqlx::query(
            r#"
            INSERT INTO token_vault_connections (
                tenant_id, user_id, connection_name, provider, external_account_id,
                access_token_sealed, refresh_token_sealed, token_type, scopes, expires_at,
                updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW())
            ON CONFLICT (tenant_id, user_id, connection_name) DO UPDATE SET
                provider = EXCLUDED.provider,
                external_account_id = EXCLUDED.external_account_id,
                access_token_sealed = EXCLUDED.access_token_sealed,
                refresh_token_sealed = EXCLUDED.refresh_token_sealed,
                token_type = EXCLUDED.token_type,
                scopes = EXCLUDED.scopes,
                expires_at = EXCLUDED.expires_at,
                generation = token_vault_connections.generation + 1,
                refresh_state = 'ready',
                refresh_lease_id = NULL,
                refresh_lease_expires_at = NULL,
                updated_at = NOW()
            "#,
        )
        .bind(request.tenant_id.0)
        .bind(request.user_id)
        .bind(request.connection_name)
        .bind(request.provider)
        .bind(request.external_account_id)
        .bind(access_sealed)
        .bind(refresh_sealed)
        .bind(request.token_type)
        .bind(request.scopes)
        .bind(request.expires_at)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(())
    }

    /// Refresh an expired access token via the OAuth `refresh_token` grant and
    /// return the rotated token.
    ///
    /// Fails closed: if no refresher is attached, no endpoint is configured for
    /// the connection, no refresh token is stored, or the refresh round-trip
    /// fails, this returns an error and never the stale access token. On success
    /// the rotated access token (and any rotated refresh token) is re-sealed and
    /// persisted before being returned. The provider does not rotate the stored
    /// refresh token when the endpoint omits a new one, so it is re-persisted as
    /// is. A persistence failure surfaces as an error rather than handing back a
    /// token whose rotated refresh counterpart was not durably saved.
    async fn refresh_expired_token(
        &self,
        user_id: Uuid,
        connection_name: &str,
        row: &TokenRow,
    ) -> Result<VaultToken, TokenVaultError> {
        let Some(refresher) = self.refresher.as_deref() else {
            return Err(TokenVaultError::Unavailable(
                "access token expired and no refresh capability is configured".to_string(),
            ));
        };
        let Some(endpoint) = refresher.endpoint(connection_name) else {
            return Err(TokenVaultError::Unavailable(format!(
                "access token expired and connection '{connection_name}' has no refresh endpoint \
                 configured"
            )));
        };
        if row.refresh_token_sealed.is_none() {
            return Err(TokenVaultError::Unavailable(
                "access token expired and no refresh token is stored for this connection"
                    .to_string(),
            ));
        }

        loop {
            match self.claim_refresh(user_id, connection_name).await? {
                RefreshClaim::Current(current) => {
                    return self
                        .open_access_token(user_id, connection_name, current)
                        .await;
                }
                RefreshClaim::Winner(lease) => {
                    return self
                        .run_refresh(user_id, connection_name, refresher, endpoint, lease)
                        .await;
                }
                RefreshClaim::Wait => tokio::time::sleep(REFRESH_POLL_INTERVAL).await,
                RefreshClaim::RelinkRequired => {
                    return Err(TokenVaultError::Unavailable(format!(
                        "connection '{connection_name}' requires relinking after an uncertain refresh"
                    )));
                }
                RefreshClaim::MissingRefreshToken => {
                    return Err(TokenVaultError::Unavailable(
                        "access token expired and no refresh token is stored for this connection"
                            .to_string(),
                    ));
                }
            }
        }
    }

    /// Claim the right to refresh one expired connection in a short transaction.
    async fn claim_refresh(
        &self,
        user_id: Uuid,
        connection_name: &str,
    ) -> Result<RefreshClaim, TokenVaultError> {
        let mut conn = ScopedConn::begin_control_plane(&self.pool)
            .await
            .map_err(map_db_error)?;
        conn.assume_app_role().await.map_err(map_db_error)?;
        let row: Option<TokenRow> = sqlx::query_as(
            r#"
            SELECT id, tenant_id, provider, access_token_sealed, refresh_token_sealed,
                   scopes, expires_at, generation, refresh_state,
                   COALESCE(refresh_lease_expires_at > NOW(), FALSE) AS refresh_lease_active
            FROM token_vault_connections
            WHERE user_id = $1 AND connection_name = $2
            FOR UPDATE
            "#,
        )
        .bind(user_id)
        .bind(connection_name)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        let Some(row) = row else {
            conn.commit().await.map_err(map_db_error)?;
            return Err(TokenVaultError::NotLinked);
        };
        let state = RefreshState::parse(&row.refresh_state)?;

        let claim = match state {
            RefreshState::Ready if !is_expired(row.expires_at, Utc::now()) => {
                RefreshClaim::Current(row)
            }
            RefreshState::Ready if row.refresh_token_sealed.is_none() => {
                RefreshClaim::MissingRefreshToken
            }
            RefreshState::Ready => {
                let lease_id = Uuid::new_v4();
                let updated = sqlx::query(
                    r#"
                    UPDATE token_vault_connections
                    SET refresh_state = 'refreshing',
                        refresh_lease_id = $2,
                        refresh_lease_expires_at = NOW() + ($3 * INTERVAL '1 second'),
                        updated_at = NOW()
                    WHERE id = $1 AND generation = $4 AND refresh_state = 'ready'
                    "#,
                )
                .bind(row.id)
                .bind(lease_id)
                .bind(REFRESH_LEASE_SECONDS)
                .bind(row.generation)
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx_error)?;
                if updated.rows_affected() != 1 {
                    return Err(TokenVaultError::Internal(
                        "refresh claim changed concurrently while row was locked".to_string(),
                    ));
                }
                RefreshClaim::Winner(RefreshLease { lease_id, row })
            }
            RefreshState::Refreshing if row.refresh_lease_active => RefreshClaim::Wait,
            RefreshState::Refreshing => {
                let updated = sqlx::query(
                    r#"
                    UPDATE token_vault_connections
                    SET refresh_state = 'relink_required',
                        refresh_lease_id = NULL,
                        refresh_lease_expires_at = NULL,
                        updated_at = NOW()
                    WHERE id = $1 AND generation = $2 AND refresh_state = 'refreshing'
                    "#,
                )
                .bind(row.id)
                .bind(row.generation)
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx_error)?;
                if updated.rows_affected() != 1 {
                    return Err(TokenVaultError::Internal(
                        "expired refresh lease changed concurrently while row was locked"
                            .to_string(),
                    ));
                }
                RefreshClaim::RelinkRequired
            }
            RefreshState::RelinkRequired => RefreshClaim::RelinkRequired,
        };
        conn.commit().await.map_err(map_db_error)?;
        Ok(claim)
    }

    /// Execute remote refresh and encryption after the lease transaction commits.
    async fn run_refresh(
        &self,
        user_id: Uuid,
        connection_name: &str,
        refresher: &TokenRefresher,
        endpoint: &OAuthRefreshEndpoint,
        lease: RefreshLease,
    ) -> Result<VaultToken, TokenVaultError> {
        let Some(refresh_sealed) = lease.row.refresh_token_sealed.as_deref() else {
            return Err(self
                .fail_refresh(
                    &lease,
                    TokenVaultError::Unavailable(
                        "refresh lease has no stored refresh token".to_string(),
                    ),
                )
                .await);
        };
        let ctx = token_encryption_context(lease.row.tenant_id, user_id, connection_name);
        let refresh_plaintext = match open_token(self.kms.as_ref(), refresh_sealed, &ctx).await {
            Ok(plaintext) => plaintext,
            Err(error) => {
                return Err(self
                    .fail_refresh(
                        &lease,
                        TokenVaultError::Internal(format!("open refresh token: {error}")),
                    )
                    .await);
            }
        };
        let existing_refresh = match String::from_utf8(refresh_plaintext) {
            Ok(refresh) => refresh,
            Err(error) => {
                return Err(self
                    .fail_refresh(
                        &lease,
                        TokenVaultError::Internal(format!("refresh token utf8: {error}")),
                    )
                    .await);
            }
        };

        tracing::info!(
            connection = connection_name,
            provider = %lease.row.provider,
            generation = lease.row.generation,
            "refreshing expired access token under durable lease"
        );
        let refreshed = match refresher.refresh(endpoint, &existing_refresh).await {
            Ok(refreshed) => refreshed,
            Err(error) => return Err(self.fail_refresh(&lease, error).await),
        };

        let new_refresh = refreshed.refresh_token.unwrap_or(existing_refresh);
        let new_scopes = refreshed.scopes.unwrap_or_else(|| lease.row.scopes.clone());
        let access_token = refreshed.access_token;
        let expires_at = refreshed.expires_at;
        let access_sealed = match seal_token(self.kms.as_ref(), access_token.as_bytes(), &ctx).await
        {
            Ok(sealed) => sealed,
            Err(error) => {
                return Err(self
                    .fail_refresh(
                        &lease,
                        TokenVaultError::Internal(format!("seal access token: {error}")),
                    )
                    .await);
            }
        };
        let refresh_sealed = match seal_token(self.kms.as_ref(), new_refresh.as_bytes(), &ctx).await
        {
            Ok(sealed) => sealed,
            Err(error) => {
                return Err(self
                    .fail_refresh(
                        &lease,
                        TokenVaultError::Internal(format!("seal refresh token: {error}")),
                    )
                    .await);
            }
        };

        if !self
            .persist_refreshed_token(
                &lease,
                access_sealed,
                refresh_sealed,
                &new_scopes,
                expires_at,
            )
            .await?
        {
            return self
                .resolve_after_lost_lease(user_id, connection_name)
                .await;
        }

        tracing::info!(
            connection = connection_name,
            provider = %lease.row.provider,
            generation = lease.row.generation,
            "rotated access token persisted under durable lease"
        );
        Ok(VaultToken {
            access_token: SecretString::new(access_token.into_boxed_str()),
            expires_at,
            scopes: new_scopes,
        })
    }

    /// Persist rotated secrets only while both durable fences still match.
    async fn persist_refreshed_token(
        &self,
        lease: &RefreshLease,
        access_sealed: Vec<u8>,
        refresh_sealed: Vec<u8>,
        scopes: &[String],
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<bool, TokenVaultError> {
        let mut conn = ScopedConn::begin_as_app(
            &self.pool,
            &RlsContext::tenant(TenantId::from(lease.row.tenant_id)),
            true,
        )
        .await
        .map_err(map_db_error)?;
        let updated = sqlx::query(
            r#"
            UPDATE token_vault_connections
            SET access_token_sealed = $2,
                refresh_token_sealed = $3,
                scopes = $4,
                expires_at = $5,
                refresh_state = 'ready',
                refresh_lease_id = NULL,
                refresh_lease_expires_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND generation = $6
              AND refresh_state = 'refreshing'
              AND refresh_lease_id = $7
            "#,
        )
        .bind(lease.row.id)
        .bind(access_sealed)
        .bind(refresh_sealed)
        .bind(scopes)
        .bind(expires_at)
        .bind(lease.row.generation)
        .bind(lease.lease_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(updated.rows_affected() == 1)
    }

    /// Mark an owned lease uncertain so no replica retries a possibly-consumed token.
    async fn mark_relink_required(&self, lease: &RefreshLease) -> Result<(), TokenVaultError> {
        let mut conn = ScopedConn::begin_as_app(
            &self.pool,
            &RlsContext::tenant(TenantId::from(lease.row.tenant_id)),
            true,
        )
        .await
        .map_err(map_db_error)?;
        sqlx::query(
            r#"
            UPDATE token_vault_connections
            SET refresh_state = 'relink_required',
                refresh_lease_id = NULL,
                refresh_lease_expires_at = NULL,
                updated_at = NOW()
            WHERE id = $1
              AND generation = $2
              AND refresh_state = 'refreshing'
              AND refresh_lease_id = $3
            "#,
        )
        .bind(lease.row.id)
        .bind(lease.row.generation)
        .bind(lease.lease_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(())
    }

    /// Preserve the original refresh failure unless durable fencing also fails.
    async fn fail_refresh(&self, lease: &RefreshLease, error: TokenVaultError) -> TokenVaultError {
        match self.mark_relink_required(lease).await {
            Ok(()) => error,
            Err(mark_error) => mark_error,
        }
    }

    /// Resolve the current durable row after this winner loses its CAS fence.
    async fn resolve_after_lost_lease(
        &self,
        user_id: Uuid,
        connection_name: &str,
    ) -> Result<VaultToken, TokenVaultError> {
        let row = self.load_token_row(user_id, connection_name).await?;
        if RefreshState::parse(&row.refresh_state)? == RefreshState::Ready
            && !is_expired(row.expires_at, Utc::now())
        {
            return self.open_access_token(user_id, connection_name, row).await;
        }
        Err(TokenVaultError::Unavailable(
            "refresh result lost its durable lease or generation fence".to_string(),
        ))
    }

    /// Load one connection through the control-plane RLS path.
    async fn load_token_row(
        &self,
        user_id: Uuid,
        connection_name: &str,
    ) -> Result<TokenRow, TokenVaultError> {
        let mut conn = ScopedConn::begin_control_plane(&self.pool)
            .await
            .map_err(map_db_error)?;
        conn.assume_app_role().await.map_err(map_db_error)?;
        let row: Option<TokenRow> = sqlx::query_as(
            r#"
            SELECT id, tenant_id, provider, access_token_sealed, refresh_token_sealed,
                   scopes, expires_at, generation, refresh_state,
                   COALESCE(refresh_lease_expires_at > NOW(), FALSE) AS refresh_lease_active
            FROM token_vault_connections
            WHERE user_id = $1 AND connection_name = $2
            "#,
        )
        .bind(user_id)
        .bind(connection_name)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        row.ok_or(TokenVaultError::NotLinked)
    }

    /// Open a stored access token after its durable state has been accepted.
    async fn open_access_token(
        &self,
        user_id: Uuid,
        connection_name: &str,
        row: TokenRow,
    ) -> Result<VaultToken, TokenVaultError> {
        let ctx = token_encryption_context(row.tenant_id, user_id, connection_name);
        let plaintext = open_token(self.kms.as_ref(), &row.access_token_sealed, &ctx)
            .await
            .map_err(|error| TokenVaultError::Internal(format!("open access token: {error}")))?;
        let access_token = String::from_utf8(plaintext)
            .map_err(|error| TokenVaultError::Internal(format!("access token utf8: {error}")))?;
        Ok(VaultToken {
            access_token: SecretString::new(access_token.into_boxed_str()),
            expires_at: row.expires_at,
            scopes: row.scopes,
        })
    }
}

/// Outbound OAuth refresh capability for the self-hosted token vault.
///
/// Maps a connection name to its provider token endpoint and client
/// credentials, and owns the HTTP client used for the `refresh_token` grant.
/// Attached to a [`PostgresTokenVaultProvider`] via
/// [`PostgresTokenVaultProvider::with_refresher`]; when absent, an expired
/// access token surfaces as [`TokenVaultError::Unavailable`] rather than being
/// refreshed.
pub struct TokenRefresher {
    http: reqwest::Client,
    endpoints: HashMap<String, OAuthRefreshEndpoint>,
}

impl TokenRefresher {
    /// Construct a refresher over a set of per-connection OAuth endpoints.
    ///
    /// Each entry maps a connection name to its token endpoint and client
    /// credentials. Fails only if the shared HTTP client cannot be built.
    pub fn new(endpoints: HashMap<String, OAuthRefreshEndpoint>) -> Result<Self, TokenVaultError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| {
                TokenVaultError::Unavailable(format!("refresh http client: {error}"))
            })?;
        Ok(Self { http, endpoints })
    }

    /// Look up the configured refresh endpoint for a connection, if any.
    fn endpoint(&self, connection_name: &str) -> Option<&OAuthRefreshEndpoint> {
        self.endpoints.get(connection_name)
    }

    /// Perform the OAuth `refresh_token` grant against a connection's endpoint.
    ///
    /// POSTs a form-encoded `grant_type=refresh_token` request (RFC 6749 §6) and
    /// parses the rotated access token, optional rotated refresh token, expiry,
    /// and scopes. Any transport, status, or parse failure maps to
    /// [`TokenVaultError::Unavailable`] so the caller fails closed. Never logs
    /// the request body, tokens, or client secret.
    async fn refresh(
        &self,
        endpoint: &OAuthRefreshEndpoint,
        refresh_token: &str,
    ) -> Result<RefreshedToken, TokenVaultError> {
        #[derive(Deserialize)]
        struct RefreshResponse {
            access_token: String,
            #[serde(default)]
            refresh_token: Option<String>,
            #[serde(default)]
            expires_in: Option<i64>,
            #[serde(default)]
            scope: Option<String>,
        }

        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", endpoint.client_id.as_str()),
        ];
        if let Some(secret) = endpoint.client_secret.as_ref() {
            form.push(("client_secret", secret.expose_secret()));
        }

        let response: RefreshResponse = self
            .http
            .post(endpoint.token_endpoint.as_str())
            .form(&form)
            .send()
            .await
            .map_err(|error| {
                TokenVaultError::Unavailable(format!("refresh request failed: {error}"))
            })?
            .error_for_status()
            .map_err(|error| {
                TokenVaultError::Unavailable(format!("refresh endpoint returned error: {error}"))
            })?
            .json()
            .await
            .map_err(|error| {
                TokenVaultError::Unavailable(format!("refresh response parse failed: {error}"))
            })?;

        let expires_at = response
            .expires_in
            .map(|seconds| Utc::now() + ChronoDuration::seconds(seconds));
        let scopes = response
            .scope
            .map(|scope| scope.split_whitespace().map(ToOwned::to_owned).collect());
        Ok(RefreshedToken {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at,
            scopes,
        })
    }
}

/// One connection's outbound OAuth token endpoint and client credentials, used
/// to refresh an expired access token via the `refresh_token` grant.
pub struct OAuthRefreshEndpoint {
    /// Provider token endpoint that accepts the `refresh_token` grant.
    pub token_endpoint: String,
    /// OAuth client id registered for this connection.
    pub client_id: String,
    /// Optional OAuth client secret. Absent for public clients that refresh
    /// without a secret.
    pub client_secret: Option<SecretString>,
}

/// Rotated token material parsed from a successful `refresh_token` grant.
struct RefreshedToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    scopes: Option<Vec<String>>,
}

/// Row shape returned when retrieving a stored access token.
///
/// Carries the sealed refresh token and the identity columns needed to
/// re-persist rotated material after an OAuth refresh, alongside the access
/// token returned to the caller.
#[derive(sqlx::FromRow)]
struct TokenRow {
    id: Uuid,
    tenant_id: Uuid,
    provider: String,
    access_token_sealed: Vec<u8>,
    refresh_token_sealed: Option<Vec<u8>>,
    scopes: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
    generation: i64,
    refresh_state: String,
    refresh_lease_active: bool,
}

/// Parsed durable refresh state enforced by V338.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshState {
    Ready,
    Refreshing,
    RelinkRequired,
}

impl RefreshState {
    fn parse(value: &str) -> Result<Self, TokenVaultError> {
        match value {
            "ready" => Ok(Self::Ready),
            "refreshing" => Ok(Self::Refreshing),
            "relink_required" => Ok(Self::RelinkRequired),
            other => Err(TokenVaultError::Internal(format!(
                "invalid token-vault refresh state '{other}'"
            ))),
        }
    }
}

/// Outcome of one short durable refresh-claim transaction.
enum RefreshClaim {
    Current(TokenRow),
    Winner(RefreshLease),
    Wait,
    RelinkRequired,
    MissingRefreshToken,
}

/// Fences one remote refresh attempt to the row generation that elected it.
struct RefreshLease {
    lease_id: Uuid,
    row: TokenRow,
}

#[async_trait]
impl TokenVaultProvider for PostgresTokenVaultProvider {
    async fn get_token(
        &self,
        user_id: Uuid,
        connection_name: &str,
    ) -> Result<VaultToken, TokenVaultError> {
        // SAFETY: control-plane read keyed by the globally-unique user_id. The
        // TokenVaultProvider contract carries no tenant, and linked-connection
        // identity is tenant-less (mirroring the Auth0 provider), so exactly one
        // tenant's row can match. RLS still isolates every tenant-scoped path.
        let row = self.load_token_row(user_id, connection_name).await?;

        if is_expired(row.expires_at, Utc::now()) {
            return self
                .refresh_expired_token(user_id, connection_name, &row)
                .await;
        }
        self.open_access_token(user_id, connection_name, row).await
    }

    async fn list_connections(&self, user_id: Uuid) -> Result<Vec<String>, TokenVaultError> {
        // SAFETY: informational listing keyed by the globally-unique user_id;
        // see get_token for the control-plane rationale.
        let mut conn = ScopedConn::begin_control_plane(&self.pool)
            .await
            .map_err(map_db_error)?;
        conn.assume_app_role().await.map_err(map_db_error)?;
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT connection_name
            FROM token_vault_connections
            WHERE user_id = $1
            ORDER BY connection_name
            "#,
        )
        .bind(user_id)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await.map_err(map_db_error)?;
        Ok(rows.into_iter().map(|(connection,)| connection).collect())
    }

    fn name(&self) -> &'static str {
        "postgres"
    }
}

/// Return whether an access token should be treated as expired at `now`.
///
/// A token with no expiry never expires. The refresh skew treats a token that
/// expires within the next [`EXPIRY_SKEW_SECONDS`] as already expired.
fn is_expired(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match expires_at {
        None => false,
        Some(expiry) => expiry <= now + ChronoDuration::seconds(EXPIRY_SKEW_SECONDS),
    }
}

/// Build the authenticated encryption context for one linked connection.
fn token_encryption_context(
    tenant_id: Uuid,
    user_id: Uuid,
    connection_name: &str,
) -> EncryptionContext {
    EncryptionContext::new(tenant_id, user_id, connection_name, CREDENTIAL_PII_CLASS)
}

/// Envelope-encrypt token material into the opaque database representation.
async fn seal_token(
    kms: &dyn KeyManagementProvider,
    plaintext: &[u8],
    ctx: &EncryptionContext,
) -> Result<Vec<u8>, moa_crypto::Error> {
    moa_crypto::encrypt(kms, plaintext, ctx)
        .await
        .map(|ciphertext| ciphertext.to_bytes())
}

/// Decode and envelope-decrypt token material from its database representation.
async fn open_token(
    kms: &dyn KeyManagementProvider,
    sealed: &[u8],
    ctx: &EncryptionContext,
) -> Result<Vec<u8>, moa_crypto::Error> {
    let ciphertext = Ciphertext::from_bytes(sealed)?;
    moa_crypto::decrypt(kms, &ciphertext, ctx).await
}

/// Maps a [`moa_db`] storage error to a [`TokenVaultError`].
fn map_db_error(error: MoaError) -> TokenVaultError {
    TokenVaultError::Internal(format!("db: {error}"))
}

/// Maps a raw sqlx error to a [`TokenVaultError`].
fn map_sqlx_error(error: sqlx::Error) -> TokenVaultError {
    TokenVaultError::Internal(format!("db: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_crypto::LocalKmsProvider;

    #[test]
    fn no_expiry_is_never_expired() {
        // Pins: tokens without an expiry timestamp are always usable.
        assert!(!is_expired(None, Utc::now()));
    }

    #[test]
    fn future_expiry_beyond_skew_is_not_expired() {
        // Pins: a token comfortably in the future is returned as-is.
        let now = Utc::now();
        let expires_at = now + ChronoDuration::seconds(EXPIRY_SKEW_SECONDS + 120);
        assert!(!is_expired(Some(expires_at), now));
    }

    #[test]
    fn past_expiry_is_expired() {
        // Pins: an already-elapsed token is flagged for refresh.
        let now = Utc::now();
        let expires_at = now - ChronoDuration::seconds(1);
        assert!(is_expired(Some(expires_at), now));
    }

    #[test]
    fn expiry_within_skew_is_expired() {
        // Pins: a token expiring inside the refresh skew is treated as expired so
        // it never goes stale mid-request.
        let now = Utc::now();
        let expires_at = now + ChronoDuration::seconds(EXPIRY_SKEW_SECONDS - 1);
        assert!(is_expired(Some(expires_at), now));
    }

    /// Lazy pool that never connects, for offline tests of paths that fail
    /// closed before any database access.
    fn lazy_pool() -> Arc<PgPool> {
        Arc::new(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://moa_owner:dev@127.0.0.1:1/moa")
                .expect("lazy pool should not connect"),
        )
    }

    fn local_kms() -> Arc<dyn KeyManagementProvider> {
        Arc::new(LocalKmsProvider::new())
    }

    #[tokio::test]
    async fn token_ciphertext_round_trips_only_under_matching_context() {
        // Pins: the vault's direct moa-crypto path encrypts token material and
        // binds it to the owning tenant, user, and connection.
        let kms = local_kms();
        let plaintext = b"ya29.super-secret-access-token";
        let google = token_encryption_context(Uuid::from_u128(1), Uuid::from_u128(2), "google");
        let github = token_encryption_context(Uuid::from_u128(1), Uuid::from_u128(2), "github");
        let sealed = seal_token(kms.as_ref(), plaintext, &google)
            .await
            .expect("seal token");

        assert_ne!(sealed.as_slice(), plaintext);
        assert_eq!(
            open_token(kms.as_ref(), &sealed, &google)
                .await
                .expect("open token"),
            plaintext
        );
        assert!(open_token(kms.as_ref(), &sealed, &github).await.is_err());
    }

    /// A synthetic already-expired row, so the refresh decision can be exercised
    /// without a database round-trip.
    fn expired_row() -> TokenRow {
        TokenRow {
            id: Uuid::from_u128(3),
            tenant_id: Uuid::from_u128(1),
            provider: "google".to_string(),
            access_token_sealed: b"sealed-access".to_vec(),
            refresh_token_sealed: Some(b"sealed-refresh".to_vec()),
            scopes: vec!["email".to_string()],
            expires_at: Some(Utc::now() - ChronoDuration::hours(1)),
            generation: 1,
            refresh_state: "ready".to_string(),
            refresh_lease_active: false,
        }
    }

    #[tokio::test]
    async fn expired_without_refresher_is_unavailable_not_stale_token() {
        // Pins: with no refresh capability configured, an expired token fails
        // closed with Unavailable before any network or database work — the
        // stale credential is never returned.
        let provider = PostgresTokenVaultProvider::new(lazy_pool(), local_kms());
        match provider
            .refresh_expired_token(Uuid::from_u128(9), "google", &expired_row())
            .await
        {
            Err(TokenVaultError::Unavailable(_)) => {}
            Err(other) => panic!("expected Unavailable, got {other:?}"),
            Ok(_) => panic!("expired token must not be returned without a refresher"),
        }
    }

    #[tokio::test]
    async fn expired_with_refresher_but_no_endpoint_is_unavailable() {
        // Pins: a refresher without an endpoint for the connection still fails
        // closed rather than returning the stale token.
        let refresher = Arc::new(TokenRefresher::new(HashMap::new()).expect("refresher builds"));
        let provider =
            PostgresTokenVaultProvider::new(lazy_pool(), local_kms()).with_refresher(refresher);
        match provider
            .refresh_expired_token(Uuid::from_u128(9), "google", &expired_row())
            .await
        {
            Err(TokenVaultError::Unavailable(_)) => {}
            Err(other) => panic!("expected Unavailable, got {other:?}"),
            Ok(_) => panic!("missing endpoint must not return a token"),
        }
    }

    #[tokio::test]
    async fn refresh_returns_rotated_token_from_endpoint() {
        // Pins: a successful refresh_token grant is parsed into the rotated access
        // token, rotated refresh token, expiry, and scopes.
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600,
                "scope": "email profile"
            })))
            .mount(&server)
            .await;

        let endpoint = OAuthRefreshEndpoint {
            token_endpoint: format!("{}/token", server.uri()),
            client_id: "client-123".to_string(),
            client_secret: Some(SecretString::new(
                "client-secret".to_string().into_boxed_str(),
            )),
        };
        let refresher = TokenRefresher::new(HashMap::new()).expect("refresher builds");
        let refreshed = refresher
            .refresh(&endpoint, "old-refresh")
            .await
            .expect("refresh succeeds");

        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(
            refreshed.scopes,
            Some(vec!["email".to_string(), "profile".to_string()])
        );
        assert!(refreshed.expires_at.is_some());
    }

    #[tokio::test]
    async fn refresh_http_failure_is_fail_closed() {
        // Pins: a non-2xx token endpoint fails closed with an error, never a
        // token, so an expired credential is never handed back on refresh failure.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "invalid_grant" })),
            )
            .mount(&server)
            .await;

        let endpoint = OAuthRefreshEndpoint {
            token_endpoint: format!("{}/token", server.uri()),
            client_id: "client-123".to_string(),
            client_secret: None,
        };
        let refresher = TokenRefresher::new(HashMap::new()).expect("refresher builds");
        // RefreshedToken carries token material and is intentionally not Debug,
        // so match on the result rather than using expect_err.
        match refresher.refresh(&endpoint, "old-refresh").await {
            Err(TokenVaultError::Unavailable(_)) => {}
            Err(other) => panic!("expected Unavailable, got {other:?}"),
            Ok(_) => panic!("refresh must fail closed on a non-2xx response"),
        }
    }
}
