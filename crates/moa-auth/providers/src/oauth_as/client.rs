//! OAuth client validation and deterministic bootstrap identities.
//!
//! Configuration is only startup input. [`OAuthClientRegistry`] validates and
//! canonicalizes that input, while request-time client resolution always reads
//! the authoritative row from Postgres.

use std::collections::HashSet;

use moa_core::config::{OAuthClientConfig, OAuthClientType};
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::{OAuthError, digest_hex};

/// A validated OAuth client loaded from the authoritative database row.
#[derive(Debug, Clone)]
pub struct OAuthClient {
    /// Public client identifier.
    pub client_id: String,
    /// Whether the client authenticates with a secret.
    pub client_type: OAuthClientType,
    pub(super) redirect_uris: Vec<String>,
    pub(super) scopes: Vec<String>,
    pub(super) client_secret_hash: Option<String>,
    pub(super) config_hash: String,
}

impl OAuthClient {
    pub(super) fn from_config(config: &OAuthClientConfig) -> Result<Self, OAuthError> {
        let client_id = config.client_id.trim().to_string();
        if client_id.is_empty() {
            return Err(OAuthError::InvalidClientConfiguration(
                "client_id must be non-empty".to_string(),
            ));
        }

        let mut redirect_uris = normalized_nonempty(&config.redirect_uris, "redirect_uris")?;
        for redirect_uri in &redirect_uris {
            let parsed = reqwest::Url::parse(redirect_uri).map_err(|error| {
                OAuthError::InvalidClientConfiguration(format!(
                    "client {client_id} has invalid redirect URI: {error}"
                ))
            })?;
            if parsed.fragment().is_some() {
                return Err(OAuthError::InvalidClientConfiguration(format!(
                    "client {client_id} redirect URI must not contain a fragment"
                )));
            }
        }
        redirect_uris.sort();

        let mut scopes = normalized_nonempty(&config.scopes, "scopes")?;
        if scopes
            .iter()
            .any(|scope| !matches!(scope.as_str(), "mcp:read" | "mcp:write"))
        {
            return Err(OAuthError::InvalidClientConfiguration(format!(
                "client {client_id} requests a non-MCP scope"
            )));
        }
        scopes.sort();

        let client_secret_hash = match config.client_type {
            OAuthClientType::Public => {
                if config.client_secret_sha256.is_some() {
                    return Err(OAuthError::InvalidClientConfiguration(format!(
                        "public client {client_id} must not configure a secret hash"
                    )));
                }
                None
            }
            OAuthClientType::Confidential => {
                let hash = config
                    .client_secret_sha256
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .filter(|hash| is_sha256_hex(hash))
                    .ok_or_else(|| {
                        OAuthError::InvalidClientConfiguration(format!(
                            "confidential client {client_id} requires a SHA-256 secret hash"
                        ))
                    })?;
                Some(hash)
            }
        };

        let config_hash = config_hash(
            &client_id,
            config.client_type,
            &redirect_uris,
            &scopes,
            client_secret_hash.as_deref(),
        );
        Ok(Self {
            client_id,
            client_type: config.client_type,
            redirect_uris,
            scopes,
            client_secret_hash,
            config_hash,
        })
    }

    pub(super) fn from_storage(
        client_id: String,
        client_type: &str,
        redirect_uris: Vec<String>,
        scopes: Vec<String>,
        client_secret_hash: Option<String>,
        config_hash: String,
    ) -> Result<Self, OAuthError> {
        let client_type = match client_type {
            "public" => OAuthClientType::Public,
            "confidential" => OAuthClientType::Confidential,
            other => {
                return Err(OAuthError::Storage(format!(
                    "invalid persisted OAuth client type {other}"
                )));
            }
        };
        Ok(Self {
            client_id,
            client_type,
            redirect_uris,
            scopes,
            client_secret_hash,
            config_hash,
        })
    }

    /// Whether `redirect_uri` is registered for this client.
    #[must_use]
    pub fn allows_redirect(&self, redirect_uri: &str) -> bool {
        self.redirect_uris.iter().any(|uri| uri == redirect_uri)
    }

