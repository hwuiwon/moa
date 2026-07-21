//! Data-class egress governance for outbound MCP tool calls.
//!
//! An external MCP server is a place where restricted data can leave the trust
//! boundary. [`McpEgressGuard`] is the single decision primitive that classifies
//! an outbound payload and blocks the call when the payload carries a data class
//! the destination server is not allowlisted to receive.
//!
//! The policy is a per-server allowlist
//! ([`moa_core::config::McpServerConfig::allowed_data_classes`]). It is
//! conservative: [`SensitivityClass::None`] content may always leave, but `pii`,
//! `phi`, and `restricted` payloads are blocked unless the operator explicitly
//! lists the class for that server. The guard fails closed — a classification
//! error, a classifier abstention (genuine uncertainty), or a payload whose
//! class is not allowlisted all block the call and the payload is never sent.
//! Failing closed on abstain matches the LLM egress boundary's unconditional
//! treatment of uncertainty as unsafe.
//!
//! Detection is the injected [`PiiClassifier`], mirroring the LLM egress guard in
//! `moa-providers`. Only the classifier verdict's aggregate class crosses into
//! this module; payload content is never logged.

use std::collections::BTreeSet;
use std::sync::Arc;

use moa_core::types::security::SensitivityClass;
use moa_memory_pii::{PiiClassifier, PiiError};

/// Errors returned by the MCP egress guard. Every variant blocks the outbound
/// call: the payload is never sent.
#[derive(Debug, thiserror::Error)]
pub enum McpEgressError {
    /// The classified payload carries a data class the destination server is not
    /// allowlisted to receive.
    #[error("mcp egress blocked: server '{server}' is not permitted to receive {class} data")]
    ClassNotAllowed {
        /// Destination MCP server name.
        server: String,
        /// The disallowed data class detected in the payload.
        class: SensitivityClass,
    },
    /// The payload could not be classified, so the guard fails closed rather than
    /// risk sending unclassified restricted data.
    #[error("mcp egress blocked: classification failed for server '{server}'")]
    ClassificationFailed {
        /// Destination MCP server name.
        server: String,
        /// Underlying classifier error. Carries no payload content.
        #[source]
        source: PiiError,
    },
    /// The classifier returned a result but *abstained* — genuine uncertainty
    /// about the payload's data class rather than a confident "no restricted
    /// content". The guard fails closed: uncertainty must not leak restricted
    /// data to an external server.
    #[error("mcp egress blocked: classifier abstained for server '{server}'")]
    ClassifierAbstained {
        /// Destination MCP server name.
        server: String,
    },
}

impl From<McpEgressError> for moa_core::error::MoaError {
    /// Maps an egress block onto the shared crate error as a permission denial so
    /// the outbound-MCP call path can propagate it with `?`.
    fn from(error: McpEgressError) -> Self {
        moa_core::error::MoaError::PermissionDenied(error.to_string())
    }
}

/// Per-server egress policy: the set of data classes the server may receive.
#[derive(Debug, Clone)]
pub struct McpEgressPolicy {
    /// Classes permitted to leave to the server. Always contains
    /// [`SensitivityClass::None`].
    allowed: BTreeSet<SensitivityClass>,
}

impl McpEgressPolicy {
    /// Builds a policy from a server's `allowed_data_classes` allowlist.
    ///
    /// [`SensitivityClass::None`] is always permitted — non-sensitive content may
    /// always leave — so an empty allowlist yields the conservative default that
    /// permits `none` only and blocks `pii`/`phi`/`restricted`.
    #[must_use]
    pub fn from_allowlist(allowed: &[SensitivityClass]) -> Self {
        let mut set: BTreeSet<SensitivityClass> = allowed.iter().copied().collect();
        set.insert(SensitivityClass::None);
        Self { allowed: set }
    }

    /// Returns whether `class` is permitted to leave to this server.
    #[must_use]
    pub fn permits(&self, class: SensitivityClass) -> bool {
        self.allowed.contains(&class)
    }

    /// Returns whether the policy permits every classifiable data class.
    ///
    /// When it does, no payload can ever be blocked, so the classifier need not
    /// run at all — an unrestricted server pays zero egress-guard overhead.
    #[must_use]
    pub fn permits_all(&self) -> bool {
        self.allowed.contains(&SensitivityClass::Pii)
            && self.allowed.contains(&SensitivityClass::Phi)
            && self.allowed.contains(&SensitivityClass::Restricted)
    }
}

