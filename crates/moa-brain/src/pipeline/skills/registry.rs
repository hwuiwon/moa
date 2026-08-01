//! Skill registry reads and database-row conversion.

use std::collections::{BTreeSet, HashMap};

use moa_artifacts::document::{ArtifactDefinition, ArtifactDocument};
use moa_core::{
    error::MoaError, error::Result, types::action_policy::CallOrigin,
    types::agent::AgentSkillPolicyMode, types::agent::ResolvedArtifactRevisionRef,
    types::context::WorkingContext, types::context::estimate_text_tokens,
    types::hands::SandboxFile, types::memory::SkillMetadata,
};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub(super) async fn load_skills(pool: &PgPool, ctx: &WorkingContext) -> Result<Vec<SkillMetadata>> {
    let skill_policy = ctx
        .agent_policy_snapshot()?
        .map(|snapshot| snapshot.skill_policy)
        .unwrap_or_default();
    let locked_skills = locked_skill_dependencies(ctx, &skill_policy.refs);
    if matches!(skill_policy.mode, AgentSkillPolicyMode::Allowlist) && !locked_skills.is_empty() {
        return load_locked_skills(pool, ctx, &locked_skills).await;
    }
    let mut skills = load_visible_skills(pool, ctx).await?;
    if matches!(skill_policy.mode, AgentSkillPolicyMode::Pinned) && !locked_skills.is_empty() {
        let locked = load_locked_skills(pool, ctx, &locked_skills).await?;
        for locked_skill in locked {
            skills.retain(|skill| skill.name != locked_skill.name);
            skills.push(locked_skill);
        }
        skills.sort_by(|left, right| left.name.cmp(&right.name));
    }
    Ok(skills)
}

async fn load_visible_skills(pool: &PgPool, ctx: &WorkingContext) -> Result<Vec<SkillMetadata>> {
    let tenant_id = ctx.tenant_id.to_string();
    let rows = sqlx::query(
        r#"
        -- Serving is the type-owned pointer, so the manifest offers exactly what
        -- the tenant serves. A newer or merely validated revision is invisible
        -- here until an audited learned-skill activation moves the pointer to it.
        SELECT a.name, a.description, a.tags, r.revision_uid, r.definition, r.source_text
        FROM moa.artifact_serving_pointer p
        JOIN moa.artifact a ON a.artifact_uid = p.artifact_uid
        JOIN moa.artifact_revision r ON r.revision_uid = p.revision_uid
        WHERE a.valid_to IS NULL
          AND r.valid_to IS NULL
          AND a.kind = 'skill'
          AND p.storage_partition_id = $1
          AND a.user_id IS NULL
        ORDER BY a.name ASC
        "#,
    )
    .bind(&tenant_id)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut skills = rows
        .into_iter()
        .map(skill_metadata_from_row)
        .collect::<Result<Vec<_>>>()?;
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(skills)
}

