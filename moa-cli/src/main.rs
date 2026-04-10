//! CLI entry point for launching the TUI or one-shot exec mode.

mod exec;

use std::env;

use anyhow::Result;
use clap::{Parser, Subcommand};
use moa_core::MoaConfig;

/// Top-level MOA command line interface.
#[derive(Debug, Parser)]
#[command(name = "moa", about = "MOA local terminal agent", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// Supported CLI subcommands for the local Step 08 binary.
#[derive(Debug, Subcommand)]
enum Command {
    /// Runs one prompt and prints the final assistant response to stdout.
    Exec {
        /// Prompt text to submit to the current model.
        #[arg(required = true)]
        prompt: String,
    },
    /// Prints version information.
    Version,
    /// Prints a basic local environment diagnostic report.
    Doctor,
}

/// Runs the `moa` CLI binary.
#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();
    let cli = Cli::parse();
    let config = MoaConfig::load()?;

    match cli.command {
        None => {
            moa_tui::run_tui(config).await?;
        }
        Some(Command::Exec { prompt }) => {
            exec::run_exec(config, prompt).await?;
        }
        Some(Command::Version) => {
            println!("{}", version_text());
        }
        Some(Command::Doctor) => {
            print!("{}", doctor_report(&config));
        }
    }

    Ok(())
}

fn version_text() -> String {
    format!("moa {}", env!("CARGO_PKG_VERSION"))
}

fn doctor_report(config: &MoaConfig) -> String {
    let anthropic_env = &config.providers.anthropic.api_key_env;
    let anthropic_status = if env::var(anthropic_env).is_ok() {
        "present"
    } else {
        "missing"
    };
    let openai_env = &config.providers.openai.api_key_env;
    let openai_status = if env::var(openai_env).is_ok() {
        "present"
    } else {
        "missing"
    };
    let openrouter_env = &config.providers.openrouter.api_key_env;
    let openrouter_status = if env::var(openrouter_env).is_ok() {
        "present"
    } else {
        "missing"
    };

    format!(
        concat!(
            "MOA doctor\n",
            "provider: {}\n",
            "model: {}\n",
            "anthropic_key: {} ({})\n",
            "openai_key: {} ({})\n",
            "openrouter_key: {} ({})\n",
            "session_db: {}\n",
            "memory_dir: {}\n",
            "sandbox_dir: {}\n"
        ),
        config.general.default_provider,
        config.general.default_model,
        anthropic_status,
        anthropic_env,
        openai_status,
        openai_env,
        openrouter_status,
        openrouter_env,
        config.local.session_db,
        config.local.memory_dir,
        config.local.sandbox_dir,
    )
}

#[cfg(test)]
mod tests {
    use super::{doctor_report, version_text};
    use moa_core::MoaConfig;

    #[test]
    fn version_command_uses_package_version() {
        assert_eq!(version_text(), format!("moa {}", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn doctor_report_includes_model_and_paths() {
        let report = doctor_report(&MoaConfig::default());
        assert!(report.contains("model: gpt-5.4"));
        assert!(report.contains("session_db:"));
        assert!(report.contains("memory_dir:"));
    }
}
