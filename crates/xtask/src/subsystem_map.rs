//! Validated subsystem routing and bounded audit-packet planning.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

const REGISTRY_PATH: &str = ".agents/subsystems.toml";
const NEXTEST_CONFIG_PATH: &str = ".config/nextest.toml";
const MAKEFILE_PATH: &str = "Makefile";
const AUDIT_ARTIFACT_ROOT: &str = "target/agent-audits";
const REGISTRY_VERSION: u8 = 1;
const HARD_MAX_AGENTS: usize = 4;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubsystemRegistry {
    version: u8,
    audit: AuditPolicy,
    subsystem: Vec<Subsystem>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditPolicy {
    max_agents: usize,
    report_word_limit: usize,
    artifact_root: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Subsystem {
    id: String,
    owner: String,
    path_prefixes: Vec<String>,
    docs: Vec<String>,
    agent_files: Vec<String>,
    #[serde(default)]
    local_agents: Vec<LocalAgentFile>,
    #[serde(default)]
    test_profiles: Vec<String>,
    #[serde(default)]
    make_targets: Vec<String>,
    #[serde(default)]
    live_gates: Vec<LiveGate>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalAgentFile {
    path_prefix: String,
    file: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveGate {
    id: String,
    make_target: String,
    authorization_env: Vec<String>,
    #[serde(default)]
    credentials_any_of: Vec<String>,
    #[serde(default)]
    budget_env: Vec<String>,
    #[serde(default)]
    services: Vec<String>,
    billed: bool,
}

#[derive(Debug)]
struct WorkspaceInfo {
    package_names: BTreeSet<String>,
    member_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct AuditPlan {
    schema_version: u8,
    reviewer_cap: usize,
    report_word_limit: usize,
    packet_count: usize,
    packets: Vec<AuditPacket>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct AuditPacket {
    id: String,
    subsystem_ids: Vec<String>,
    paths: Vec<String>,
    docs: Vec<String>,
    agent_files: Vec<String>,
    test_profiles: Vec<String>,
    make_targets: Vec<String>,
    live_gates: Vec<PacketLiveGate>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct PacketLiveGate {
    subsystem_id: String,
    id: String,
    make_target: String,
    authorization_env: Vec<String>,
    credentials_any_of: Vec<String>,
    budget_env: Vec<String>,
    services: Vec<String>,
    billed: bool,
}

#[derive(Debug, Default)]
struct PacketBuilder {
    subsystem_ids: BTreeSet<String>,
    paths: BTreeSet<String>,
    docs: BTreeSet<String>,
    agent_files: BTreeSet<String>,
    test_profiles: BTreeSet<String>,
    make_targets: BTreeSet<String>,
    live_gates: BTreeSet<PacketLiveGate>,
}

#[derive(Debug, Default)]
struct PlanOptions {
    base: Option<String>,
    paths: Vec<String>,
    max_agents: Option<usize>,
    output: Option<PathBuf>,
}

/// Validates the checked-in subsystem registry against the current workspace.
pub(crate) fn check() -> Result<()> {
    let root = repository_root()?;
    let registry = load_registry(&root)?;
    let workspace = load_workspace_info(&root)?;
    let profiles = load_nextest_profiles(&root)?;
    let make_targets = load_make_targets(&root)?;
    validate_registry(&root, &registry, &workspace, &profiles, &make_targets)?;
    println!(
        "subsystem registry clean: {} groups cover {} workspace members",
        registry.subsystem.len(),
        workspace.member_paths.len()
    );
    Ok(())
}

/// Plans bounded, read-only subsystem audit packets without launching agents.
pub(crate) fn plan(args: impl Iterator<Item = String>) -> Result<()> {
    let root = repository_root()?;
    let Some(options) = parse_plan_options(args)? else {
        return Ok(());
    };
    let registry = load_registry(&root)?;
    let workspace = load_workspace_info(&root)?;
    let profiles = load_nextest_profiles(&root)?;
    let make_targets = load_make_targets(&root)?;
    validate_registry(&root, &registry, &workspace, &profiles, &make_targets)?;

    let paths = collect_plan_paths(&root, &options)?;
    let plan = build_audit_plan(&registry, paths, options.max_agents)?;
    let output = resolve_audit_output(
        &root,
        &registry.audit.artifact_root,
        options.output.as_deref(),
    )?;
    write_audit_plan(&output, &plan)?;
    println!(
        "planned {} bounded audit packet(s) under {}",
        plan.packet_count,
        output.display()
    );
    Ok(())
}

fn repository_root() -> Result<PathBuf> {
    let current = env::current_dir().context("resolve current directory")?;
    current
        .ancestors()
        .find(|path| {
            path.join("Cargo.toml").is_file() && path.join("crates/xtask/Cargo.toml").is_file()
        })
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            anyhow!(
                "could not locate the MOA repository root from {}",
                current.display()
            )
        })
}

fn load_registry(root: &Path) -> Result<SubsystemRegistry> {
    let path = root.join(REGISTRY_PATH);
    let body = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

fn load_workspace_info(root: &Path) -> Result<WorkspaceInfo> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(root)
        .output()
        .context("run cargo metadata for subsystem coverage")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed while checking subsystem coverage: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    #[derive(Deserialize)]
    struct Metadata {
        packages: Vec<Package>,
        workspace_members: Vec<String>,
    }

    #[derive(Deserialize)]
    struct Package {
        id: String,
        name: String,
        manifest_path: PathBuf,
    }

    let metadata: Metadata = serde_json::from_slice(&output.stdout)
        .context("parse cargo metadata for subsystem coverage")?;
    let member_ids = metadata
        .workspace_members
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut package_names = BTreeSet::new();
    let mut member_paths = Vec::new();
    for package in metadata
        .packages
        .into_iter()
        .filter(|package| member_ids.contains(&package.id))
    {
        package_names.insert(package.name);
        let package_dir = package
            .manifest_path
            .parent()
            .context("workspace package manifest has no parent directory")?;
        let relative = package_dir.strip_prefix(root).with_context(|| {
            format!(
                "workspace package {} is outside repository root {}",
                package_dir.display(),
                root.display()
            )
        })?;
        member_paths.push(format!("{}/", path_to_slashes(relative)));
    }
    member_paths.sort();
    Ok(WorkspaceInfo {
        package_names,
        member_paths,
    })
}

fn load_nextest_profiles(root: &Path) -> Result<BTreeSet<String>> {
    let path = root.join(NEXTEST_CONFIG_PATH);
    let body = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(body
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("[profile.")
                .and_then(|value| value.strip_suffix(']'))
                .map(str::to_string)
        })
        .collect())
}

fn load_make_targets(root: &Path) -> Result<BTreeSet<String>> {
    let path = root.join(MAKEFILE_PATH);
    let body = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(body
        .lines()
        .filter_map(|line| {
            if line.starts_with(|character: char| character.is_whitespace() || character == '#') {
                return None;
            }
            let (candidate, _) = line.split_once(':')?;
            let candidate = candidate.trim();
            (!candidate.is_empty()
                && !candidate.starts_with('.')
                && !candidate.contains(char::is_whitespace)
                && !candidate.contains('='))
            .then(|| candidate.to_string())
        })
        .collect())
}

fn validate_registry(
    root: &Path,
    registry: &SubsystemRegistry,
    workspace: &WorkspaceInfo,
    profiles: &BTreeSet<String>,
    make_targets: &BTreeSet<String>,
) -> Result<()> {
    if registry.version != REGISTRY_VERSION {
        bail!(
            "unsupported subsystem registry version {}; expected {REGISTRY_VERSION}",
            registry.version
        );
    }
    if !(1..=HARD_MAX_AGENTS).contains(&registry.audit.max_agents) {
        bail!(
            "audit.max_agents must be between 1 and {HARD_MAX_AGENTS}; saw {}",
            registry.audit.max_agents
        );
    }
    if registry.audit.report_word_limit == 0 {
        bail!("audit.report_word_limit must be positive");
    }
    validate_relative_path(&registry.audit.artifact_root, "audit.artifact_root")?;
    if registry.audit.artifact_root != AUDIT_ARTIFACT_ROOT {
        bail!("audit.artifact_root must be `{AUDIT_ARTIFACT_ROOT}`");
    }

    let mut ids = BTreeSet::new();
    let mut prefixes = BTreeMap::<String, String>::new();
    for subsystem in &registry.subsystem {
        validate_identifier(&subsystem.id, "subsystem id")?;
        if !ids.insert(subsystem.id.clone()) {
            bail!("duplicate subsystem id `{}`", subsystem.id);
        }
        if !workspace.package_names.contains(&subsystem.owner) {
            bail!(
                "subsystem `{}` names unknown workspace owner `{}`",
                subsystem.id,
                subsystem.owner
            );
        }
        if subsystem.path_prefixes.is_empty() {
            bail!("subsystem `{}` has no path_prefixes", subsystem.id);
        }
        for prefix in &subsystem.path_prefixes {
            validate_relative_path(prefix, "path prefix")?;
            validate_configured_path(root, prefix, "path prefix", &subsystem.id)?;
            if let Some(previous) = prefixes.insert(prefix.clone(), subsystem.id.clone()) {
                bail!(
                    "ambiguous path prefix `{prefix}` is declared by `{previous}` and `{}`",
                    subsystem.id
                );
            }
        }

        if subsystem.docs.is_empty() {
            bail!("subsystem `{}` has no canonical docs", subsystem.id);
        }
        for doc in &subsystem.docs {
            validate_relative_path(doc, "doc path")?;
            validate_configured_path(root, doc, "doc", &subsystem.id)?;
        }
        if subsystem.agent_files.is_empty() {
            bail!("subsystem `{}` has no agent_files", subsystem.id);
        }
        for agent_file in &subsystem.agent_files {
            validate_agent_file(root, agent_file, &subsystem.id)?;
        }
        for local in &subsystem.local_agents {
            validate_relative_path(&local.path_prefix, "local agent path prefix")?;
            if !subsystem
                .path_prefixes
                .iter()
                .any(|prefix| local.path_prefix.starts_with(prefix))
            {
                bail!(
                    "subsystem `{}` local agent prefix `{}` is outside its routed prefixes",
                    subsystem.id,
                    local.path_prefix
                );
            }
            validate_agent_file(root, &local.file, &subsystem.id)?;
        }

        if subsystem.test_profiles.is_empty() && subsystem.make_targets.is_empty() {
            bail!(
                "subsystem `{}` must reference at least one nextest profile or Make target",
                subsystem.id
            );
        }
        for profile in &subsystem.test_profiles {
            if !profiles.contains(profile) {
                bail!(
                    "subsystem `{}` references unknown nextest profile `{profile}`",
                    subsystem.id
                );
            }
        }
        for target in &subsystem.make_targets {
            if !make_targets.contains(target) {
                bail!(
                    "subsystem `{}` references unknown Make target `{target}`",
                    subsystem.id
                );
            }
        }
        validate_live_gates(subsystem, make_targets)?;
    }

    for member in &workspace.member_paths {
        if route_subsystem(&registry.subsystem, member)?.is_none() {
            bail!("workspace member `{member}` is not covered by any subsystem prefix");
        }
    }
    Ok(())
}

fn validate_live_gates(subsystem: &Subsystem, make_targets: &BTreeSet<String>) -> Result<()> {
    let mut ids = BTreeSet::new();
    for gate in &subsystem.live_gates {
        validate_identifier(&gate.id, "live gate id")?;
        if !ids.insert(&gate.id) {
            bail!(
                "subsystem `{}` has duplicate live gate id `{}`",
                subsystem.id,
                gate.id
            );
        }
        if gate.authorization_env.is_empty() {
            bail!(
                "subsystem `{}` live gate `{}` has no explicit authorization_env",
                subsystem.id,
                gate.id
            );
        }
        if !make_targets.contains(&gate.make_target)
            || !subsystem.make_targets.contains(&gate.make_target)
        {
            bail!(
                "subsystem `{}` live gate `{}` must reference one of its checked-in Make targets; saw `{}`",
                subsystem.id,
                gate.id,
                gate.make_target
            );
        }
        for variable in gate
            .authorization_env
            .iter()
            .chain(&gate.credentials_any_of)
            .chain(&gate.budget_env)
        {
            if !valid_env_name(variable) {
                bail!(
                    "subsystem `{}` live gate `{}` has invalid environment variable `{variable}`",
                    subsystem.id,
                    gate.id
                );
            }
        }
        if gate.billed && gate.credentials_any_of.is_empty() {
            bail!(
                "subsystem `{}` billed live gate `{}` must name credentials_any_of",
                subsystem.id,
                gate.id
            );
        }
        if gate
            .services
            .iter()
            .any(|service| service.trim().is_empty())
        {
            bail!(
                "subsystem `{}` live gate `{}` contains an empty service name",
                subsystem.id,
                gate.id
            );
        }
    }
    Ok(())
}

fn validate_agent_file(root: &Path, path: &str, subsystem_id: &str) -> Result<()> {
    validate_relative_path(path, "agent file")?;
    if Path::new(path).file_name().and_then(|name| name.to_str()) != Some("AGENTS.md") {
        bail!("subsystem `{subsystem_id}` agent file must be named AGENTS.md; saw `{path}`");
    }
    validate_configured_path(root, path, "agent file", subsystem_id)
}

fn validate_configured_path(root: &Path, value: &str, kind: &str, owner: &str) -> Result<()> {
    let path = root.join(value.trim_end_matches('/'));
    if !path.exists() {
        bail!("subsystem `{owner}` {kind} does not exist: `{value}`");
    }
    Ok(())
}

fn validate_relative_path(value: &str, label: &str) -> Result<()> {
    if value.is_empty() || Path::new(value).is_absolute() || value.starts_with("./") {
        bail!("{label} must be a non-empty normalized repository-relative path: `{value}`");
    }
    if Path::new(value)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("{label} must not contain parent traversal: `{value}`");
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("{label} must use lowercase kebab-case: `{value}`");
    }
    Ok(())
}

fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn route_subsystem<'a>(subsystems: &'a [Subsystem], path: &str) -> Result<Option<&'a Subsystem>> {
    let mut best: Option<(&Subsystem, usize)> = None;
    for subsystem in subsystems {
        for prefix in &subsystem.path_prefixes {
            if !path_matches_prefix(path, prefix) {
                continue;
            }
            match best {
                None => best = Some((subsystem, prefix.len())),
                Some((_, length)) if prefix.len() > length => {
                    best = Some((subsystem, prefix.len()));
                }
                Some((current, length)) if prefix.len() == length && current.id != subsystem.id => {
                    bail!(
                        "path `{path}` is ambiguous between `{}` and `{}` at prefix length {length}",
                        current.id,
                        subsystem.id
                    );
                }
                Some(_) => {}
            }
        }
    }
    Ok(best.map(|(subsystem, _)| subsystem))
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix.ends_with('/') {
        path.starts_with(prefix)
    } else {
        path == prefix
    }
}

