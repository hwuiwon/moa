//! Provider-family and model-name routing helpers.

/// Default Anthropic model used when Anthropic is the first configured provider.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
/// Default OpenAI model used when OpenAI is the first configured provider.
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-5.4";
/// Default Google model used when Google is the first configured provider.
pub const DEFAULT_GOOGLE_MODEL: &str = "gemini-3-flash-preview";
/// Default Anthropic model for query rewriting.
pub const REWRITER_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";
/// Default OpenAI model for query rewriting.
pub const REWRITER_OPENAI_MODEL: &str = "gpt-5.4-mini";
/// Default Google model for query rewriting.
pub const REWRITER_GOOGLE_MODEL: &str = "gemini-3-flash-preview";

/// Provider family selected for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// Anthropic Claude models.
    Anthropic,
    /// OpenAI GPT/o-series models.
    OpenAI,
    /// Google Gemini models.
    Google,
}

impl ProviderKind {
    /// Returns the stable provider-name string used in config and telemetry.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::Google => "google",
        }
    }
}

pub(crate) fn split_explicit_provider(model: &str) -> Option<(ProviderKind, &str)> {
    let (provider, model_id) = model.split_once(':')?;
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }

    let kind = match provider.trim() {
        "anthropic" => ProviderKind::Anthropic,
        "openai" => ProviderKind::OpenAI,
        "google" => ProviderKind::Google,
        _ => return None,
    };

    Some((kind, model_id))
}

pub(crate) fn infer_provider_kind(model: &str) -> Option<ProviderKind> {
    if model.starts_with("claude-") {
        return Some(ProviderKind::Anthropic);
    }
    if model.starts_with("gemini-") {
        return Some(ProviderKind::Google);
    }
    if model.starts_with("gpt-")
        || model.starts_with("chatgpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        return Some(ProviderKind::OpenAI);
    }

    None
}
