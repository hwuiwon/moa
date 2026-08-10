//! Docker/RustFS persistence scenario for portable sandbox workspaces.

include!("local_tools_support/sandbox_profile.rs");

use std::path::Path;
use std::sync::Arc;
use std::{panic::AssertUnwindSafe, panic::resume_unwind};

use chrono::{Duration, Utc};
use futures_util::FutureExt as _;
use moa_core::{
    error::MoaError,
    traits::{HandProvider, SandboxStorageProvider},
    types::identifiers::{
        ProviderAccountId, SandboxWorkspaceId, TenantId, WorkspaceCheckpointId,
        WorkspaceOperationId,
    },
    types::sandbox_workspace::{
        WorkspaceCheckpointPublishRequest, WorkspaceOperationKind, WorkspaceRestoreRequest,
        WorkspaceStorageOperation,
    },
};
use moa_hands::{
    LocalHandProvider,
    core::sandbox_workspace::checkpoint::{
        archive::ArchiveLimits,
        store::{
            CheckpointObjectStore, CheckpointPrefixObservation, CheckpointStoreContext,
            ObservedCheckpointBucketVersioning,
        },
        versioning::CheckpointBucketVersioningObserver,
    },
};
use moa_test_support::RustFsFixture;
use object_store::{
    ObjectStore,
    aws::{AmazonS3Builder, S3ConditionalPut},
};
use serde_json::json;
use tempfile::{TempDir, tempdir, tempdir_in};

fn docker_mountable_tempdir() -> TempDir {
    let macos_docker_tmp = Path::new("/private/tmp");
    if macos_docker_tmp.exists() {
        return tempdir_in(macos_docker_tmp).expect("create Docker-mountable temporary directory");
    }
    tempdir().expect("create Docker-mountable temporary directory")
}

fn rustfs_checkpoint_store(
    fixture: &RustFsFixture,
) -> (CheckpointObjectStore, CheckpointBucketVersioningObserver) {
    let mut config = moa_config::MoaConfig::default();
    fixture.apply_checkpoint_config(&mut config);
    CheckpointObjectStore::from_config_with_versioning_observer(
        &config,
        Arc::new(moa_crypto::LocalKmsProvider::new()),
    )
    .expect("build production RustFS checkpoint store and versioning observer")
}

fn checkpoint_context() -> CheckpointStoreContext {
    CheckpointStoreContext {
        tenant_id: TenantId::new(),
        workspace_id: SandboxWorkspaceId::new(),
        checkpoint_id: WorkspaceCheckpointId::new(),
        provider_account_id: ProviderAccountId::new(),
        provider_account_generation: 1,
    }
}

fn fixture_checkpoint_suffix(fixture: &RustFsFixture, resource_id: &str) -> String {
    resource_id
        .strip_prefix(fixture.prefix())
        .and_then(|suffix| suffix.strip_prefix('/'))
        .expect("checkpoint reference must remain beneath the fixture prefix")
        .to_string()
}

#[tokio::test]
#[ignore = "requires local Docker; uses only a disposable isolated RustFS testcontainer"]
async fn checkpoint_partial_manifest_docker() {
    // Pins: manifest-last publication is the only completeness authority; real
    // RustFS chunks without that manifest stay unpublished and are still deleted.
    let fixture = RustFsFixture::start()
        .await
        .expect("start isolated RustFS fixture");
    let result = AssertUnwindSafe(async {
        let (store, observer) = rustfs_checkpoint_store(&fixture);
        let observation = observer
            .observe_unversioned()
            .await
            .expect("authenticate unversioned RustFS bucket policy");
        assert_eq!(
            observation.state(),
            ObservedCheckpointBucketVersioning::Unversioned
        );
        let context = checkpoint_context();
        let storage = store.storage_reference(context);
        let root = fixture_checkpoint_suffix(&fixture, &storage.resource_id);
        fixture
            .put_probe(
                &format!("{root}/chunks/00000000.bin"),
                b"partial-ciphertext",
            )
            .await
            .expect("seed a real manifest-less RustFS chunk");

        assert!(
            store
                .inspect_publication(context, &storage)
                .await
                .expect("inspect incomplete RustFS checkpoint")
                .is_none(),
            "a manifest-less checkpoint must never become published"
        );
        assert!(
            !store
                .verify_reference(&storage)
                .await
                .expect("verify incomplete RustFS checkpoint"),
            "a partial prefix must not become a valid portable reference"
        );

        store
            .delete(context)
            .await
            .expect("delete manifest-less checkpoint objects through production cleanup");
        assert!(matches!(
            store
                .observe_absence(context, None, Utc::now())
                .await
                .expect("observe cleaned partial prefix"),
            CheckpointPrefixObservation::EmptyPending(_)
        ));
    })
    .catch_unwind()
    .await;
    let cleanup = fixture.cleanup_namespace().await;
    match result {
        Ok(()) => cleanup.expect("clean isolated RustFS partial-manifest namespace"),
        Err(panic) => {
            cleanup.expect("clean isolated RustFS partial-manifest namespace after panic");
            resume_unwind(panic);
        }
    }
}

