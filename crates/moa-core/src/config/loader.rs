//! Configuration loading.

use crate::error::Result;

use super::{EnvOverlay, MoaConfig};

impl MoaConfig {
    /// Loads runtime configuration from environment variables.
    pub fn load() -> Result<Self> {
        Self::load_from_env()
    }

    /// Loads runtime configuration from environment variables.
    pub fn load_from_env() -> Result<Self> {
        // envy silently ignores misspelled `MOA_*` variables, so audit the
        // process environment before the overlay swallows the typo.
        EnvOverlay::audit_env_registry(
            std::env::vars().map(|(name, _)| name),
            EnvOverlay::env_registry_strict_from_env(),
        )?;
        let mut config = Self::default();
        EnvOverlay::from_env()?.apply_to(&mut config)?;
        config.validate()?;
        Ok(config)
    }
}
