use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use moa_config::CloudHandProviderKind;
use moa_core::{
    error::MoaError,
    traits::{HandProvider, SandboxStorageProvider},
    types::{
        hands::{EgressPolicy, HandHandle, SandboxTier},
        identifiers::{HandProvisioningOperationId, WorkspaceCheckpointId, WorkspaceOperationId},
        sandbox_workspace::{
            WorkspaceCheckpointPublishRequest, WorkspaceOperationKind, WorkspaceRevisionRef,
            WorkspaceStorageOperation,
        },
    },
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use super::{E2B_PROVISIONING_OPERATION_METADATA_KEY, E2BHandProvider};

struct FixtureE2BApi {
    addr: std::net::SocketAddr,
    create_requests: mpsc::UnboundedReceiver<Value>,
    discovery_requests: mpsc::UnboundedReceiver<String>,
}

impl FixtureE2BApi {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture E2B API should bind an ephemeral port");
        let addr = listener
            .local_addr()
            .expect("fixture E2B API should expose its local address");
        let (create_request_tx, create_requests) = mpsc::unbounded_channel();
        let (discovery_request_tx, discovery_requests) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut created_metadata = None;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = vec![0_u8; 8192];
                let bytes = socket
                    .read(&mut buffer)
                    .await
                    .expect("fixture E2B API should read a request");
                let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                let first_line = request.lines().next().unwrap_or_default();
                let (status, content_type, body) = fixture_response(
                    first_line,
                    &request,
                    &mut created_metadata,
                    &create_request_tx,
                    &discovery_request_tx,
                );
                let headers = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    body.len(),
                );
                socket
                    .write_all(headers.as_bytes())
                    .await
                    .expect("fixture E2B API should write response headers");
                socket
                    .write_all(body.as_bytes())
                    .await
                    .expect("fixture E2B API should write the response body");
            }
        });

        Self {
            addr,
            create_requests,
            discovery_requests,
        }
    }

    fn api_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn next_create_request(&mut self) -> Value {
        self.create_requests
            .recv()
            .await
            .expect("mock server should receive create sandbox request")
    }

    async fn next_discovery_request(&mut self) -> String {
        self.discovery_requests
            .recv()
            .await
            .expect("mock server should receive operation discovery request")
    }
}

fn fixture_response(
    first_line: &str,
    request: &str,
    created_metadata: &mut Option<Value>,
    create_request_tx: &mpsc::UnboundedSender<Value>,
    discovery_request_tx: &mpsc::UnboundedSender<String>,
) -> (&'static str, &'static str, String) {
    if first_line.starts_with("GET /v2/sandboxes?") {
        let target = first_line
            .split_whitespace()
            .nth(1)
            .expect("E2B discovery request should have a target")
            .to_string();
        discovery_request_tx
            .send(target)
            .expect("E2B discovery request receiver should remain available");
        let body = created_metadata.as_ref().map_or_else(
            || "[]".to_string(),
            |metadata| {
                serde_json::json!([{
                    "sandboxID": "sbx-123",
                    "metadata": metadata,
                }])
                .to_string()
            },
        );
        return ("200 OK", "application/json", body);
    }
    if first_line.starts_with("POST /sandboxes ") {
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("E2B create request should contain a body");
        let create_request: Value =
            serde_json::from_str(body).expect("E2B create request should be valid JSON");
        *created_metadata = create_request.get("metadata").cloned();
        create_request_tx
            .send(create_request)
            .expect("E2B create request receiver should remain available");
        return (
            "200 OK",
            "application/json",
            r#"{"sandboxID":"sbx-123","domain":"example.e2b.test","envdAccessToken":"envd-token","envdVersion":"0.1.1"}"#.to_string(),
        );
    }
    if first_line.starts_with("POST /sandboxes/sbx-123/connect ") {
        return (
            "409 Conflict",
            "application/json",
            r#"{"error":"connect must not run after GET returns a running access token"}"#
                .to_string(),
        );
    }
    if first_line.starts_with("POST /process.Process/Start ") {
        let stdout = if request.contains("echo hello") {
            "aGVsbG8K"
        } else {
            ""
        };
        return (
            "200 OK",
            "application/connect+json",
            encode_test_envelopes(&[
                serde_json::json!({"event":{"start":{"pid": 12}}}),
                serde_json::json!({"event":{"data":{"stdout":stdout}}}),
                serde_json::json!({"event":{"end":{"exited":true,"status":"exit status 0"}}}),
                serde_json::json!({}),
            ]),
        );
    }
    if first_line.starts_with("POST /filesystem.Filesystem/ListDir ") {
        return (
            "200 OK",
            "application/json",
            serde_json::json!({
                "entries": [{
                    "name": "marker.txt",
                    "path": "/workspace/marker.txt",
                    "type": "file",
                    "size": "6",
                    "mode": 0o644,
                    "permissions": "-rw-r--r--",
                    "owner": "user",
                    "group": "user",
                    "modifiedTime": "2026-08-09T00:00:00Z"
                }]
            })
            .to_string(),
        );
    }
    if first_line.starts_with("GET /files?") {
        return ("200 OK", "application/octet-stream", "marker".to_string());
    }
    if first_line.starts_with("DELETE /sandboxes/sbx-123 ") {
        *created_metadata = None;
        return ("204 No Content", "application/json", String::new());
    }
    if first_line.starts_with("GET /sandboxes/sbx-123 ") {
        return created_metadata.as_ref().map_or_else(
            || {
                (
                    "404 Not Found",
                    "application/json",
                    r#"{"error":"not found"}"#.to_string(),
                )
            },
            |metadata| {
                (
                    "200 OK",
                    "application/json",
                    serde_json::json!({
                        "sandboxID": "sbx-123",
                        "state": "running",
                        "metadata": metadata,
                        "domain": "example.e2b.test",
                        "envdAccessToken": "envd-token",
                        "envdVersion": "0.5.0",
                    })
                    .to_string(),
                )
            },
        );
    }
    (
        "404 Not Found",
        "application/json",
        r#"{"error":"unexpected"}"#.to_string(),
    )
}