#[tokio::test]
#[ignore = "requires local Docker; uses only a disposable isolated RustFS testcontainer"]
async fn checkpoint_prefix_absence_docker() {
    // Pins: production cleanup enumerates the exact real RustFS prefix, and one
    // empty read cannot prove absence until a second stable read crosses the window.
    let fixture = RustFsFixture::start()
        .await
        .expect("start isolated RustFS fixture");
    let result = AssertUnwindSafe(async {
        let (store, observer) = rustfs_checkpoint_store(&fixture);
        observer
            .observe_unversioned()
            .await
            .expect("authenticate unversioned RustFS bucket policy");
        let context = checkpoint_context();
        let storage = store.storage_reference(context);
        let root = fixture_checkpoint_suffix(&fixture, &storage.resource_id);
        for (suffix, bytes) in [
            ("chunks/00000000.bin", b"first".as_slice()),
            ("multipart/abandoned", b"second".as_slice()),
        ] {
            fixture
                .put_probe(&format!("{root}/{suffix}"), bytes)
                .await
                .expect("seed exact RustFS checkpoint prefix");
        }
        let present = store
            .observe_absence(context, None, Utc::now())
            .await
            .expect("inventory populated RustFS prefix");
        let CheckpointPrefixObservation::Present(inventory) = present else {
            panic!("non-empty RustFS prefix must report present");
        };
        assert_eq!(inventory.object_count, 2);
        assert_eq!(inventory.stored_bytes, 11);

        store
            .delete(context)
            .await
            .expect("delete exact RustFS checkpoint prefix");
        let first_at = Utc::now();
        let first = store
            .observe_absence(context, None, first_at)
            .await
            .expect("record first empty RustFS observation");
        let CheckpointPrefixObservation::EmptyPending(first) = first else {
            panic!("first empty RustFS observation must remain pending");
        };
        let early = store
            .observe_absence(
                context,
                Some(&first),
                first_at + Duration::milliseconds(500),
            )
            .await
            .expect("record too-early second RustFS observation");
        assert!(matches!(
            early,
            CheckpointPrefixObservation::EmptyPending(_)
        ));
        let confirmed = store
            .observe_absence(context, Some(&first), first_at + Duration::seconds(1))
            .await
            .expect("record separated second RustFS observation");
        let CheckpointPrefixObservation::Absent(proof) = confirmed else {
            panic!("two separated empty RustFS observations must prove absence");
        };
        assert_eq!(proof.first_observed_at, first_at);
        assert_eq!(proof.last_observed_at, first_at + Duration::seconds(1));
        assert_eq!(proof.inventory_digest, first.inventory_digest);
    })
    .catch_unwind()
    .await;
    let cleanup = fixture.cleanup_namespace().await;
    match result {
        Ok(()) => cleanup.expect("clean isolated RustFS prefix-absence namespace"),
        Err(panic) => {
            cleanup.expect("clean isolated RustFS prefix-absence namespace after panic");
            resume_unwind(panic);
        }
    }
}

#[tokio::test]
#[ignore = "requires local Docker; uses only a disposable isolated RustFS testcontainer"]
async fn checkpoint_versioning_policy_docker() {
    // Pins: a typed store built through production configuration stays closed
    // until the authenticated RustFS versioning API proves unversioned state,
    // and invalidating that observation immediately closes object mutation again.
    let fixture = RustFsFixture::start()
        .await
        .expect("start isolated RustFS fixture");
    let result = AssertUnwindSafe(async {
        let (store, observer) = rustfs_checkpoint_store(&fixture);
        assert!(!store.bucket_versioning_verified());
        assert!(matches!(
            store
                .preflight_create_only_namespace()
                .await
                .expect_err("unobserved versioning state must block checkpoint mutation"),
            MoaError::StorageError(_)
        ));

        let observation = observer
            .observe_unversioned()
            .await
            .expect("authenticate real RustFS bucket versioning state");
        assert_eq!(
            observation.state(),
            ObservedCheckpointBucketVersioning::Unversioned
        );
        assert!(observer.is_ready());
        assert!(store.bucket_versioning_verified());
        store
            .preflight_create_only_namespace()
            .await
            .expect("verified unversioned RustFS bucket must admit create-only preflight");

        observer.invalidate();
        assert!(!observer.is_ready());
        assert!(!store.bucket_versioning_verified());
        assert!(matches!(
            store
                .preflight_create_only_namespace()
                .await
                .expect_err("invalidated versioning evidence must fail closed"),
            MoaError::StorageError(_)
        ));
    })
    .catch_unwind()
    .await;
    let cleanup = fixture.cleanup_namespace().await;
    match result {
        Ok(()) => cleanup.expect("clean isolated RustFS versioning-policy namespace"),
        Err(panic) => {
            cleanup.expect("clean isolated RustFS versioning-policy namespace after panic");
            resume_unwind(panic);
        }
    }
}

