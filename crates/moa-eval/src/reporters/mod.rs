//! Built-in reporters for terminal and JSON output.

mod json;
mod terminal;

use std::io::IsTerminal;
use std::path::PathBuf;

use moa_eval_core::{EvalError, Result};

use crate::Reporter;

pub use json::JsonReporter;
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
