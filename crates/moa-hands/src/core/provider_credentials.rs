//! Rotating, provider-account-scoped credentials for cloud sandbox control planes.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::os::unix::fs::MetadataExt as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_config::{
    CloudHandProviderAccountConfig, CloudHandProviderKind, CloudHandsConfig,
    ProviderSecretFileSelector,
};
use moa_core::error::{MoaError, Result};
use moa_core::types::identifiers::ProviderAccountId;
use moa_security::outbound_http::{
    OutboundHttpClientLimits, build_admitted_http_client, parse_canonical_origin,
};
use moa_security::{OutboundHttpPolicy, TokioOutboundHostResolver};
use secrecy::{ExposeSecret as _, SecretString};
use tokio::io::AsyncReadExt as _;

const MAX_PROVIDER_CREDENTIAL_BYTES: u64 = 16 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_HEADER_LIMIT: u32 = 64 * 1024;

/// Which exact operator-reviewed origin one request attempt targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderEndpoint {
    /// Provider control-plane API.
    Api,
    /// Daytona toolbox API.
    Toolbox,
}

/// One provider attempt with a freshly read secret and DNS-pinned client.
///
/// Debug output intentionally reports only durable account metadata. This type
/// has no serialization implementation so credentials cannot enter Restate or
/// event payloads by accident.
pub struct ProviderHttpAttempt {
    provider_account_id: ProviderAccountId,
    provider_account_generation: u64,
    provider: CloudHandProviderKind,
    client: reqwest::Client,
    origin: String,
    credential: SecretString,
    sandbox_domain: Option<String>,
    default_runtime: Option<String>,
}

/// One admitted provider-issued sandbox traffic origin.
pub struct ProviderSandboxAttempt {
    client: reqwest::Client,
    origin: String,
}

impl ProviderSandboxAttempt {
    /// Returns the DNS-pinned, no-redirect client for this attempt.
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Returns the admitted exact sandbox traffic origin.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

impl fmt::Debug for ProviderHttpAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderHttpAttempt")
            .field("provider_account_id", &self.provider_account_id)
            .field(
                "provider_account_generation",
                &self.provider_account_generation,
            )
            .field("provider", &self.provider)
            .field("credential", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl ProviderHttpAttempt {
    /// Returns the freshly admitted HTTP client.
    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Returns the exact canonical origin admitted for this attempt.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Returns the provider credential to the trusted adapter request builder.
    #[must_use]
    pub(crate) fn credential(&self) -> &str {
        self.credential.expose_secret()
    }

    /// Returns a non-reversible fingerprint for rotation diagnostics and tests.
    #[must_use]
    pub fn credential_fingerprint(&self) -> String {
        use sha2::{Digest as _, Sha256};
        format!(
            "sha256:{:x}",
            Sha256::digest(self.credential.expose_secret().as_bytes())
        )
    }

    /// Returns the configured sandbox traffic domain, when applicable.
    #[must_use]
    pub fn sandbox_domain(&self) -> Option<&str> {
        self.sandbox_domain.as_deref()
    }

    /// Returns the configured default image or template, when applicable.
    #[must_use]
    pub fn default_runtime(&self) -> Option<&str> {
        self.default_runtime.as_deref()
    }
}

/// Resolves a cloud provider credential from persisted account context.
#[async_trait]
pub trait ProviderCredentialSource: Send + Sync {
    /// Resolves and admits one exact endpoint for one provider request attempt.
    async fn resolve_attempt(
        &self,
        provider_account_id: ProviderAccountId,
        provider_account_generation: u64,
        expected_provider: CloudHandProviderKind,
        endpoint: ProviderEndpoint,
        total_timeout: Duration,
    ) -> Result<ProviderHttpAttempt>;

    /// Validates every configured mapping and credential file before serving.
    async fn validate_all(&self) -> Result<()>;

    /// Admits one provider-issued sandbox traffic origin for a persisted account.
    async fn admit_sandbox_attempt(
        &self,
        provider_account_id: ProviderAccountId,
        provider_account_generation: u64,
        expected_provider: CloudHandProviderKind,
        origin: &str,
        total_timeout: Duration,
    ) -> Result<ProviderSandboxAttempt>;

    /// Returns whether at least one mapping exists for the named provider.
    fn has_provider(&self, provider: CloudHandProviderKind) -> bool;
}

/// File-backed provider credential source used by production construction.
///
/// Only non-secret selectors are retained. Credential bytes are read from the
/// open file descriptor for every attempt, so an atomic Kubernetes projected
/// volume rotation is observed without process restart.
pub struct FileProviderCredentialSource {
    accounts: HashMap<ProviderAccountId, CloudHandProviderAccountConfig>,
    outbound_policy: OutboundHttpPolicy,
}

impl fmt::Debug for FileProviderCredentialSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileProviderCredentialSource")
            .field("configured_accounts", &self.accounts.len())
            .finish_non_exhaustive()
    }
}

