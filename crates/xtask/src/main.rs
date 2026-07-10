//! Repository maintenance commands.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

mod calibrate_external_memory_judge;
mod check_architecture_boundaries;
mod check_eval_budgets;
mod compare_eval_reports;
mod compute_memory_quality_scores;
mod fetch_memory_benchmark;
mod generate_memory_eval_corpus;
mod record_memory_extractions;
mod record_memory_merges;
mod run_external_memory_eval;
mod run_memory_retrieval_eval;
mod wixqa_rag_eval;

const CENTRAL_MIGRATIONS_DIR: &str = "crates/moa-migrations/migrations/postgres";
const CENTRAL_MIGRATIONS_ROOT: &str = "crates/moa-migrations/migrations";

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("audit-paths") => cmd_audit_paths(),
        Some("check-architecture-boundaries") => check_architecture_boundaries::run(),
        Some("check-migrations") => cmd_check_migrations(),
        Some("check-eval-budgets") => check_eval_budgets::run(args),
        Some("calibrate-external-memory-judge") => calibrate_external_memory_judge::run(args),
        Some("compare-eval-reports") => compare_eval_reports::run(args),
        Some("compute-memory-quality-scores") => compute_memory_quality_scores::run(args),
        Some("fetch-memory-benchmark") => fetch_memory_benchmark::run(args),
        Some("generate-memory-eval-corpus") => generate_memory_eval_corpus::run(args),
        Some("record-memory-extractions") => record_memory_extractions::run(args),
        Some("record-memory-merges") => record_memory_merges::run(args),
        Some("run-external-memory-eval") => run_external_memory_eval::run(args),
        Some("run-memory-retrieval-eval") => run_memory_retrieval_eval::run(args),
        Some("wixqa-rag-eval") => wixqa_rag_eval::run(args),
        Some(command) => bail!("unknown xtask command: {command}"),
        None => bail!("missing xtask command; try `cargo xtask audit-paths`"),
    }
}

fn cmd_check_migrations() -> Result<()> {
    check_central_migration_files()?;
    check_no_noncentral_migration_dirs()?;
    check_duplicate_table_ownership()?;
    println!("migration checks clean");
    Ok(())
}

fn check_central_migration_files() -> Result<()> {
    let migrations_dir = Path::new(CENTRAL_MIGRATIONS_DIR);
    let mut versions = BTreeMap::<u64, String>::new();
    for entry in fs::read_dir(migrations_dir).context("read central migrations directory")? {
        let entry = entry.context("read central migration entry")?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("sql") {
            let name = file_name(&path)?;
            let version = parse_refinery_version(name)?;
            if let Some(existing) = versions.insert(version, name.to_string()) {
                bail!("duplicate migration version {version}: {existing}, {name}");
            }
        }
    }

    if versions.is_empty() {
        bail!("no central migrations found under {CENTRAL_MIGRATIONS_DIR}");
    }

    Ok(())
}

fn parse_refinery_version(file_name: &str) -> Result<u64> {
    let Some(rest) = file_name.strip_prefix('V') else {
        bail!("migration file must start with V: {file_name}");
    };
    let Some((version, description)) = rest.split_once("__") else {
        bail!("migration file must use V<version>__<description>.sql: {file_name}");
    };
    if !description.ends_with(".sql") {
        bail!("migration file must end with .sql: {file_name}");
    }
    if !version.chars().all(|character| character.is_ascii_digit()) {
        bail!("migration version must be numeric: {file_name}");
    }
    version
        .parse::<u64>()
        .with_context(|| format!("parse migration version in {file_name}"))
}

fn check_no_noncentral_migration_dirs() -> Result<()> {
    let mut dirs = Vec::new();
    for root in ["crates", "services"] {
        collect_migration_dirs(Path::new(root), &mut dirs)?;
    }

    let allowed_root = Path::new(CENTRAL_MIGRATIONS_ROOT);
    let violations = dirs
        .into_iter()
        .filter(|path| path != allowed_root)
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if !violations.is_empty() {
        bail!(
            "migration directories must live under {CENTRAL_MIGRATIONS_ROOT}; found:\n{}",
            violations.join("\n")
        );
    }

    Ok(())
}

fn check_duplicate_table_ownership() -> Result<()> {
    let mut migration_files = Vec::new();
    collect_migration_sql_files(Path::new(CENTRAL_MIGRATIONS_DIR), &mut migration_files)?;

    let mut owners = BTreeMap::<String, Vec<PathBuf>>::new();
    for path in migration_files {
        let sql = fs::read_to_string(&path)
            .with_context(|| format!("read migration {}", path.display()))?;
        for table in extract_create_table_if_not_exists(&sql) {
            owners.entry(table).or_default().push(path.clone());
        }
    }

    let mut violations = Vec::new();
    for (table, paths) in owners {
        if paths.len() <= 1 {
            continue;
        }
        let owner_keys = paths
            .iter()
            .map(|path| migration_owner_key(path))
            .collect::<BTreeSet<_>>();
        if owner_keys.len() == 1 {
            continue;
        }
        let path_list = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        violations.push(format!("{table}: {path_list}"));
    }

    if !violations.is_empty() {
        bail!(
            "duplicate CREATE TABLE IF NOT EXISTS ownership detected:\n{}",
            violations.join("\n")
        );
    }

    Ok(())
}