fn parse_plan_options(mut args: impl Iterator<Item = String>) -> Result<Option<PlanOptions>> {
    let mut options = PlanOptions::default();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--base" => {
                options.base = Some(next_value(&mut args, "--base")?);
            }
            "--path" => options.paths.push(next_value(&mut args, "--path")?),
            "--max-agents" => {
                let value = next_value(&mut args, "--max-agents")?;
                options.max_agents = Some(
                    value
                        .parse::<usize>()
                        .with_context(|| format!("parse --max-agents value `{value}`"))?,
                );
            }
            "--output" => {
                options.output = Some(PathBuf::from(next_value(&mut args, "--output")?));
            }
            "-h" | "--help" => {
                println!(
                    "usage: cargo xtask plan-subsystem-audit (--base REV | --path PATH...) [--max-agents N] [--output DIR]"
                );
                return Ok(None);
            }
            _ => bail!("unknown plan-subsystem-audit argument `{argument}`"),
        }
    }
    if options.base.is_none() && options.paths.is_empty() {
        bail!("plan-subsystem-audit requires --base REV or at least one --path PATH");
    }
    Ok(Some(options))
}

fn next_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow!("{option} requires a value"))
}

fn collect_plan_paths(root: &Path, options: &PlanOptions) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    if let Some(base) = &options.base {
        for path in git_lines(root, &["diff", "--name-only", base, "--"])? {
            paths.insert(normalize_input_path(&path)?);
        }
        for path in git_lines(root, &["ls-files", "--others", "--exclude-standard"])? {
            paths.insert(normalize_input_path(&path)?);
        }
    }
    for path in &options.paths {
        paths.insert(normalize_input_path(path)?);
    }
    Ok(paths.into_iter().collect())
}

