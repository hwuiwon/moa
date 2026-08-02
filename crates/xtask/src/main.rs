//! Repository maintenance commands.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

#[cfg(feature = "eval-tools")]
mod calibrate_external_memory_judge;
#[cfg(feature = "eval-tools")]
mod certify_platform_simulator;
mod check_architecture_boundaries;
#[cfg(feature = "eval-tools")]
mod check_eval_budgets;
#[cfg(feature = "eval-tools")]
mod compare_eval_reports;
#[cfg(feature = "eval-tools")]
mod compute_memory_quality_scores;
#[cfg(feature = "eval-tools")]
mod eval_control_mutants;
#[cfg(feature = "eval-tools")]
mod eval_suite_controls;
#[cfg(feature = "eval-tools")]
mod execution_eval;
mod execution_trace_manifest;
#[cfg(feature = "eval-tools")]
mod fetch_memory_benchmark;
#[cfg(feature = "eval-tools")]
mod generate_memory_eval_corpus;
#[cfg(feature = "eval-tools")]
mod record_memory_extractions;
#[cfg(feature = "eval-tools")]
mod record_memory_merges;
#[cfg(feature = "eval-tools")]
mod run_external_memory_eval;
#[cfg(feature = "eval-tools")]
mod run_memory_retrieval_eval;
#[cfg(feature = "eval-tools")]
mod wixqa_rag_eval;

const EVAL_TOOL_COMMANDS: &[&str] = &[
    "check-eval-budgets",
    "calibrate-external-memory-judge",
    "certify-platform-simulator",
    "compare-eval-reports",
    "compute-memory-quality-scores",
    "eval-control-mutants",
    "eval-suite-controls",
    "execution-eval",
    "fetch-memory-benchmark",
    "generate-memory-eval-corpus",
    "record-memory-extractions",
    "record-memory-merges",
    "run-external-memory-eval",
    "run-memory-retrieval-eval",
    "wixqa-rag-eval",
];

const CENTRAL_MIGRATIONS_DIR: &str = "crates/moa-migrations/migrations/postgres";
const CENTRAL_MIGRATIONS_ROOT: &str = "crates/moa-migrations/migrations";
const MIGRATION_OWNERSHIP_MANIFEST: &str = "crates/moa-migrations/migration-ownership.toml";

#[derive(Debug, Deserialize)]
struct MigrationOwnershipManifest {
    table: Vec<MigrationOwnership>,
}

