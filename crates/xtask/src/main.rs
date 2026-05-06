//! Repository maintenance commands.

use std::env;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("audit-paths") => cmd_audit_paths(),
        Some(command) => bail!("unknown xtask command: {command}"),
        None => bail!("missing xtask command; try `cargo xtask audit-paths`"),
    }
}

fn cmd_audit_paths() -> Result<()> {
    for old in [
        ["crates/moa-memory", "-graph"].concat(),
        ["crates/moa-memory", "-vector"].concat(),
        ["crates/moa-memory", "-pii"].concat(),
        ["crates/moa-memory", "-ingest"].concat(),
    ] {
        if Path::new(&old).exists() {
            bail!("forbidden directory exists: {old}");
        }
    }

    for forbidden_file in [
        "crates/moa-memory/Cargo.toml",
        "crates/moa-memory/src/lib.rs",
    ] {
        if Path::new(forbidden_file).exists() {
            bail!("forbidden parent memory crate file exists: {forbidden_file}");
        }
    }

    let removed_shim_pattern = [
        "use ",
        "moa_memory",
        "::|",
        "moa_memory",
        "::vector|",
        "moa_memory",
        "::embedder|",
        "moa_memory",
        "::chunking",
    ]
    .concat();
    rg_forbid(
        "removed moa-memory shim references",
        &removed_shim_pattern,
        &["crates/"],
        &["--type", "rust"],
    )?;

    let connector_pattern = ["Mock", "Connector|Connector", "Client|connector", "_inbox"].concat();
    rg_forbid(
        "connector code",
        &connector_pattern,
        &["crates/"],
        &["--type", "rust"],
    )?;

    let envelope_paths = existing_paths(&["crates/", "migrations/"]);
    let envelope_pattern = ["crypto", "_shred|wrapped", "_dek|Envelope", "Cipher"].concat();
    rg_forbid(
        "envelope-encryption code",
        &envelope_pattern,
        &envelope_paths,
        &["--type-add", "sql:*.sql", "--type", "rust", "--type", "sql"],
    )?;

    let doc_paths = existing_paths(&["docs/", "examples/"]);
    let removed_doc_pattern = [
        "MEMORY",
        r"\.md|File",
        "Memory",
        "Store|wiki",
        "_branch|reconcile",
        "_pages|File",
        "Wiki",
    ]
    .concat();
    rg_forbid(
        "removed memory documentation",
        &removed_doc_pattern,
        &doc_paths,
        &[],
    )?;

    println!("path audit clean");
    Ok(())
}

fn existing_paths<'a>(paths: &'a [&'a str]) -> Vec<&'a str> {
    paths
        .iter()
        .copied()
        .filter(|path| Path::new(path).exists())
        .collect()
}

fn rg_forbid(label: &str, pattern: &str, paths: &[&str], options: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    let mut command = Command::new("rg");
    command.arg("-l").args(options).arg(pattern).args(paths);
    let output = command
        .output()
        .with_context(|| format!("run rg for {label}"))?;

    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "rg failed while checking {label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    if !output.stdout.is_empty() {
        eprintln!(
            "Forbidden {label}:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        bail!("{label} detected");
    }

    Ok(())
}
