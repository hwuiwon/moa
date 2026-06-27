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
            pii_vault_secret_hex: None,
        }
    }
}

impl super::MoaEnvOverlay {
    /// Applies compliance, privacy, and lineage signing environment overrides.
    pub(in crate::config) fn apply_compliance_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_if_some, set_option_if_some};

        set_option_if_some(
            &mut config.compliance.privacy_approval_public_key_hex,
            &self.privacy_approval_public_key_hex,
        );
        set_option_if_some(
            &mut config.compliance.privacy_export_signing_key_hex,
            &self.privacy_export_signing_key_hex,
        );
        set_if_some(
            &mut config.compliance.privacy_export_signing_key_id,
            &self.privacy_export_signing_key_id,
        );
        set_option_if_some(
            &mut config.compliance.lineage_audit_signing_key_hex,
            &self.lineage_audit_signing_key_hex,
        );
        set_if_some(
            &mut config.compliance.lineage_audit_signing_key_id,
            &self.lineage_audit_signing_key_id,
        );
        set_option_if_some(
            &mut config.compliance.pii_vault_secret_hex,
            &self.pii_vault_secret_hex,
        );
    }
}