#[derive(Debug, Deserialize)]
struct MigrationOwnership {
    name: String,
    schema: String,
    owner: String,
    #[serde(default)]
    readers: Vec<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TableIdentity {
    schema: String,
    name: String,
}

impl TableIdentity {
    fn display(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("audit-paths") => cmd_audit_paths(),
        Some("check-architecture-boundaries") => check_architecture_boundaries::run(),
        Some("check-migrations") => cmd_check_migrations(),
        #[cfg(feature = "eval-tools")]
        Some("check-eval-budgets") => check_eval_budgets::run(args),
        #[cfg(feature = "eval-tools")]
        Some("calibrate-external-memory-judge") => calibrate_external_memory_judge::run(args),
        #[cfg(feature = "eval-tools")]
        Some("certify-platform-simulator") => certify_platform_simulator::run(args),
        #[cfg(feature = "eval-tools")]
        Some("compare-eval-reports") => compare_eval_reports::run(args),
        #[cfg(feature = "eval-tools")]
        Some("compute-memory-quality-scores") => compute_memory_quality_scores::run(args),
        #[cfg(feature = "eval-tools")]
        Some("eval-control-mutants") => eval_control_mutants::run(args),
        #[cfg(feature = "eval-tools")]
        Some("eval-suite-controls") => eval_suite_controls::run(args),
        #[cfg(feature = "eval-tools")]
        Some("execution-eval") => execution_eval::run(args),
        #[cfg(feature = "eval-tools")]
        Some("fetch-memory-benchmark") => fetch_memory_benchmark::run(args),
        #[cfg(feature = "eval-tools")]
        Some("generate-memory-eval-corpus") => generate_memory_eval_corpus::run(args),
        #[cfg(feature = "eval-tools")]
        Some("record-memory-extractions") => record_memory_extractions::run(args),
        #[cfg(feature = "eval-tools")]
        Some("record-memory-merges") => record_memory_merges::run(args),
        #[cfg(feature = "eval-tools")]
        Some("run-external-memory-eval") => run_external_memory_eval::run(args),
        #[cfg(feature = "eval-tools")]
        Some("run-memory-retrieval-eval") => run_memory_retrieval_eval::run(args),
        #[cfg(feature = "eval-tools")]
        Some("wixqa-rag-eval") => wixqa_rag_eval::run(args),
        Some(command) if EVAL_TOOL_COMMANDS.contains(&command) => bail!(
            "xtask command `{command}` requires `cargo run -p xtask --features eval-tools -- {command}`"
        ),
        Some(command) => bail!("unknown xtask command: {command}"),
        None => bail!("missing xtask command; try `cargo xtask audit-paths`"),
    }
}

fn cmd_check_migrations() -> Result<()> {
    check_central_migration_files()?;
    check_no_noncentral_migration_dirs()?;
    let (statements, families) = check_migration_ownership()?;
    println!(
        "migration ownership clean: {statements} CREATE TABLE statements, {families} owned logical families"
    );
    println!("migration checks clean");
    Ok(())
}

fn check_central_migration_files() -> Result<()> {
    let migrations_dir = Path::new(CENTRAL_MIGRATIONS_DIR);
    let mut entries = Vec::new();
    for entry in fs::read_dir(migrations_dir).context("read central migrations directory")? {
        let entry = entry.context("read central migration entry")?;
        let name = entry.file_name().into_string().map_err(|name| {
            anyhow!(
                "central migration entry name is not valid UTF-8: {}",
                name.to_string_lossy()
            )
        })?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("read central migration entry type for {name}"))?;
        let kind = if file_type.is_symlink() {
            MigrationEntryKind::Symlink
        } else if file_type.is_file() {
            MigrationEntryKind::File
        } else if file_type.is_dir() {
            MigrationEntryKind::Directory
        } else {
            MigrationEntryKind::Other
        };
        entries.push(MigrationEntry { name, kind });
    }
    validate_migration_entries(&entries)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MigrationEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug)]
struct MigrationEntry {
    name: String,
    kind: MigrationEntryKind,
}

fn validate_migration_entries(entries: &[MigrationEntry]) -> Result<()> {
    if entries.is_empty() {
        bail!("no central migrations found under {CENTRAL_MIGRATIONS_DIR}");
    }

    let mut versions = BTreeMap::<u64, &str>::new();
    for entry in entries {
        if entry.kind != MigrationEntryKind::File {
            bail!(
                "central migration directory must contain regular files only; found {:?}: {}",
                entry.kind,
                entry.name
            );
        }
        let version = parse_refinery_version(&entry.name)?;
        if let Some(existing) = versions.insert(version, &entry.name) {
            bail!(
                "duplicate migration version {version}: {existing}, {}",
                entry.name
            );
        }
    }

    for (index, version) in versions.keys().copied().enumerate() {
        let expected = u64::try_from(index + 1).context("migration count exceeds u64")?;
        if version != expected {
            bail!(
                "central migration versions must be exactly contiguous from V000001; expected V{expected:06}, found V{version:06}"
            );
        }
    }
    Ok(())
}

