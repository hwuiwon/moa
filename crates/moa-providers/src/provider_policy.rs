//! Deployment-wide provider routing policy and endpoint capabilities.
//!
//! [`crate::governance`] tokenizes restricted *content* before it leaves the
//! trust boundary. This module governs *where* a completion is allowed
//! to go: a deployment can be constrained to providers whose capability
//! assertions satisfy a [`DeploymentProviderPolicy`] (e.g. zero retention,
//! private deployment, a data-residency class, or an explicit allow/deny list).
//!
//! Enforcement is a single gate at the one place the provider router builds a
//! provider ([`ProviderRegistry::provider_for_id`](crate::ProviderRegistry)):
//! every routed provider — main, auxiliary, rewriter, and each failover
//! fallback — is checked against [`DeploymentProviderPolicy::evaluate`], the one
//! compliance primitive. A non-compliant provider **fails closed** (an error is
//! returned; it is never built, cached, or handed out) rather than being
//! silently rerouted or used as a fallback. A deployment with no active policy hits
//! a cheap early return and routes exactly as before with zero added overhead.
//!
//! Capability metadata is intentionally conservative: [`ProviderCapabilities`]
//! defaults to "no guarantees", because a public cloud endpoint does not
//! guarantee zero retention or private residency unless the deployment operator
//! asserts it through provider credential config
//! ([`ProviderCredentialConfig`](moa_core::config::ProviderCredentialConfig)).

use moa_core::config::{DeploymentProviderPolicyConfig, MoaConfig, ProviderCredentialConfig};
use moa_core::error::MoaError;
use moa_core::types::provider::ProviderId;

/// Capabilities a configured provider account or endpoint satisfies.
///
/// This is the effective capability MOA routes against. It is built from an
/// operator's [`ProviderCredentialConfig`] assertions; the default
/// ([`ProviderCapabilities::conservative`]) claims nothing, so a provider only
/// counts as compliant when the operator has explicitly said it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// The provider guarantees no training on, and no retention of, request
    /// content (a zero-data-retention contract or a self-hosted deployment).
    pub zero_retention: bool,
    /// The provider runs on a private/self-hosted deployment rather than a
    /// shared public endpoint.
    pub private_deployment: bool,
    /// The data-residency class this provider account is pinned to (e.g. `"us"`,
    /// `"eu"`), or `None` when no residency is guaranteed.
    pub data_residency: Option<String>,
}

impl ProviderCapabilities {
    /// The conservative baseline every built-in public provider defaults to: no
    /// zero-retention guarantee, not a private deployment, and no asserted
    /// residency. This is the honest default — MOA never over-claims a public
    /// endpoint's compliance.
    #[must_use]
    pub fn conservative() -> Self {
        Self::default()
    }

    /// Builds the effective capability from an operator's credential assertions.
    #[must_use]
    pub fn from_credential(credential: &ProviderCredentialConfig) -> Self {
        Self {
            zero_retention: credential.capabilities.zero_retention,
            private_deployment: credential.capabilities.private_deployment,
            data_residency: credential
                .capabilities
                .data_residency
                .as_deref()
                .map(str::trim)
                .filter(|residency| !residency.is_empty())
                .map(str::to_string),
        }
    }
}

/// Resolves the effective capabilities for one provider family from
/// runtime config.
///
/// This is the module's single capability accessor: it owns the mapping from a
/// [`ProviderId`] to its credential config so the rest of the crate never has to
/// know which config field backs which provider. Unconfigured providers fall
/// back to the conservative baseline.
#[must_use]
pub fn provider_capabilities(config: &MoaConfig, id: ProviderId) -> ProviderCapabilities {
    let credential = match id {
        ProviderId::OpenAI => &config.providers.openai,
        ProviderId::Anthropic => &config.providers.anthropic,
        ProviderId::Google => &config.providers.google,
    };
    ProviderCapabilities::from_credential(credential)
}

