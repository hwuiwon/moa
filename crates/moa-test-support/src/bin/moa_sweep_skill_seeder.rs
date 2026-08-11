//! Seeds active skill artifacts into an isolated persona-sweep database.

use std::io::{self, BufReader};

use anyhow::{Context, Result, ensure};
use moa_artifacts::document::ArtifactDocument;
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::release::{ActivationTarget, TenantScope};
use moa_artifacts::test_fixtures::activate_revision;
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[derive(Deserialize)]
struct SeedRequest {
    tenant_id: Uuid,
    skills: Vec<SeedSkill>,
}

#[derive(Deserialize)]
struct SeedSkill {
    name: String,
    description: String,
    tags: Vec<String>,
    skill_markdown: String,
}

#[derive(Serialize)]
struct SeedResponse {
    count: usize,
    names: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .context("MOA_DATABASE_URL is required for sweep skill seeding")?;
    let request: SeedRequest = serde_json::from_reader(BufReader::new(io::stdin().lock()))
        .context("decode sweep skill seed request")?;
    ensure!(
        !request.skills.is_empty(),
        "sweep skill seed request is empty"
    );

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .context("connect to isolated sweep database")?;
    let tenant_id = TenantId::from(request.tenant_id);
    let scope = ActionRuleScope::Tenant { tenant_id };
    let tenant_scope = TenantScope::new(tenant_id);
    let registry = ArtifactRegistry::new(pool.clone());
    let mut names = Vec::with_capacity(request.skills.len());

    for skill in request.skills {
        ensure!(!skill.name.trim().is_empty(), "sweep skill name is empty");
        let document: ArtifactDocument = serde_json::from_value(serde_json::json!({
            "api_version": "moa.artifact/v1",
            "kind": "skill",
            "metadata": {
                "name": skill.name,
                "description": skill.description,
                "tags": skill.tags,
            },
            "definition": {
                "type": "skill",
                "spec": {
                    "instructions": { "path": "SKILL.md" }
                }
            }
        }))
        .context("construct sweep skill artifact document")?;
        let source = document
            .to_yaml()
            .context("serialize sweep skill artifact document")?;
        let files = [NewArtifactFile {
            path: "SKILL.md".to_string(),
            content: skill.skill_markdown.into_bytes(),
            content_type: Some("text/markdown; charset=utf-8".to_string()),
            executable: false,
        }];
        let draft = registry
            .create_draft(
                &scope,
                NewArtifactDraft {
                    document: &document,
                    source_format: "yaml",
                    source_text: source.as_bytes(),
                    files: &files,
                },
            )
            .await
            .with_context(|| format!("create sweep skill draft `{}`", document.metadata.name))?;
        activate_revision(
            &pool,
            tenant_scope,
            ActivationTarget::SkillVisibility {
                artifact_uid: draft.artifact_uid,
            },
            draft.revision_uid,
        )
        .await
        .with_context(|| format!("activate sweep skill `{}`", document.metadata.name))?;
        names.push(document.metadata.name);
    }

    serde_json::to_writer(
        io::stdout().lock(),
        &SeedResponse {
            count: names.len(),
            names,
        },
    )
    .context("write sweep skill seed response")?;
    Ok(())
}