fn parse_refinery_version(file_name: &str) -> Result<u64> {
    let Some(stem) = file_name.strip_suffix(".sql") else {
        bail!("migration file must end with .sql: {file_name}");
    };
    let Some((version, description)) = stem.split_once("__") else {
        bail!("migration file must use V<six digits>__<snake_case>.sql: {file_name}");
    };
    if version.len() != 7 || !version.starts_with('V') {
        bail!("migration version must use uppercase V plus exactly six digits: {file_name}");
    }
    let digits = &version[1..];
    if !digits.chars().all(|character| character.is_ascii_digit()) {
        bail!("migration version must use uppercase V plus exactly six digits: {file_name}");
    }
    if description.is_empty()
        || description.split('_').any(|segment| {
            segment.is_empty()
                || !segment
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        })
    {
        bail!("migration description must be non-empty lowercase snake_case: {file_name}");
    }

    let parsed = digits
        .parse::<u64>()
        .with_context(|| format!("parse migration version in {file_name}"))?;
    if parsed == 0 {
        bail!("migration versions start at V000001, not V000000: {file_name}");
    }
    Ok(parsed)
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

fn check_migration_ownership() -> Result<(usize, usize)> {
    let mut migration_files = Vec::new();
    collect_migration_sql_files(Path::new(CENTRAL_MIGRATIONS_DIR), &mut migration_files)?;
    migration_files.sort();

    let mut statements = 0;
    let mut declared = BTreeSet::new();
    for path in migration_files {
        let sql = fs::read_to_string(&path)
            .with_context(|| format!("read migration {}", path.display()))?;
        let tables = extract_create_tables(&sql)
            .with_context(|| format!("validate fresh-only catalog in {}", path.display()))?;
        statements += tables.len();
        declared.extend(tables);
    }

    let body = fs::read_to_string(MIGRATION_OWNERSHIP_MANIFEST)
        .with_context(|| format!("read {MIGRATION_OWNERSHIP_MANIFEST}"))?;
    let manifest: MigrationOwnershipManifest =
        toml::from_str(&body).with_context(|| format!("parse {MIGRATION_OWNERSHIP_MANIFEST}"))?;
    validate_migration_ownership(&manifest, &declared)?;
    Ok((statements, declared.len()))
}

fn validate_migration_ownership(
    manifest: &MigrationOwnershipManifest,
    declared: &BTreeSet<TableIdentity>,
) -> Result<()> {
    let mut owned = BTreeMap::<TableIdentity, &MigrationOwnership>::new();
    for entry in &manifest.table {
        let table = manifest_table_identity(entry)?;
        if owned.insert(table.clone(), entry).is_some() {
            bail!("duplicate migration ownership row: {}", table.display());
        }
        validate_owner_identifier(&entry.owner)
            .with_context(|| format!("invalid owner for {}", table.display()))?;
        for reader in &entry.readers {
            validate_owner_identifier(reader)
                .with_context(|| format!("invalid reader for {}", table.display()))?;
        }
    }

    let owned_tables = owned.keys().cloned().collect::<BTreeSet<_>>();
    let missing = declared
        .difference(&owned_tables)
        .map(TableIdentity::display)
        .collect::<Vec<_>>();
    let stale = owned_tables
        .difference(declared)
        .map(TableIdentity::display)
        .collect::<Vec<_>>();
    if !missing.is_empty() || !stale.is_empty() {
        let mut details = Vec::new();
        if !missing.is_empty() {
            details.push(format!("missing ownership rows:\n{}", missing.join("\n")));
        }
        if !stale.is_empty() {
            details.push(format!("stale ownership rows:\n{}", stale.join("\n")));
        }
        bail!(
            "migration ownership manifest does not match DDL:\n{}",
            details.join("\n")
        );
    }
    Ok(())
}

fn manifest_table_identity(entry: &MigrationOwnership) -> Result<TableIdentity> {
    let schema = canonical_identifier(&entry.schema);
    let name = canonical_identifier(&entry.name);
    if schema.is_empty() || name.is_empty() {
        bail!("migration ownership schema and name must not be empty");
    }
    if name.contains('.') {
        bail!(
            "migration ownership name must be unqualified: {}",
            entry.name
        );
    }
    Ok(normalize_table_family(TableIdentity { schema, name }))
}

fn validate_owner_identifier(identifier: &str) -> Result<()> {
    let root = workspace_root();
    if let Some(group) = identifier.strip_suffix("/*") {
        let path = root.join("crates").join(group);
        if !path.is_dir() {
            bail!("crate group does not exist: {identifier}");
        }
        let mut manifests = Vec::new();
        collect_named_files(&path, "Cargo.toml", &mut manifests)?;
        if manifests.is_empty() {
            bail!("crate group contains no crates: {identifier}");
        }
        return Ok(());
    }
    let crate_dir = root.join("crates").join(identifier);
    let service_dir = root.join(identifier);
    if crate_dir.join("Cargo.toml").is_file()
        || (identifier.starts_with("services/") && service_dir.is_dir())
        || workspace_contains_package(identifier)?
    {
        return Ok(());
    }
    bail!("crate or service does not exist: {identifier}")
}

fn workspace_contains_package(package_name: &str) -> Result<bool> {
    let mut manifests = Vec::new();
    collect_named_files(
        &workspace_root().join("crates"),
        "Cargo.toml",
        &mut manifests,
    )?;
    for manifest in manifests {
        let body = fs::read_to_string(&manifest)
            .with_context(|| format!("read {}", manifest.display()))?;
        let parsed: toml::Value =
            toml::from_str(&body).with_context(|| format!("parse {}", manifest.display()))?;
        if parsed
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            == Some(package_name)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask must live beneath the workspace root")
        .to_path_buf()
}

/// Recursively walks `root`, pushing every entry the `matches` predicate accepts.
///
/// A missing `root` yields no entries. Directories are always descended into, so
/// each predicate only decides whether the current path is collected; the
/// per-walk predicates below carry the byte-for-byte matching rules that the
/// previous dedicated walkers used.
fn collect_matching<F>(root: &Path, matches: &F, out: &mut Vec<PathBuf>) -> Result<()>
where
    F: Fn(&Path) -> bool,
{
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry.with_context(|| format!("read entry under {}", root.display()))?;
        let path = entry.path();
        if matches(&path) {
            out.push(path.clone());
        }
        if path.is_dir() {
            collect_matching(&path, matches, out)?;
        }
    }
    Ok(())
}

fn collect_named_files(root: &Path, name: &str, out: &mut Vec<PathBuf>) -> Result<()> {
    collect_matching(
        root,
        &|path: &Path| {
            !path.is_dir() && path.file_name().and_then(|value| value.to_str()) == Some(name)
        },
        out,
    )
}

fn collect_migration_dirs(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    collect_matching(
        root,
        &|path: &Path| {
            path.is_dir() && path.file_name().and_then(|name| name.to_str()) == Some("migrations")
        },
        out,
    )
}

fn collect_migration_sql_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    collect_matching(
        root,
        &|path: &Path| {
            !path.is_dir()
                && path.extension().and_then(|extension| extension.to_str()) == Some("sql")
                && path
                    .components()
                    .any(|component| component.as_os_str() == "migrations")
        },
        out,
    )
}

