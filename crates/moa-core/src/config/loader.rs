//! Configuration loading.

use crate::error::Result;

use super::{MoaConfig, MoaEnvOverlay};

impl MoaConfig {
    /// Loads runtime configuration from environment variables.
    pub fn load() -> Result<Self> {
        Self::load_from_env()
    }

    /// Loads runtime configuration from environment variables.
    pub fn load_from_env() -> Result<Self> {
        let mut config = Self::default();
        MoaEnvOverlay::from_env()?.apply_to(&mut config)?;
        config.validate()?;
        Ok(config)
    }
}
