//! Offline provider-account credential rotation and fail-closed validation.

use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_config::{
    CloudHandProviderAccountConfig, CloudHandProviderKind, CloudHandsConfig,
    ProviderSecretFileSelector,
};
use moa_core::types::identifiers::ProviderAccountId;
use moa_hands::{FileProviderCredentialSource, ProviderCredentialSource, ProviderEndpoint};
use moa_security::{OutboundHostResolutionError, OutboundHostResolver, OutboundHttpPolicy};
use tempfile::tempdir;

#[tokio::test]
async fn provider_credentials_rotate_without_restart_offline() {
    // Pins: a projected-file rotation changes the next account-fenced attempt
    // without rebuilding a provider, while debug output and stale generations
    // never expose or select credential material.
    let dir = tempdir().expect("credential tempdir");
    let path = dir.path().join("e2b");
    write_secret(&path, "first-secret-material");
    let owner_uid = std::fs::metadata(&path).expect("credential metadata").uid();
    let account_id = ProviderAccountId::new();
    let config = CloudHandsConfig {
        provider_accounts: vec![CloudHandProviderAccountConfig {
            provider_account_id: account_id,
            generation: 7,
            provider: CloudHandProviderKind::E2b,
            isolation_cell: "offline-cell".to_string(),
            api_origin: "https://api.e2b.test:18991".to_string(),
            toolbox_origin: None,
            sandbox_domain: Some("e2b.test".to_string()),
            default_runtime: Some("base".to_string()),
            project_fingerprint: None,
            credential: ProviderSecretFileSelector {
                path: path.clone(),
                owner_uid,
            },
        }],
        ..CloudHandsConfig::default()
    };
    let source = FileProviderCredentialSource::with_policy(
        &config,
        OutboundHttpPolicy::production(Arc::new(PublicResolver)),
    )
    .expect("credential source");
    source.validate_all().await.expect("initial validation");

    let first = source
        .resolve_attempt(
            account_id,
            7,
            CloudHandProviderKind::E2b,
            ProviderEndpoint::Api,
            Duration::from_secs(20),
        )
        .await
        .expect("first attempt");
    let first_fingerprint = first.credential_fingerprint();
    let first_debug = format!("{first:?}");
    assert!(!first_debug.contains("first-secret-material"));
    assert!(first_debug.contains("[REDACTED]"));

    let rotated = dir.path().join("e2b.rotated");
    write_secret(&rotated, "second-secret-material");
    std::fs::rename(&rotated, &path).expect("atomic secret rotation");
    let second = source
        .resolve_attempt(
            account_id,
            7,
            CloudHandProviderKind::E2b,
            ProviderEndpoint::Api,
            Duration::from_secs(20),
        )
        .await
        .expect("rotated attempt");
    assert_ne!(first_fingerprint, second.credential_fingerprint());
    assert!(!format!("{second:?}").contains("second-secret-material"));

    let stale = source
        .resolve_attempt(
            account_id,
            6,
            CloudHandProviderKind::E2b,
            ProviderEndpoint::Api,
            Duration::from_secs(20),
        )
        .await
        .expect_err("stale account generation must fail closed");
    assert!(!stale.to_string().contains(path.to_string_lossy().as_ref()));

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o440))
        .expect("make credential group-readable");
    source
        .validate_all()
        .await
        .expect_err("group-readable provider credential must fail startup validation");
}

#[test]
fn provider_account_mapping_rejects_non_https_and_subpath_origins_offline() {
    // Pins: operator account mappings fail before startup when a provider
    // origin could downgrade transport or smuggle an unreviewed URL path.
    let account_id = ProviderAccountId::new();
    let mut account = CloudHandProviderAccountConfig {
        provider_account_id: account_id,
        generation: 1,
        provider: CloudHandProviderKind::E2b,
        isolation_cell: "offline-cell".to_string(),
        api_origin: "http://api.e2b.dev".to_string(),
        toolbox_origin: None,
        sandbox_domain: Some("e2b.app".to_string()),
        default_runtime: Some("base".to_string()),
        project_fingerprint: None,
        credential: ProviderSecretFileSelector {
            path: "/run/secrets/moa/e2b".into(),
            owner_uid: 10001,
        },
    };
    let policy = || OutboundHttpPolicy::production(Arc::new(PublicResolver));
    let error = FileProviderCredentialSource::with_policy(
        &CloudHandsConfig {
            provider_accounts: vec![account.clone()],
            ..CloudHandsConfig::default()
        },
        policy(),
    )
    .expect_err("plain HTTP provider origin must fail before credential access");
    assert_eq!(
        error.to_string(),
        "configuration error: provider origin mapping is invalid"
    );

    account.api_origin = "https://api.e2b.dev/unreviewed".to_string();
    let error = FileProviderCredentialSource::with_policy(
        &CloudHandsConfig {
            provider_accounts: vec![account],
            ..CloudHandsConfig::default()
        },
        policy(),
    )
    .expect_err("provider subpath must fail before credential access");
    assert_eq!(
        error.to_string(),
        "configuration error: provider origin mapping is invalid"
    );
}

#[derive(Debug)]
struct PublicResolver;

#[async_trait]
impl OutboundHostResolver for PublicResolver {
    async fn resolve(
        &self,
        _host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, OutboundHostResolutionError> {
        Ok(vec![SocketAddr::from((Ipv4Addr::new(1, 1, 1, 1), port))])
    }
}

fn write_secret(path: &std::path::Path, value: &str) {
    std::fs::write(path, value).expect("write provider credential");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400))
        .expect("chmod provider credential");
}