fn extract_create_tables(sql: &str) -> Result<Vec<TableIdentity>> {
    let tokens = sql_tokens_without_comments(sql);
    if tokens.windows(2).any(|tokens| {
        tokens[0].eq_ignore_ascii_case("drop") && tokens[1].eq_ignore_ascii_case("table")
    }) {
        bail!(
            "central migrations must not contain DROP TABLE; ownership describes the final fresh-database catalog"
        );
    }

    let mut tables = Vec::new();
    let mut index = 0;
    while index + 2 < tokens.len() {
        if !tokens[index].eq_ignore_ascii_case("create")
            || !tokens[index + 1].eq_ignore_ascii_case("table")
        {
            index += 1;
            continue;
        }
        let mut name_index = index + 2;
        if tokens
            .get(name_index)
            .is_some_and(|token| token.eq_ignore_ascii_case("if"))
            && tokens
                .get(name_index + 1)
                .is_some_and(|token| token.eq_ignore_ascii_case("not"))
            && tokens
                .get(name_index + 2)
                .is_some_and(|token| token.eq_ignore_ascii_case("exists"))
        {
            name_index += 3;
        }
        if let Some(table) = tokens
            .get(name_index)
            .and_then(|token| parse_table_identity(token))
        {
            tables.push(normalize_table_family(table));
        }
        index = name_index + 1;
    }
    Ok(tables)
}

