//! Gemini model aliases and capability metadata.

use super::tools::native_google_search_tools;
use super::*;
use crate::core::models::{self, PROVIDER_GOOGLE};

pub(crate) fn canonical_model_id(model: &str) -> Result<String> {
    let model = model.trim();
    if model.starts_with("gemini-2.") {
        return Err(MoaError::Unsupported(
            "Gemini 2 models are no longer supported; use gemini-3-flash-preview".to_string(),
        ));
    }
    if models::find_for_provider_model(PROVIDER_GOOGLE, model).is_some() {
        return Ok(model.to_string());
    }

    Err(MoaError::Unsupported(format!(
        "unsupported Google Gemini model '{model}'"
    )))
}

pub(crate) fn capabilities_for_model(model: &str) -> Result<ModelCapabilities> {
    models::capabilities_for_provider_model(PROVIDER_GOOGLE, model, native_google_search_tools())
}