impl FileProviderCredentialSource {
    /// Builds the production source from non-secret cloud-hands configuration.
    pub fn from_config(config: &CloudHandsConfig) -> Result<Self> {
        Self::with_policy(
            config,
            OutboundHttpPolicy::production(Arc::new(TokioOutboundHostResolver)),
        )
    }

    /// Builds a source with an explicit outbound policy.
    ///
    /// The injected form is used by deterministic loopback tests. Production
    /// construction always calls [`Self::from_config`].
    pub fn with_policy(
        config: &CloudHandsConfig,
        outbound_policy: OutboundHttpPolicy,
    ) -> Result<Self> {
        let mut accounts = HashMap::new();
        let mut provider_cells = HashSet::new();
        for account in &config.provider_accounts {
            validate_account_mapping(account)?;
            if accounts
                .insert(account.provider_account_id, account.clone())
                .is_some()
            {
                return Err(MoaError::ConfigError(
                    "duplicate sandbox provider account mapping".to_string(),
                ));
            }
            if !provider_cells.insert((account.provider, account.isolation_cell.clone())) {
                return Err(MoaError::ConfigError(
                    "duplicate sandbox provider isolation-cell mapping".to_string(),
                ));
            }
        }
        Ok(Self {
            accounts,
            outbound_policy,
        })
    }

    async fn resolve_file(selector: &ProviderSecretFileSelector) -> Result<SecretString> {
        let before = tokio::fs::symlink_metadata(&selector.path)
            .await
            .map_err(|_| safe_credential_error("credential file is missing"))?;
        validate_file_metadata(&before, selector.owner_uid)?;

        let mut file = tokio::fs::File::open(&selector.path)
            .await
            .map_err(|_| safe_credential_error("credential file could not be opened"))?;
        let opened = file
            .metadata()
            .await
            .map_err(|_| safe_credential_error("credential file metadata could not be read"))?;
        validate_file_metadata(&opened, selector.owner_uid)?;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            return Err(safe_credential_error(
                "credential file changed during validation",
            ));
        }
        if opened.len() > MAX_PROVIDER_CREDENTIAL_BYTES {
            return Err(safe_credential_error("credential file is too large"));
        }

        let mut value = String::new();
        file.read_to_string(&mut value)
            .await
            .map_err(|_| safe_credential_error("credential file could not be read"))?;
        let value = value.trim();
        if value.is_empty() || value.contains(['\r', '\n', '\0']) {
            return Err(safe_credential_error(
                "credential file must contain one non-empty line",
            ));
        }
        Ok(SecretString::from(value.to_string()))
    }
}

#[async_trait]
impl ProviderCredentialSource for FileProviderCredentialSource {
    async fn resolve_attempt(
        &self,
        provider_account_id: ProviderAccountId,
        provider_account_generation: u64,
        expected_provider: CloudHandProviderKind,
        endpoint: ProviderEndpoint,
        total_timeout: Duration,
    ) -> Result<ProviderHttpAttempt> {
        let account = self
            .accounts
            .get(&provider_account_id)
            .filter(|account| {
                account.generation == provider_account_generation
                    && account.provider == expected_provider
            })
            .ok_or_else(|| safe_credential_error("provider account mapping is unavailable"))?;
        let origin = match endpoint {
            ProviderEndpoint::Api => &account.api_origin,
            ProviderEndpoint::Toolbox => account
                .toolbox_origin
                .as_ref()
                .ok_or_else(|| safe_credential_error("provider endpoint mapping is unavailable"))?,
        };
        let credential = Self::resolve_file(&account.credential).await?;
        let admitted = self
            .outbound_policy
            .admit(origin, CONNECT_TIMEOUT)
            .await
            .map_err(|_| safe_credential_error("provider origin was not admitted"))?;
        let limits =
            OutboundHttpClientLimits::new(CONNECT_TIMEOUT, total_timeout, RESPONSE_HEADER_LIMIT)
                .map_err(|_| safe_credential_error("provider client limits are invalid"))?;
        let client = build_admitted_http_client(&admitted, limits)
            .map_err(|_| safe_credential_error("provider client could not be built"))?;
        Ok(ProviderHttpAttempt {
            provider_account_id,
            provider_account_generation,
            provider: account.provider,
            client,
            origin: admitted.canonical_origin().origin().ascii_serialization(),
            credential,
            sandbox_domain: account.sandbox_domain.clone(),
            default_runtime: account.default_runtime.clone(),
        })
    }

