//! Skill import, export, listing, and bootstrap CLI commands.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use moa_core::{MemoryScope, MoaConfig, UserId, WorkspaceId};
use moa_session::create_session_store;
use moa_skills::{NewSkill, SkillRegistry, parse_skill_markdown, slugify_skill_name};
use tokio::fs;

/// Skill management CLI commands.
#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    /// Exports visible workspace skills to markdown files.
    Export(SkillsExportArgs),
    /// Imports markdown skill files into Postgres.
    Import(SkillsImportArgs),
    /// Lists visible skills for a workspace.
    List(SkillsListArgs),
    /// Imports repo-authored skills as global skills.
    #[command(name = "bootstrap_global")]
    BootstrapGlobal(SkillsBootstrapGlobalArgs),
}

/// Arguments for `moa skills export`.
#[derive(Debug, Args)]
pub struct SkillsExportArgs {
    /// Workspace id, or `.` for the current directory workspace.
    pub workspace: String,
    /// Output directory for `<skill-name>.md` files.
    #[arg(long)]
    pub to: PathBuf,
}

/// Arguments for `moa skills import`.
#[derive(Debug, Args)]
pub struct SkillsImportArgs {
    /// Workspace id, or `.` for the current directory workspace.
    pub workspace: String,
    /// Directory containing `<skill-name>.md` or `<skill-name>/SKILL.md` files.
    #[arg(long)]
    pub from: PathBuf,
    /// Scope tier to import into: `global`, `workspace`, or `user`.
    #[arg(long, default_value = "workspace")]
    pub scope: String,
    /// User id required when `--scope user` is selected.
    #[arg(long)]
    pub user: Option<String>,
}

/// Arguments for `moa skills list`.
#[derive(Debug, Args)]
pub struct SkillsListArgs {
    /// Workspace id, or `.` for the current directory workspace.
    pub workspace: String,
}

/// Arguments for `moa skills bootstrap_global`.
#[derive(Debug, Args)]
pub struct SkillsBootstrapGlobalArgs {
    /// Directory containing authored skill markdown.
    #[arg(long, default_value = "skills")]
    pub from: PathBuf,
}

/// Runs one skill CLI command and returns the rendered report.
pub async fn handle_skills_command(config: &MoaConfig, command: SkillsCommand) -> Result<String> {
    let store = create_session_store(config)
        .await
        .context("opening session store")?;
    let registry = SkillRegistry::new(store.pool().clone());

    match command {
        SkillsCommand::Export(args) => export_skills(&registry, args).await,
        SkillsCommand::Import(args) => import_skills(&registry, args).await,
        SkillsCommand::List(args) => list_skills(&registry, args).await,
        SkillsCommand::BootstrapGlobal(args) => bootstrap_global_skills(&registry, args).await,
    }
}

async fn export_skills(registry: &SkillRegistry, args: SkillsExportArgs) -> Result<String> {
    let workspace_id = resolve_workspace_arg(&args.workspace);
    fs::create_dir_all(&args.to)
        .await
        .with_context(|| format!("creating {}", args.to.display()))?;
    let scope = MemoryScope::Workspace { workspace_id };
    let skills = registry.load_for_scope(&scope).await?;

    for skill in &skills {
        let filename = format!("{}.md", slugify_skill_name(&skill.name));
        let path = args.to.join(filename);
        fs::write(&path, &skill.body)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
    }

    Ok(format!(
        "exported {} skills to {}\n",
        skills.len(),
        args.to.display()
    ))
}

async fn import_skills(registry: &SkillRegistry, args: SkillsImportArgs) -> Result<String> {
    let workspace_id = resolve_workspace_arg(&args.workspace);
    let scope = parse_scope(&args.scope, workspace_id, args.user.as_deref())?;
    let files = discover_skill_files(&args.from).await?;
    let mut imported = 0usize;

    for file in files {
        let markdown = fs::read_to_string(&file)
            .await
            .with_context(|| format!("reading {}", file.display()))?;
        let document = parse_skill_markdown(&markdown)
            .with_context(|| format!("parsing {}", file.display()))?;
        registry
            .upsert_by_name(NewSkill::from_document(scope.clone(), &document, markdown))
            .await
            .with_context(|| format!("importing {}", file.display()))?;
        imported += 1;
    }

    Ok(format!(
        "imported {imported} skills into {} scope\n",
        scope_label(&scope)
    ))
}

async fn list_skills(registry: &SkillRegistry, args: SkillsListArgs) -> Result<String> {
    let workspace_id = resolve_workspace_arg(&args.workspace);
    let scope = MemoryScope::Workspace { workspace_id };
    let skills = registry.load_for_scope(&scope).await?;
    if skills.is_empty() {
        return Ok("skills: none\n".to_string());
    }

    let mut output = String::from("scope\tversion\tname\tdescription\n");
    for skill in skills {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            skill.scope,
            skill.version,
            skill.name,
            skill.description.unwrap_or_default()
        ));
    }
    Ok(output)
}

