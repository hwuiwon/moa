//! File loaders and discovery helpers for evaluation suites and agent configs.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{EvalError, Result};
use crate::types::{AgentConfig, TestSuite};

/// Loads a test suite from a TOML file.
pub fn load_suite(path: &Path) -> Result<TestSuite> {
    let raw = fs::read_to_string(path).map_err(|source| EvalError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let suite: TestSuite = toml::from_str(&raw).map_err(|source| EvalError::ParseToml {
        path: path.to_path_buf(),
        source,
    })?;
    validate_suite(path, suite)
}

/// Loads an agent config from a TOML file.
pub fn load_agent_config(path: &Path) -> Result<AgentConfig> {
    let raw = fs::read_to_string(path).map_err(|source| EvalError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let config: AgentConfig = toml::from_str(&raw).map_err(|source| EvalError::ParseToml {
        path: path.to_path_buf(),
        source,
    })?;
    validate_agent_config(path, config)
}

/// Discovers suite TOML files in a directory.
pub fn discover_suites(dir: &Path) -> Result<Vec<PathBuf>> {
    discover_matching_toml_files(dir, load_suite)
}

/// Discovers agent-config TOML files in a directory.
pub fn discover_configs(dir: &Path) -> Result<Vec<PathBuf>> {
    discover_matching_toml_files(dir, load_agent_config)
}

fn discover_toml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = fs::read_dir(dir).map_err(|source| EvalError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| EvalError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "toml") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn discover_matching_toml_files<T>(
    dir: &Path,
    loader: fn(&Path) -> Result<T>,
) -> Result<Vec<PathBuf>> {
    let candidates = discover_toml_files(dir)?;
    Ok(candidates
        .into_iter()
        .filter(|path| loader(path).is_ok())
        .collect())
}

fn validate_suite(path: &Path, suite: TestSuite) -> Result<TestSuite> {
    if suite.name.trim().is_empty() {
        return Err(EvalError::InvalidConfig(format!(
            "suite file {} is missing [suite].name",
            path.display()
        )));
    }
    Ok(suite)
}

fn validate_agent_config(path: &Path, config: AgentConfig) -> Result<AgentConfig> {
    if config.name.trim().is_empty() {
        return Err(EvalError::InvalidConfig(format!(
            "agent config file {} is missing [agent].name",
            path.display()
        )));
    }
    Ok(config)
}