fn git_lines(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .context("git output was not UTF-8")?
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn normalize_input_path(path: &str) -> Result<String> {
    let normalized = path.strip_prefix("./").unwrap_or(path);
    validate_relative_path(normalized, "audit input path")?;
    Ok(normalized.to_string())
}

fn build_audit_plan(
    registry: &SubsystemRegistry,
    paths: Vec<String>,
    requested_max_agents: Option<usize>,
) -> Result<AuditPlan> {
    let reviewer_cap = requested_max_agents
        .unwrap_or(registry.audit.max_agents)
        .min(registry.audit.max_agents)
        .min(HARD_MAX_AGENTS);
    if reviewer_cap == 0 {
        bail!("--max-agents must be positive");
    }

    let mut routed = BTreeMap::<String, (&Subsystem, BTreeSet<String>)>::new();
    let mut uncovered = Vec::new();
    for path in paths {
        match route_subsystem(&registry.subsystem, &path)? {
            Some(subsystem) => {
                routed
                    .entry(subsystem.id.clone())
                    .or_insert_with(|| (subsystem, BTreeSet::new()))
                    .1
                    .insert(path);
            }
            None => uncovered.push(path),
        }
    }
    if !uncovered.is_empty() {
        bail!(
            "audit paths are not covered by the subsystem registry:\n{}",
            uncovered.join("\n")
        );
    }

    let packet_count = routed.len().min(reviewer_cap);
    let mut builders = (0..packet_count)
        .map(|_| PacketBuilder::default())
        .collect::<Vec<_>>();
    for (index, (_, (subsystem, subsystem_paths))) in routed.into_iter().enumerate() {
        let builder = &mut builders[index % packet_count];
        builder.subsystem_ids.insert(subsystem.id.clone());
        builder.paths.extend(subsystem_paths.iter().cloned());
        builder.docs.extend(subsystem.docs.iter().cloned());
        builder
            .agent_files
            .extend(subsystem.agent_files.iter().cloned());
        for local in &subsystem.local_agents {
            if subsystem_paths
                .iter()
                .any(|path| path_matches_prefix(path, &local.path_prefix))
            {
                builder.agent_files.insert(local.file.clone());
            }
        }
        builder
            .test_profiles
            .extend(subsystem.test_profiles.iter().cloned());
        builder
            .make_targets
            .extend(subsystem.make_targets.iter().cloned());
        builder
            .live_gates
            .extend(subsystem.live_gates.iter().map(|gate| PacketLiveGate {
                subsystem_id: subsystem.id.clone(),
                id: gate.id.clone(),
                make_target: gate.make_target.clone(),
                authorization_env: gate.authorization_env.clone(),
                credentials_any_of: gate.credentials_any_of.clone(),
                budget_env: gate.budget_env.clone(),
                services: gate.services.clone(),
                billed: gate.billed,
            }));
    }

    let packets = builders
        .into_iter()
        .enumerate()
        .map(|(index, builder)| AuditPacket {
            id: format!("packet-{:02}", index + 1),
            subsystem_ids: builder.subsystem_ids.into_iter().collect(),
            paths: builder.paths.into_iter().collect(),
            docs: builder.docs.into_iter().collect(),
            agent_files: builder.agent_files.into_iter().collect(),
            test_profiles: builder.test_profiles.into_iter().collect(),
            make_targets: builder.make_targets.into_iter().collect(),
            live_gates: builder.live_gates.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    Ok(AuditPlan {
        schema_version: 1,
        reviewer_cap,
        report_word_limit: registry.audit.report_word_limit,
        packet_count: packets.len(),
        packets,
    })
}

fn current_revision(root: &Path) -> Result<String> {
    git_lines(root, &["rev-parse", "--short", "HEAD"])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("git rev-parse returned no revision"))
}

fn resolve_audit_output(
    root: &Path,
    artifact_root: &str,
    requested: Option<&Path>,
) -> Result<PathBuf> {
    let relative = match requested {
        Some(path) => {
            let value = path
                .to_str()
                .context("audit output path must be valid UTF-8")?;
            validate_relative_path(value, "audit output path")?;
            let path = Path::new(value);
            let suffix = path.strip_prefix(artifact_root).with_context(|| {
                format!("audit output must stay beneath `{artifact_root}`: `{value}`")
            })?;
            if suffix.as_os_str().is_empty() {
                bail!("audit output must name a run directory beneath `{artifact_root}`");
            }
            path.to_path_buf()
        }
        None => Path::new(artifact_root).join(current_revision(root)?),
    };
    reject_symlinked_output_components(root, &relative)?;
    Ok(root.join(relative))
}

fn reject_symlinked_output_components(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "audit output may not traverse symlinked path component `{}`",
                    current.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect audit output path {}", current.display()));
            }
        }
    }
    Ok(())
}

