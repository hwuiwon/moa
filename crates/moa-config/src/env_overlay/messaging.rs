//! Messaging, session transport, cache, and orchestrator endpoint overlays.

use super::*;

pub(super) fn exact_overlay_path(field: &str) -> Option<Vec<String>> {
    let path = match field {
        "restate_ingress_url" => &["orchestrator", "restate_ingress_url"][..],
        "restate_llm_gateway_url" => &["orchestrator", "llm_gateway_url"],
        "session_attachment_bucket" => &["session", "attachments", "storage", "bucket"],
        "session_attachment_prefix" => &["session", "attachments", "storage", "prefix"],
        "sandbox_checkpoint_enabled" => &["sandbox_checkpoints", "enabled"],
        "sandbox_checkpoint_bucket" => &["sandbox_checkpoints", "storage", "bucket"],
        "sandbox_checkpoint_prefix" => &["sandbox_checkpoints", "storage", "prefix"],
        "sandbox_checkpoint_bucket_versioning" => &["sandbox_checkpoints", "bucket_versioning"],
        "sandbox_checkpoint_versioning_observation_maximum_age_seconds" => &[
            "sandbox_checkpoints",
            "versioning_observation",
            "maximum_age_seconds",
        ],
        "sandbox_checkpoint_versioning_observation_timeout_seconds" => &[
            "sandbox_checkpoints",
            "versioning_observation",
            "timeout_seconds",
        ],
        "sandbox_checkpoint_retained_ancestor_count" => &[
            "sandbox_checkpoints",
            "retention",
            "retained_ancestor_count",
        ],
        "sandbox_checkpoint_minimum_age_seconds" => {
            &["sandbox_checkpoints", "retention", "minimum_age_seconds"]
        }
        "sandbox_checkpoint_gc_batch_size" => {
            &["sandbox_checkpoints", "retention", "gc_batch_size"]
        }
        "sandbox_checkpoint_claim_ttl_seconds" => {
            &["sandbox_checkpoints", "retention", "claim_ttl_seconds"]
        }
        "sandbox_checkpoint_retry_backoff_seconds" => {
            &["sandbox_checkpoints", "retention", "retry_backoff_seconds"]
        }
        "sandbox_checkpoint_deletion_max_objects" => {
            &["sandbox_checkpoints", "deletion", "max_objects"]
        }
        "sandbox_checkpoint_deletion_max_bytes" => {
            &["sandbox_checkpoints", "deletion", "max_bytes"]
        }
        "sandbox_checkpoint_absence_window_seconds" => &[
            "sandbox_checkpoints",
            "deletion",
            "consistency_window_seconds",
        ],
        "sandbox_checkpoint_max_entries" => &["sandbox_checkpoints", "max_entries"],
        "sandbox_checkpoint_max_path_depth" => &["sandbox_checkpoints", "max_path_depth"],
        "sandbox_checkpoint_max_file_bytes" => &["sandbox_checkpoints", "max_file_bytes"],
        "sandbox_checkpoint_max_total_bytes" => &["sandbox_checkpoints", "max_total_bytes"],
        "sandbox_checkpoint_max_chunk_bytes" => &["sandbox_checkpoints", "max_chunk_bytes"],
        "sandbox_checkpoint_max_compressed_chunk_bytes" => {
            &["sandbox_checkpoints", "max_compressed_chunk_bytes"]
        }
        "sandbox_workspace_mode" => &["sandbox_workspaces", "mode"],
        "sandbox_workspace_operation_retention_seconds" => {
            &["sandbox_workspaces", "operation_retention_seconds"]
        }
        "sandbox_workspace_maximum_operation_seconds" => {
            &["sandbox_workspaces", "maximum_operation_seconds"]
        }
        "sandbox_workspace_reconciliation_claim_ttl_seconds" => {
            &["sandbox_workspaces", "reconciliation_claim_ttl_seconds"]
        }
        "sandbox_workspace_reaper_heartbeat_maximum_age_seconds" => {
            &["sandbox_workspaces", "reaper_heartbeat_maximum_age_seconds"]
        }
        _ => return None,
    };
    Some(strings(path))
}

pub(super) fn validate_urls(overlay: &EnvOverlay) -> Result<()> {
    validate_url("MOA_RESTATE_INGRESS_URL", &overlay.restate_ingress_url)?;
    validate_url(
        "MOA_RESTATE_LLM_GATEWAY_URL",
        &overlay.restate_llm_gateway_url,
    )?;
    validate_url("MOA_ORCHESTRATOR_ENDPOINT", &overlay.orchestrator_endpoint)?;
    validate_url(
        "MOA_RUNTIME_CACHE_REDIS_URL",
        &overlay.runtime_cache_redis_url,
    )?;
    validate_url("MOA_OBJECT_STORE_ENDPOINT", &overlay.object_store_endpoint)
}

