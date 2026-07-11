//! Shared helpers for `provider:model` model selectors.

use moa_core::{error::MoaError, error::Result};

/// A model selector split into provider and model segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExplicitProviderModel<'a> {
    /// Provider segment before the first colon.
    pub provider: &'a str,
    /// Model segment after the first colon.
    pub model: &'a str,
}

/// Splits a `provider:model` selector and validates that both segments exist.
pub(crate) fn split_explicit_provider_model<'a>(
    value: &'a str,
    field_name: &str,
) -> Result<Option<ExplicitProviderModel<'a>>> {
    let value = value.trim();
    let Some((provider, model)) = value.split_once(':') else {
        return Ok(None);
    };
    let provider = provider.trim();
    let model = model.trim();
    if provider.is_empty() || model.is_empty() {
        return Err(MoaError::ConfigError(format!(
            "{field_name} must use provider:model with both segments present"
        )));
    }
    Ok(Some(ExplicitProviderModel { provider, model }))
}

/// Normalizes a provider name to lowercase with `-` separators for selector matching.
pub(crate) fn normalize_provider_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "-")
}
