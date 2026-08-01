//! Integration tests for graph-backed skill registry behavior.

use moa_artifacts::document::ArtifactStatus;
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_core::{error::MoaError, error::Result, types::identifiers::TenantId};
use moa_skills::artifact::skill_artifact_document_from_package;
use moa_skills::package::{SkillPackage, SkillPackageFile, ValidatedSkillPackage};
use moa_skills::registry::SkillRegistry;
use uuid::Uuid;

use super::skill_graph::{
    DISTILLED_SKILL, GRAPH_TEST_LOCK, IMPROVED_SKILL, map_sqlx_error, purge_test_skill_name,
    serve_skill_package, tenant_scope,
};

#[tokio::test]
async fn registry_lists_skill_metadata() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    purge_test_skill_name(&store, "scope-skill").await?;
    let workspace_name = Uuid::now_v7().to_string();
    let scope = tenant_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());
    serve_skill_package(
        store.pool(),
        scope,
        SkillPackage::from_skill_markdown(DISTILLED_SKILL.to_string()),
    )
    .await?;
    let tenant_id = TenantId::from(Uuid::parse_str(&workspace_name).expect("fixture is a UUID"));
    let skills = registry.list_for_pipeline(tenant_id).await?;
    let package = registry
        .load_package_by_name(&tenant_scope(&workspace_name), "debug-oauth-refresh")
        .await?
        .expect("stored package exists");

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "debug-oauth-refresh");
    assert_eq!(skills[0].estimated_tokens, 900);
    assert_eq!(package.skill.file_count, 1);
    assert_eq!(package.files.len(), 1);
    assert_eq!(package.files[0].path, "SKILL.md");
    assert!(
        std::str::from_utf8(&package.files[0].content)
            .expect("stored SKILL.md is UTF-8")
            .contains("# Debug OAuth refresh")
    );
    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn registry_lists_latest_activated_skill_version() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    purge_test_skill_name(&store, "scope-skill").await?;
    let workspace_name = Uuid::now_v7().to_string();
    let scope = tenant_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());
    let first_uid = serve_skill_package(
        store.pool(),
        scope,
        SkillPackage::from_skill_markdown(DISTILLED_SKILL.to_string()),
    )
    .await?;
    let second_uid = serve_skill_package(
        store.pool(),
        scope,
        SkillPackage::from_skill_markdown(IMPROVED_SKILL.to_string()),
    )
    .await?;
    assert_ne!(first_uid, second_uid);

    let skills = registry
        .load_for_scope(&tenant_scope(&workspace_name))
        .await?;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].skill_uid, second_uid);
    assert_eq!(skills[0].version, 2);
    let artifact_registry = ArtifactRegistry::new(store.pool().clone());
    let serving = artifact_registry
        .load_serving(
            &tenant_scope(&workspace_name),
            moa_artifacts::document::ArtifactKind::Skill,
            "debug-oauth-refresh",
        )
        .await?
        .expect("the activated revision serves");
    assert_eq!(serving.status, ArtifactStatus::Ready);
    assert_eq!(serving.version, 2);
    assert_eq!(
        skill_artifact_revision_count(&store, &workspace_name, "debug-oauth-refresh").await?,
        2
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn registry_loads_the_activated_skill_artifact_without_duplicate_revision() -> Result<()> {
    // Pins: activation makes exactly one canonical skill artifact revision serve,
    // and loading it inserts nothing.
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = Uuid::now_v7().to_string();
    let scope = tenant_scope(&workspace_name);
    let skill_registry = SkillRegistry::new(store.pool().clone());
    let artifact_registry = ArtifactRegistry::new(store.pool().clone());
    let package = SkillPackage::from_skill_markdown(DISTILLED_SKILL.to_string()).validate()?;
    let document = skill_artifact_document_from_package(&package, ArtifactStatus::Draft)?;
    let source = document
        .to_yaml()
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let artifact_files = artifact_files_from_package(&package);
    let draft = artifact_registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &artifact_files,
            },
        )
        .await?;
    moa_artifacts::test_fixtures::activate_revision(
        store.pool(),
        moa_artifacts::release::TenantScope::from_action_rule_scope(&scope)
            .map_err(|error| MoaError::ValidationError(error.to_string()))?,
        moa_artifacts::release::ActivationTarget::SkillVisibility {
            artifact_uid: draft.artifact_uid,
        },
        draft.revision_uid,
    )
    .await
    .map_err(|error| MoaError::ValidationError(error.to_string()))?;

    let package = skill_registry
        .load_package_by_name(&scope, "debug-oauth-refresh")
        .await?
        .expect("the activated skill artifact package serves");

    assert_eq!(package.skill.skill_uid, draft.revision_uid);
    assert_eq!(package.skill.version, 1);
    assert_eq!(package.files.len(), 1);
    assert!(
        package
            .files
            .iter()
            .any(|file| file.path == "SKILL.md" && !file.executable)
    );
    assert_eq!(
        skill_artifact_revision_count(&store, &workspace_name, "debug-oauth-refresh").await?,
        1,
        "loading must not insert another artifact revision"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn registry_versions_when_supporting_file_changes() -> Result<()> {
    // Pins: changing only a supporting package file creates a new active skill version.
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = Uuid::now_v7().to_string();
    let scope = tenant_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());

    let first_uid = serve_skill_package(
        store.pool(),
        scope,
        package_with_script(b"printf first\n".to_vec()),
    )
    .await?;
    let second_uid = serve_skill_package(
        store.pool(),
        scope,
        package_with_script(b"printf second\n".to_vec()),
    )
    .await?;
    let package = registry
        .load_package_by_name(&scope, "debug-oauth-refresh")
        .await?
        .expect("stored package exists");

    assert_ne!(first_uid, second_uid);
    assert_eq!(package.skill.version, 2);
    assert!(
        package
            .files
            .iter()
            .any(|file| file.path == "SKILL.md" && !file.executable)
    );
    assert!(
        package
            .files
            .iter()
            .any(|file| file.path == "scripts/run.sh" && file.executable)
    );
    assert_eq!(
        package
            .files
            .iter()
            .find(|file| file.path == "scripts/run.sh")
            .expect("supporting script stored")
            .content,
        b"printf second\n".to_vec()
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

fn package_with_script(script: Vec<u8>) -> SkillPackage {
    SkillPackage::new(vec![
        SkillPackageFile::new("scripts/run.sh", script)
            .with_content_type("text/x-shellscript")
            .with_executable(true),
        SkillPackageFile::new("SKILL.md", DISTILLED_SKILL.as_bytes().to_vec())
            .with_content_type("text/markdown; charset=utf-8"),
    ])
}

fn artifact_files_from_package(package: &ValidatedSkillPackage) -> Vec<NewArtifactFile> {
    package
        .files
        .iter()
        .map(|file| NewArtifactFile {
            path: file.path.clone(),
            content: file.content.clone(),
            content_type: file.content_type.clone(),
            executable: file.executable,
        })
        .collect()
}

async fn skill_artifact_revision_count(
    store: &moa_session::PostgresSessionStore,
    storage_partition_id: &str,
    skill_name: &str,
) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) \
         FROM moa.artifact a \
         JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid \
         WHERE a.storage_partition_id = $1 AND a.kind = 'skill' AND a.name = $2",
    )
    .bind(storage_partition_id)
    .bind(skill_name)
    .fetch_one(store.pool())
    .await
    .map_err(map_sqlx_error)
}

