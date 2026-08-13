//! Unit tests for the provider adapter.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use moa_config::CloudHandProviderKind;
use moa_core::{
    traits::{HandProvider, SandboxStorageProvider},
    types::hands::{HandHandle, SandboxProfile, SandboxTier},
    types::identifiers::{WorkspaceCheckpointId, WorkspaceOperationId},
    types::resource::ResourceBudget,
    types::sandbox_workspace::{
        WorkspaceCheckpointPublishRequest, WorkspaceOperationKind, WorkspaceRevisionRef,
        WorkspaceStorageOperation,
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{
    DEFAULT_DAYTONA_IMAGE, DaytonaHandProvider, DaytonaProvisioningIdentity,
    PROVISIONING_OPERATION_LABEL, PROVISIONING_SPEC_LABEL, ProviderEndpoint,
    daytona_auto_stop_minutes, daytona_hand_status, daytona_sandbox_name, volume,
};

async fn read_request(socket: &mut TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let bytes = socket.read(&mut chunk).await.unwrap();
        if bytes == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..bytes]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default();
        if request.len() >= body_start + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&request).to_string()
}

#[tokio::test]
async fn provisions_executes_and_destroys_workspace() {
    let spec = crate::core::profile::test_support::hand_spec(
        SandboxTier::Container,
        SandboxProfile::unrestricted(),
    );
    let operation_id = spec.provisioning_operation_id;
    let sandbox_name = daytona_sandbox_name(operation_id);
    let auto_stop_minutes = daytona_auto_stop_minutes(spec.effective_profile.profile()).unwrap();
    let spec_fingerprint =
        DaytonaProvisioningIdentity::for_spec(&spec, DEFAULT_DAYTONA_IMAGE, auto_stop_minutes)
            .unwrap()
            .spec_fingerprint
            .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let seen_server = seen.clone();
    let created = Arc::new(AtomicBool::new(false));
    let created_server = created.clone();
    let deleted = Arc::new(AtomicBool::new(false));
    let deleted_server = deleted.clone();
    let running = Arc::new(AtomicBool::new(false));
    let running_server = running.clone();
    let sandbox_name_server = sandbox_name.clone();
    let spec_fingerprint_server = spec_fingerprint.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let seen = seen_server.clone();
            let created = created_server.clone();
            let deleted = deleted_server.clone();
            let running = running_server.clone();
            let sandbox_name = sandbox_name_server.clone();
            let spec_fingerprint = spec_fingerprint_server.clone();
            tokio::spawn(async move {
                let request = read_request(&mut socket).await;
                let first_line = request.lines().next().unwrap_or_default().to_string();
                seen.lock().await.push(first_line.clone());
                let (status, body) = if first_line.starts_with("POST /api/sandbox ") {
                    if request.contains(&format!("\"name\":\"{sandbox_name}\""))
                        && request.contains(PROVISIONING_OPERATION_LABEL)
                        && request.contains(&operation_id.to_string())
                        && request.contains(PROVISIONING_SPEC_LABEL)
                        && request.contains(&spec_fingerprint)
                    {
                        created.store(true, Ordering::SeqCst);
                        (
                            "200 OK",
                            format!(
                                r#"{{"id":"sbx-123","name":"{sandbox_name}","state":"started"}}"#
                            ),
                        )
                    } else {
                        (
                            "400 Bad Request",
                            r#"{"error":"missing durable identity"}"#.to_string(),
                        )
                    }
                } else if first_line.starts_with("GET /api/sandbox?") {
                    if created.load(Ordering::SeqCst) && !deleted.load(Ordering::SeqCst) {
                        (
                            "200 OK",
                            format!(
                                r#"{{"items":[{{"id":"sbx-123","name":"{sandbox_name}","labels":{{"{PROVISIONING_OPERATION_LABEL}":"{operation_id}","{PROVISIONING_SPEC_LABEL}":"{spec_fingerprint}"}},"state":"paused"}}],"nextCursor":null}}"#
                            ),
                        )
                    } else {
                        ("200 OK", r#"{"items":[],"nextCursor":null}"#.to_string())
                    }
                } else if first_line.starts_with(&format!("GET /api/sandbox/{sandbox_name} ")) {
                    if created.load(Ordering::SeqCst) && !deleted.load(Ordering::SeqCst) {
                        (
                            "200 OK",
                            format!(
                                r#"{{"id":"sbx-123","name":"{sandbox_name}","labels":{{"{PROVISIONING_OPERATION_LABEL}":"{operation_id}","{PROVISIONING_SPEC_LABEL}":"{spec_fingerprint}"}},"state":"started"}}"#
                            ),
                        )
                    } else {
                        ("404 Not Found", r#"{"error":"not found"}"#.to_string())
                    }
                } else if first_line.starts_with("GET /api/sandbox/sbx-123 ") {
                    if deleted.load(Ordering::SeqCst) {
                        ("404 Not Found", r#"{"error":"not found"}"#.to_string())
                    } else {
                        (
                            "200 OK",
                            format!(
                                r#"{{"id":"sbx-123","name":"{sandbox_name}","state":"{}"}}"#,
                                if running.load(Ordering::SeqCst) {
                                    "started"
                                } else {
                                    "stopped"
                                }
                            ),
                        )
                    }
                } else if first_line.starts_with("POST /api/sandbox/sbx-123/start ") {
                    running.store(true, Ordering::SeqCst);
                    ("200 OK", r#"{"ok":true}"#.to_string())
                } else if first_line.starts_with("POST /api/sandbox/sbx-123/stop ") {
                    running.store(false, Ordering::SeqCst);
                    ("200 OK", r#"{"ok":true}"#.to_string())
                } else if first_line.starts_with("POST /toolbox/sbx-123/process/execute ") {
                    if running.load(Ordering::SeqCst) {
                        ("200 OK", r#"{"exitCode":0,"result":"hello\n"}"#.to_string())
                    } else {
                        (
                            "409 Conflict",
                            r#"{"error":"sandbox is stopped"}"#.to_string(),
                        )
                    }
                } else if first_line.starts_with("DELETE /api/sandbox/sbx-123 ") {
                    deleted.store(true, Ordering::SeqCst);
                    ("200 OK", r#"{"ok":true}"#.to_string())
                } else {
                    ("404 Not Found", r#"{"error":"unexpected"}"#.to_string())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });

    let provider = DaytonaHandProvider::new(Arc::new(
        crate::core::provider_credentials::TestProviderCredentialSource::new(
            CloudHandProviderKind::Daytona,
            format!("http://{addr}"),
            Some(format!("http://{addr}")),
            None,
            Some(DEFAULT_DAYTONA_IMAGE.to_string()),
            "test-key",
        ),
    ));
    let account_id = spec.workspace.provider_account_id;
    let account_generation = spec.workspace.provider_account_generation;
    let handle = provider.provision(spec.clone()).await.unwrap();

    assert_eq!(provider.provision(spec.clone()).await.unwrap(), handle);

    let mut stale_spec = spec;
    stale_spec.workspace.writer_epoch += 1;
    let error = provider
        .provision(stale_spec)
        .await
        .expect_err("changed workspace binding must invalidate Daytona reuse");
    assert!(
        matches!(error, moa_core::error::MoaError::ProviderError(ref message) if message.contains("does not match the durable provisioning identity")),
        "unexpected reuse error: {error}"
    );

    assert_eq!(
        provider
            .provisioned_hands(account_id, account_generation, operation_id)
            .await
            .unwrap(),
        vec![handle.clone()]
    );

    let output = provider
        .execute(&handle, "bash", r#"{"cmd":"echo hello"}"#)
        .await
        .unwrap();
    assert_eq!(output.process_stdout(), Some("hello\n"));

    // Pins: Daytona suspend does not return until the real provider state is
    // stopped, and the next dispatch waits for an exact running state.
    provider.suspend(&handle).await.unwrap();
    assert_eq!(
        provider.status(&handle).await.unwrap(),
        moa_core::types::hands::HandStatus::Stopped
    );
    let resumed = provider
        .execute(&handle, "bash", r#"{"cmd":"echo hello"}"#)
        .await
        .unwrap();
    assert_eq!(resumed.process_stdout(), Some("hello\n"));
    assert_eq!(
        provider.status(&handle).await.unwrap(),
        moa_core::types::hands::HandStatus::Running
    );

    provider.destroy(&handle).await.unwrap();

    let seen = seen.lock().await.join("\n");
    assert!(seen.contains("GET /api/sandbox?"));
    assert!(seen.contains(&format!("GET /api/sandbox/{sandbox_name} ")));
    assert!(seen.contains("POST /api/sandbox "));
    assert!(seen.contains("POST /toolbox/sbx-123/process/execute "));
    assert!(seen.contains("DELETE /api/sandbox/sbx-123 "));
    assert!(seen.contains("GET /api/sandbox/sbx-123 "));
    assert_eq!(
        seen.lines()
            .filter(|line| line.starts_with("POST /api/sandbox "))
            .count(),
        1
    );
}

#[test]
fn daytona_transitional_states_never_report_running() {
    // Pins: asynchronous stop/start states cannot be mistaken for executable
    // or capacity-free terminal states.
    assert_eq!(
        daytona_hand_status("starting"),
        moa_core::types::hands::HandStatus::Provisioning
    );
    assert_eq!(
        daytona_hand_status("resuming"),
        moa_core::types::hands::HandStatus::Provisioning
    );
    assert_eq!(
        daytona_hand_status("stopping"),
        moa_core::types::hands::HandStatus::Paused
    );
    assert_eq!(
        daytona_hand_status("pausing"),
        moa_core::types::hands::HandStatus::Paused
    );
    assert_eq!(
        daytona_hand_status("stopped"),
        moa_core::types::hands::HandStatus::Stopped
    );
    assert_eq!(
        daytona_hand_status("unexpected"),
        moa_core::types::hands::HandStatus::Failed
    );
}

#[test]
fn daytona_effective_timeout_never_exceeds_default_or_run_budget() {
    // Pins: the value declared to the watchdog is also the largest wall-clock
    // duration the provider can spend, for bash and non-bash calls alike.
    assert_eq!(
        crate::tools::bash::effective_synchronous_timeout(
            "bash",
            r#"{"cmd":"true"}"#,
            crate::tools::bash::DEFAULT_BASH_TIMEOUT,
            ResourceBudget::UNBOUNDED.time_remaining(chrono::Utc::now()),
        )
        .expect("default bash timeout resolves"),
        crate::tools::bash::DEFAULT_BASH_TIMEOUT
    );
    let bounded = crate::tools::bash::effective_synchronous_timeout(
        "file_read",
        r#"{"path":"marker.txt"}"#,
        crate::tools::bash::DEFAULT_BASH_TIMEOUT,
        ResourceBudget::until(chrono::Utc::now() + chrono::Duration::seconds(10))
            .time_remaining(chrono::Utc::now()),
    )
    .expect("bounded file timeout resolves");
    assert!(bounded <= std::time::Duration::from_secs(10));
    assert!(
        crate::tools::bash::effective_synchronous_timeout(
            "file_read",
            r#"{"path":"marker.txt"}"#,
            crate::tools::bash::DEFAULT_BASH_TIMEOUT,
            ResourceBudget::until(chrono::Utc::now() - chrono::Duration::seconds(1))
                .time_remaining(chrono::Utc::now()),
        )
        .is_err()
    );
}

#[tokio::test]
async fn daytona_rejects_a_mutable_root_outside_the_volume_mount_boundary() {
    // Pins: volume attachment and checkpoint export share one exact mutable
    // root; trusted/runtime roots cannot be substituted into that mount.
    let mut spec = crate::core::profile::test_support::hand_spec(
        SandboxTier::Container,
        SandboxProfile::unrestricted(),
    );
    spec.filesystem.mutable_root = std::path::PathBuf::from("/opt/moa/trusted");
    let provider = DaytonaHandProvider::new(Arc::new(
        crate::core::provider_credentials::TestProviderCredentialSource::new(
            CloudHandProviderKind::Daytona,
            "http://127.0.0.1:1",
            Some("http://127.0.0.1:1".to_string()),
            None,
            Some(DEFAULT_DAYTONA_IMAGE.to_string()),
            "test-key",
        ),
    ));

    let error = provider
        .provision(spec)
        .await
        .expect_err("nonstandard Daytona mutable root must fail before provider I/O");

    assert!(matches!(
        error,
        moa_core::error::MoaError::ValidationError(message) if message.contains("mutable root")
    ));
}

#[tokio::test]
async fn daytona_commit_rejects_a_parent_at_generation_zero_before_provider_io() {
    // Pins: invalid initial-parent state is rejected before credentials,
    // storage dependencies, capacity reservations, or checkpoint upload.
    let mut binding = crate::core::profile::test_support::hand_spec(
        SandboxTier::Container,
        SandboxProfile::unrestricted(),
    )
    .workspace;
    binding.current_revision = None;
    let parent = WorkspaceRevisionRef {
        checkpoint_id: WorkspaceCheckpointId::new(),
        generation: 1,
        format_version: 1,
    };
    let hand = HandHandle::daytona(
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
    let provider = DaytonaHandProvider::new(Arc::new(
        crate::core::provider_credentials::TestProviderCredentialSource::new(
            CloudHandProviderKind::Daytona,
            "http://127.0.0.1:1",
            Some("http://127.0.0.1:1".to_string()),
            None,
            Some(DEFAULT_DAYTONA_IMAGE.to_string()),
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

    assert!(matches!(
        error,
        moa_core::error::MoaError::ValidationError(message) if message.contains("parent")
    ));
}

#[tokio::test]
async fn volume_rest_shapes_and_typed_conflict_rate_limit_are_exact_offline() {
    // Pins: Daytona volume routes use the documented bare-array/200 shapes,
    // preserve path-segment encoding, and retain 409/429 as typed outcomes.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let seen_server = seen.clone();
    tokio::spawn(async move {
        for request_index in 0..6 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let first_line = request.lines().next().unwrap_or_default().to_string();
            seen_server.lock().await.push(request.clone());
            let dto = r#"{"id":"vol/1","name":"moa_tenant_1","organizationId":"org-1","state":"ready","createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:01Z","lastUsedAt":null,"errorReason":null}"#;
            let (status, extra_headers, body) = if request_index == 5 {
                (
                        "429 Too Many Requests",
                        "Retry-After-volume: 7\r\n",
                        r#"{"statusCode":429,"message":"Rate limit exceeded","error":"Too Many Requests"}"#.to_string(),
                    )
            } else if first_line.starts_with("POST /api/volumes ") {
                ("200 OK", "", dto.to_string())
            } else if first_line.starts_with("GET /api/volumes?includeDeleted=false ") {
                ("200 OK", "", format!("[{dto}]"))
            } else if first_line.starts_with("GET /api/volumes/vol%2F1 ")
                || first_line.starts_with("GET /api/volumes/by-name/moa_tenant_1 ")
            {
                ("200 OK", "", dto.to_string())
            } else if first_line.starts_with("DELETE /api/volumes/vol%2F1 ") {
                (
                    "409 Conflict",
                    "",
                    r#"{"message":"Volume is in use by one or more sandboxes"}"#.to_string(),
                )
            } else {
                (
                    "404 Not Found",
                    "",
                    r#"{"message":"unexpected route"}"#.to_string(),
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{extra_headers}connection: close\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let spec = crate::core::profile::test_support::hand_spec(
        SandboxTier::Container,
        SandboxProfile::unrestricted(),
    );
    let provider = DaytonaHandProvider::new(Arc::new(
        crate::core::provider_credentials::TestProviderCredentialSource::new(
            CloudHandProviderKind::Daytona,
            format!("http://{addr}"),
            Some(format!("http://{addr}")),
            None,
            Some(DEFAULT_DAYTONA_IMAGE.to_string()),
            "test-key",
        ),
    ));
    let attempt = provider
        .attempt(
            spec.workspace.provider_account_id,
            spec.workspace.provider_account_generation,
            ProviderEndpoint::Api,
        )
        .await
        .unwrap();

    assert_eq!(
        volume::create_volume(&attempt, "moa_tenant_1")
            .await
            .unwrap()
            .id,
        "vol/1"
    );
    assert_eq!(volume::list_volumes(&attempt).await.unwrap().len(), 1);
    assert_eq!(
        volume::get_volume(&attempt, "vol/1")
            .await
            .unwrap()
            .unwrap()
            .name,
        "moa_tenant_1"
    );
    assert!(
        volume::get_volume_by_name(&attempt, "moa_tenant_1")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        volume::delete_volume(&attempt, "vol/1").await.unwrap(),
        volume::DaytonaVolumeDeleteOutcome::MountedConflict
    );
    let error = volume::list_volumes(&attempt)
        .await
        .expect_err("429 must remain a typed rate-limit error");
    assert!(matches!(
        error,
        moa_core::error::MoaError::HttpStatus {
            status: 429,
            retry_after: Some(delay),
            ..
        } if delay == std::time::Duration::from_secs(7)
    ));

    let requests = seen.lock().await.join("\n");
    assert!(requests.contains("authorization: Bearer test-key"));
    assert!(requests.contains(r#"{"name":"moa_tenant_1"}"#));
    assert!(requests.contains("GET /api/volumes/vol%2F1 "));
}