fn assert_discovery_request(target: &str, operation_id: HandProvisioningOperationId) {
    let url = reqwest::Url::parse(&format!("http://e2b.test{target}"))
        .expect("fixture should capture a valid E2B discovery target");
    assert_eq!(url.path(), "/v2/sandboxes");
    assert_eq!(
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>(),
        vec![
            (
                "metadata".to_string(),
                format!("{E2B_PROVISIONING_OPERATION_METADATA_KEY}={operation_id}"),
            ),
            ("limit".to_string(), "100".to_string()),
            ("state".to_string(), "running,paused".to_string()),
        ]
    );
}

#[tokio::test]
async fn provisions_executes_and_destroys_sandbox() {
    let mut fixture = FixtureE2BApi::start().await;

    let provider = E2BHandProvider::new(Arc::new(
        crate::core::provider_credentials::TestProviderCredentialSource::new(
            CloudHandProviderKind::E2b,
            fixture.api_url(),
            None,
            Some("example.e2b.test".to_string()),
            Some("base".to_string()),
            "test-key",
        ),
    ))
    .with_sandbox_base_url(fixture.api_url());
    let spec = crate::core::profile::test_support::hand_spec(
        SandboxTier::MicroVM,
        e2b_test_profile(EgressPolicy::DenyAll),
    );
    let operation_id = spec.provisioning_operation_id;
    let account_id = spec.workspace.provider_account_id;
    let account_generation = spec.workspace.provider_account_generation;
    let binding = spec.workspace.clone();
    let handle = provider.provision(spec.clone()).await.unwrap();
    assert_discovery_request(&fixture.next_discovery_request().await, operation_id);

    let mut stale_spec = spec;
    stale_spec.workspace.instance_generation += 1;
    let error = provider
        .provision(stale_spec)
        .await
        .expect_err("changed workspace binding must invalidate E2B reuse");
    assert_discovery_request(&fixture.next_discovery_request().await, operation_id);
    assert!(
        matches!(error, MoaError::ProviderError(ref message) if message.contains("different creation spec")),
        "unexpected reuse error: {error}"
    );
    let create_request = fixture.next_create_request().await;
    // Pins: the effective profile's egress mode — not a provider-level flag —
    // decides E2B's internet switch, and the profile's maximum lifetime becomes
    // E2B's `timeout`.
    assert_eq!(
        create_request
            .get("allow_internet_access")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        create_request.get("timeout").and_then(Value::as_u64),
        Some(300)
    );
    assert_eq!(
        create_request.get("autoPause").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        create_request
            .pointer("/autoResume/enabled")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert!(
        create_request.get("volumeMounts").is_none(),
        "E2B volumes must not enter production sandbox creation"
    );
    let metadata = create_request
        .get("metadata")
        .and_then(Value::as_object)
        .expect("create request should carry workspace metadata");
    assert_eq!(
        metadata
            .get(super::E2B_TENANT_METADATA_KEY)
            .and_then(Value::as_str)
            .map(str::to_string),
        Some(binding.tenant_id.to_string())
    );
    assert_eq!(
        metadata
            .get(super::E2B_WORKSPACE_METADATA_KEY)
            .and_then(Value::as_str)
            .map(str::to_string),
        Some(binding.workspace_id.to_string())
    );
    assert_eq!(
        provider
            .provisioned_hands(account_id, account_generation, operation_id)
            .await
            .unwrap(),
        vec![handle.clone()]
    );
    assert_discovery_request(&fixture.next_discovery_request().await, operation_id);

    // Pins: direct provider calls use the shared BashToolInput validator, so an
    // oversized timeout cannot bypass the router and reach E2B.
    let error = provider
        .execute(
            &handle,
            "bash",
            r#"{"cmd":"echo bypass","timeout_secs":301}"#,
        )
        .await
        .expect_err("an out-of-policy timeout must be rejected before dispatch");
    assert!(
        matches!(&error, MoaError::ValidationError(message) if message.contains("301") && message.contains("300")),
        "unexpected validation error: {error}"
    );

    let output = provider
        .execute(&handle, "bash", r#"{"cmd":"echo hello"}"#)
        .await
        .unwrap();
    assert_eq!(output.process_stdout(), Some("hello\n"));

    // Pins: E2B pause/resume retain process memory and cannot be selected by
    // the durable filesystem path.
    assert!(matches!(
        provider.pause(&handle).await,
        Err(MoaError::Unsupported(_))
    ));
    assert!(matches!(
        provider.resume(&handle).await,
        Err(MoaError::Unsupported(_))
    ));
    provider.destroy(&handle).await.unwrap();
}

#[tokio::test]
async fn e2b_rejects_a_mutable_root_outside_the_checkpoint_boundary() {
    // Pins: E2B export/import is hard-bound to the standard mutable root, so a
    // caller cannot redirect checkpoint traversal into trusted or runtime state.
    let provider = E2BHandProvider::new(Arc::new(
        crate::core::provider_credentials::TestProviderCredentialSource::new(
            CloudHandProviderKind::E2b,
            "http://127.0.0.1:1",
            None,
            Some("example.e2b.test".to_string()),
            Some("base".to_string()),
            "test-key",
        ),
    ));
    let mut spec = crate::core::profile::test_support::hand_spec(
        SandboxTier::MicroVM,
        e2b_test_profile(EgressPolicy::DenyAll),
    );
    spec.filesystem.mutable_root = PathBuf::from("/opt/moa/trusted");

    let error = provider
        .provision(spec)
        .await
        .expect_err("nonstandard E2B mutable root must fail before provider I/O");

    assert!(
        matches!(error, MoaError::ValidationError(message) if message.contains("mutable root"))
    );
}

#[tokio::test]
async fn e2b_commit_rejects_a_parent_at_generation_zero_before_provider_io() {
    // Pins: invalid initial-parent state is rejected before credentials,
    // sandbox inspection, checkpoint export, or object publication.
    let mut binding = crate::core::profile::test_support::hand_spec(
        SandboxTier::MicroVM,
        e2b_test_profile(EgressPolicy::DenyAll),
    )
    .workspace;
    binding.current_revision = None;
    let parent = WorkspaceRevisionRef {
        checkpoint_id: WorkspaceCheckpointId::new(),
        generation: 1,
        format_version: 1,
    };
    let hand = HandHandle::e2b(
        "unreached",
        binding.provider_account_id,
        binding.provider_account_generation,
    );
    let operation = WorkspaceStorageOperation {
        operation_id: WorkspaceOperationId::new(),
        kind: WorkspaceOperationKind::Commit,
        binding,
        deadline: chrono::Utc::now() + chrono::Duration::minutes(1),
        request_hash: "a".repeat(64),
    };
    let provider = E2BHandProvider::new(Arc::new(
        crate::core::provider_credentials::TestProviderCredentialSource::new(
            CloudHandProviderKind::E2b,
            "http://127.0.0.1:1",
            None,
            Some("example.e2b.test".to_string()),
            Some("base".to_string()),
            "test-key",
        ),
    ));

    let error = provider
        .publish_workspace_checkpoint(WorkspaceCheckpointPublishRequest {
            operation,
            hand,
            parent_revision: Some(parent),
            release_compute: false,
        })
        .await
        .expect_err("generation-zero parent must fail before provider I/O");

    assert!(matches!(error, MoaError::ValidationError(message) if message.contains("parent")));
}

#[tokio::test]
async fn e2b_egress_posture_comes_from_the_effective_profile() {
    let mut fixture = FixtureE2BApi::start().await;

    let provider = E2BHandProvider::new(Arc::new(
        crate::core::provider_credentials::TestProviderCredentialSource::new(
            CloudHandProviderKind::E2b,
            fixture.api_url(),
            None,
            Some("example.e2b.test".to_string()),
            Some("base".to_string()),
            "test-key",
        ),
    ));

    // An egress allowlist has no E2B field that enforces it, so it is refused
    // before any sandbox is created rather than degraded into unrestricted.
    let allow_list = provider
        .provision(crate::core::profile::test_support::hand_spec(
            SandboxTier::MicroVM,
            e2b_test_profile(EgressPolicy::allow_list(["a.example.com"]).expect("allowlist")),
        ))
        .await;
    assert!(
        matches!(allow_list, Err(moa_core::error::MoaError::Unsupported(_))),
        "E2B must refuse an egress allowlist instead of serializing it away"
    );
    assert!(
        fixture.create_requests.try_recv().is_err(),
        "a refused profile must not reach the E2B create call"
    );
    assert!(
        fixture.discovery_requests.try_recv().is_err(),
        "a refused profile must not reach E2B operation discovery"
    );

    let spec = crate::core::profile::test_support::hand_spec(
        SandboxTier::MicroVM,
        e2b_test_profile(EgressPolicy::Unrestricted),
    );
    let operation_id = spec.provisioning_operation_id;
    let account_id = spec.workspace.provider_account_id;
    let account_generation = spec.workspace.provider_account_generation;
    let handle = provider.provision(spec).await.unwrap();

    assert_discovery_request(&fixture.next_discovery_request().await, operation_id);
    let create_request = fixture.next_create_request().await;
    assert_eq!(
        create_request
            .get("allow_internet_access")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        provider
            .provisioned_hands(account_id, account_generation, operation_id)
            .await
            .unwrap(),
        vec![handle]
    );
    assert_discovery_request(&fixture.next_discovery_request().await, operation_id);
}

// Pins: E2B checkpoint export streams only the reserved data root through a
// permission-restricted operation temp directory and Task 6's canonical
// archive builder; successful cleanup leaves no plaintext operation root.
#[tokio::test]
async fn exports_reserved_data_root_through_canonical_archive_and_wipes_temp() {
    let mut fixture = FixtureE2BApi::start().await;
    let provider = E2BHandProvider::new(Arc::new(
        crate::core::provider_credentials::TestProviderCredentialSource::new(
            CloudHandProviderKind::E2b,
            fixture.api_url(),
            None,
            Some("example.e2b.test".to_string()),
            Some("base".to_string()),
            "test-key",
        ),
    ))
    .with_sandbox_base_url(fixture.api_url());
    let spec = crate::core::profile::test_support::hand_spec(
        SandboxTier::MicroVM,
        e2b_test_profile(EgressPolicy::DenyAll),
    );
    let binding = spec.workspace.clone();
    let before = e2b_operation_temp_dirs();
    let handle = provider
        .provision(spec)
        .await
        .expect("provision E2B fixture");
    let _ = fixture.next_discovery_request().await;
    let (sandbox_id, attempt, sandbox) = provider
        .workspace_sandbox(&handle, &binding)
        .await
        .expect("inspect exact running sandbox");

    let archive = super::storage::export_data_root(
        &provider,
        &attempt,
        &sandbox_id,
        &sandbox,
        crate::core::sandbox_workspace::checkpoint::archive::ArchiveLimits {
            max_entries: 8,
            max_path_depth: 8,
            max_file_bytes: 16,
            max_total_bytes: 16,
            max_chunk_bytes: 8,
            max_compressed_chunk_bytes: 64,
        },
    )
    .await
    .expect("export canonical E2B data root");

    assert_eq!(archive.manifest.logical_bytes, 6);
    assert_eq!(archive.manifest.entries.len(), 1);
    assert_eq!(archive.manifest.entries[0].path, "marker.txt");
    assert_eq!(e2b_operation_temp_dirs(), before);
    provider
        .destroy(&handle)
        .await
        .expect("destroy fixture sandbox");
}

fn e2b_operation_temp_dirs() -> BTreeSet<PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
        .expect("read host temp directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".moa-e2b-"))
        })
        .collect()
}

fn encode_test_envelopes(messages: &[Value]) -> String {
    let mut bytes = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let payload = serde_json::to_vec(message).unwrap();
        let flags = if index + 1 == messages.len() {
            super::client::CONNECT_END_STREAM_FLAG
        } else {
            0
        };
        bytes.push(flags);
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&payload);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The profile E2B can actually serve: a bounded maximum lifetime, which is the
/// only deadline E2B's create call carries, plus whatever egress posture the
/// caller wants to exercise.
fn e2b_test_profile(egress: EgressPolicy) -> moa_core::types::hands::SandboxProfile {
    use moa_core::types::hands::{CpuLimit, DiskLimit, LifetimeLimit, MemoryLimit, SandboxProfile};
    SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        egress,
        LifetimeLimit::Bounded {
            seconds: std::num::NonZeroU64::new(120).expect("nonzero seconds"),
        },
        LifetimeLimit::Bounded {
            seconds: std::num::NonZeroU64::new(300).expect("nonzero seconds"),
        },
    )
    .expect("E2B test profile should validate")
}
