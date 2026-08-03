//! Workspace package graph loading and dependency-direction policy.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::report::{Finding, Rule};

const NON_DOMAIN_ORCHESTRATOR_DEPENDENTS: &[&str] = &[
    "moa-orchestrator",
    "moa-edge",
    "moa-loadtest",
    "moa-fga-bootstrap",
    "xtask",
    "workspace-hack",
];

pub(super) const FORBIDDEN_DEPENDENCY_RULES: &[ForbiddenDependencyRule] = &[
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-connectors"),
        target: DependencySelector::Exact("moa-hands"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps connector lifecycle and invocation independent of tool projection",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-connectors"),
        target: DependencySelector::Exact("moa-knowledge"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps generic connector parents independent of managed knowledge projections",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-connectors"),
        target: DependencySelector::Exact("moa-wire"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps connector domain types independent of transport DTOs",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-connectors"),
        target: DependencySelector::Exact("moa-edge"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps connector domain types independent of the public HTTP edge",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-connectors"),
        target: DependencySelector::Exact("moa-orchestrator"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps connector domain types below the Restate composition boundary",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-knowledge"),
        target: DependencySelector::Exact("moa-connectors"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 composes managed knowledge projections with connector parents in the orchestrator",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-knowledge"),
        target: DependencySelector::Exact("moa-artifacts"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps managed knowledge projections independent of connector artifacts",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::Exact("moa-core"),
        target: DependencySelector::Prefix("moa-memory-"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps memory-owned graph/vector/PII/ingest types out of moa-core",
    },
    ForbiddenDependencyRule {
        source: DependencySelector::WorkspaceExcept(NON_DOMAIN_ORCHESTRATOR_DEPENDENTS),
        target: DependencySelector::Exact("moa-orchestrator"),
        edge_kinds: ALL_DEPENDENCY_KINDS,
        reason: "docs/15 keeps moa-orchestrator as the Restate transport/workflow/composition boundary",
    },
];

#[derive(Debug, Clone, Copy)]
pub(super) struct ForbiddenDependencyRule {
    pub(super) source: DependencySelector,
    pub(super) target: DependencySelector,
    pub(super) edge_kinds: &'static [DependencyKind],
    pub(super) reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DependencyKind {
    NormalBuild,
    Dev,
}

impl fmt::Display for DependencyKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NormalBuild => formatter.write_str("normal/build"),
            Self::Dev => formatter.write_str("dev"),
        }
    }
}

const ALL_DEPENDENCY_KINDS: &[DependencyKind] = &[DependencyKind::NormalBuild, DependencyKind::Dev];

