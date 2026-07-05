//! Security-audit configuration.

use serde::{Deserialize, Serialize};

/// OCSF security-event audit settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditSecurityConfig {
    /// Emit allowed authorization decisions in addition to denied decisions.
    pub emit_authz_allows: bool,
}