    async fn validate_all(&self) -> Result<()> {
        for account in self.accounts.values() {
            let _credential = Self::resolve_file(&account.credential).await?;
        }
        Ok(())
    }

    async fn admit_sandbox_attempt(
        &self,
        provider_account_id: ProviderAccountId,
        provider_account_generation: u64,
        expected_provider: CloudHandProviderKind,
        origin: &str,
        total_timeout: Duration,
    ) -> Result<ProviderSandboxAttempt> {
        let account = self
            .accounts
            .get(&provider_account_id)
            .filter(|account| {
                account.generation == provider_account_generation
                    && account.provider == expected_provider
            })
            .ok_or_else(|| safe_credential_error("provider account mapping is unavailable"))?;
        let expected_domain = account
            .sandbox_domain
            .as_deref()
            .ok_or_else(|| safe_credential_error("sandbox domain mapping is unavailable"))?;
        let parsed = parse_canonical_origin(origin)
            .map_err(|_| safe_credential_error("sandbox traffic origin is invalid"))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| safe_credential_error("sandbox traffic origin is invalid"))?;
        if parsed.scheme() != "https"
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || (host != expected_domain
                && !host
                    .strip_suffix(expected_domain)
                    .is_some_and(|prefix| prefix.ends_with('.')))
        {
            return Err(safe_credential_error(
                "sandbox traffic origin is outside the configured provider domain",
            ));
        }
        let admitted = self
            .outbound_policy
            .admit(origin, CONNECT_TIMEOUT)
            .await
            .map_err(|_| safe_credential_error("sandbox traffic origin was not admitted"))?;
        let limits =
            OutboundHttpClientLimits::new(CONNECT_TIMEOUT, total_timeout, RESPONSE_HEADER_LIMIT)
                .map_err(|_| safe_credential_error("provider client limits are invalid"))?;
        let client = build_admitted_http_client(&admitted, limits)
            .map_err(|_| safe_credential_error("provider client could not be built"))?;
        Ok(ProviderSandboxAttempt {
            client,
            origin: admitted.canonical_origin().origin().ascii_serialization(),
        })
    }

    fn has_provider(&self, provider: CloudHandProviderKind) -> bool {
        self.accounts
            .values()
            .any(|account| account.provider == provider)
    }
}

fn validate_account_mapping(account: &CloudHandProviderAccountConfig) -> Result<()> {
    if account.generation == 0
        || account.isolation_cell.trim().is_empty()
        || account.isolation_cell.trim() != account.isolation_cell
        || !account.credential.path.is_absolute()
        || account
            .default_runtime
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.trim() != value)
        || account
            .project_fingerprint
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.trim() != value)
    {
        return Err(safe_credential_error("provider account mapping is invalid"));
    }
    validate_exact_https_origin(&account.api_origin)?;
    match account.provider {
        CloudHandProviderKind::Daytona => {
            let toolbox_origin = account.toolbox_origin.as_deref().ok_or_else(|| {
                safe_credential_error("Daytona provider account requires a toolbox origin")
            })?;
            validate_exact_https_origin(toolbox_origin)?;
            if account.sandbox_domain.is_some() {
                return Err(safe_credential_error(
                    "Daytona provider account mapping is invalid",
                ));
            }
        }
        CloudHandProviderKind::E2b => {
            if account.toolbox_origin.is_some() {
                return Err(safe_credential_error(
                    "E2B provider account mapping is invalid",
                ));
            }
            let sandbox_domain = account.sandbox_domain.as_deref().ok_or_else(|| {
                safe_credential_error("E2B provider account requires a sandbox domain")
            })?;
            validate_dns_suffix(sandbox_domain)?;
        }
    }
    Ok(())
}

fn validate_exact_https_origin(origin: &str) -> Result<()> {
    let parsed = parse_canonical_origin(origin)
        .map_err(|_| safe_credential_error("provider origin mapping is invalid"))?;
    if parsed.scheme() != "https" || parsed.origin().ascii_serialization() != origin {
        return Err(safe_credential_error("provider origin mapping is invalid"));
    }
    Ok(())
}

fn validate_dns_suffix(domain: &str) -> Result<()> {
    let valid = domain.len() <= 253
        && domain.contains('.')
        && domain.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    if !valid {
        return Err(safe_credential_error("sandbox domain mapping is invalid"));
    }
    Ok(())
}

