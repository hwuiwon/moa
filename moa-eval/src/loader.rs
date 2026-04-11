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
    toml::from_str(&raw).map_err(|source| EvalError::ParseToml {
        path: path.to_path_buf(),
        source,
    })
}

/// Loads an agent config from a TOML file.
pub fn load_agent_config(path: &Path) -> Result<AgentConfig> {
    let raw = fs::read_to_string(path).map_err(|source| EvalError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&raw).map_err(|source| EvalError::ParseToml {
        path: path.to_path_buf(),
        source,
    })
}

/// Discovers suite TOML files in a directory.
pub fn discover_suites(dir: &Path) -> Result<Vec<PathBuf>> {
    discover_toml_files(dir)
}

/// Discovers agent-config TOML files in a directory.
pub fn discover_configs(dir: &Path) -> Result<Vec<PathBuf>> {
    discover_toml_files(dir)
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
