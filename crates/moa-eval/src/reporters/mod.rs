//! Built-in reporters for terminal, JSON, and optional Langfuse output.

mod json;
#[cfg(feature = "langfuse")]
mod langfuse;
mod terminal;

use std::io::IsTerminal;
use std::path::PathBuf;

use crate::{EvalError, Reporter, Result};

pub use json::JsonReporter;
#[cfg(feature = "langfuse")]
pub use langfuse::LangfuseReporter;
pub use terminal::TerminalReporter;

/// Options that influence reporter construction.
#[derive(Debug, Clone)]
pub struct ReporterOptions {
    /// Whether terminal output should include per-case detail.
    pub verbose: bool,
    /// Whether terminal output should use ANSI color.
    pub color: bool,
    /// Whether JSON output should be pretty-printed.
    pub json_pretty: bool,
}

impl Default for ReporterOptions {
    fn default() -> Self {
        Self {
            verbose: false,
            color: std::io::stdout().is_terminal(),
            json_pretty: true,
        }
    }
}

/// Builds the requested reporter set by spec string.
pub fn build_reporters(
    specs: &[String],
    options: &ReporterOptions,
) -> Result<Vec<Box<dyn Reporter>>> {
    let mut reporters: Vec<Box<dyn Reporter>> = Vec::new();

    for spec in specs {
        if spec == "terminal" {
            reporters.push(Box::new(TerminalReporter {
                verbose: options.verbose,
                color: options.color,
            }));
            continue;
        }

        if let Some(path) = spec.strip_prefix("json:") {
            reporters.push(Box::new(JsonReporter {
                output_path: PathBuf::from(path),
                pretty: options.json_pretty,
            }));
            continue;
        }

        #[cfg(feature = "langfuse")]
        if spec == "langfuse" {
            reporters.push(Box::new(LangfuseReporter::from_env()?));
            continue;
        }

        #[cfg(not(feature = "langfuse"))]
        if spec == "langfuse" {
            return Err(EvalError::InvalidConfig(
                "langfuse reporter requires the 'langfuse' feature".to_string(),
            ));
        }

        return Err(EvalError::InvalidConfig(format!(
            "unknown report target '{spec}'"
        )));
    }

    if reporters.is_empty() {
        reporters.push(Box::new(TerminalReporter {
            verbose: options.verbose,
            color: options.color,
        }));
    }

    Ok(reporters)
}

#[cfg(feature = "langfuse")]
use std::env;

#[cfg(feature = "langfuse")]
fn required_env_var(key: &str) -> Result<String> {
    env::var(key).map_err(|_| {
        EvalError::InvalidConfig(format!("missing required environment variable {key}"))
    })
}