#[tokio::test]
#[ignore = "requires Docker plus `docker compose up -d rustfs rustfs-init`"]
async fn docker_compute_replacement_restores_committed_workspace_from_rustfs() {
    // Pins: destroying local Docker compute cannot destroy the encrypted
    // portable checkpoint; a fresh instance restores exact committed bytes.
    let endpoint = std::env::var("MOA_OBJECT_STORE_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:9000".to_string());
    let bucket = std::env::var("MOA_SANDBOX_CHECKPOINT_BUCKET")
        .unwrap_or_else(|_| "moa-workspace-checkpoints".to_string());
    let access_key =
        std::env::var("MOA_OBJECT_STORE_ACCESS_KEY_ID").unwrap_or_else(|_| "moaadmin".to_string());
    let secret_key = std::env::var("MOA_OBJECT_STORE_SECRET_ACCESS_KEY")
        .unwrap_or_else(|_| "moa-local-dev-secret".to_string());
    let store: Arc<dyn ObjectStore> = Arc::new(
        AmazonS3Builder::new()
            .with_bucket_name(bucket)
            .with_region("us-east-1")
            .with_endpoint(endpoint)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret_key)
            .with_allow_http(true)
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .build()
            .expect("build authenticated RustFS client"),
    );
    let checkpoint_store = Arc::new(
        CheckpointObjectStore::new(
            store,
            Arc::new(moa_crypto::LocalKmsProvider::new()),
            "workspace-checkpoints",
            ArchiveLimits::default(),
            moa_hands::core::sandbox_workspace::checkpoint::store::ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("build checkpoint store"),
    );
    let directory = docker_mountable_tempdir();
    let provider = LocalHandProvider::new(directory.path())
        .await
        .expect("construct local Docker provider")
        .with_checkpoint_store(checkpoint_store);
    assert!(
        provider.docker_available(),
        "Docker persistence lane must fail loudly when Docker is unavailable"
    );
    let first_spec = hand_spec(moa_core::types::hands::SandboxTier::Container);
    let binding = first_spec.workspace.clone();
    let first = provider
        .provision(first_spec)
        .await
        .expect("provision first Docker compute");
    provider
        .execute(
            &first,
            "file_write",
            &json!({"path": "nested/marker.txt", "content": "durable across compute"}).to_string(),
        )
        .await
        .expect("write marker through the production file tool");
    let commit_operation = WorkspaceStorageOperation {
        operation_id: WorkspaceOperationId::new(),
        kind: WorkspaceOperationKind::Commit,
        binding: binding.clone(),
        deadline: Utc::now() + Duration::minutes(5),
        request_hash: "docker-rustfs-commit-v1".to_string(),
    };
    let committed = provider
        .publish_workspace_checkpoint(WorkspaceCheckpointPublishRequest {
            operation: commit_operation,
            hand: first.clone(),
            parent_revision: binding.current_revision.clone(),
        })
        .await
        .expect("commit encrypted portable checkpoint");
    let publication = committed
        .checkpoint_publication
        .expect("commit should return a verified checkpoint publication");
    let revision = publication.revision;
    let checkpoint = publication.storage;
    provider
        .destroy(&first)
        .await
        .expect("destroy first compute without touching RustFS");

    let mut second_spec = hand_spec(moa_core::types::hands::SandboxTier::Container);
    second_spec.workspace = moa_core::types::sandbox_workspace::WorkspaceBinding {
        instance_generation: binding.instance_generation + 1,
        current_revision: Some(revision.clone()),
        ..binding
    };
    let second = provider
        .provision(second_spec.clone())
        .await
        .expect("provision fresh Docker compute");
    provider
        .restore_workspace(WorkspaceRestoreRequest {
            operation: WorkspaceStorageOperation {
                operation_id: WorkspaceOperationId::new(),
                kind: WorkspaceOperationKind::Restore,
                binding: second_spec.workspace,
                deadline: Utc::now() + Duration::minutes(5),
                request_hash: "docker-rustfs-restore-v1".to_string(),
            },
            hand: second.clone(),
            revision,
            checkpoint,
        })
        .await
        .expect("restore verified checkpoint into fresh compute");
    let marker = provider
        .execute(
            &second,
            "file_read",
            &json!({"path": "nested/marker.txt"}).to_string(),
        )
        .await
        .expect("read restored marker through the production file tool");

    assert_eq!(marker.to_text(), "durable across compute");
    provider
        .destroy(&second)
        .await
        .expect("destroy second compute non-destructively");
}