/// Why a provider was excluded by the deployment routing policy.
///
/// Display messages carry only provider-policy metadata (provider id, the
/// required capability) and never request content, so they are safe to log and
/// surface. Mapped into [`MoaError::ProviderError`] at the fail-closed boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderPolicyExclusion {
    /// The deployment policy requires zero retention; this provider does not assert
    /// it.
    #[error(
        "provider '{provider}' is excluded: deployment policy requires zero-retention providers"
    )]
    ZeroRetentionRequired {
        /// Stable id of the excluded provider.
        provider: &'static str,
    },
    /// The deployment policy requires a private deployment; this provider does not
    /// assert it.
    #[error(
        "provider '{provider}' is excluded: deployment policy requires a private-deployment provider"
    )]
    PrivateDeploymentRequired {
        /// Stable id of the excluded provider.
        provider: &'static str,
    },
    /// The deployment policy has an allowlist and this provider is not on it.
    #[error("provider '{provider}' is excluded: deployment policy allowlist does not include it")]
    NotAllowlisted {
        /// Stable id of the excluded provider.
        provider: &'static str,
    },
    /// The deployment policy explicitly denies this provider.
    #[error("provider '{provider}' is excluded: deployment policy denies it")]
    Denied {
        /// Stable id of the excluded provider.
        provider: &'static str,
    },
    /// The deployment policy requires a residency class this provider does not
    /// assert.
    #[error(
        "provider '{provider}' is excluded: deployment policy requires data residency '{required}'"
    )]
    ResidencyMismatch {
        /// Stable id of the excluded provider.
        provider: &'static str,
        /// Residency class the policy requires.
        required: String,
    },
}

impl From<ProviderPolicyExclusion> for MoaError {
    fn from(exclusion: ProviderPolicyExclusion) -> Self {
        MoaError::ProviderError(exclusion.to_string())
    }
}

/// A deployment-wide provider-routing requirement evaluated during routing.
///
/// Built from [`DeploymentProviderPolicyConfig`] via [`from_config`](Self::from_config).
/// The router holds one of these (when active) and consults
/// [`evaluate`](Self::evaluate) — the single compliance decision point — at the
/// one routing gate, before a concrete provider is built.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeploymentProviderPolicy {
    /// Require the selected provider to assert zero request retention.
    require_zero_retention: bool,
    /// Require the selected provider to assert a private/self-hosted deployment.
    require_private_deployment: bool,
    /// `None` means no allowlist (any provider may be considered); `Some` limits
    /// routing to exactly these providers.
    allowed_providers: Option<Vec<ProviderId>>,
    /// Providers that must never serve this deployment.
    denied_providers: Vec<ProviderId>,
    /// Required data-residency class, if any.
    required_residency: Option<String>,
}

impl DeploymentProviderPolicy {
    /// Builds a runtime policy from config.
    ///
    /// Provider identifiers are typed by config deserialization, so unknown
    /// names fail startup before the runtime policy is constructed.
    #[must_use]
    pub fn from_config(config: &DeploymentProviderPolicyConfig) -> Self {
        let allowed_providers = if config.allowed_providers.is_empty() {
            None
        } else {
            Some(config.allowed_providers.clone())
        };
        Self {
            require_zero_retention: config.require_zero_retention,
            require_private_deployment: config.require_private_deployment,
            allowed_providers,
            denied_providers: config.denied_providers.clone(),
            required_residency: config
                .required_residency
                .as_deref()
                .map(str::trim)
                .filter(|residency| !residency.is_empty())
                .map(str::to_string),
        }
    }

