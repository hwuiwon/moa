//! Behavior tests for outbound HTTP destination admission.

use std::collections::VecDeque;
use std::future::pending;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{
    AdmittedHttpDestination, OutboundHostResolutionError, OutboundHostResolver,
    OutboundHttpAdmissionError, OutboundHttpClientLimits, OutboundHttpPolicy,
    build_admitted_http_client,
};

#[derive(Default)]
struct FakeResolver {
    answers: Mutex<VecDeque<Result<Vec<SocketAddr>, OutboundHostResolutionError>>>,
    calls: Mutex<Vec<(String, u16)>>,
}

impl FakeResolver {
    fn with_answers(answers: impl IntoIterator<Item = Vec<SocketAddr>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into_iter().map(Ok).collect()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(String, u16)> {
        self.calls
            .lock()
            .expect("fake resolver call log lock should remain available")
            .clone()
    }
}

#[async_trait]
impl OutboundHostResolver for FakeResolver {
    async fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, OutboundHostResolutionError> {
        self.calls
            .lock()
            .expect("fake resolver call log lock should remain available")
            .push((host.to_owned(), port));
        self.answers
            .lock()
            .expect("fake resolver answer lock should remain available")
            .pop_front()
            .unwrap_or(Err(OutboundHostResolutionError::Failed))
    }
}

struct PendingResolver;

#[async_trait]
impl OutboundHostResolver for PendingResolver {
    async fn resolve(
        &self,
        _host: &str,
        _port: u16,
    ) -> Result<Vec<SocketAddr>, OutboundHostResolutionError> {
        pending().await
    }
}

fn address(value: &str) -> SocketAddr {
    value
        .parse()
        .expect("fixture socket address should be syntactically valid")
}

fn admission_error(
    result: Result<AdmittedHttpDestination, OutboundHttpAdmissionError>,
) -> OutboundHttpAdmissionError {
    match result {
        Ok(_) => panic!("destination admission should have failed"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn outbound_http_public_https_origin_is_canonical_and_dns_pinned() {
    // Pins: one production admission exposes only the canonical origin and the
    // exact sorted, duplicate-free public address set resolved for that attempt.
    let resolver = FakeResolver::with_answers([vec![
        address("[2606:4700:4700::1111]:8443"),
        address("93.184.216.34:8443"),
        address("93.184.216.34:8443"),
    ]]);
    let policy = OutboundHttpPolicy::production(resolver.clone());

    let admitted = policy
        .admit("https://api.example.test:8443", Duration::from_secs(1))
        .await
        .expect("public HTTPS origin should be admitted");

    assert_eq!(
        admitted.canonical_origin().as_str(),
        "https://api.example.test:8443/"
    );
    assert_eq!(admitted.host(), "api.example.test");
    assert_eq!(admitted.port(), 8443);
    assert_eq!(
        admitted.socket_addrs(),
        &[
            address("93.184.216.34:8443"),
            address("[2606:4700:4700::1111]:8443")
        ]
    );
    assert_eq!(
        resolver.calls(),
        vec![("api.example.test".to_string(), 8443)]
    );
}

#[tokio::test]
async fn outbound_http_unsafe_ipv4_and_ipv6_ranges_fail_closed() {
    // Pins: private, loopback, link-local, metadata, documentation, transition,
    // multicast, and reserved destinations never survive production admission.
    let denied = [
        "10.0.0.1:443",
        "127.0.0.1:443",
        "169.254.169.254:443",
        "168.63.129.16:443",
        "100.100.100.200:443",
        "192.0.2.1:443",
        "198.18.0.1:443",
        "224.0.0.1:443",
        "240.0.0.1:443",
        "[::1]:443",
        "[fe80::1]:443",
        "[fc00::1]:443",
        "[ff02::1]:443",
        "[2001:db8::1]:443",
        "[64:ff9b::808:808]:443",
        "[2002:0808:0808::1]:443",
        "[2620:4f:8000::1]:443",
        "[4000::1]:443",
        "[::ffff:8.8.8.8]:443",
    ];

    for denied_address in denied {
        let resolver = FakeResolver::with_answers([vec![address(denied_address)]]);
        let error = admission_error(
            OutboundHttpPolicy::production(resolver)
                .admit("https://api.example.test", Duration::from_secs(1))
                .await,
        );
        assert_eq!(error, OutboundHttpAdmissionError::AddressDenied);
    }
}

#[tokio::test]
async fn outbound_http_mixed_public_and_unsafe_address_set_is_rejected_whole() {
    // Pins: one unsafe DNS answer invalidates the complete set instead of
    // silently selecting a public sibling address.
    let resolver =
        FakeResolver::with_answers([vec![address("93.184.216.34:443"), address("10.0.0.1:443")]]);

    let error = admission_error(
        OutboundHttpPolicy::production(resolver)
            .admit("https://api.example.test", Duration::from_secs(1))
            .await,
    );

    assert_eq!(error, OutboundHttpAdmissionError::AddressDenied);
}

#[tokio::test]
async fn outbound_http_re_resolves_every_attempt_and_blocks_dns_rebinding() {
    // Pins: a retry never reuses a previously admitted DNS answer; a hostname
    // that rebinds from public to private fails before a second request can run.
    let resolver = FakeResolver::with_answers([
        vec![address("93.184.216.34:443")],
        vec![address("10.0.0.1:443")],
    ]);
    let policy = OutboundHttpPolicy::production(resolver.clone());

    policy
        .admit("https://api.example.test", Duration::from_secs(1))
        .await
        .expect("first public answer should be admitted");
    let rebound = admission_error(
        policy
            .admit("https://api.example.test", Duration::from_secs(1))
            .await,
    );

    assert_eq!(rebound, OutboundHttpAdmissionError::AddressDenied);
    assert_eq!(
        resolver.calls(),
        vec![
            ("api.example.test".to_string(), 443),
            ("api.example.test".to_string(), 443)
        ]
    );
}

#[tokio::test]
async fn outbound_http_loopback_http_requires_explicit_test_policy() {
    // Pins: production construction cannot admit plaintext or loopback, while
    // the feature-gated fixture policy accepts only an all-loopback HTTP set.
    let production = OutboundHttpPolicy::production(FakeResolver::with_answers([vec![address(
        "127.0.0.1:32123",
    )]]));
    let production_error = admission_error(
        production
            .admit("http://fixture.test:32123", Duration::from_secs(1))
            .await,
    );
    assert_eq!(production_error, OutboundHttpAdmissionError::HttpsRequired);

    let test_policy =
        OutboundHttpPolicy::loopback_http_for_tests(FakeResolver::with_answers([vec![address(
            "127.0.0.1:32123",
        )]]));
    let admitted = test_policy
        .admit("http://fixture.test:32123", Duration::from_secs(1))
        .await
        .expect("explicit fixture policy should admit loopback HTTP");
    assert_eq!(admitted.socket_addrs(), &[address("127.0.0.1:32123")]);

    let public_http = admission_error(
        OutboundHttpPolicy::loopback_http_for_tests(FakeResolver::with_answers([vec![address(
            "93.184.216.34:32123",
        )]]))
        .admit("http://fixture.test:32123", Duration::from_secs(1))
        .await,
    );
    assert_eq!(public_http, OutboundHttpAdmissionError::AddressDenied);
}

#[tokio::test]
async fn outbound_http_rejects_non_origin_and_noncanonical_inputs_before_dns() {
    // Pins: request data cannot smuggle userinfo, paths, queries, fragments,
    // wildcards, alternate scheme casing, or a non-normalized host into DNS.
    let invalid = [
        "https://user@api.example.test",
        "https://api.example.test/path",
        "https://api.example.test?query=1",
        "https://api.example.test#fragment",
        "https://*.example.test",
        "HTTPS://api.example.test",
        "https://API.example.test",
        "https://api.example.test.",
        " https://api.example.test",
    ];

    for origin in invalid {
        let resolver = Arc::new(FakeResolver::default());
        let error = admission_error(
            OutboundHttpPolicy::production(resolver.clone())
                .admit(origin, Duration::from_secs(1))
                .await,
        );
        assert!(matches!(
            error,
            OutboundHttpAdmissionError::InvalidOrigin
                | OutboundHttpAdmissionError::NonCanonicalHost
        ));
        assert!(resolver.calls().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn outbound_http_dns_resolution_obeys_connect_timeout() {
    // Pins: a stalled resolver cannot consume more than the transport attempt's
    // connect-time budget, and the resulting error contains no destination.
    let error = admission_error(
        OutboundHttpPolicy::production(Arc::new(PendingResolver))
            .admit(
                "https://fixture-secret.example.test",
                Duration::from_secs(3),
            )
            .await,
    );

    assert_eq!(error, OutboundHttpAdmissionError::ResolutionTimedOut);
    let debug = format!("{error:?}");
    let display = error.to_string();
    assert!(!debug.contains("fixture-secret"));
    assert!(!display.contains("fixture-secret"));
}

#[tokio::test]
async fn outbound_http_empty_or_wrong_port_resolution_fails_closed() {
    // Pins: a resolver cannot silently erase the destination or redirect the
    // request to a port outside the reviewed connection origin.
    let empty = admission_error(
        OutboundHttpPolicy::production(FakeResolver::with_answers([Vec::new()]))
            .admit("https://api.example.test", Duration::from_secs(1))
            .await,
    );
    assert_eq!(empty, OutboundHttpAdmissionError::EmptyAddressSet);

    let wrong_port = admission_error(
        OutboundHttpPolicy::production(FakeResolver::with_answers([vec![address(
            "93.184.216.34:8443",
        )]]))
        .admit("https://api.example.test", Duration::from_secs(1))
        .await,
    );
    assert_eq!(wrong_port, OutboundHttpAdmissionError::PortMismatch);
}

#[tokio::test]
async fn outbound_http_admitted_client_never_follows_redirects() {
    // Pins: provider authorization cannot be forwarded to a redirect target;
    // callers receive the redirect response and must re-admit any next origin.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("redirect fixture bind");
    let address = listener.local_addr().expect("redirect fixture address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("first request");
        let mut buffer = [0_u8; 2048];
        let count = socket.read(&mut buffer).await.expect("read first request");
        let request = String::from_utf8_lossy(&buffer[..count]);
        assert!(request.starts_with("GET /start "));
        socket
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: /credential-leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write redirect");
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    });
    let policy =
        OutboundHttpPolicy::loopback_http_for_tests(FakeResolver::with_answers([vec![address]]));
    let origin = format!("http://{address}");
    let admitted = policy
        .admit(&origin, Duration::from_secs(1))
        .await
        .expect("loopback fixture admission");
    let client = build_admitted_http_client(
        &admitted,
        OutboundHttpClientLimits::new(Duration::from_secs(1), Duration::from_secs(2), 8192)
            .expect("client limits"),
    )
    .expect("admitted client");

    let response = client
        .get(format!("{origin}/start"))
        .header("authorization", "Bearer must-not-forward")
        .send()
        .await
        .expect("redirect response");

    assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    assert!(server.await.expect("redirect fixture task"));
}
