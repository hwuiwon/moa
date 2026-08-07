use moa_core::{
    error::MoaError,
    traits::HandProvider,
    types::{
        hands::{EgressPolicy, SandboxTier},
        identifiers::HandProvisioningOperationId,
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
            "200 OK",
            "application/json",
            r#"{"sandboxID":"sbx-123","domain":"example.e2b.test","envdAccessToken":"envd-token","envdVersion":"0.1.1"}"#.to_string(),
        );
    }
    if first_line.starts_with("POST /process.Process/Start ") {
        return (
            "200 OK",
            "application/connect+json",
            encode_test_envelopes(&[
                serde_json::json!({"event":{"start":{"pid": 12}}}),
                serde_json::json!({"event":{"data":{"stdout":"aGVsbG8K"}}}),
                serde_json::json!({"event":{"end":{"exited":true,"status":"exit status 0"}}}),
                serde_json::json!({}),
            ]),
        );
    }
    if first_line.starts_with("DELETE /sandboxes/sbx-123 ") {
        *created_metadata = None;
        return ("204 No Content", "application/json", String::new());
    }
    if first_line.starts_with("GET /sandboxes/sbx-123 ") {
        return (
            "200 OK",
            "application/json",
            r#"{"state":"paused"}"#.to_string(),
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

    let provider =
        E2BHandProvider::with_api_url("test-key", fixture.api_url(), "example.e2b.test", "base")
            .unwrap()
            .with_sandbox_base_url(fixture.api_url());
    let spec = crate::core::profile::test_support::hand_spec(
        SandboxTier::MicroVM,
        e2b_test_profile(EgressPolicy::DenyAll),
    );
    let operation_id = spec.provisioning_operation_id;
    let handle = provider.provision(spec).await.unwrap();
    assert_discovery_request(&fixture.next_discovery_request().await, operation_id);
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
        provider.provisioned_hands(operation_id).await.unwrap(),
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

    provider.destroy(&handle).await.unwrap();
}

#[tokio::test]
async fn e2b_egress_posture_comes_from_the_effective_profile() {
    let mut fixture = FixtureE2BApi::start().await;

    let provider =
        E2BHandProvider::with_api_url("test-key", fixture.api_url(), "example.e2b.test", "base")
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
        provider.provisioned_hands(operation_id).await.unwrap(),
        vec![handle]
    );
    assert_discovery_request(&fixture.next_discovery_request().await, operation_id);
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