fn validate_file_metadata(metadata: &std::fs::Metadata, owner_uid: u32) -> Result<()> {
    if !metadata.is_file()
        || metadata.uid() != owner_uid
        || metadata.mode() & 0o077 != 0
        || metadata.mode() & 0o400 == 0
    {
        return Err(safe_credential_error(
            "credential file ownership or mode is invalid",
        ));
    }
    Ok(())
}

fn safe_credential_error(message: &str) -> MoaError {
    MoaError::ConfigError(message.to_string())
}

/// Deterministic source used only by adapter unit tests.
#[cfg(test)]
pub(crate) struct TestProviderCredentialSource {
    /// Provider served by the fixture.
    pub provider: CloudHandProviderKind,
    /// Exact fixture control-plane origin.
    pub api_origin: String,
    /// Exact fixture toolbox origin.
    pub toolbox_origin: Option<String>,
    /// Fixture sandbox domain.
    pub sandbox_domain: Option<String>,
    /// Fixture default runtime.
    pub default_runtime: Option<String>,
    /// Scripted secret material.
    pub credential: SecretString,
    policy: OutboundHttpPolicy,
}

#[cfg(test)]
impl TestProviderCredentialSource {
    /// Creates a loopback-admitted unit-test source.
    pub(crate) fn new(
        provider: CloudHandProviderKind,
        api_origin: impl Into<String>,
        toolbox_origin: Option<String>,
        sandbox_domain: Option<String>,
        default_runtime: Option<String>,
        credential: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            api_origin: api_origin.into(),
            toolbox_origin,
            sandbox_domain,
            default_runtime,
            credential: SecretString::from(credential.into()),
            policy: OutboundHttpPolicy::loopback_http_for_tests(Arc::new(
                TokioOutboundHostResolver,
            )),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl ProviderCredentialSource for TestProviderCredentialSource {
    async fn resolve_attempt(
        &self,
        provider_account_id: ProviderAccountId,
        provider_account_generation: u64,
        expected_provider: CloudHandProviderKind,
        endpoint: ProviderEndpoint,
        total_timeout: Duration,
    ) -> Result<ProviderHttpAttempt> {
        if expected_provider != self.provider {
            return Err(safe_credential_error("test provider kind mismatch"));
        }
        let origin = match endpoint {
            ProviderEndpoint::Api => &self.api_origin,
            ProviderEndpoint::Toolbox => self
                .toolbox_origin
                .as_ref()
                .ok_or_else(|| safe_credential_error("test toolbox origin is unavailable"))?,
        };
        let admitted = self
            .policy
            .admit(origin, CONNECT_TIMEOUT)
            .await
            .map_err(|_| safe_credential_error("test origin was not admitted"))?;
        let limits =
            OutboundHttpClientLimits::new(CONNECT_TIMEOUT, total_timeout, RESPONSE_HEADER_LIMIT)
                .map_err(|_| safe_credential_error("test client limits are invalid"))?;
        Ok(ProviderHttpAttempt {
            provider_account_id,
            provider_account_generation,
            provider: self.provider,
            client: build_admitted_http_client(&admitted, limits)
                .map_err(|_| safe_credential_error("test client could not be built"))?,
            origin: admitted.canonical_origin().origin().ascii_serialization(),
            credential: self.credential.clone(),
            sandbox_domain: self.sandbox_domain.clone(),
            default_runtime: self.default_runtime.clone(),
        })
    }

    async fn validate_all(&self) -> Result<()> {
        Ok(())
    }

    async fn admit_sandbox_attempt(
        &self,
        _provider_account_id: ProviderAccountId,
        _provider_account_generation: u64,
        _expected_provider: CloudHandProviderKind,
        origin: &str,
        total_timeout: Duration,
    ) -> Result<ProviderSandboxAttempt> {
        let admitted = self
            .policy
            .admit(origin, CONNECT_TIMEOUT)
            .await
            .map_err(|_| safe_credential_error("test sandbox origin was not admitted"))?;
        let limits =
            OutboundHttpClientLimits::new(CONNECT_TIMEOUT, total_timeout, RESPONSE_HEADER_LIMIT)
                .map_err(|_| safe_credential_error("test client limits are invalid"))?;
        Ok(ProviderSandboxAttempt {
            client: build_admitted_http_client(&admitted, limits)
                .map_err(|_| safe_credential_error("test client could not be built"))?,
            origin: admitted.canonical_origin().origin().ascii_serialization(),
        })
    }

    fn has_provider(&self, provider: CloudHandProviderKind) -> bool {
        provider == self.provider
    }
}
