//! Compliance, privacy, and DSAR signing configuration.

use serde::{Deserialize, Serialize};

/// Default key identifier recorded on privacy export manifests.
pub const PRIVACY_EXPORT_SIGNING_KEY_ID_DEFAULT: &str = "moa-privacy-export-ops";

/// Default key identifier used for lineage audit-root signatures.
pub const LINEAGE_AUDIT_SIGNING_KEY_ID_DEFAULT: &str = "moa-lineage-audit-ops";

/// Provider used to sign lineage audit roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineageAuditSigningProvider {
    /// Use locally configured Ed25519 key material.
    Local,
    /// Delegate signing to the configured MOA signer HTTP endpoint.
    Http,
}

impl Default for LineageAuditSigningProvider {
    fn default() -> Self {
        Self::Local
    }
}

/// Compliance secrets and signing metadata loaded from runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComplianceConfig {
    /// Public key material used to verify signed privacy approval tokens.
    pub privacy_approval_public_key_hex: Option<String>,
    /// Private key material used to sign privacy export and lineage DSAR manifests.
    pub privacy_export_signing_key_hex: Option<String>,
    /// Stable key identifier recorded on privacy export manifests.
    pub privacy_export_signing_key_id: String,
    /// Private key material used to verify lineage audit roots.
    pub lineage_audit_signing_key_hex: Option<String>,
    /// Stable key identifier used for lineage audit-root signatures.
    pub lineage_audit_signing_key_id: String,
    /// Provider used to sign lineage audit roots.
    pub lineage_audit_signing_provider: LineageAuditSigningProvider,
    /// HTTP signer endpoint for lineage audit roots when using the HTTP provider.
    pub lineage_audit_signing_endpoint: Option<String>,
    /// Environment variable name containing the HTTP signer bearer token.
    pub lineage_audit_signing_bearer_token_env: Option<String>,
    /// Optional secret used to compute PII-vault subject pseudonyms.
    pub pii_vault_secret_hex: Option<String>,
}

impl Default for ComplianceConfig {
    fn default() -> Self {
        Self {
            privacy_approval_public_key_hex: None,
            privacy_export_signing_key_hex: None,
            privacy_export_signing_key_id: PRIVACY_EXPORT_SIGNING_KEY_ID_DEFAULT.to_string(),
            lineage_audit_signing_key_hex: None,
            lineage_audit_signing_key_id: LINEAGE_AUDIT_SIGNING_KEY_ID_DEFAULT.to_string(),
            lineage_audit_signing_provider: LineageAuditSigningProvider::Local,
            lineage_audit_signing_endpoint: None,
            lineage_audit_signing_bearer_token_env: None,
            pii_vault_secret_hex: None,
        }
    }
}
