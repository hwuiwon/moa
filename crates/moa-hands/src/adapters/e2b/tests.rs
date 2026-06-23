use std::time::Duration;

use moa_core::{HandProvider, HandResources, HandSpec, SandboxTier};
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
        .provision(HandSpec {
            sandbox_tier: SandboxTier::MicroVM,
            image: None,
            resources: HandResources::default(),
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(300),
        })
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
        Some(false)
    );

    let output = provider
        .execute(&handle, "bash", r#"{"cmd":"echo hello"}"#)
        .await
        .unwrap();
    assert_eq!(output.process_stdout(), Some("hello\n"));

    provider.destroy(&handle).await.unwrap();
}

#[tokio::test]
async fn explicit_e2b_internet_opt_in_sets_create_payload() {
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
    .unwrap()
    .with_allow_internet_access(true);

    let _handle = provider
        .provision(HandSpec {
            sandbox_tier: SandboxTier::MicroVM,
            image: None,
            resources: HandResources::default(),
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(300),
        })
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