/// Fail-closed data-class egress guard for outbound MCP payloads.
///
/// Holds the injected [`PiiClassifier`] used to derive the aggregate data class
/// of an outbound payload. Construct one and share it across calls.
pub struct McpEgressGuard {
    /// Detector that produces the aggregate class checked against the allowlist.
    classifier: Arc<dyn PiiClassifier>,
}

impl McpEgressGuard {
    /// Builds a guard that classifies payloads with `classifier`.
    #[must_use]
    pub fn new(classifier: Arc<dyn PiiClassifier>) -> Self {
        Self { classifier }
    }

    /// Classifies `payload` and checks it against `server`'s `allowed` allowlist.
    ///
    /// Returns `Ok(())` when the payload may be sent. Returns an error — and the
    /// caller must never send the payload — when the payload carries a class the
    /// server is not allowlisted for, when classification fails, or when the
    /// classifier abstains (uncertainty fails closed).
    ///
    /// Performance: when the allowlist permits every class the classifier is not
    /// invoked at all, so an unrestricted server adds zero overhead. Only a
    /// restrictive allowlist pays for one classification per call.
    pub async fn check(
        &self,
        server: &str,
        allowed: &[SensitivityClass],
        payload: &str,
    ) -> Result<(), McpEgressError> {
        let policy = McpEgressPolicy::from_allowlist(allowed);
        if policy.permits_all() {
            return Ok(());
        }

        let result = self.classifier.classify(payload).await.map_err(|source| {
            // Fail closed: never send a payload we could not classify. Log the
            // governance decision only — the server, never the payload content.
            tracing::warn!(
                mcp.server = server,
                "mcp egress blocked: payload classification failed"
            );
            McpEgressError::ClassificationFailed {
                server: server.to_string(),
                source,
            }
        })?;

        // Fail closed on abstain. An abstaining classifier reports genuine
        // uncertainty ("I could not decide"), not a clean bill of health, so its
        // (typically empty) span set must not be read as "no restricted content".
        // Uncertainty must never leak restricted data to an external server.
        // There is no configurable cleartext escape hatch: abstain always blocks.
        if result.abstained {
            // Governance decision only: server id, never payload content.
            tracing::warn!(
                mcp.server = server,
                "mcp egress blocked: payload classifier abstained"
            );
            return Err(McpEgressError::ClassifierAbstained {
                server: server.to_string(),
            });
        }

        let class = result.class;

        if policy.permits(class) {
            Ok(())
        } else {
            // Governance decision only: server id + disallowed class, never payload.
            tracing::warn!(
                mcp.server = server,
                egress.blocked_class = %class,
                "mcp egress blocked: payload data class not allowlisted for server"
            );
            Err(McpEgressError::ClassNotAllowed {
                server: server.to_string(),
                class,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use moa_core::types::security::SensitivityClass;
    use moa_memory_pii::{PiiClassifier, PiiError, PiiResult};

    use super::{McpEgressError, McpEgressGuard};

    /// Test classifier that returns one fixed aggregate class, abstains, or fails
    /// on demand.
    struct FixedClassClassifier {
        class: SensitivityClass,
        fail: bool,
        abstain: bool,
    }

    impl FixedClassClassifier {
        fn returning(class: SensitivityClass) -> Arc<dyn PiiClassifier> {
            Arc::new(Self {
                class,
                fail: false,
                abstain: false,
            })
        }

        fn failing() -> Arc<dyn PiiClassifier> {
            Arc::new(Self {
                class: SensitivityClass::None,
                fail: true,
                abstain: false,
            })
        }

        /// Returns a `none`-class result with `abstained = true` — the classifier
        /// could not decide, so its empty span set means "unknown", not "safe".
        fn abstaining() -> Arc<dyn PiiClassifier> {
            Arc::new(Self {
                class: SensitivityClass::None,
                fail: false,
                abstain: true,
            })
        }
    }

    #[async_trait]
    impl PiiClassifier for FixedClassClassifier {
        async fn classify(&self, _text: &str) -> moa_memory_pii::Result<PiiResult> {
            if self.fail {
                return Err(PiiError::Inference("classifier unavailable".to_string()));
            }
            Ok(PiiResult {
                class: self.class,
                spans: Vec::new(),
                model_version: "test-fixed-class".to_string(),
                abstained: self.abstain,
            })
        }
    }

    #[tokio::test]
    async fn restricted_payload_blocked_when_server_lacks_restricted_offline() {
        // Pins: a restricted-classified payload to a server whose allowlist does not
        // include `restricted` fails closed with the disallowed class named.
        let guard = McpEgressGuard::new(FixedClassClassifier::returning(
            SensitivityClass::Restricted,
        ));

        let error = guard
            .check(
                "external-search",
                &[SensitivityClass::Pii],
                "sensitive payload",
            )
            .await
            .expect_err("restricted payload must be blocked when restricted is not allowlisted");

        assert!(matches!(
            error,
            McpEgressError::ClassNotAllowed { class: SensitivityClass::Restricted, ref server }
                if server == "external-search"
        ));
    }

    #[tokio::test]
    async fn restricted_payload_allowed_when_server_allowlists_restricted_offline() {
        // Pins: the identical restricted payload is allowed once the server's
        // allowlist explicitly includes `restricted`.
        let guard = McpEgressGuard::new(FixedClassClassifier::returning(
            SensitivityClass::Restricted,
        ));

        guard
            .check(
                "external-search",
                &[SensitivityClass::Restricted],
                "sensitive payload",
            )
            .await
            .expect("restricted payload must be allowed when restricted is allowlisted");
    }

    #[tokio::test]
    async fn none_payload_allowed_by_default_server_offline() {
        // Pins: a `none`-class payload is allowed against a default (empty) allowlist.
        let guard = McpEgressGuard::new(FixedClassClassifier::returning(SensitivityClass::None));

        guard
            .check("external-search", &[], "hello world")
            .await
            .expect("none-class payload must always be allowed");
    }

    #[tokio::test]
    async fn conservative_default_denies_pii_phi_and_restricted_offline() {
        // Pins: the conservative default (empty allowlist) blocks every sensitive
        // class, mapping each classifier verdict onto the named disallowed class.
        for (pii_class, expected) in [
            (SensitivityClass::Pii, SensitivityClass::Pii),
            (SensitivityClass::Phi, SensitivityClass::Phi),
            (SensitivityClass::Restricted, SensitivityClass::Restricted),
        ] {
            let guard = McpEgressGuard::new(FixedClassClassifier::returning(pii_class));

            let error = guard
                .check("external-search", &[], "payload")
                .await
                .expect_err("default allowlist must deny every sensitive class");

            assert!(matches!(
                error,
                McpEgressError::ClassNotAllowed { class, .. } if class == expected
            ));
        }
    }

    #[tokio::test]
    async fn classification_error_blocks_offline() {
        // Pins: a classifier error on a restrictive allowlist fails closed rather
        // than sending an unclassified payload.
        let guard = McpEgressGuard::new(FixedClassClassifier::failing());

        let error = guard
            .check("external-search", &[SensitivityClass::Pii], "payload")
            .await
            .expect_err("a classification failure must block the outbound call");

        assert!(matches!(
            error,
            McpEgressError::ClassificationFailed { ref server, .. } if server == "external-search"
        ));
    }

    #[tokio::test]
    async fn abstaining_classifier_blocks_on_restrictive_server_offline() {
        // Pins: an abstaining classifier fails closed on a restrictive allowlist.
        // The classifier reports `none` with `abstained = true`; `none` would
        // otherwise be allowed by every allowlist, so the block proves abstain is
        // treated as uncertainty rather than a clean result. A `check` that let
        // abstain fall through to the class mapping would ALLOW here.
        let guard = McpEgressGuard::new(FixedClassClassifier::abstaining());

        let error = guard
            .check("external-search", &[], "ambiguous payload")
            .await
            .expect_err("an abstaining classifier must fail closed");

        assert!(matches!(
            error,
            McpEgressError::ClassifierAbstained { ref server } if server == "external-search"
        ));
    }

    #[tokio::test]
    async fn permits_all_skips_classification_offline() {
        // Pins: when the allowlist permits every class, the classifier is never
        // invoked (zero overhead) — proven by a classifier that would otherwise
        // fail closed still yielding an allow.
        let guard = McpEgressGuard::new(FixedClassClassifier::failing());

        guard
            .check(
                "external-search",
                &[
                    SensitivityClass::Pii,
                    SensitivityClass::Phi,
                    SensitivityClass::Restricted,
                ],
                "payload",
            )
            .await
            .expect("an all-classes allowlist must skip classification and allow");
    }
}