async fn load_locked_skills(
    pool: &PgPool,
    ctx: &WorkingContext,
    locked_skills: &[ResolvedArtifactRevisionRef],
) -> Result<Vec<SkillMetadata>> {
    let tenant_id = ctx.tenant_id.to_string();
    let revision_uids = locked_skills
        .iter()
        .map(|dependency| dependency.revision_uid)
        .collect::<Vec<_>>();
    let allow_unpublished = allows_unpublished_locked_skills(ctx);
    let rows = sqlx::query(
        r#"
        SELECT a.name, a.description, a.tags, r.revision_uid, r.definition, r.source_text
        FROM moa.artifact_revision r
        JOIN moa.artifact a ON a.artifact_uid = r.artifact_uid
        WHERE r.revision_uid = ANY($2::uuid[])
          AND a.valid_to IS NULL
          AND r.valid_to IS NULL
          AND a.kind = 'skill'
          AND a.storage_partition_id = $1
          AND a.user_id IS NULL
          AND (
              -- Eval-owned sessions may preview only the exact unpublished
              -- revisions already named by their durable agent lock.
              ($3::boolean AND r.status IN ('draft', 'evaluating'))
              OR (
                  -- Production and evaluation both require serving provenance
                  -- for activated or historical exact pins.
                  r.status IN ('ready', 'superseded')
                  AND (
                      EXISTS (
                          SELECT 1 FROM moa.artifact_serving_pointer p
                          WHERE p.revision_uid = r.revision_uid
                      )
                      OR EXISTS (
                          SELECT 1 FROM moa.artifact_activation_audit audit
                          WHERE audit.activated_revision_uid = r.revision_uid
                            AND audit.decision_kind = 'activation'
                      )
                  )
              )
          )
        ORDER BY array_position($2::uuid[], r.revision_uid), a.name ASC
        "#,
    )
    .bind(&tenant_id)
    .bind(&revision_uids)
    .bind(allow_unpublished)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    if rows.len() != revision_uids.len() {
        return Err(MoaError::StorageError(format!(
            "agent policy locked {} skill revisions but {} are executable",
            revision_uids.len(),
            rows.len()
        )));
    }

    rows.into_iter().map(skill_metadata_from_row).collect()
}