fn write_audit_plan(output: &Path, plan: &AuditPlan) -> Result<()> {
    fs::create_dir_all(output.join("reports"))
        .with_context(|| format!("create audit output directory {}", output.display()))?;
    remove_stale_generated_files(output)?;
    let mut json = serde_json::to_string_pretty(plan).context("serialize audit plan")?;
    json.push('\n');
    fs::write(output.join("plan.json"), json)
        .with_context(|| format!("write {}/plan.json", output.display()))?;
    for packet in &plan.packets {
        fs::write(
            output.join(format!("{}.md", packet.id)),
            render_packet(packet, plan.report_word_limit),
        )
        .with_context(|| format!("write audit packet {}", packet.id))?;
    }
    fs::write(
        output.join("checkpoint.md"),
        "# Audit Checkpoint\n\n- Status: planned\n- Integration owner: unassigned\n- Completed packets: none\n- Live authorization: not granted\n",
    )
    .with_context(|| format!("write {}/checkpoint.md", output.display()))?;
    Ok(())
}

fn remove_stale_generated_files(output: &Path) -> Result<()> {
    remove_generated_file(output, "plan.json")?;
    remove_generated_file(output, "checkpoint.md")?;
    for entry in fs::read_dir(output)
        .with_context(|| format!("read audit output directory {}", output.display()))?
    {
        let entry = entry.with_context(|| format!("read entry under {}", output.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(sequence) = name
            .strip_prefix("packet-")
            .and_then(|value| value.strip_suffix(".md"))
        else {
            continue;
        };
        if sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("inspect stale audit packet {}", entry.path().display()))?;
        if !file_type.is_file() && !file_type.is_symlink() {
            bail!(
                "refusing to replace non-file audit packet path `{}`",
                entry.path().display()
            );
        }
        fs::remove_file(entry.path())
            .with_context(|| format!("remove stale audit packet {name}"))?;
    }
    Ok(())
}

fn remove_generated_file(output: &Path, name: &str) -> Result<()> {
    let path = output.join(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect generated audit file {name}"));
        }
    };
    if !metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
        bail!(
            "refusing to replace non-file generated audit path `{}`",
            path.display()
        );
    }
    fs::remove_file(path).with_context(|| format!("remove generated audit file {name}"))
}