    /// Whether every requested scope is registered for this client.
    #[must_use]
    pub fn allows_scopes(&self, requested: &[String]) -> bool {
        !requested.is_empty()
            && requested
                .iter()
                .all(|scope| self.scopes.iter().any(|allowed| allowed == scope))
    }

    /// Authenticate this client for a token endpoint operation.
    #[must_use]
    pub fn authenticate(&self, presented_secret: Option<&SecretString>) -> bool {
        match self.client_type {
            OAuthClientType::Public => presented_secret.is_none(),
            OAuthClientType::Confidential => {
                let (Some(expected), Some(secret)) =
                    (self.client_secret_hash.as_ref(), presented_secret)
                else {
                    return false;
                };
                let candidate = digest_hex(secret.expose_secret());
                candidate.as_bytes().ct_eq(expected.as_bytes()).into()
            }
        }
    }

    /// Whether this client is confidential.
    #[must_use]
    pub fn is_confidential(&self) -> bool {
        matches!(self.client_type, OAuthClientType::Confidential)
    }

    /// Exact scopes registered for this client.
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }
}

/// Validated, canonical startup client declarations.
#[derive(Debug, Clone)]
pub struct OAuthClientRegistry {
    clients: Vec<OAuthClient>,
}

impl OAuthClientRegistry {
    /// Validate and canonicalize startup client declarations.
    pub fn from_configs(configs: &[OAuthClientConfig]) -> Result<Self, OAuthError> {
        let mut seen = HashSet::with_capacity(configs.len());
        let mut clients = Vec::with_capacity(configs.len());
        for config in configs {
            let client = OAuthClient::from_config(config)?;
            if !seen.insert(client.client_id.clone()) {
                return Err(OAuthError::InvalidClientConfiguration(format!(
                    "duplicate client_id {}",
                    client.client_id
                )));
            }
            clients.push(client);
        }
        clients.sort_by(|left, right| left.client_id.cmp(&right.client_id));
        Ok(Self { clients })
    }

    pub(super) fn clients(&self) -> &[OAuthClient] {
        &self.clients
    }
}

fn normalized_nonempty(values: &[String], field: &str) -> Result<Vec<String>, OAuthError> {
    let mut normalized = values
        .iter()
        .map(|value| value.trim().to_string())
        .collect::<Vec<_>>();
    if normalized.is_empty() || normalized.iter().any(String::is_empty) {
        return Err(OAuthError::InvalidClientConfiguration(format!(
            "{field} must contain non-empty values"
        )));
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn config_hash(
    client_id: &str,
    client_type: OAuthClientType,
    redirect_uris: &[String],
    scopes: &[String],
    client_secret_hash: Option<&str>,
) -> String {
    let mut digest = Sha256::new();
    hash_field(&mut digest, client_id.as_bytes());
    hash_field(
        &mut digest,
        match client_type {
            OAuthClientType::Public => b"public",
            OAuthClientType::Confidential => b"confidential",
        },
    );
    for uri in redirect_uris {
        hash_field(&mut digest, uri.as_bytes());
    }
    for scope in scopes {
        hash_field(&mut digest, scope.as_bytes());
    }
    hash_field(
        &mut digest,
        client_secret_hash.unwrap_or_default().as_bytes(),
    );
    hex::encode(digest.finalize())
}

fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_hash_is_order_independent_for_set_fields() {
        // Pins: equivalent client configuration converges across replicas.
        let first = OAuthClient::from_config(&OAuthClientConfig {
            client_id: "client".to_string(),
            client_type: OAuthClientType::Public,
            redirect_uris: vec![
                "https://app.example/b".to_string(),
                "https://app.example/a".to_string(),
            ],
            scopes: vec!["mcp:write".to_string(), "mcp:read".to_string()],
            client_secret_sha256: None,
        })
        .expect("valid client");
        let second = OAuthClient::from_config(&OAuthClientConfig {
            client_id: "client".to_string(),
            client_type: OAuthClientType::Public,
            redirect_uris: vec![
                "https://app.example/a".to_string(),
                "https://app.example/b".to_string(),
            ],
            scopes: vec!["mcp:read".to_string(), "mcp:write".to_string()],
            client_secret_sha256: None,
        })
        .expect("valid client");
        assert_eq!(first.config_hash, second.config_hash);
    }
}
