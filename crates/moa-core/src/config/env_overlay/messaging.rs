//! Messaging, session transport, cache, and orchestrator endpoint overlays.

use super::*;

pub(super) fn exact_overlay_path(field: &str) -> Option<Vec<String>> {
    let path = match field {
        "restate_ingress_url" => &["orchestrator", "restate_ingress_url"][..],
        "restate_admin_url" => &["orchestrator", "restate_admin_url"],
        "restate_llm_gateway_url" => &["orchestrator", "llm_gateway_url"],
        _ => return None,
    };
    Some(strings(path))
}

pub(super) fn validate_urls(overlay: &MoaEnvOverlay) -> Result<()> {
    validate_url("MOA_RESTATE_INGRESS_URL", &overlay.restate_ingress_url)?;
    validate_url("MOA_RESTATE_ADMIN_URL", &overlay.restate_admin_url)?;
    validate_url(
        "MOA_RESTATE_LLM_GATEWAY_URL",
        &overlay.restate_llm_gateway_url,
    )?;
    validate_url("MOA_ORCHESTRATOR_ENDPOINT", &overlay.orchestrator_endpoint)?;
    validate_url(
        "MOA_ORCHESTRATOR_HEALTH_URL",
        &overlay.orchestrator_health_url,
    )?;
    validate_url(
        "MOA_RUNTIME_CACHE_REDIS_URL",
        &overlay.runtime_cache_redis_url,
    )?;
    validate_url(
        "MOA_SESSION_ATTACHMENT_ENDPOINT",
        &overlay.session_attachment_endpoint,
    )
}

impl MoaEnvOverlay {
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
            MoaEnvOverlay::from_iter(env_pairs([("MOA_LOCAL_DOCKER_ENABLED", "sometimes")])),
            "MOA_LOCAL_DOCKER_ENABLED",
        );
    }

    #[test]
    fn runtime_cache_overlay_applies_backend_and_redis_url() {
        // Pins: runtime cache selection uses flat MOA env names through envy.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
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
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
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
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_SESSION_ATTACHMENT_BACKEND", "gcs"),
            ("MOA_SESSION_ATTACHMENT_BUCKET", "moa-prod-attachments"),
            ("MOA_SESSION_ATTACHMENT_PREFIX", "prod/session-attachments"),
            (
                "MOA_SESSION_ATTACHMENT_GCP_APPLICATION_CREDENTIALS_PATH",
                "/var/run/secrets/gcp/application-default.json",
            ),
            ("MOA_SESSION_ATTACHMENT_ALLOW_HTTP", "false"),
        ]))
        .expect("session attachment overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("session attachment overlay should apply");

        assert_eq!(
            config.session.attachments.backend,
            SessionAttachmentBackend::Gcs
        );
        assert_eq!(config.session.attachments.bucket, "moa-prod-attachments");
        assert_eq!(
            config.session.attachments.prefix,
            "prod/session-attachments"
        );
        assert_eq!(
            config
                .session
                .attachments
                .gcp_application_credentials_path
                .as_deref(),
            Some("/var/run/secrets/gcp/application-default.json")
        );
        assert!(!config.session.attachments.allow_http);
    }
}
