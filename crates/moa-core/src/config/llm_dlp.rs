//! Egress-governance configuration for outbound LLM traffic.

use serde::{Deserialize, Serialize};

/// Data-loss-prevention settings applied at the LLM egress boundary
/// (`LLMProvider::complete`).
///
/// The egress boundary is the single point where MOA hands prompt content to a
/// third-party model provider. When [`tokenize_enabled`](Self::tokenize_enabled)
/// is set, restricted spans in every outbound message are replaced with
/// reversible DLP tokens before the provider sees them, and the provider's
/// response is detokenized inside the trust boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LlmDlpConfig {
    /// Tokenize restricted spans in outbound requests before they reach a
    /// provider, and detokenize the provider's response inside the trust
    /// boundary.
    ///
    /// Defaults to `false` (opt-in). Enabling egress tokenization is deliberately
    /// off by default because it adds one PII-classifier round trip per outbound
    /// message and changes the exact text the model receives — the model reasons
    /// over randomized request-scoped placeholders instead of the original spans. A tenant or
    /// deployment that requires restricted content never to leave the trust
    /// boundary in the clear turns it on explicitly; when it is off, providers are
    /// used directly with zero added overhead.
    pub tokenize_enabled: bool,
}
