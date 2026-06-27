//! Security-audit configuration.

use serde::{Deserialize, Serialize};

/// OCSF security-event audit settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditSecurityConfig {
    /// Emit allowed authorization decisions in addition to denied decisions.
    pub emit_authz_allows: bool,
}

impl super::MoaEnvOverlay {
    /// Applies security audit environment overrides.
    pub(in crate::config) fn apply_audit_security_overlay(&self, config: &mut super::MoaConfig) {
        super::env_overlay::set_copy_if_some(
            &mut config.audit_security.emit_authz_allows,
            self.audit_security_emit_authz_allows,
        );
    }
}