#[tokio::test]
async fn manual_skill_draft_never_serves_db_memory() -> Result<()> {
    // Pins: storing a hand-authored skill draft changes nothing a session can
    // resolve. Only a learning candidate with regression evidence may move the
    // skill's serving pointer.
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = Uuid::now_v7().to_string();
    let scope = tenant_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());
    let tenant_id = TenantId::from(Uuid::parse_str(&workspace_name).expect("fixture is a UUID"));

    let revision_uid = super::skill_graph::draft_skill_package(
        store.pool(),
        scope,
        SkillPackage::from_skill_markdown(DISTILLED_SKILL.to_string()),
    )
    .await?
    .revision_uid;

    let artifact_registry = ArtifactRegistry::new(store.pool().clone());
    let stored = artifact_registry
        .load_revision(&scope, revision_uid)
        .await?
        .expect("generic artifact authoring stored a revision");
    assert_eq!(stored.status, ArtifactStatus::Draft);
    assert!(
        artifact_registry
            .load_serving(
                &scope,
                moa_artifacts::document::ArtifactKind::Skill,
                "debug-oauth-refresh",
            )
            .await?
            .is_none(),
        "a manually authored skill draft must not serve"
    );
    assert!(
        registry
            .load_package_by_name(&scope, "debug-oauth-refresh")
            .await?
            .is_none(),
        "skill resolution must not see a draft"
    );
    assert!(
        registry.list_skill_names(tenant_id).await?.is_empty(),
        "routing coverage must not count a draft"
    );
    assert!(
        registry.load_for_scope(&scope).await?.is_empty(),
        "the pipeline must not load a draft"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn ready_status_without_activation_history_never_loads_by_revision_db_memory() -> Result<()> {
    // Pins: status is revision history, not serving proof. Even a corrupted or
    // manually written `ready` row must not bypass the activation audit checked
    // by exact-revision loading.
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = Uuid::now_v7().to_string();
    let scope = tenant_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());
    let revision_uid = super::skill_graph::draft_skill_package(
        store.pool(),
        scope,
        SkillPackage::from_skill_markdown(DISTILLED_SKILL.to_string()),
    )
    .await?
    .revision_uid;

    let updated =
        sqlx::query("UPDATE moa.artifact_revision SET status = 'ready' WHERE revision_uid = $1")
            .bind(revision_uid)
            .execute(store.pool())
            .await
            .map_err(map_sqlx_error)?
            .rows_affected();
    assert_eq!(updated, 1, "fixture must write the untrusted ready status");

    let error = registry
        .load_skill_markdown(&scope, revision_uid)
        .await
        .expect_err("ready status without activation history must stay unloadable");
    match error {
        MoaError::StorageError(message) => {
            assert_eq!(message, format!("skill not found: {revision_uid}"));
        }
        other => panic!("expected exact-revision lookup failure, got {other:?}"),
    }

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}
