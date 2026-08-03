//! Pure connector lifecycle, generation, and origin behavior.

use moa_connectors::Error;
use moa_connectors::domain::{
    ConnectionGeneration, ConnectionHealth, ConnectionOrigin, ConnectionStatus,
    ConnectorInvocationId, ConnectorInvocationState,
};
use uuid::Uuid;

#[test]
fn lifecycle_accepts_only_the_documented_directed_edges_offline() {
    // Pins: a connection can move only through the explicit lifecycle graph;
    // same-state updates and shortcuts cannot silently advance its generation.
    let states = [
        ConnectionStatus::PendingAuth,
        ConnectionStatus::Active,
        ConnectionStatus::Suspended,
        ConnectionStatus::Disconnecting,
        ConnectionStatus::Deleted,
    ];
    let allowed = [
        (ConnectionStatus::PendingAuth, ConnectionStatus::Active),
        (ConnectionStatus::PendingAuth, ConnectionStatus::Deleted),
        (ConnectionStatus::Active, ConnectionStatus::Suspended),
        (ConnectionStatus::Active, ConnectionStatus::Disconnecting),
        (ConnectionStatus::Suspended, ConnectionStatus::Active),
        (ConnectionStatus::Suspended, ConnectionStatus::Disconnecting),
        (ConnectionStatus::Disconnecting, ConnectionStatus::Deleted),
    ];

    for from in states {
        for to in states {
            let result = from.transition(to);
            if allowed.contains(&(from, to)) {
                assert_eq!(
                    result.expect("documented lifecycle edge should be accepted"),
                    to,
                    "accepted edge should return its requested state"
                );
            } else {
                assert!(
                    matches!(
                        result,
                        Err(Error::InvalidTransition {
                            from: actual_from,
                            to: actual_to,
                        }) if actual_from == from && actual_to == to
                    ),
                    "undocumented lifecycle edge {from}->{to} should return its exact typed error"
                );
            }
        }
    }
}

#[test]
fn health_does_not_make_a_non_active_connection_catalog_visible_offline() {
    // Pins: a stale ready health observation never counteracts suspension or teardown.
    assert_eq!(ConnectionHealth::Ready.as_str(), "ready");
    assert!(!ConnectionStatus::PendingAuth.is_catalog_visible());
    assert!(ConnectionStatus::Active.is_catalog_visible());
    assert!(!ConnectionStatus::Suspended.is_catalog_visible());
    assert!(!ConnectionStatus::Disconnecting.is_catalog_visible());
    assert!(!ConnectionStatus::Deleted.is_catalog_visible());
}

#[test]
fn generation_rejects_zero_and_cannot_wrap_offline() {
    // Pins: optimistic-concurrency generations remain positive and never wrap to zero.
    let zero = serde_json::from_str::<ConnectionGeneration>("0")
        .expect_err("zero generation should fail during persisted JSON decoding");
    assert!(zero.to_string().contains("must be positive"));

    let exhausted = ConnectionGeneration::new(u64::MAX)
        .expect("maximum generation remains a valid current fence")
        .next();
    assert!(matches!(exhausted, Err(Error::GenerationExhausted)));
}

#[test]
fn connection_origin_canonicalizes_fixed_http_origins_offline() {
    // Pins: connection origins are canonical fixed authorities before persistence.
    let https = ConnectionOrigin::parse("https://API.Example.COM:8443/")
        .expect("fixed HTTPS origin should parse");
    let http = ConnectionOrigin::parse("http://127.0.0.1:8080")
        .expect("T3 syntax permits IP origins for T4 runtime admission");

    assert_eq!(https.as_str(), "https://api.example.com:8443");
    assert_eq!(http.as_str(), "http://127.0.0.1:8080");
    assert_eq!(
        serde_json::to_string(&https).expect("validated origin should serialize"),
        "\"https://api.example.com:8443\""
    );
}

#[test]
fn connection_origin_rejects_non_origin_and_dynamic_urls_offline() {
    // Pins: path/query/fragment/userinfo/wildcard/template inputs fail before persistence.
    let invalid = [
        "ftp://api.example.com",
        "https://user:secret@api.example.com",
        "https://api.example.com/v1",
        "https://api.example.com/.",
        "https://api.example.com/%2e",
        "https://api.example.com?tenant=x",
        "https://api.example.com#fragment",
        "https://*.example.com",
        "https://{tenant}.example.com",
        "https://api.example.com/%7Btenant%7D",
        " https://api.example.com",
    ];

    for value in invalid {
        assert!(
            matches!(
                ConnectionOrigin::parse(value),
                Err(Error::InvalidConnectionOrigin { .. })
            ),
            "dynamic or non-origin fixture should fail: {value}"
        );
    }
}

#[test]
fn invocation_lifecycle_preserves_the_request_transmission_boundary_offline() {
    // Pins: only failures proven before send may finish from reserved; once
    // transmitting, cancellation or transport loss cannot become a safe retry.
    let invocation_id = ConnectorInvocationId(Uuid::from_u128(44));
    let states = [
        ConnectorInvocationState::Reserved,
        ConnectorInvocationState::Transmitting,
        ConnectorInvocationState::Succeeded,
        ConnectorInvocationState::FailedBeforeSend,
        ConnectorInvocationState::Failed,
        ConnectorInvocationState::UnknownOutcome,
    ];
    let allowed = [
        (
            ConnectorInvocationState::Reserved,
            ConnectorInvocationState::Transmitting,
        ),
        (
            ConnectorInvocationState::Reserved,
            ConnectorInvocationState::FailedBeforeSend,
        ),
        (
            ConnectorInvocationState::Transmitting,
            ConnectorInvocationState::Succeeded,
        ),
        (
            ConnectorInvocationState::Transmitting,
            ConnectorInvocationState::Failed,
        ),
        (
            ConnectorInvocationState::Transmitting,
            ConnectorInvocationState::UnknownOutcome,
        ),
    ];

    for from in states {
        for to in states {
            let result = from.transition(invocation_id, to);
            if allowed.contains(&(from, to)) {
                assert_eq!(
                    result.expect("documented invocation edge should be accepted"),
                    to
                );
            } else {
                assert!(matches!(
                    result,
                    Err(Error::InvocationStateConflict {
                        invocation_id: actual_id,
                        from: actual_from,
                        to: actual_to,
                    }) if actual_id == invocation_id && actual_from == from && actual_to == to
                ));
            }
        }
    }
}
