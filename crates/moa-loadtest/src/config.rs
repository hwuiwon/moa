//! Configuration and workspace helpers for load tests.

use crate::*;

pub(crate) fn load_config(path: Option<&Path>) -> Result<MoaConfig> {
    match path {
        Some(path) => MoaConfig::load_from_path(path),
        None => MoaConfig::load(),
    }
}
