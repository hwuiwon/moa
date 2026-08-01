//! DB-backed coverage for artifact skill injection under configured-agent policy.

use std::sync::Arc;

use async_trait::async_trait;
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactRegistry, NewArtifactDraft, NewArtifactFile, NewSkillEmbedding,
};
use moa_brain::pipeline::skills::{SELECTED_SKILL_NAMES_METADATA_KEY, SkillInjector};
use moa_core::{
    error::Result, traits::ContextProcessor, traits::EmbeddingProvider,
    types::action_policy::ActionRuleScope, types::agent::AgentContext,
    types::agent::AgentPolicySnapshot, types::agent::AgentRevisionLock,
    types::agent::AgentSkillPolicy, types::agent::AgentSkillPolicyMode,
    types::agent::LockedToolRef, types::agent::ResolvedArtifactRevisionRef,
    types::contact::ContactId, types::contact::ContactRef,
    types::contact::ContactVerificationState, types::contact::SessionActorRef,
    types::context::ContextMessage, types::context::ProcessorOutput,
    types::context::WorkingContext, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::UserId, types::model::ModelCapabilities,
    types::session::SessionMeta,
};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn pinned_agent_skill_policy_injects_artifact_revision_files_db_memory() -> Result<()> {
    // Pins: SkillInjector loads exact artifact skill files selected by the session-pinned agent lock.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let storage_partition_id = StoragePartitionId::new(format!("workspace-{}", Uuid::now_v7()));
    let tenant_id = tenant_id_from_storage_partition_id(&storage_partition_id);
    let user_id = UserId::new("artifact-skill-user");
    let skill_name = format!("artifact-injected-skill-{}", Uuid::now_v7().simple());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let skill_revision =
        publish_skill_revision(&ArtifactRegistry::new(pool.clone()), &scope, &skill_name).await?;
    let agent_revision_uid = Uuid::now_v7();
    let mut ctx = WorkingContext::new(
        &SessionMeta {
            tenant_id,
            contact: Some(contact_ref(tenant_id, contact_id_from_user_id(&user_id))),
            created_by: Some(SessionActorRef::Contact {
                id: contact_id_from_user_id(&user_id),
            }),
            agent_context: Some(agent_context(
                &skill_name,
                skill_revision.artifact_uid,
                skill_revision.revision_uid,
                skill_revision.version,
                agent_revision_uid,
            )),
            ..SessionMeta::default()
        },
        ModelCapabilities::default(),
    );
    ctx.append_message(ContextMessage::user(
        "Use the configured artifact skill package.",
    ));

    let output = SkillInjector::new(pool).process(&mut ctx).await?;

    assert_eq!(output.items_included, vec![skill_name.clone()]);
    assert_eq!(
        ctx.metadata().get(SELECTED_SKILL_NAMES_METADATA_KEY),
        Some(&json!([skill_name.clone()]))
    );
    let files = ctx.take_trusted_sandbox_files();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].path,
        format!(".moa/skills/{}/SKILL.md", slugify_skill_name(&skill_name))
    );
    assert_eq!(
        String::from_utf8_lossy(&files[0].content),
        "# Artifact Skill\n"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn archived_rollback_skill_rejects_exact_pin_and_auto_manifest_db_memory() -> Result<()> {
    // Pins: exact pins may load a superseded activation, but rollback archives
    // the regressed revision and makes it terminal even though activation audit
    // history and the unserved pointer tombstone remain.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope = moa_artifacts::release::TenantScope::new(tenant_id);
    let name = format!("rolled-back-skill-{}", Uuid::now_v7().simple());

    let _ = publish_described_skill(&registry, &scope, &name, "first activation").await?;
    let (regressed, regressed_activation) =
        publish_described_skill(&registry, &scope, &name, "regressed").await?;
    let current = run_skill_injection(pool.clone(), tenant_id, &name, None).await?;
    assert_eq!(
        current.items_included,
        vec![name.clone()],
        "the non-pinned path should resolve the current activation before rollback"
    );
    let pointer = registry
        .load_serving_pointer(&release_scope, regressed.artifact_uid)
        .await?
        .expect("regressed skill has a serving pointer");
    let mut conn = moa_db::ScopedConn::begin_tenant(&pool, tenant_id).await?;
    let rollback = ArtifactRegistry::rollback_serving_revision_in_tx(
        conn.as_mut(),
        &release_scope,
        regressed.revision_uid,
        regressed_activation.audit_uid,
        pointer.pointer_version,
        "brain-test",
        Some("regression"),
    )
    .await?;
    conn.commit().await?;
    assert_eq!(
        rollback,
        moa_artifacts::registry::RollbackApplication::Applied
    );

    let archived = registry
        .load_revision(&scope, regressed.revision_uid)
        .await?
        .expect("rollback retains the revision for audit");
    assert_eq!(archived.status, ArtifactStatus::Archived);
    let mut pinned_ctx = WorkingContext::new(
        &SessionMeta {
            tenant_id,
            agent_context: Some(agent_context(
                &name,
                archived.artifact_uid,
                archived.revision_uid,
                archived.version,
                Uuid::now_v7(),
            )),
            ..SessionMeta::default()
        },
        ModelCapabilities::default(),
    );
    pinned_ctx.append_message(ContextMessage::user("use the rolled back exact skill"));
    let error = SkillInjector::new(pool.clone())
        .process(&mut pinned_ctx)
        .await
        .expect_err("an archived exact pin must not inject or materialize package files");
    match error {
        moa_core::error::MoaError::StorageError(message) => assert_eq!(
            message,
            "agent policy locked 1 skill revisions but 0 are executable"
        ),
        other => panic!("expected archived exact-pin storage failure, got {other:?}"),
    }

    let output = run_skill_injection(pool, tenant_id, "use the rolled back skill", None).await?;
    assert!(
        output.items_included.is_empty(),
        "an unserved pointer tombstone must not enter the normal skill manifest"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

async fn publish_skill_revision(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    skill_name: &str,
) -> Result<moa_artifacts::registry::StoredArtifactRevision> {
    let document = skill_document(skill_name);
    let source = document.to_yaml().expect("serialize skill fixture");
    let draft = registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[NewArtifactFile {
                    path: "SKILL.md".to_string(),
                    content: b"# Artifact Skill\n".to_vec(),
                    content_type: Some("text/markdown; charset=utf-8".to_string()),
                    executable: false,
                }],
            },
        )
        .await?;
    // Skill visibility is the serving pointer, so the fixture activates the draft
    // through the real release path and returns the now-serving revision.
    moa_artifacts::test_fixtures::activate_revision(
        registry.pool(),
        moa_artifacts::release::TenantScope::from_action_rule_scope(scope)
            .map_err(|error| moa_core::error::MoaError::ValidationError(error.to_string()))?,
        moa_artifacts::release::ActivationTarget::SkillVisibility {
            artifact_uid: draft.artifact_uid,
        },
        draft.revision_uid,
    )
    .await
    .map_err(|error| moa_core::error::MoaError::ValidationError(error.to_string()))?;
    registry
        .load_revision(scope, draft.revision_uid)
        .await?
        .ok_or_else(|| {
            moa_core::error::MoaError::StorageError("activated skill revision vanished".to_string())
        })
}

fn tenant_id_from_storage_partition_id(storage_partition_id: &StoragePartitionId) -> TenantId {
    Uuid::parse_str(storage_partition_id.as_str())
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id.as_str())))
}