fn render_packet(packet: &AuditPacket, report_word_limit: usize) -> String {
    let mut body = String::new();
    body.push_str(&format!("# {}\n\n", packet.id));
    body.push_str("- Mode: read-only discovery\n");
    body.push_str(&format!("- Report limit: {report_word_limit} words\n"));
    body.push_str(&format!(
        "- Subsystems: {}\n",
        packet.subsystem_ids.join(", ")
    ));
    append_section(&mut body, "Paths", &packet.paths);
    append_section(&mut body, "Canonical docs", &packet.docs);
    append_section(&mut body, "Agent instructions", &packet.agent_files);
    append_section(&mut body, "Nextest profiles", &packet.test_profiles);
    append_section(&mut body, "Make targets", &packet.make_targets);
    body.push_str("\n## Live gates\n\n");
    if packet.live_gates.is_empty() {
        body.push_str("- None.\n");
    } else {
        body.push_str("Do not run these gates without their explicit authorization:\n\n");
        for gate in &packet.live_gates {
            body.push_str(&format!(
                "- `{}` / `{}`: authorization `{}`; billed `{}`\n",
                gate.subsystem_id,
                gate.id,
                gate.authorization_env.join(", "),
                gate.billed
            ));
        }
    }
    body.push_str(
        "\nReturn exact path evidence, unresolved gaps, and the smallest safe next change.\n",
    );
    body
}