fn locked_skill_dependencies(
    ctx: &WorkingContext,
    policy_refs: &[String],
) -> Vec<ResolvedArtifactRevisionRef> {
    let mut dependencies = ctx
        .agent_context
        .as_ref()
        .map(|agent| {
            agent
                .artifact_dependencies
                .iter()
                .filter(|dependency| {
                    dependency.kind == "skill"
                        && policy_refs
                            .iter()
                            .any(|policy_ref| policy_ref == &dependency.reference)
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    dependencies.sort_by(|left, right| left.reference.cmp(&right.reference));
    dependencies
}

pub(super) async fn load_selected_skill_files(
    pool: &PgPool,
    ctx: &WorkingContext,
    selected: &[SkillMetadata],
) -> Result<Vec<SandboxFile>> {
    if selected.is_empty() {
        return Ok(Vec::new());
    }

    let tenant_id = ctx.tenant_id.to_string();
    let revision_uids = selected
        .iter()
        .map(|skill| {
            skill.artifact_revision_uid.ok_or_else(|| {
                MoaError::StorageError(format!(
                    "registry-selected skill `{}` has no exact artifact revision",
                    skill.name
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let base_paths = selected
        .iter()
        .map(|skill| {
            Ok((
                skill.name.clone(),
                skill_base_path(&skill.path)?.to_string(),
            ))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let allow_unpublished = allows_unpublished_locked_skills(ctx);
    let rows = sqlx::query(
        r#"
        WITH requested AS (
            SELECT revision_uid, ord
            FROM unnest($2::uuid[]) WITH ORDINALITY AS requested(revision_uid, ord)
        )
        SELECT r.revision_uid, a.name, f.path, f.content, f.executable
        FROM requested
        JOIN moa.artifact_revision r ON r.revision_uid = requested.revision_uid
        JOIN moa.artifact a ON a.artifact_uid = r.artifact_uid
        JOIN moa.artifact_file f ON f.revision_uid = r.revision_uid
        WHERE a.valid_to IS NULL
          AND r.valid_to IS NULL
          AND a.kind = 'skill'
          AND a.storage_partition_id = $1
          AND a.user_id IS NULL
          AND (
              -- The same origin/status fence used by metadata loading protects
              -- the package bytes materialized into the runtime sandbox.
              ($3::boolean AND r.status IN ('draft', 'evaluating'))
              OR (
                  r.status IN ('ready', 'superseded')
                  AND (
                      EXISTS (
                          SELECT 1 FROM moa.artifact_serving_pointer p
                          WHERE p.revision_uid = r.revision_uid
                      )
                      OR EXISTS (
                          SELECT 1 FROM moa.artifact_activation_audit audit
                          WHERE audit.activated_revision_uid = r.revision_uid
                            AND audit.decision_kind = 'activation'
                      )
                  )
              )
          )
        ORDER BY requested.ord ASC, f.path ASC
        "#,
    )
    .bind(&tenant_id)
    .bind(&revision_uids)
    .bind(allow_unpublished)
    .fetch_all(pool)
    .await
    .map_err(|error| MoaError::StorageError(error.to_string()))?;

    let expected_revision_uids = revision_uids.iter().copied().collect::<BTreeSet<_>>();
    let materialized_revision_uids = rows
        .iter()
        .map(|row| row.try_get("revision_uid").map_err(map_sqlx_error))
        .collect::<Result<BTreeSet<Uuid>>>()?;
    if materialized_revision_uids != expected_revision_uids {
        return Err(MoaError::StorageError(format!(
            "selected {} exact skill revisions but {} have executable package files",
            expected_revision_uids.len(),
            materialized_revision_uids.len()
        )));
    }

    let mut files = Vec::new();
    for row in rows {
        let name: String = row
            .try_get("name")
            .map_err(|error| MoaError::StorageError(error.to_string()))?;
        let Some(base_path) = base_paths.get(&name) else {
            continue;
        };
        let package_path: String = row
            .try_get("path")
            .map_err(|error| MoaError::StorageError(error.to_string()))?;
        files.push(SandboxFile {
            path: format!("{base_path}/{package_path}"),
            content: row
                .try_get("content")
                .map_err(|error| MoaError::StorageError(error.to_string()))?,
            executable: row
                .try_get("executable")
                .map_err(|error| MoaError::StorageError(error.to_string()))?,
        });
    }

    Ok(files)
}

fn skill_metadata_from_row(row: sqlx::postgres::PgRow) -> Result<SkillMetadata> {
    let name: String = row.try_get("name").map_err(map_sqlx_error)?;
    let description: String = row.try_get("description").map_err(map_sqlx_error)?;
    let tags: Vec<String> = row.try_get("tags").map_err(map_sqlx_error)?;
    let definition: Value = row.try_get("definition").map_err(map_sqlx_error)?;
    let source_text: Vec<u8> = row.try_get("source_text").map_err(map_sqlx_error)?;
    let source_text = String::from_utf8(source_text)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let revision_uid: Uuid = row.try_get("revision_uid").map_err(map_sqlx_error)?;
    let document: ArtifactDocument = serde_json::from_value(definition)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let ArtifactDefinition::Skill(skill) = document.definition else {
        return Err(MoaError::StorageError(format!(
            "artifact `{name}` is not a skill definition"
        )));
    };
    let instruction_path = if skill.instructions.path.trim().is_empty() {
        "SKILL.md"
    } else {
        skill.instructions.path.as_str()
    };
    let has_execution_plan = skill.execution_plan.is_some();
    let actions = skill
        .actions
        .into_iter()
        .map(|action| action.id)
        .collect::<Vec<_>>();
    Ok(SkillMetadata {
        artifact_revision_uid: Some(revision_uid),
        path: format!(
            ".moa/skills/{}/{}",
            slugify_skill_name(&name),
            instruction_path
        ),
        name,
        description,
        tags,
        allowed_tools: skill.allowed_tools,
        actions,
        has_execution_plan,
        estimated_tokens: estimate_text_tokens(&source_text).max(1),
    })
}

fn allows_unpublished_locked_skills(ctx: &WorkingContext) -> bool {
    matches!(ctx.call_origin, CallOrigin::Experiment { .. })
}

fn skill_base_path(skill_md_path: &str) -> Result<&str> {
    skill_md_path
        .rsplit_once('/')
        .map(|(base, _)| base)
        .ok_or_else(|| {
            MoaError::ValidationError(format!(
                "skill path `{skill_md_path}` must include a sandbox package path"
            ))
        })
}

fn slugify_skill_name(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_was_separator = false;

    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !previous_was_separator && !slug.is_empty() {
            slug.push('-');
            previous_was_separator = true;
        }
    }

    slug.trim_matches('-').to_string()
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    //! Direct database coverage for exact skill package loading fences.

    use moa_artifacts::{
        document::{ArtifactDocument, ArtifactStatus},
        registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile, StoredArtifactRevision},
        release::TenantScope,
    };
    use moa_core::types::{
        action_policy::{ActionRuleScope, CallOrigin},
        agent::{AgentContext, AgentSkillPolicyMode, ResolvedArtifactRevisionRef},
        identifiers::TenantId,
        model::ModelCapabilities,
        session::SessionMeta,
    };
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn experiment_origin_loads_only_exact_unpublished_locked_skills_db_memory() -> Result<()>
    {
        // Pins: an artifact-release trial can execute its exact draft/evaluating
        // skill lock, while the same revisions remain unavailable to production
        // and an experiment origin alone never exposes unrelated drafts.
        let (store, database_url, schema_name) =
            moa_session::testing::create_isolated_test_store().await?;
        let pool = store.pool().clone();
        let registry = ArtifactRegistry::new(pool.clone());
        let tenant_id = TenantId::from(Uuid::now_v7());
        let scope = ActionRuleScope::Tenant { tenant_id };
        let draft_name = format!("eval-draft-{}", Uuid::now_v7().simple());
        let evaluating_name = format!("eval-running-{}", Uuid::now_v7().simple());
        let unrelated_name = format!("eval-unlocked-{}", Uuid::now_v7().simple());
        let draft = create_skill_draft(&registry, &scope, &draft_name, b"# exact draft\n").await?;
        let evaluating =
            create_skill_draft(&registry, &scope, &evaluating_name, b"# exact evaluating\n")
                .await?;
        let _unrelated =
            create_skill_draft(&registry, &scope, &unrelated_name, b"# unrelated draft\n").await?;
        sqlx::query(
            "UPDATE moa.artifact_revision SET status = 'evaluating' WHERE revision_uid = $1",
        )
        .bind(evaluating.revision_uid)
        .execute(&pool)
        .await
        .map_err(map_sqlx_error)?;

        let agent_context =
            locked_agent_context(&[(&draft_name, &draft), (&evaluating_name, &evaluating)]);
        let experiment_ctx = context_for_origin(
            tenant_id,
            CallOrigin::Experiment {
                run_uid: Uuid::now_v7(),
                trial_uid: Some(Uuid::now_v7()),
            },
            agent_context.clone(),
        );
        let loaded = load_skills(&pool, &experiment_ctx).await?;
        let loaded_revisions = loaded
            .iter()
            .map(|skill| {
                (
                    skill.name.clone(),
                    skill
                        .artifact_revision_uid
                        .expect("registry metadata carries the exact revision"),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            loaded_revisions,
            vec![
                (draft_name.clone(), draft.revision_uid),
                (evaluating_name.clone(), evaluating.revision_uid),
            ]
        );
        assert!(
            loaded_revisions
                .iter()
                .all(|(name, _)| name != &unrelated_name),
            "an experiment origin must not discover an unlocked draft"
        );

        let files = load_selected_skill_files(&pool, &experiment_ctx, &loaded).await?;
        let files_by_path = files
            .into_iter()
            .map(|file| (file.path, file.content))
            .collect::<HashMap<_, _>>();
        assert_eq!(files_by_path.len(), 2);
        assert_eq!(
            files_by_path.get(&format!(".moa/skills/{draft_name}/SKILL.md")),
            Some(&b"# exact draft\n".to_vec())
        );
        assert_eq!(
            files_by_path.get(&format!(".moa/skills/{evaluating_name}/SKILL.md")),
            Some(&b"# exact evaluating\n".to_vec())
        );

        let unlocked_experiment_ctx = context_for_origin(
            tenant_id,
            CallOrigin::Experiment {
                run_uid: Uuid::now_v7(),
                trial_uid: Some(Uuid::now_v7()),
            },
            AgentContext::system_default(),
        );
        assert_eq!(
            load_skills(&pool, &unlocked_experiment_ctx).await?,
            Vec::new(),
            "experiment provenance alone must not expose any tenant draft"
        );

        let production_ctx = context_for_origin(tenant_id, CallOrigin::Production, agent_context);
        let error = load_skills(&pool, &production_ctx)
            .await
            .expect_err("production must reject exact unpublished skill locks");
        match error {
            MoaError::StorageError(message) => assert_eq!(
                message,
                "agent policy locked 2 skill revisions but 0 are executable"
            ),
            other => panic!("expected production metadata fence, got {other:?}"),
        }
        let error = load_selected_skill_files(&pool, &production_ctx, &loaded)
            .await
            .expect_err("production must reject unpublished package bytes too");
        match error {
            MoaError::StorageError(message) => assert_eq!(
                message,
                "selected 2 exact skill revisions but 0 have executable package files"
            ),
            other => panic!("expected production package fence, got {other:?}"),
        }

        moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
    }

    #[tokio::test]
    async fn exact_files_load_for_superseded_revision_and_reject_archived_revision_db_memory()
    -> Result<()> {
        // Pins: a rollback between metadata selection and file loading cannot
        // silently emit an included manifest with no executable package. Earlier
        // superseded activations remain valid exact pins.
        let (store, database_url, schema_name) =
            moa_session::testing::create_isolated_test_store().await?;
        let pool = store.pool().clone();
        let registry = ArtifactRegistry::new(pool.clone());
        let tenant_id = TenantId::from(Uuid::now_v7());
        let scope = ActionRuleScope::Tenant { tenant_id };
        let release_scope = TenantScope::new(tenant_id);
        let name = format!("exact-file-fence-{}", Uuid::now_v7().simple());

        let v1 = create_skill_draft(&registry, &scope, &name, b"# v1\n").await?;
        moa_artifacts::test_fixtures::activate_revision(
            &pool,
            release_scope,
            moa_artifacts::release::ActivationTarget::SkillVisibility {
                artifact_uid: v1.artifact_uid,
            },
            v1.revision_uid,
        )
        .await
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
        let v2 = create_skill_draft(&registry, &scope, &name, b"# v2\n").await?;
        let v2_activation = moa_artifacts::test_fixtures::activate_revision(
            &pool,
            release_scope,
            moa_artifacts::release::ActivationTarget::SkillVisibility {
                artifact_uid: v2.artifact_uid,
            },
            v2.revision_uid,
        )
        .await
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;

        let superseded = registry
            .load_revision(&scope, v1.revision_uid)
            .await?
            .expect("first activation remains available for exact pins");
        assert_eq!(superseded.status, ArtifactStatus::Superseded);
        let ctx = WorkingContext::new(
            &SessionMeta {
                tenant_id,
                ..SessionMeta::default()
            },
            ModelCapabilities::default(),
        );
        let v1_metadata = selected_metadata(&name, v1.revision_uid);
        let files = load_selected_skill_files(&pool, &ctx, &[v1_metadata]).await?;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, b"# v1\n");

        let mut conn = moa_db::ScopedConn::begin_tenant(&pool, tenant_id).await?;
        let rollback = ArtifactRegistry::rollback_serving_revision_in_tx(
            conn.as_mut(),
            &release_scope,
            v2.revision_uid,
            v2_activation.audit_uid,
            v2_activation.pointer_version,
            "exact-file-fence-test",
            Some("regression"),
        )
        .await?;
        conn.commit().await?;
        assert_eq!(
            rollback,
            moa_artifacts::registry::RollbackApplication::Applied
        );

        let archived = registry
            .load_revision(&scope, v2.revision_uid)
            .await?
            .expect("rollback keeps exact revision audit history");
        assert_eq!(archived.status, ArtifactStatus::Archived);
        let error = load_selected_skill_files(
            &pool,
            &ctx,
            &[selected_metadata(&name, archived.revision_uid)],
        )
        .await
        .expect_err("archived exact files must fail closed after metadata selection");
        match error {
            MoaError::StorageError(message) => assert_eq!(
                message,
                "selected 1 exact skill revisions but 0 have executable package files"
            ),
            other => panic!("expected archived exact-file storage failure, got {other:?}"),
        }

        moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
    }

    async fn create_skill_draft(
        registry: &ArtifactRegistry,
        scope: &ActionRuleScope,
        name: &str,
        skill_md: &[u8],
    ) -> Result<StoredArtifactRevision> {
        let document: ArtifactDocument = serde_json::from_value(json!({
            "api_version": "moa.artifact/v1",
            "kind": "skill",
            "metadata": {
                "name": name,
                "description": "Exact file fence fixture"
            },
            "definition": {
                "type": "skill",
                "spec": {
                    "instructions": { "path": "SKILL.md" }
                }
            }
        }))
        .expect("exact file fence skill fixture is valid");
        let source = document
            .to_yaml()
            .expect("serialize exact file fence skill fixture");
        registry
            .create_draft(
                scope,
                NewArtifactDraft {
                    document: &document,
                    source_format: "yaml",
                    source_text: source.as_bytes(),
                    files: &[NewArtifactFile {
                        path: "SKILL.md".to_string(),
                        content: skill_md.to_vec(),
                        content_type: Some("text/markdown; charset=utf-8".to_string()),
                        executable: false,
                    }],
                },
            )
            .await
    }

    fn locked_agent_context(revisions: &[(&str, &StoredArtifactRevision)]) -> AgentContext {
        let dependencies = revisions
            .iter()
            .map(|(name, revision)| ResolvedArtifactRevisionRef {
                reference: format!("skill://{name}"),
                kind: "skill".to_string(),
                name: (*name).to_string(),
                artifact_uid: revision.artifact_uid,
                revision_uid: revision.revision_uid,
                version: revision.version,
            })
            .collect::<Vec<_>>();
        let mut context = AgentContext::system_default();
        let mut snapshot = context
            .parsed_policy_snapshot()
            .expect("system default agent policy snapshot is valid");
        snapshot.skill_policy.mode = AgentSkillPolicyMode::Allowlist;
        snapshot.skill_policy.refs = dependencies
            .iter()
            .map(|dependency| dependency.reference.clone())
            .collect();
        snapshot
            .revision_lock
            .as_mut()
            .expect("system default agent carries a revision lock")
            .artifact_dependencies = dependencies.clone();
        context.artifact_dependencies = dependencies;
        context.policy_snapshot =
            serde_json::to_value(snapshot).expect("locked experiment policy snapshot serializes");
        context
    }

    fn context_for_origin(
        tenant_id: TenantId,
        call_origin: CallOrigin,
        agent_context: AgentContext,
    ) -> WorkingContext {
        WorkingContext::new(
            &SessionMeta {
                tenant_id,
                agent_context: Some(agent_context),
                call_origin,
                ..SessionMeta::default()
            },
            ModelCapabilities::default(),
        )
    }

    fn selected_metadata(name: &str, revision_uid: Uuid) -> SkillMetadata {
        SkillMetadata {
            artifact_revision_uid: Some(revision_uid),
            path: format!(".moa/skills/{name}/SKILL.md"),
            name: name.to_string(),
            description: "Exact file fence fixture".to_string(),
            tags: Vec::new(),
            allowed_tools: Vec::new(),
            actions: Vec::new(),
            has_execution_plan: false,
            estimated_tokens: 1,
        }
    }
}
