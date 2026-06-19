//! Repository maintenance commands.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

mod check_architecture_boundaries;
mod check_eval_budgets;
mod compare_eval_reports;
mod compute_memory_quality_scores;
mod generate_memory_eval_corpus;
mod record_memory_extractions;
mod record_memory_merges;
mod run_memory_retrieval_eval;

const CENTRAL_MIGRATIONS_DIR: &str = "crates/moa-migrations/migrations/postgres";
const CENTRAL_MIGRATIONS_ROOT: &str = "crates/moa-migrations/migrations";

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("audit-paths") => cmd_audit_paths(),
        Some("check-architecture-boundaries") => check_architecture_boundaries::run(),
        Some("check-migrations") => cmd_check_migrations(),
        Some("check-eval-budgets") => check_eval_budgets::run(args),
        Some("compare-eval-reports") => compare_eval_reports::run(args),
        Some("compute-memory-quality-scores") => compute_memory_quality_scores::run(args),
        Some("generate-memory-eval-corpus") => generate_memory_eval_corpus::run(args),
        Some("migrate-test-db") => cmd_migrate_test_db(),
        Some("record-memory-extractions") => record_memory_extractions::run(args),
        Some("record-memory-merges") => record_memory_merges::run(args),
        Some("run-memory-retrieval-eval") => run_memory_retrieval_eval::run(args),
        Some(command) => bail!("unknown xtask command: {command}"),
        None => bail!("missing xtask command; try `cargo xtask audit-paths`"),
    }
}

fn cmd_migrate_test_db() -> Result<()> {
    let database_url = env::var("MOA_DATABASE_URL").context("MOA_DATABASE_URL must be set")?;
    let redacted = redact_password(&database_url);
    println!(
        "test database configured at {redacted}; MOA integration tests create migrated isolated schemas during bootstrap"
    );
    Ok(())
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

    audit_removed_segment_score_names()?;
    audit_learning_candidate_promotion_paths()?;
    audit_moa_test_support_dev_dependency_only()?;

    println!("path audit clean");
    Ok(())
}

fn audit_removed_segment_score_names() -> Result<()> {
    let paths = existing_paths(&[
        "crates/moa-core",
        "crates/moa-brain",
        "crates/moa-session",
        "crates/moa-orchestrator",
        "docs",
    ]);
    let removed_segment_pattern = [
        "ResolutionScore|",
        "ResolutionScorer|",
        "ScoringPhase|",
        "UpdateSegmentResolutionRequest|",
        "UpdateSegmentResolutionScoreRequest|",
        "update_segment_resolution_score|",
        "update_segment_resolution|",
        "Session::run_turn|",
        "resolution_scored",
    ]
    .concat();
    rg_forbid(
        "removed segment-score compatibility names",
        &removed_segment_pattern,
        &paths,
        &[],
    )
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
