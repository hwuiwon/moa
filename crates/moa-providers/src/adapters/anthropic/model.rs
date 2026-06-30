//! Anthropic model aliases and capability metadata.

use super::tools::native_web_search_tools;
use super::*;
use crate::core::models::{self, PROVIDER_ANTHROPIC};

pub(crate) fn canonical_model_id(model: &str) -> Result<String> {
    models::canonical_model_id(PROVIDER_ANTHROPIC, "Anthropic", model)
}

pub(crate) fn capabilities_for_model(model: &str) -> Result<ModelCapabilities> {
    models::capabilities_for_provider_model(PROVIDER_ANTHROPIC, model, native_web_search_tools())
}
