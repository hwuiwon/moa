//! Compliance, privacy, and DSAR signing configuration.

use serde::{Deserialize, Serialize};

/// Default key identifier recorded on privacy export manifests.
pub const PRIVACY_EXPORT_SIGNING_KEY_ID_DEFAULT: &str = "moa-privacy-export-ops";

/// Default key identifier used for lineage audit-root signatures.
pub const LINEAGE_AUDIT_SIGNING_KEY_ID_DEFAULT: &str = "moa-lineage-audit-ops";

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
    /// Optional 32-byte deployment root seed (hex or base64) for per-tenant
    /// audit-root signing keys. When set, lineage audit roots are signed and
    /// verified with a key derived from this seed and the tenant's storage
    /// partition, so one tenant's key compromise cannot forge another's trail.
    pub lineage_audit_root_seed_hex: Option<String>,
    /// Optional secret used to compute PII-vault subject pseudonyms.
    pub pii_vault_secret_hex: Option<String>,
    /// When true, privacy erasure requires a four-eyes dual-control approval by a
    /// second, distinct tenant admin before it may execute (SOX/finance
    /// segregation-of-duties control). Defaults to false so erasure keeps its
    /// single-admin behavior unless a deployment explicitly opts in.
    pub require_dual_control_for_erasure: bool,
}

impl Default for ComplianceConfig {
    fn default() -> Self {
        Self {
            privacy_approval_public_key_hex: None,
            privacy_export_signing_key_hex: None,
            privacy_export_signing_key_id: PRIVACY_EXPORT_SIGNING_KEY_ID_DEFAULT.to_string(),
            lineage_audit_signing_key_hex: None,
            lineage_audit_signing_key_id: LINEAGE_AUDIT_SIGNING_KEY_ID_DEFAULT.to_string(),
            lineage_audit_root_seed_hex: None,
            pii_vault_secret_hex: None,
            require_dual_control_for_erasure: false,
        }
    }
}
