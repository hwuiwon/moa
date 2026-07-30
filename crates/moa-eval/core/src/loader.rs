//! File loaders for evaluation suites and agent configs.

use std::fs;
use std::path::Path;

use crate::error::{Error, Result};
use crate::types::{AgentConfig, TestSuite};

/// Loads a test suite from a TOML file.
pub fn load_suite(path: &Path) -> Result<TestSuite> {
    let raw = fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let suite: TestSuite = toml::from_str(&raw).map_err(|source| Error::ParseToml {
        path: path.to_path_buf(),
        source,
    })?;
    validate_suite(path, suite)
}

/// Loads an agent config from a TOML file.
pub fn load_agent_config(path: &Path) -> Result<AgentConfig> {
    let raw = fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let config: AgentConfig = toml::from_str(&raw).map_err(|source| Error::ParseToml {
        path: path.to_path_buf(),
        source,
    })?;
    validate_agent_config(path, config)
}

/// Rejects a suite document this build cannot execute exactly as authored.
///
/// Version and assertion-registry validation happen here, at load time, so a
/// suite that would fail closed on every case never reaches the scheduler.
fn validate_suite(path: &Path, suite: TestSuite) -> Result<TestSuite> {
    suite.validate().map_err(|error| match error {
        Error::InvalidConfig(message) => {
            Error::InvalidConfig(format!("suite file {}: {message}", path.display()))
        }
        other => other,
    })?;
    Ok(suite)
}

fn validate_agent_config(path: &Path, config: AgentConfig) -> Result<AgentConfig> {
    if config.name.trim().is_empty() {
        return Err(Error::InvalidConfig(format!(
            "agent config file {} is missing [agent].name",
            path.display()
        )));
    }
    Ok(config)
}