async fn bootstrap_global_skills(
    registry: &SkillRegistry,
    args: SkillsBootstrapGlobalArgs,
) -> Result<String> {
    if !args.from.exists() {
        return Ok(format!(
            "global skill bootstrap skipped: {} does not exist\n",
            args.from.display()
        ));
    }

    let files = discover_skill_files(&args.from).await?;
    let mut imported = 0usize;
    for file in files {
        let markdown = fs::read_to_string(&file)
            .await
            .with_context(|| format!("reading {}", file.display()))?;
        let document = parse_skill_markdown(&markdown)
            .with_context(|| format!("parsing {}", file.display()))?;
        registry
            .upsert_by_name(NewSkill::from_document(
                MemoryScope::Global,
                &document,
                markdown,
            ))
            .await
            .with_context(|| format!("bootstrapping {}", file.display()))?;
        imported += 1;
    }

    Ok(format!("bootstrapped {imported} global skills\n"))
}

async fn discover_skill_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = fs::read_dir(dir)
        .await
        .with_context(|| format!("reading {}", dir.display()))?;
    let mut files = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_file() && is_skill_markdown_file(&path) {
            files.push(path);
        } else if file_type.is_dir() {
            let skill_path = path.join("SKILL.md");
            if skill_path.exists() {
                files.push(skill_path);
            }
        }
    }

    files.sort();
    Ok(files)
}

fn is_skill_markdown_file(path: &Path) -> bool {
    path.extension().and_then(|value| value.to_str()) == Some("md")
        && path.file_name().and_then(|value| value.to_str()) != Some("README.md")
}

fn parse_scope(scope: &str, workspace_id: WorkspaceId, user: Option<&str>) -> Result<MemoryScope> {
    match scope {
        "global" => Ok(MemoryScope::Global),
        "workspace" => Ok(MemoryScope::Workspace { workspace_id }),
        "user" => {
            let user_id = user
                .map(UserId::new)
                .ok_or_else(|| anyhow::anyhow!("--user is required with --scope user"))?;
            Ok(MemoryScope::User {
                workspace_id,
                user_id,
            })
        }
        value => bail!("invalid skill scope `{value}`; expected global, workspace, or user"),
    }
}

fn scope_label(scope: &MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Global => "global",
        MemoryScope::Workspace { .. } => "workspace",
        MemoryScope::User { .. } => "user",
    }
}

fn resolve_workspace_arg(value: &str) -> WorkspaceId {
    if value == "." {
        return current_workspace_id();
    }

    WorkspaceId::new(value)
}

fn current_workspace_id() -> WorkspaceId {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let name = cwd
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("default");
    WorkspaceId::new(name)
}

#[cfg(test)]
mod tests {
    use super::{SkillsExportArgs, SkillsImportArgs, export_skills, import_skills, list_skills};
    use moa_session::testing;
    use tempfile::tempdir;

    const SKILL: &str = r#"---
name: debug-oauth-refresh
description: "Investigate OAuth refresh bugs"
metadata:
  moa-one-liner: "Debug refresh-token failures"
  moa-tags: "oauth, auth"
  moa-estimated-tokens: "42"
---

# Debug OAuth refresh

Check token refresh state.
"#;

    #[tokio::test]
    async fn cli_export_import_round_trips_skill_body() {
        let (store, database_url, schema_name) = testing::create_isolated_test_store()
            .await
            .expect("isolated test store");
        let registry = moa_skills::SkillRegistry::new(store.pool().clone());
        let workspace = format!("workspace-skills-{}", uuid::Uuid::now_v7());
        let dir = tempdir().expect("temp dir");
        let import_dir = dir.path().join("import");
        let export_dir = dir.path().join("export");
        tokio::fs::create_dir_all(&import_dir)
            .await
            .expect("create import dir");
        tokio::fs::write(import_dir.join("debug-oauth-refresh.md"), SKILL)
            .await
            .expect("write skill");
        tokio::fs::write(import_dir.join("README.md"), "# Authoring notes\n")
            .await
            .expect("write README");

        let report = import_skills(
            &registry,
            SkillsImportArgs {
                workspace: workspace.clone(),
                from: import_dir,
                scope: "workspace".to_string(),
                user: None,
            },
        )
        .await
        .expect("import skills");
        assert!(report.contains("imported 1 skills"));

        let report = export_skills(
            &registry,
            SkillsExportArgs {
                workspace: workspace.clone(),
                to: export_dir.clone(),
            },
        )
        .await
        .expect("export skills");
        assert!(report.contains("exported 1 skills"));

        let exported = tokio::fs::read_to_string(export_dir.join("debug-oauth-refresh.md"))
            .await
            .expect("read exported skill");
        assert_eq!(exported, SKILL);

        let listed = list_skills(&registry, super::SkillsListArgs { workspace })
            .await
            .expect("list skills");
        assert!(listed.contains("debug-oauth-refresh"));

        testing::cleanup_test_schema(&database_url, &schema_name)
            .await
            .expect("cleanup schema");
    }
}