#[derive(Debug, Clone, Copy)]
pub(super) enum DependencySelector {
    Exact(&'static str),
    Prefix(&'static str),
    WorkspaceExcept(&'static [&'static str]),
}

impl DependencySelector {
    fn matches(self, package: &str, graph: &PackageGraph) -> bool {
        match self {
            Self::Exact(expected) => package == expected,
            Self::Prefix(prefix) => package.starts_with(prefix),
            Self::WorkspaceExcept(excluded) => {
                graph.workspace_members.contains(package) && !excluded.contains(&package)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct PackageGraph {
    workspace_members: BTreeSet<String>,
    default_members: BTreeSet<String>,
    normal_build_dependencies: BTreeMap<String, BTreeSet<String>>,
    dev_dependencies: BTreeMap<String, BTreeSet<String>>,
}

impl PackageGraph {
    pub(super) fn package_count(&self) -> usize {
        self.workspace_members.len()
    }

    pub(super) fn default_member_count(&self) -> usize {
        self.default_members.len()
    }

    pub(super) fn direct_reverse_dependencies(&self, package: &str) -> BTreeSet<String> {
        self.normal_build_dependencies
            .iter()
            .filter(|(candidate, dependencies)| {
                candidate.as_str() != package && dependencies.contains(package)
            })
            .map(|(candidate, _dependencies)| candidate.clone())
            .collect()
    }

    pub(super) fn transitive_reverse_dependencies(&self, package: &str) -> BTreeSet<String> {
        self.workspace_members
            .iter()
            .filter(|candidate| candidate.as_str() != package)
            .filter(|candidate| self.depends_on(candidate, package))
            .cloned()
            .collect()
    }

    fn depends_on(&self, source: &str, target: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = self
            .normal_build_dependencies
            .get(source)
            .into_iter()
            .flat_map(|dependencies| dependencies.iter().cloned())
            .collect::<Vec<_>>();

        while let Some(candidate) = stack.pop() {
            if candidate == target {
                return true;
            }
            if !seen.insert(candidate.clone()) {
                continue;
            }
            if let Some(dependencies) = self.normal_build_dependencies.get(&candidate) {
                stack.extend(dependencies.iter().cloned());
            }
        }

        false
    }

    pub(super) fn dependencies(&self, kind: DependencyKind) -> &BTreeMap<String, BTreeSet<String>> {
        match kind {
            DependencyKind::NormalBuild => &self.normal_build_dependencies,
            DependencyKind::Dev => &self.dev_dependencies,
        }
    }

    pub(super) fn dev_only_edges(&self) -> Vec<(String, String)> {
        self.dev_dependencies
            .iter()
            .flat_map(|(source, dependencies)| {
                dependencies.iter().filter_map(move |target| {
                    let is_normal_build = self
                        .normal_build_dependencies
                        .get(source)
                        .is_some_and(|normal_build| normal_build.contains(target));
                    (!is_normal_build).then(|| (source.clone(), target.clone()))
                })
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn for_tests(
        packages: &[&str],
        default_members: &[&str],
        edges: &[(&str, &str)],
    ) -> Self {
        Self::for_tests_with_kinds(packages, default_members, edges, &[])
    }

    #[cfg(test)]
    pub(super) fn for_tests_with_kinds(
        packages: &[&str],
        default_members: &[&str],
        normal_build_edges: &[(&str, &str)],
        dev_edges: &[(&str, &str)],
    ) -> Self {
        let workspace_members = packages
            .iter()
            .map(|package| (*package).to_string())
            .collect::<BTreeSet<_>>();
        let default_members = default_members
            .iter()
            .map(|package| (*package).to_string())
            .collect::<BTreeSet<_>>();
        let mut normal_build_dependencies = workspace_members
            .iter()
            .map(|package| (package.clone(), BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        let mut dev_dependencies = normal_build_dependencies.clone();

        for (source, target) in normal_build_edges {
            normal_build_dependencies
                .entry((*source).to_string())
                .or_default()
                .insert((*target).to_string());
        }
        for (source, target) in dev_edges {
            dev_dependencies
                .entry((*source).to_string())
                .or_default()
                .insert((*target).to_string());
        }

        Self {
            workspace_members,
            default_members,
            normal_build_dependencies,
            dev_dependencies,
        }
    }
}

pub(super) fn load_package_graph() -> Result<PackageGraph> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .output()
        .context("run cargo metadata for architecture boundary check")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse_package_graph(&output.stdout)
}

pub(super) fn parse_package_graph(metadata_json: &[u8]) -> Result<PackageGraph> {
    let metadata = serde_json::from_slice::<Value>(metadata_json)
        .context("parse cargo metadata JSON for architecture boundary check")?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .context("cargo metadata JSON missing `packages` array")?;

    let mut id_to_name = BTreeMap::new();
    let mut package_values = BTreeMap::new();
    for package in packages {
        let name = value_string_field(package, "name")?.to_string();
        let id = value_string_field(package, "id")?.to_string();
        id_to_name.insert(id, name.clone());
        package_values.insert(name, package);
    }

    let workspace_members =
        package_names_from_metadata_ids(&metadata, "workspace_members", &id_to_name)?;
    let default_members =
        package_names_from_metadata_ids(&metadata, "workspace_default_members", &id_to_name)?;
    let mut normal_build_dependencies = BTreeMap::new();
    let mut dev_dependencies = BTreeMap::new();
    for package_name in &workspace_members {
        let Some(package) = package_values.get(package_name) else {
            bail!("workspace package `{package_name}` missing from cargo metadata package list");
        };
        let package_dependencies = package
            .get("dependencies")
            .and_then(Value::as_array)
            .with_context(|| format!("package `{package_name}` missing dependencies array"))?;
        let mut package_normal_build_dependencies = BTreeSet::new();
        let mut package_dev_dependencies = BTreeSet::new();
        for dependency in package_dependencies {
            let dependency_name = value_string_field(dependency, "name")?;
            if !workspace_members.contains(dependency_name) {
                continue;
            }
            match dependency.get("kind").and_then(Value::as_str) {
                None | Some("normal" | "build") => {
                    package_normal_build_dependencies.insert(dependency_name.to_string());
                }
                Some("dev") => {
                    package_dev_dependencies.insert(dependency_name.to_string());
                }
                Some(kind) => bail!(
                    "package `{package_name}` dependency `{dependency_name}` has unsupported Cargo dependency kind `{kind}`"
                ),
            }
        }
        normal_build_dependencies.insert(package_name.clone(), package_normal_build_dependencies);
        dev_dependencies.insert(package_name.clone(), package_dev_dependencies);
    }

    Ok(PackageGraph {
        workspace_members,
        default_members,
        normal_build_dependencies,
        dev_dependencies,
    })
}

fn package_names_from_metadata_ids(
    metadata: &Value,
    field: &str,
    id_to_name: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>> {
    let ids = metadata
        .get(field)
        .and_then(Value::as_array)
        .with_context(|| format!("cargo metadata JSON missing `{field}` array"))?;
    ids.iter()
        .map(|value| {
            let id = value
                .as_str()
                .with_context(|| format!("cargo metadata `{field}` contains a non-string id"))?;
            id_to_name
                .get(id)
                .cloned()
                .with_context(|| format!("cargo metadata `{field}` references unknown id `{id}`"))
        })
        .collect()
}

fn value_string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("cargo metadata value missing string field `{field}`"))
}

pub(super) fn forbidden_dependency_findings(
    graph: &PackageGraph,
    rules: &[ForbiddenDependencyRule],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for kind in [DependencyKind::NormalBuild, DependencyKind::Dev] {
        for (source, dependencies) in graph.dependencies(kind) {
            for target in dependencies {
                for rule in rules {
                    if !rule.edge_kinds.contains(&kind)
                        || !rule.source.matches(source, graph)
                        || !rule.target.matches(target, graph)
                    {
                        continue;
                    }
                    findings.push(Finding::budget(
                        Rule::ForbiddenDependency,
                        "Cargo metadata",
                        format!(
                            "forbidden {kind} workspace dependency `{source} -> {target}`; reason: {}",
                            rule.reason
                        ),
                    ));
                    break;
                }
            }
        }
    }
    findings
}