    /// Returns whether any constraint is set. When `false` the router skips
    /// policy checks entirely (unchanged routing, zero overhead).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.require_zero_retention
            || self.require_private_deployment
            || self.allowed_providers.is_some()
            || !self.denied_providers.is_empty()
            || self.required_residency.is_some()
    }

    /// Decides whether `provider`, with the given effective `capabilities`,
    /// satisfies this policy. This is the single source of truth for provider
    /// compliance; both the candidate filter and the fail-closed routing gate
    /// call it.
    pub fn evaluate(
        &self,
        provider: ProviderId,
        capabilities: &ProviderCapabilities,
    ) -> Result<(), ProviderPolicyExclusion> {
        let id = provider.as_str();

        if self.denied_providers.contains(&provider) {
            return Err(ProviderPolicyExclusion::Denied { provider: id });
        }

        if let Some(allowed) = &self.allowed_providers
            && !allowed.contains(&provider)
        {
            return Err(ProviderPolicyExclusion::NotAllowlisted { provider: id });
        }

        if self.require_zero_retention && !capabilities.zero_retention {
            return Err(ProviderPolicyExclusion::ZeroRetentionRequired { provider: id });
        }

        if self.require_private_deployment && !capabilities.private_deployment {
            return Err(ProviderPolicyExclusion::PrivateDeploymentRequired { provider: id });
        }

        if let Some(required) = &self.required_residency {
            let matches = capabilities
                .data_residency
                .as_deref()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(required));
            if !matches {
                return Err(ProviderPolicyExclusion::ResidencyMismatch {
                    provider: id,
                    required: required.clone(),
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_retention_caps() -> ProviderCapabilities {
        ProviderCapabilities {
            zero_retention: true,
            ..ProviderCapabilities::default()
        }
    }

    #[test]
    fn typed_config_ids_map_directly_to_routing_descriptors() {
        // Pins: the shared provider id is used directly by deployment policy and
        // every provider descriptor, with no second string vocabulary to drift.
        for descriptor in crate::routing::PROVIDER_DESCRIPTORS {
            let providers = moa_core::config::ProvidersConfig {
                routing_policy: DeploymentProviderPolicyConfig {
                    allowed_providers: vec![descriptor.id],
                    ..DeploymentProviderPolicyConfig::default()
                },
                ..moa_core::config::ProvidersConfig::default()
            };
            providers.validate().unwrap_or_else(|error| {
                panic!(
                    "routing provider id {} rejected by moa-core config validation: {error}",
                    descriptor.explicit_prefix
                )
            });

            let resolved = DeploymentProviderPolicy::from_config(&providers.routing_policy);
            assert_eq!(
                resolved.allowed_providers,
                Some(vec![descriptor.id]),
                "config id {} did not resolve to its routing provider",
                descriptor.explicit_prefix
            );
        }
    }

    #[test]
    fn conservative_capabilities_claim_nothing() {
        // Pins: the default capability over-claims nothing, so a public provider
        // is non-compliant against any active policy until an operator asserts it.
        let caps = ProviderCapabilities::conservative();
        assert!(!caps.zero_retention);
        assert!(!caps.private_deployment);
        assert_eq!(caps.data_residency, None);
    }

    #[test]
    fn zero_retention_policy_admits_compliant_and_excludes_others() {
        // Pins: evaluate is the compliance decision — a zero-retention provider
        // passes, a conservative one is excluded with a content-free reason.
        let policy = DeploymentProviderPolicy::from_config(&DeploymentProviderPolicyConfig {
            require_zero_retention: true,
            ..DeploymentProviderPolicyConfig::default()
        });
        assert!(policy.is_active());

        policy
            .evaluate(ProviderId::Anthropic, &zero_retention_caps())
            .expect("a zero-retention provider satisfies the policy");

        let excluded = policy
            .evaluate(ProviderId::OpenAI, &ProviderCapabilities::conservative())
            .expect_err("a non-zero-retention provider must be excluded");
        assert!(matches!(
            excluded,
            ProviderPolicyExclusion::ZeroRetentionRequired { provider: "openai" }
        ));
        // The reason is safe to log: it names the provider and requirement only.
        assert!(excluded.to_string().contains("zero-retention"));
    }

    #[test]
    fn denylist_beats_every_other_capability() {
        // Pins: a denied provider is excluded even when it satisfies every
        // capability requirement.
        let policy = DeploymentProviderPolicy::from_config(&DeploymentProviderPolicyConfig {
            denied_providers: vec![ProviderId::Anthropic],
            ..DeploymentProviderPolicyConfig::default()
        });
        let error = policy
            .evaluate(ProviderId::Anthropic, &zero_retention_caps())
            .expect_err("a denied provider is always excluded");
        assert!(matches!(
            error,
            ProviderPolicyExclusion::Denied {
                provider: "anthropic"
            }
        ));
    }

    #[test]
    fn residency_requirement_matches_case_insensitively() {
        // Pins: residency class comparison ignores case, and a missing residency
        // assertion fails closed.
        let policy = DeploymentProviderPolicy::from_config(&DeploymentProviderPolicyConfig {
            required_residency: Some("EU".to_string()),
            ..DeploymentProviderPolicyConfig::default()
        });
        let eu = ProviderCapabilities {
            data_residency: Some("eu".to_string()),
            ..ProviderCapabilities::default()
        };
        policy
            .evaluate(ProviderId::Google, &eu)
            .expect("a matching residency (case-insensitive) satisfies the policy");
        assert!(
            policy
                .evaluate(ProviderId::Google, &ProviderCapabilities::conservative())
                .is_err(),
            "a provider with no residency assertion fails a residency requirement"
        );
    }

    #[test]
    fn inactive_policy_admits_everything() {
        // Pins: an empty policy is inactive and admits any provider.
        let policy =
            DeploymentProviderPolicy::from_config(&DeploymentProviderPolicyConfig::default());
        assert!(!policy.is_active());
        assert!(
            policy
                .evaluate(ProviderId::OpenAI, &ProviderCapabilities::conservative())
                .is_ok()
        );
    }
}