fn migration_owner_key(path: &Path) -> String {
    file_name(path).unwrap_or("<unknown>").to_string()
}

fn collect_migration_dirs(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("migrations") {
            out.push(path.clone());
        }
        collect_migration_dirs(&path, out)?;
    }
    Ok(())
}

fn collect_migration_sql_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", root.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_migration_sql_files(&path, out)?;
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) == Some("sql")
            && path
                .components()
                .any(|component| component.as_os_str() == "migrations")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn extract_create_table_if_not_exists(sql: &str) -> Vec<String> {
    let mut tables = Vec::new();
    let sql_without_line_comments = sql
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join(" ");
    let lower = sql_without_line_comments.to_ascii_lowercase();
    let marker = "create table if not exists ";
    let mut offset = 0;
    while let Some(relative_index) = lower[offset..].find(marker) {
        let name_start = offset + relative_index + marker.len();
        let remainder = sql_without_line_comments[name_start..].trim_start();
        let Some(token) = remainder.split_whitespace().next() else {
            break;
        };
        if let Some(table) = normalize_table_name(token) {
            tables.push(table);
        }
        offset = name_start + token.len();
    }
    tables
}

fn normalize_table_name(token: &str) -> Option<String> {
    let trimmed = token
        .trim_end_matches('(')
        .trim_end_matches(';')
        .trim_matches('"')
        .to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('.') {
        Some(
            trimmed
                .split('.')
                .map(|part| part.trim_matches('"'))
                .collect::<Vec<_>>()
                .join("."),
        )
    } else {
        Some(format!("public.{trimmed}"))
    }
}

fn file_name(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("path has no UTF-8 file name: {}", path.display()))
}

fn cmd_audit_paths() -> Result<()> {
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

    audit_learning_candidate_promotion_paths()?;
    audit_moa_test_support_dev_dependency_only()?;

    println!("path audit clean");
    Ok(())
}

fn audit_learning_candidate_promotion_paths() -> Result<()> {
    rg_forbid(
        "direct learning-candidate promoted construction",
        r"status:\s*LearningCandidateStatus::Promoted",
        &["crates/"],
        &["--type", "rust"],
    )
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

#[cfg(test)]
mod task_11_workflow_contract {
    use std::path::Path;

    #[test]
    fn task_11_workflow_contract_is_manual_protected_and_read_only() {
        // Pins: the paid lane is manual/main-only, protected, provenance-gated, and never writes baselines.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("xtask is beneath the workspace root");
        let path = root.join(".github/workflows/memory-benchmarks.yml");
        let text = std::fs::read_to_string(&path).expect("read memory benchmark workflow");
        let parsed: serde_yaml::Value = serde_yaml::from_str(&text).expect("workflow YAML parses");
        assert!(parsed.is_mapping());
        assert!(text.contains("on:\n  workflow_dispatch:"));
        assert!(!text.contains("pull_request:"));
        assert!(!text.contains("schedule:"));
        for exact in [
            "permissions:\n  contents: read",
            "environment: memory-benchmarks",
            "refs/heads/main",
            "ghcr.io/hwuiwon/moa-postgres:pg17-pgvector0.8.2-pgaudit",
            "POSTGRES_DB: moa",
            "POSTGRES_PASSWORD: ci",
            "postgres://postgres:ci@localhost:5432/moa",
            "MOA_RUN_NETWORK_MEMORY_BENCHMARKS: \"1\"",
            "MOA_RUN_LIVE_MEMORY_BENCHMARKS: \"1\"",
            "--fetch-summary",
            "--migrate-database",
            "--reader-context-window",
            "--reader-output-token-reserve",
            "--controls \"$CONTROLS\"",
            "no-memory,full-context,oracle-evidence",
            "--package-manifest",
            "MOA_OPENAI_API_KEY: ${{ secrets.MOA_OPENAI_API_KEY }}",
            "MOA_GOOGLE_API_KEY: ${{ secrets.MOA_GOOGLE_API_KEY }}",
            "MOA_ANTHROPIC_API_KEY: ${{ secrets.MOA_ANTHROPIC_API_KEY }}",
        ] {
            assert!(
                text.contains(exact),
                "missing workflow contract marker: {exact}"
            );
        }
        assert_eq!(text.matches("MOA_ANTHROPIC_API_KEY:").count(), 1);
        assert!(!text.contains("git push"));
        assert!(!text.contains("docs/eval/baselines/"));
        assert!(!text.contains("contents: write"));
    }
}
