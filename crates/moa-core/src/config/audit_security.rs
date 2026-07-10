//! Security-audit configuration.

use serde::{Deserialize, Serialize};

/// OCSF security-event audit settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditSecurityConfig {
    /// Emit allowed authorization decisions in addition to denied decisions.
    ///
    /// Defaults to `true`: compliance regimes generally require successful access
    /// to be logged, not just denials. This increases audit volume, but the OCSF
    /// audit sink has bounded drop behavior plus alertable drop metrics, so the
    /// trail degrades gracefully under load rather than blocking requests.
    /// Operators may set this to `false` once an alternative access-log
    /// system-of-record is accepted.
    pub emit_authz_allows: bool,
}

impl Default for AuditSecurityConfig {
    fn default() -> Self {
        Self {
            emit_authz_allows: true,
        }
    }
}
