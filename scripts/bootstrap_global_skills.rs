//! Convenience wrapper for importing authored skills as global Postgres rows.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let from = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("skills"));
    let status = Command::new("cargo")
        .args([
            "run",
            "-p",
            "moa-cli",
            "--",
            "skills",
            "bootstrap_global",
            "--from",
        ])
        .arg(from)
        .status();

    match status {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("failed to run moa skill bootstrap: {error}");
            ExitCode::FAILURE
        }
    }
}