fn sql_tokens_without_comments(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut index = 0;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while index < bytes.len() {
        if in_line_comment {
            if bytes[index] == b'\n' {
                in_line_comment = false;
            }
            index += 1;
            continue;
        }
        if in_block_comment {
            if bytes[index..].starts_with(b"*/") {
                in_block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"--") {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            in_line_comment = true;
            index += 2;
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            in_block_comment = true;
            index += 2;
            continue;
        }
        let character = bytes[index] as char;
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '%' | '"') {
            current.push(character);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        index += 1;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_table_identity(token: &str) -> Option<TableIdentity> {
    let parts = token
        .split('.')
        .map(canonical_identifier)
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [name] if !name.is_empty() => Some(TableIdentity {
            schema: "public".to_string(),
            name: name.clone(),
        }),
        [schema, name] if !schema.is_empty() && !name.is_empty() => Some(TableIdentity {
            schema: schema.clone(),
            name: name.clone(),
        }),
        _ => None,
    }
}

fn canonical_identifier(identifier: &str) -> String {
    identifier.trim_matches('"').to_ascii_lowercase()
}

fn normalize_table_family(mut table: TableIdentity) -> TableIdentity {
    let parent = match (table.schema.as_str(), table.name.as_str()) {
        ("public", name) if name.starts_with("events_p%s") => Some("events"),
        ("public", name) if name.starts_with("session_event_dedupe_p%s") => {
            Some("session_event_dedupe")
        }
        ("moa", name) if name.starts_with("embeddings_p%s") => Some("embeddings"),
        ("moa", name) if name.starts_with("graph_changelog_%s") => Some("graph_changelog"),
        _ => None,
    };
    if let Some(parent) = parent {
        table.name = parent.to_string();
    }
    table
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
mod check_migrations_tests {
    use super::*;

    fn migration(name: &str) -> MigrationEntry {
        MigrationEntry {
            name: name.to_string(),
            kind: MigrationEntryKind::File,
        }
    }

    #[test]
    fn check_migrations_accepts_only_a_contiguous_canonical_sequence() {
        // Pins: central migration filenames form exactly V000001..V00000N.
        validate_migration_entries(&[
            migration("V000001__epoch.sql"),
            migration("V000002__session_baseline.sql"),
        ])
        .expect("canonical contiguous sequence");
    }

    #[test]
    fn check_migrations_rejects_noncanonical_file_names() {
        // Pins: spelling, width, casing, extension, and unrelated files fail closed.
        for name in [
            "V000000__epoch.sql",
            "V00001__epoch.sql",
            "V0000001__epoch.sql",
            "v000001__epoch.sql",
            "V000001_epoch.sql",
            "V000001__.sql",
            "V000001__Not_snake.sql",
            "V000001__not--snake.sql",
            "V000001__not__snake.sql",
            "V000001__epoch.SQL",
            "README.md",
        ] {
            let error = validate_migration_entries(&[migration(name)])
                .expect_err("noncanonical entry must fail");
            assert!(!error.to_string().is_empty(), "{name}");
        }
    }

    #[test]
    fn check_migrations_rejects_gaps_and_duplicate_versions() {
        // Pins: neither a missing identity nor two names sharing one identity can pass.
        let gap = validate_migration_entries(&[
            migration("V000001__epoch.sql"),
            migration("V000003__later.sql"),
        ])
        .expect_err("gap must fail");
        assert!(gap.to_string().contains("expected V000002"));

        let duplicate = validate_migration_entries(&[
            migration("V000001__epoch.sql"),
            migration("V000001__other.sql"),
        ])
        .expect_err("duplicate must fail");
        assert!(
            duplicate
                .to_string()
                .contains("duplicate migration version 1")
        );
    }

    #[test]
    fn check_migrations_rejects_non_file_entries() {
        // Pins: nested directories, symlinks, and special entries cannot hide from validation.
        for kind in [
            MigrationEntryKind::Directory,
            MigrationEntryKind::Symlink,
            MigrationEntryKind::Other,
        ] {
            let error = validate_migration_entries(&[MigrationEntry {
                name: "V000001__epoch.sql".to_string(),
                kind,
            }])
            .expect_err("non-file entry must fail");
            assert!(error.to_string().contains("regular files only"));
        }
    }

    fn ownership(schema: &str, name: &str, owner: &str) -> MigrationOwnership {
        MigrationOwnership {
            name: name.to_string(),
            schema: schema.to_string(),
            owner: owner.to_string(),
            readers: Vec::new(),
        }
    }

    fn identities(values: &[(&str, &str)]) -> BTreeSet<TableIdentity> {
        values
            .iter()
            .map(|(schema, name)| TableIdentity {
                schema: (*schema).to_string(),
                name: (*name).to_string(),
            })
            .collect()
    }

    #[test]
    fn check_migrations_extracts_supported_table_forms_and_ignores_comments() {
        // Pins: migration inventory recognizes real and generated DDL without treating comments as declarations.
        let sql = r#"
            CREATE TABLE alpha (id bigint);
            CREATE TABLE IF NOT EXISTS "moa"."Beta" (id bigint);
            -- CREATE TABLE ignored_line (id bigint);
            /* CREATE TABLE IF NOT EXISTS ignored_block (id bigint); */
            'CREATE TABLE IF NOT EXISTS events_p%s (id bigint)';
            'CREATE TABLE IF NOT EXISTS session_event_dedupe_p%s (id bigint)';
            'CREATE TABLE IF NOT EXISTS moa.embeddings_p%s (id bigint)';
            'CREATE TABLE IF NOT EXISTS moa.graph_changelog_%s (id bigint)';
            'CREATE TABLE IF NOT EXISTS %I.session_blobs (id bigint)';
        "#;

        let tables = extract_create_tables(sql)
            .expect("supported CREATE TABLE forms must be accepted")
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            tables,
            identities(&[
                ("%i", "session_blobs"),
                ("moa", "beta"),
                ("moa", "embeddings"),
                ("moa", "graph_changelog"),
                ("public", "alpha"),
                ("public", "events"),
                ("public", "session_event_dedupe"),
            ])
        );
    }

    #[test]
    fn check_migrations_rejects_tables_removed_later_from_the_fresh_catalog() {
        // Pins: ownership is a bijection with the final catalog, not historical CREATE TABLE names.
        let error = extract_create_tables(
            "CREATE TABLE public.retired (id bigint); DROP TABLE public.retired;",
        )
        .expect_err("a later DROP TABLE must fail the fresh-only catalog invariant");

        assert!(
            error.to_string().contains("must not contain DROP TABLE"),
            "{error:#}"
        );
    }

    #[test]
    fn check_migrations_rejects_duplicate_manifest_rows() {
        // Pins: one logical table family has exactly one ownership row.
        let manifest = MigrationOwnershipManifest {
            table: vec![
                ownership("public", "sessions", "moa-session"),
                ownership("public", "sessions", "moa-session"),
            ],
        };
        let error = validate_migration_ownership(&manifest, &identities(&[("public", "sessions")]))
            .expect_err("duplicate ownership must fail");
        assert!(
            error
                .to_string()
                .contains("duplicate migration ownership row: public.sessions"),
            "{error:#}"
        );
    }

    #[test]
    fn check_migrations_reports_missing_and_stale_manifest_rows() {
        // Pins: ownership validation is an exact inventory comparison in both directions.
        let manifest = MigrationOwnershipManifest {
            table: vec![
                ownership("public", "sessions", "moa-session"),
                ownership("public", "stale", "moa-session"),
            ],
        };
        let error = validate_migration_ownership(
            &manifest,
            &identities(&[("public", "sessions"), ("public", "missing")]),
        )
        .expect_err("inventory mismatch must fail");
        let message = error.to_string();
        assert!(
            message.contains("missing ownership rows:\npublic.missing"),
            "{error:#}"
        );
        assert!(
            message.contains("stale ownership rows:\npublic.stale"),
            "{error:#}"
        );
    }

    #[test]
    fn check_migrations_rejects_invalid_owner_identifiers() {
        // Pins: manifest ownership cannot silently name a crate or service that does not exist.
        let manifest = MigrationOwnershipManifest {
            table: vec![ownership("public", "sessions", "does-not-exist")],
        };
        let error = validate_migration_ownership(&manifest, &identities(&[("public", "sessions")]))
            .expect_err("invalid owner must fail");
        assert!(
            error
                .to_string()
                .contains("invalid owner for public.sessions")
        );

        let mut entry = ownership("public", "sessions", "moa-session");
        entry.readers.push("missing-reader".to_string());
        let manifest = MigrationOwnershipManifest { table: vec![entry] };
        let error = validate_migration_ownership(&manifest, &identities(&[("public", "sessions")]))
            .expect_err("invalid reader must fail");
        assert!(
            error
                .to_string()
                .contains("invalid reader for public.sessions")
        );
    }
}