impl EnvOverlay {
    pub(super) fn finalize_intentional_fanout(&self, config: &mut MoaConfig) {
        if let Some(restate_ingress_url) = &self.restate_ingress_url {
            config.orchestrator.endpoint = Some(restate_ingress_url.clone());
        }
        if let Some(endpoint) = &self.orchestrator_endpoint {
            config.orchestrator.endpoint = Some(endpoint.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_bool_reports_env_name() {
        // Pins: boolean parse failures name the canonical env var.
        assert_config_error_contains(
            EnvOverlay::from_iter(env_pairs([("MOA_LOCAL_DOCKER_ENABLED", "sometimes")])),
            "MOA_LOCAL_DOCKER_ENABLED",
        );
    }

    #[test]
    fn runtime_cache_overlay_applies_backend_and_redis_url() {
        // Pins: runtime cache selection uses flat MOA env names through envy.
        let overlay = EnvOverlay::from_iter(env_pairs([
            ("MOA_RUNTIME_CACHE_BACKEND", "redis"),
            (
                "MOA_RUNTIME_CACHE_REDIS_URL",
                "redis://cache.example:6379/0",
            ),
        ]))
        .expect("runtime cache overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("runtime cache overlay should apply");

        assert_eq!(config.runtime_cache.backend, RuntimeCacheBackend::Redis);
        assert_eq!(
            config.runtime_cache.redis_url.as_deref(),
            Some("redis://cache.example:6379/0")
        );
    }

    #[test]
    fn session_blob_overlay_applies_backend_and_local_path() {
        // Pins: claim-check blob storage is selected through explicit flat MOA env names.
        let overlay = EnvOverlay::from_iter(env_pairs([
            ("MOA_SESSION_BLOB_BACKEND", "local"),
            ("MOA_SESSION_BLOB_DIR", "/var/lib/moa/blobs"),
        ]))
        .expect("session blob overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("session blob overlay should apply");

        assert_eq!(config.session.blob_backend, SessionBlobBackend::Local);
        assert_eq!(
            config.session.blob_dir.as_deref(),
            Some("/var/lib/moa/blobs")
        );
    }

    #[test]
    fn session_attachment_overlay_applies_object_store_settings() {
        // Pins: session upload bytes use explicit object storage config rather than Postgres bytes.
        let overlay = EnvOverlay::from_iter(env_pairs([
            ("MOA_OBJECT_STORE_BACKEND", "gcs"),
            ("MOA_SESSION_ATTACHMENT_BUCKET", "moa-prod-attachments"),
            ("MOA_SESSION_ATTACHMENT_PREFIX", "prod/session-attachments"),
            (
                "MOA_OBJECT_STORE_GCP_APPLICATION_CREDENTIALS_PATH",
                "/var/run/secrets/gcp/application-default.json",
            ),
            ("MOA_OBJECT_STORE_ALLOW_HTTP", "false"),
        ]))
        .expect("session attachment overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("session attachment overlay should apply");

        assert_eq!(config.object_store.backend, ObjectStoreBackend::Gcs);
        assert_eq!(
            config.session.attachments.storage.bucket,
            "moa-prod-attachments"
        );
        assert_eq!(
            config.session.attachments.storage.prefix,
            "prod/session-attachments"
        );
        assert_eq!(
            config
                .object_store
                .gcp_application_credentials_path
                .as_deref(),
            Some("/var/run/secrets/gcp/application-default.json")
        );
        assert!(!config.object_store.allow_http);
    }

    #[test]
    fn sandbox_checkpoint_overlay_uses_shared_transport_and_separate_namespace() {
        // Pins: checkpoint bytes share one credential owner with attachments
        // while retaining a distinct bucket and opaque prefix.
        let overlay = EnvOverlay::from_iter(env_pairs([
            ("MOA_OBJECT_STORE_ENDPOINT", "http://rustfs:9000"),
            ("MOA_OBJECT_STORE_ALLOW_HTTP", "true"),
            ("MOA_SANDBOX_CHECKPOINT_BUCKET", "tenant-checkpoints"),
            ("MOA_SANDBOX_CHECKPOINT_PREFIX", "portable/v1"),
            (
                "MOA_SANDBOX_CHECKPOINT_BUCKET_VERSIONING",
                "unversioned_required",
            ),
            ("MOA_SANDBOX_CHECKPOINT_MAX_CHUNK_BYTES", "1048576"),
            ("MOA_SANDBOX_CHECKPOINT_RETAINED_ANCESTOR_COUNT", "5"),
            ("MOA_SANDBOX_CHECKPOINT_MINIMUM_AGE_SECONDS", "7200"),
            ("MOA_SANDBOX_CHECKPOINT_GC_BATCH_SIZE", "25"),
            ("MOA_SANDBOX_CHECKPOINT_CLAIM_TTL_SECONDS", "120"),
            ("MOA_SANDBOX_CHECKPOINT_RETRY_BACKOFF_SECONDS", "30"),
            ("MOA_SANDBOX_CHECKPOINT_DELETION_MAX_OBJECTS", "500"),
            ("MOA_SANDBOX_CHECKPOINT_DELETION_MAX_BYTES", "8589934592"),
            ("MOA_SANDBOX_CHECKPOINT_ABSENCE_WINDOW_SECONDS", "2"),
        ]))
        .expect("sandbox checkpoint overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("sandbox checkpoint overlay should apply");

        assert_eq!(
            config.object_store.endpoint.as_deref(),
            Some("http://rustfs:9000")
        );
        assert!(config.object_store.allow_http);
        assert_eq!(
            config.sandbox_checkpoints.storage.bucket,
            "tenant-checkpoints"
        );
        assert_eq!(config.sandbox_checkpoints.storage.prefix, "portable/v1");
        assert_eq!(config.sandbox_checkpoints.max_chunk_bytes, 1_048_576);
        assert_eq!(
            config.sandbox_checkpoints.bucket_versioning,
            crate::CheckpointBucketVersioningPolicy::UnversionedRequired
        );
        assert_eq!(
            config.sandbox_checkpoints.retention,
            crate::CheckpointRetentionConfig {
                retained_ancestor_count: 5,
                minimum_age_seconds: 7_200,
                gc_batch_size: 25,
                claim_ttl_seconds: 120,
                retry_backoff_seconds: 30,
            }
        );
        assert_eq!(config.sandbox_checkpoints.deletion.max_objects, 500);
        assert_eq!(config.sandbox_checkpoints.deletion.max_bytes, 8_589_934_592);
        assert_eq!(
            config
                .sandbox_checkpoints
                .deletion
                .consistency_window_seconds,
            2
        );
    }
}