fn contact_id_from_user_id(user_id: &UserId) -> ContactId {
    Uuid::parse_str(user_id.as_str())
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(user_id.as_str())))
}

use moa_test_support::fixtures::stable_uuid_from_label;

fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
    ContactRef {
        contact_id,
        tenant_id,
        state: ContactVerificationState::Verified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}

fn skill_document(skill_name: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": {
            "name": skill_name,
            "description": "Artifact skill injection fixture"
        },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": { "path": "SKILL.md" }
            }
        }
    }))
    .expect("skill fixture should be valid")
}

fn agent_context(
    skill_name: &str,
    artifact_uid: Uuid,
    revision_uid: Uuid,
    version: i32,
    agent_revision_uid: Uuid,
) -> AgentContext {
    let dependency = ResolvedArtifactRevisionRef {
        reference: format!("skill://{skill_name}"),
        kind: "skill".to_string(),
        name: skill_name.to_string(),
        artifact_uid,
        revision_uid,
        version,
    };
    let revision_lock = AgentRevisionLock {
        agent_revision_uid,
        artifact_dependencies: vec![dependency.clone()],
        tool_dependencies: vec![LockedToolRef {
            name: "file_read".to_string(),
            identity_hash: "file-read-schema".to_string(),
            provider: None,
        }],
        canonical_policy_hash: "artifact-skill-injection-policy".to_string(),
    };
    let snapshot = AgentPolicySnapshot {
        skill_policy: AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Pinned,
            refs: vec![format!("skill://{skill_name}")],
            max_visible: Some(1),
        },
        revision_lock: Some(revision_lock),
        ..AgentPolicySnapshot::default()
    };
    AgentContext {
        agent_id: None,
        installation_uid: None,
        deployment_uid: None,
        definition_ref: "agent://artifact-skill-injection".to_string(),
        revision_uid: agent_revision_uid,
        policy_hash: "artifact-skill-injection-policy".to_string(),
        display_name: "Artifact Skill Injection".to_string(),
        artifact_dependencies: vec![dependency],
        tool_dependencies: vec![LockedToolRef {
            name: "file_read".to_string(),
            identity_hash: "file-read-schema".to_string(),
            provider: None,
        }],
        policy_snapshot: serde_json::to_value(snapshot).expect("serialize policy snapshot"),
    }
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