fn append_section(body: &mut String, heading: &str, values: &[String]) {
    body.push_str(&format!("\n## {heading}\n\n"));
    for value in values {
        body.push_str(&format!("- `{value}`\n"));
    }
}

fn path_to_slashes(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subsystem(id: &str, prefix: &str) -> Subsystem {
        Subsystem {
            id: id.to_string(),
            owner: "owner".to_string(),
            path_prefixes: vec![prefix.to_string()],
            docs: vec!["docs/01.md".to_string()],
            agent_files: vec!["AGENTS.md".to_string()],
            local_agents: Vec::new(),
            test_profiles: vec!["fast-pr".to_string()],
            make_targets: Vec::new(),
            live_gates: Vec::new(),
        }
    }

    fn registry(subsystems: Vec<Subsystem>) -> SubsystemRegistry {
        SubsystemRegistry {
            version: REGISTRY_VERSION,
            audit: AuditPolicy {
                max_agents: HARD_MAX_AGENTS,
                report_word_limit: 600,
                artifact_root: AUDIT_ARTIFACT_ROOT.to_string(),
            },
            subsystem: subsystems,
        }
    }

    fn workspace(paths: &[&str]) -> WorkspaceInfo {
        WorkspaceInfo {
            package_names: BTreeSet::from(["owner".to_string()]),
            member_paths: paths.iter().map(|path| (*path).to_string()).collect(),
        }
    }

    fn create_validation_tree(root: &Path, prefixes: &[&str]) {
        fs::create_dir_all(root.join("docs")).expect("create synthetic docs directory");
        fs::write(root.join("docs/01.md"), "# Architecture\n")
            .expect("write synthetic architecture doc");
        fs::write(root.join("AGENTS.md"), "# Instructions\n")
            .expect("write synthetic agent instructions");
        for prefix in prefixes {
            fs::create_dir_all(root.join(prefix.trim_end_matches('/')))
                .expect("create synthetic routed prefix");
        }
    }

    #[test]
    fn longest_prefix_routes_to_the_most_specific_subsystem() {
        // Pins: grouped parent routing never steals a path from a more-specific owner.
        let subsystems = vec![
            subsystem("repository", "crates/"),
            subsystem("memory", "crates/moa-memory/"),
        ];

        let routed = route_subsystem(&subsystems, "crates/moa-memory/graph/src/lib.rs")
            .expect("longest-prefix routing should be unambiguous")
            .expect("memory source should have a routed subsystem");

        assert_eq!(routed.id, "memory");
    }

    #[test]
    fn explicit_directory_path_preserves_prefix_routing() {
        // Pins: an explicit crate directory remains routable instead of losing its trailing slash.
        let normalized = normalize_input_path("./crates/moa-hands/")
            .expect("normalize explicit crate directory");
        assert_eq!(normalized, "crates/moa-hands/");

        let plan = build_audit_plan(
            &registry(vec![subsystem("hands", "crates/moa-hands/")]),
            vec![normalized],
            Some(1),
        )
        .expect("explicit crate directory should route to its subsystem");

        assert_eq!(plan.packet_count, 1);
        assert_eq!(plan.packets[0].subsystem_ids, ["hands"]);
        assert_eq!(plan.packets[0].paths, ["crates/moa-hands/"]);
    }

    #[test]
    fn duplicate_prefixes_are_rejected_as_ambiguous() {
        // Pins: two owners cannot silently receive identical context for one path.
        let temporary = tempfile::tempdir().expect("create validation tree");
        create_validation_tree(temporary.path(), &["crates/shared/"]);
        let registry = registry(vec![
            subsystem("alpha", "crates/shared/"),
            subsystem("beta", "crates/shared/"),
        ]);

        let error = validate_registry(
            temporary.path(),
            &registry,
            &workspace(&["crates/shared/"]),
            &BTreeSet::from(["fast-pr".to_string()]),
            &BTreeSet::new(),
        )
        .expect_err("duplicate prefixes must fail validation");

        assert!(error.to_string().contains("ambiguous path prefix"));
    }

    #[test]
    fn missing_configured_paths_are_rejected() {
        // Pins: stale registry paths fail before audit planning can omit their owner.
        let temporary = tempfile::tempdir().expect("create validation tree");
        create_validation_tree(temporary.path(), &[]);
        let registry = registry(vec![subsystem("missing", "crates/missing/")]);

        let error = validate_registry(
            temporary.path(),
            &registry,
            &workspace(&["crates/missing/"]),
            &BTreeSet::from(["fast-pr".to_string()]),
            &BTreeSet::new(),
        )
        .expect_err("missing routed paths must fail validation");

        assert!(error.to_string().contains("path prefix does not exist"));
    }

    #[test]
    fn uncovered_workspace_members_are_rejected() {
        // Pins: adding a workspace crate requires assigning it to one subsystem.
        let temporary = tempfile::tempdir().expect("create validation tree");
        create_validation_tree(temporary.path(), &["crates/covered/"]);
        let registry = registry(vec![subsystem("covered", "crates/covered/")]);

        let error = validate_registry(
            temporary.path(),
            &registry,
            &workspace(&["crates/covered/", "crates/uncovered/"]),
            &BTreeSet::from(["fast-pr".to_string()]),
            &BTreeSet::new(),
        )
        .expect_err("uncovered workspace members must fail validation");

        assert!(
            error
                .to_string()
                .contains("workspace member `crates/uncovered/` is not covered")
        );
    }

    #[test]
    fn reviewer_count_is_capped_by_registry_policy() {
        // Pins: explicit requests cannot expand a broad audit beyond four reviewers.
        let registry = registry(
            (0..6)
                .map(|index| subsystem(&format!("group-{index}"), &format!("group-{index}/")))
                .collect(),
        );
        let paths = (0..6)
            .map(|index| format!("group-{index}/file.rs"))
            .collect();

        let plan = build_audit_plan(&registry, paths, Some(99))
            .expect("covered paths should produce a bounded plan");

        assert_eq!(plan.reviewer_cap, 4);
        assert_eq!(plan.packet_count, 4);
        assert_eq!(
            plan.packets
                .iter()
                .flat_map(|packet| &packet.subsystem_ids)
                .collect::<BTreeSet<_>>()
                .len(),
            6
        );
    }

    #[test]
    fn audit_packet_output_is_deterministic_for_input_order() {
        // Pins: identical change sets produce byte-stable packets across sessions.
        let registry = registry(vec![
            subsystem("alpha", "alpha/"),
            subsystem("beta", "beta/"),
        ]);
        let first = build_audit_plan(
            &registry,
            vec!["beta/z.rs".to_string(), "alpha/a.rs".to_string()],
            Some(2),
        )
        .expect("first plan should build");
        let second = build_audit_plan(
            &registry,
            vec!["alpha/a.rs".to_string(), "beta/z.rs".to_string()],
            Some(2),
        )
        .expect("second plan should build");

        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_string_pretty(&first).expect("serialize first plan"),
            serde_json::to_string_pretty(&second).expect("serialize second plan")
        );
        assert_eq!(
            render_packet(&first.packets[0], first.report_word_limit),
            render_packet(&second.packets[0], second.report_word_limit)
        );
    }

    #[test]
    fn audit_output_rejects_absolute_traversal_and_tracked_paths() {
        // Pins: --output cannot escape the ignored audit-artifact root or overwrite repo files.
        let temporary = tempfile::tempdir().expect("create audit output root");
        let artifact_root = AUDIT_ARTIFACT_ROOT;
        let allowed = resolve_audit_output(
            temporary.path(),
            artifact_root,
            Some(Path::new("target/agent-audits/review-01")),
        )
        .expect("nested audit output should be accepted");
        assert_eq!(
            allowed,
            temporary.path().join("target/agent-audits/review-01")
        );

        for rejected in [
            temporary.path().join("absolute-review"),
            PathBuf::from("target/agent-audits/../tracked-review"),
            PathBuf::from("docs/engineering-discipline/review"),
            PathBuf::from("target/agent-audits"),
        ] {
            assert!(
                resolve_audit_output(temporary.path(), artifact_root, Some(&rejected)).is_err(),
                "unsafe audit output should be rejected: {}",
                rejected.display()
            );
        }
    }

    #[test]
    fn repeated_packet_write_removes_only_stale_owned_packets() {
        // Pins: replanning one HEAD cannot leave obsolete packets or delete reviewer-owned files.
        let temporary = tempfile::tempdir().expect("create audit output directory");
        let output = temporary.path().join("target/agent-audits/review-01");
        let registry = registry(vec![
            subsystem("alpha", "alpha/"),
            subsystem("beta", "beta/"),
        ]);
        let first = build_audit_plan(
            &registry,
            vec!["alpha/a.rs".to_string(), "beta/b.rs".to_string()],
            Some(2),
        )
        .expect("build initial two-packet plan");
        write_audit_plan(&output, &first).expect("write initial audit packets");
        assert!(output.join("packet-02.md").is_file());

        fs::write(output.join("packet-notes.md"), "reviewer notes\n")
            .expect("write reviewer-owned packet notes");
        fs::write(output.join("checkpoint.md"), "stale checkpoint\n")
            .expect("write stale generated checkpoint");
        let second = build_audit_plan(&registry, vec!["alpha/a.rs".to_string()], Some(1))
            .expect("build replacement one-packet plan");
        write_audit_plan(&output, &second).expect("replace generated audit packets");

        assert!(output.join("packet-01.md").is_file());
        assert!(!output.join("packet-02.md").exists());
        assert_eq!(
            fs::read_to_string(output.join("packet-notes.md"))
                .expect("read preserved reviewer notes"),
            "reviewer notes\n"
        );
        assert_eq!(
            fs::read_to_string(output.join("checkpoint.md")).expect("read replacement checkpoint"),
            "# Audit Checkpoint\n\n- Status: planned\n- Integration owner: unassigned\n- Completed packets: none\n- Live authorization: not granted\n"
        );
        let expected_plan = format!(
            "{}\n",
            serde_json::to_string_pretty(&second).expect("serialize expected replacement plan")
        );
        assert_eq!(
            fs::read_to_string(output.join("plan.json")).expect("read replacement plan"),
            expected_plan
        );
    }

    #[cfg(unix)]
    #[test]
    fn packet_write_replaces_generated_symlink_without_following_it() {
        // Pins: a stale generated filename cannot redirect planner writes outside the run folder.
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("create audit output directory");
        let output = temporary.path().join("target/agent-audits/review-01");
        fs::create_dir_all(&output).expect("create audit run directory");
        let outside = temporary.path().join("tracked-plan.json");
        fs::write(&outside, "do not overwrite\n").expect("write protected outside file");
        symlink(&outside, output.join("plan.json")).expect("link stale generated plan");
        let plan = build_audit_plan(
            &registry(vec![subsystem("alpha", "alpha/")]),
            vec!["alpha/a.rs".to_string()],
            Some(1),
        )
        .expect("build replacement plan");

        write_audit_plan(&output, &plan).expect("replace generated symlink safely");

        assert_eq!(
            fs::read_to_string(&outside).expect("read protected outside file"),
            "do not overwrite\n"
        );
        assert!(
            fs::symlink_metadata(output.join("plan.json"))
                .expect("inspect replacement plan")
                .file_type()
                .is_file()
        );
    }

    #[test]
    fn billed_live_gate_requires_explicit_credentials() {
        // Pins: a billed lane cannot be represented as an unauthenticated generic target.
        let mut entry = subsystem("providers", "crates/providers/");
        entry.make_targets.push("test-provider-e2e".to_string());
        entry.live_gates.push(LiveGate {
            id: "provider-e2e".to_string(),
            make_target: "test-provider-e2e".to_string(),
            authorization_env: vec!["MOA_RUN_LIVE_PROVIDER_TESTS".to_string()],
            credentials_any_of: Vec::new(),
            budget_env: Vec::new(),
            services: vec!["restate".to_string()],
            billed: true,
        });

        let error = validate_live_gates(&entry, &BTreeSet::from(["test-provider-e2e".to_string()]))
            .expect_err("billed gates without credentials must fail");

        assert!(error.to_string().contains("must name credentials_any_of"));
    }
}
