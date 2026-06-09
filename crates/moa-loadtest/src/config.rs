//! Configuration and workspace helpers for load tests.

use crate::*;

pub(crate) fn load_config() -> Result<MoaConfig> {
    MoaConfig::load()
}