/// Embedder identity shared by the fake probe embedder and the seeded rows.
const FIXED_MODEL: &str = "mock-embedding-1024";

/// Deterministic 1024-dim embedder that maps every input to the same probe
/// direction, so the seeded skill sharing that direction is the nearest neighbor.
struct FixedEmbedder;

#[async_trait]
impl EmbeddingProvider for FixedEmbedder {
    fn model_id(&self) -> &str {
        FIXED_MODEL
    }

    fn dimensions(&self) -> usize {
        1024
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(inputs.iter().map(|_| one_hot(0)).collect())
    }
}

fn one_hot(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0_f32; 1024];
    vector[index] = 1.0;
    vector
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn embedding_ranked_manifest_keeps_semantic_match_under_truncation_db_memory() -> Result<()> {
    // Pins: end to end, when the skill manifest must truncate, the SkillInjector
    // ranks a tenant's skills by embedding similarity to the turn query, so a
    // semantically-matching but lexically-divergent skill survives while a
    // lexically-overlapping but semantically-distant skill is dropped. The
    // no-embedder run is the built-in control: it flips the survivor to the
    // lexical keyword winner, so the embedding signal is load-bearing, not
    // incidental.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id };

    // The query keyword "refund" lexically hits the glossary skill's description
    // but not the reversal skill's; the seeded embeddings invert that ranking.
    let semantic_name = format!("charge-reversal-{}", Uuid::now_v7().simple());
    let lexical_name = format!("billing-glossary-{}", Uuid::now_v7().simple());
    let _ = publish_described_skill(&registry, &scope, &semantic_name, "Undo a completed charge")
        .await?;
    let _ = publish_described_skill(
        &registry,
        &scope,
        &lexical_name,
        "Refund terminology reference",
    )
    .await?;

    // Seed identity embeddings in the fake embedder's vector space: the reversal
    // skill shares the query probe's direction; the glossary skill is orthogonal.
    for row in registry
        .list_skills_missing_embedding(FIXED_MODEL, 1, 10)
        .await?
    {
        let embedding = if row.name == semantic_name {
            one_hot(0)
        } else {
            one_hot(1)
        };
        registry
            .set_skill_embedding(NewSkillEmbedding {
                artifact_uid: row.artifact_uid,
                revision_uid: row.revision_uid,
                storage_partition_id: row.storage_partition_id.as_str(),
                embedding: &embedding,
                model: FIXED_MODEL,
                model_version: 1,
                source_hash: row.name.as_bytes(),
                observed_artifact_updated_at: row.artifact_updated_at,
            })
            .await?;
    }

    let query = "process a refund for the buyer";

    // With the embedder and a one-skill visibility cap (forcing truncation), the
    // semantic match survives and the lexical distractor is excluded.
    let with_embedder = run_skill_injection(
        pool.clone(),
        tenant_id,
        query,
        Some(Arc::new(FixedEmbedder)),
    )
    .await?;
    assert_eq!(
        with_embedder.items_included,
        vec![semantic_name.clone()],
        "the semantically-matching skill survives truncation under embedding ranking",
    );
    assert_eq!(
        with_embedder.items_excluded,
        vec![lexical_name.clone()],
        "the lexically-overlapping but semantically-distant skill is dropped",
    );

    // Control: without an embedder the ranking is lexical, so the "refund" keyword
    // overlap makes the glossary skill the survivor instead — proving the
    // embedding signal, not some other factor, chose the survivor above.
    let without_embedder = run_skill_injection(pool.clone(), tenant_id, query, None).await?;
    assert_eq!(
        without_embedder.items_included,
        vec![lexical_name.clone()],
        "lexical ranking keeps the keyword-overlapping skill instead",
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

/// Runs the stage-5 skill injector for one turn and returns its processor output.
async fn run_skill_injection(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    query: &str,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
) -> Result<ProcessorOutput> {
    let mut ctx = WorkingContext::new(
        &SessionMeta {
            tenant_id,
            agent_context: Some(auto_policy_agent_context(1)),
            ..SessionMeta::default()
        },
        ModelCapabilities::default(),
    );
    ctx.append_message(ContextMessage::user(query));
    let mut injector = SkillInjector::new(pool);
    if let Some(embedder) = embedder {
        injector = injector.with_embedder(embedder);
    }
    injector.process(&mut ctx).await
}

/// Builds an Auto-policy agent context that caps the manifest at `max_visible`
/// skills, forcing truncation so relevance ranking selects the survivors.
fn auto_policy_agent_context(max_visible: u32) -> AgentContext {
    let snapshot = AgentPolicySnapshot {
        skill_policy: AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Auto,
            refs: Vec::new(),
            max_visible: Some(max_visible),
        },
        ..AgentPolicySnapshot::default()
    };
    AgentContext {
        agent_id: None,
        installation_uid: None,
        deployment_uid: None,
        definition_ref: "agent://embedding-rank-test".to_string(),
        revision_uid: Uuid::now_v7(),
        policy_hash: "embedding-rank-policy".to_string(),
        display_name: "Embedding Rank Test".to_string(),
        artifact_dependencies: Vec::new(),
        tool_dependencies: Vec::new(),
        policy_snapshot: serde_json::to_value(snapshot).expect("serialize policy snapshot"),
    }
}

/// Publishes one tenant-visible skill carrying the given description.
async fn publish_described_skill(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    name: &str,
    description: &str,
) -> Result<(
    moa_artifacts::registry::StoredArtifactRevision,
    moa_artifacts::test_fixtures::ActivatedRevision,
)> {
    let document: ArtifactDocument = serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": { "name": name, "description": description },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": { "path": "SKILL.md" }
            }
        }
    }))
    .expect("skill fixture should be valid");
    let source = document.to_yaml().expect("serialize skill fixture");
    let draft = registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[NewArtifactFile {
                    path: "SKILL.md".to_string(),
                    content: b"# Artifact Skill\n".to_vec(),
                    content_type: Some("text/markdown; charset=utf-8".to_string()),
                    executable: false,
                }],
            },
        )
        .await?;
    // Skill visibility is the serving pointer, so the fixture activates the draft
    // through the real release path and returns the now-serving revision.
    let activation = moa_artifacts::test_fixtures::activate_revision(
        registry.pool(),
        moa_artifacts::release::TenantScope::from_action_rule_scope(scope)
            .map_err(|error| moa_core::error::MoaError::ValidationError(error.to_string()))?,
        moa_artifacts::release::ActivationTarget::SkillVisibility {
            artifact_uid: draft.artifact_uid,
        },
        draft.revision_uid,
    )
    .await
    .map_err(|error| moa_core::error::MoaError::ValidationError(error.to_string()))?;
    let revision = registry
        .load_revision(scope, draft.revision_uid)
        .await?
        .ok_or_else(|| {
            moa_core::error::MoaError::StorageError("activated skill revision vanished".to_string())
        })?;
    Ok((revision, activation))
}
