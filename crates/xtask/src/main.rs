//! Repository maintenance commands.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

mod check_eval_budgets;
mod compare_eval_reports;
mod generate_memory_eval_corpus;
mod run_memory_retrieval_eval;

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("audit-paths") => cmd_audit_paths(),
        Some("check-eval-budgets") => check_eval_budgets::run(args),
        Some("compare-eval-reports") => compare_eval_reports::run(args),
        Some("generate-memory-eval-corpus") => generate_memory_eval_corpus::run(args),
        Some("migrate-test-db") => cmd_migrate_test_db(),
        Some("run-memory-retrieval-eval") => run_memory_retrieval_eval::run(args),
        Some(command) => bail!("unknown xtask command: {command}"),
        None => bail!("missing xtask command; try `cargo xtask audit-paths`"),
    }
}

fn cmd_migrate_test_db() -> Result<()> {
    let database_url = env::var("MOA_TEST_POSTGRES_URL")
        .or_else(|_| env::var("TEST_DATABASE_URL"))
        .or_else(|_| env::var("DATABASE_URL"))
        .context("MOA_TEST_POSTGRES_URL, TEST_DATABASE_URL, or DATABASE_URL must be set")?;
    let redacted = redact_password(&database_url);
    println!(
        "test database configured at {redacted}; MOA integration tests create migrated isolated schemas during bootstrap"
    );
    Ok(())
}

fn redact_password(database_url: &str) -> String {
    let Some(scheme_end) = database_url.find("://") else {
        return database_url.to_string();
    };
    let auth_start = scheme_end + 3;
    let Some(at_offset) = database_url[auth_start..].find('@') else {
        return database_url.to_string();
    };
    let at_index = auth_start + at_offset;
    let auth = &database_url[auth_start..at_index];
    let Some(colon_offset) = auth.rfind(':') else {
        return database_url.to_string();
    };
    let password_start = auth_start + colon_offset + 1;
    format!(
        "{}***{}",
        &database_url[..password_start],
        &database_url[at_index..]
    )
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

    audit_moa_test_support_dev_dependency_only()?;

    println!("path audit clean");
    Ok(())
}

fn audit_moa_test_support_dev_dependency_only() -> Result<()> {
    let mut manifests = cargo_manifest_paths()?;
    manifests.push("Cargo.toml".to_string());

    for manifest in manifests {
        if manifest == "crates/moa-test-support/Cargo.toml" {
            continue;
        }
        let body = fs::read_to_string(&manifest)
            .with_context(|| format!("read manifest for moa-test-support audit: {manifest}"))?;
        let mut section = String::new();
        for (line_index, line) in body.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                section = trimmed.trim_matches(&['[', ']'][..]).to_string();
                continue;
            }
            if !trimmed.contains("moa-test-support") {
                continue;
            }
            if manifest == "Cargo.toml"
                && (section == "workspace" || section == "workspace.dependencies")
            {
                continue;
            }
            if section.ends_with("dev-dependencies") {
                continue;
            }
            bail!(
                "moa-test-support must only be used from dev-dependencies; found {manifest}:{} in [{section}]",
                line_index + 1
            );
        }
    }

    rg_forbid(
        "non-test moa-test-support Rust imports",
        r"moa_test_support::",
        &["crates/"],
        &[
            "--type",
            "rust",
            "--glob",
            "**/src/**",
            "--glob",
            "!crates/moa-test-support/**",
            "--glob",
            "!crates/xtask/**",
        ],
    )
}

fn cargo_manifest_paths() -> Result<Vec<String>> {
    let output = Command::new("rg")
        .args(["--files", "-g", "Cargo.toml", "crates"])
        .output()
        .context("list crate Cargo.toml files")?;
    if !output.status.success() {
        bail!(
            "rg failed while listing Cargo.toml files: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(ToString::to_string)
        .collect())
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
