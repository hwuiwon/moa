use moa_core::{
    error::MoaError, traits::HandProvider, types::hands::EgressPolicy, types::hands::SandboxTier,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use super::E2BHandProvider;

#[tokio::test]
async fn provisions_executes_and_destroys_sandbox() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (create_request_tx, mut create_request_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let create_request_tx = create_request_tx.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 8192];
                let bytes = socket.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                let first_line = request.lines().next().unwrap_or_default();
                let (status, content_type, body) = if first_line.starts_with("POST /sandboxes ")
                    || first_line.starts_with("POST /sandboxes/sbx-123/connect ")
                {
                    if first_line.starts_with("POST /sandboxes ")
                        && let Some((_, body)) = request.split_once("\r\n\r\n")
                    {
                        let _ = create_request_tx.send(
                            serde_json::from_str(body)
                                .expect("E2B create request should be valid JSON"),
                        );
                    }
                    (
                            "200 OK",
                            "application/json",
                            r#"{"sandboxID":"sbx-123","domain":"example.e2b.test","envdAccessToken":"envd-token","envdVersion":"0.1.1"}"#.to_string(),
                        )
                } else if first_line.starts_with("POST /process.Process/Start ") {
                    (
                        "200 OK",
                        "application/connect+json",
                        encode_test_envelopes(&[
                            serde_json::json!({"event":{"start":{"pid": 12}}}),
                            serde_json::json!({"event":{"data":{"stdout":"aGVsbG8K"}}}),
                            serde_json::json!({"event":{"end":{"exited":true,"status":"exit status 0"}}}),
                            serde_json::json!({}),
                        ]),
                    )
                } else if first_line.starts_with("DELETE /sandboxes/sbx-123 ") {
                    ("204 No Content", "application/json", String::new())
                } else if first_line.starts_with("GET /sandboxes/sbx-123 ") {
                    (
                        "200 OK",
                        "application/json",
                        r#"{"state":"paused"}"#.to_string(),
                    )
                } else {
                    (
                        "404 Not Found",
                        "application/json",
                        r#"{"error":"unexpected"}"#.to_string(),
                    )
                };
                let headers = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    body.len(),
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(body.as_bytes()).await.unwrap();
            });
        }
    });

    let provider = E2BHandProvider::with_api_url(
        "test-key",
        format!("http://{addr}"),
        "example.e2b.test",
        "base",
    )
    .unwrap()
    .with_sandbox_base_url(format!("http://{addr}"));
    let handle = provider
        .provision(crate::core::profile::test_support::hand_spec(
            SandboxTier::MicroVM,
            e2b_test_profile(EgressPolicy::DenyAll),
        ))
        .await
        .unwrap();
    let create_request = create_request_rx
        .recv()
        .await
        .expect("mock server should receive create sandbox request");
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

    provider.destroy(&handle).await.unwrap();
}

#[tokio::test]
async fn e2b_egress_posture_comes_from_the_effective_profile() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (create_request_tx, mut create_request_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buffer = vec![0_u8; 8192];
        let bytes = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
        if let Some((_, body)) = request.split_once("\r\n\r\n") {
            let _ = create_request_tx
                .send(serde_json::from_str(body).expect("E2B create request should be valid JSON"));
        }
        let body = r#"{"sandboxID":"sbx-123","domain":"example.e2b.test","envdAccessToken":"envd-token","envdVersion":"0.1.1"}"#;
        let headers = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
            body.len(),
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    });

    let provider = E2BHandProvider::with_api_url(
        "test-key",
        format!("http://{addr}"),
        "example.e2b.test",
        "base",
    )
    .unwrap();

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
        create_request_rx.try_recv().is_err(),
        "a refused profile must not reach the E2B create call"
    );

    let _handle = provider
        .provision(crate::core::profile::test_support::hand_spec(
            SandboxTier::MicroVM,
            e2b_test_profile(EgressPolicy::Unrestricted),
        ))
        .await
        .unwrap();

    let create_request = create_request_rx
        .recv()
        .await
        .expect("mock server should receive create sandbox request");
    assert_eq!(
        create_request
            .get("allow_internet_access")
            .and_then(Value::as_bool),
        Some(true)
    );
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
