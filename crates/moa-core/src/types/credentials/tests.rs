//! Behavior tests for the typed credential identity and redaction contract.

use super::*;

#[test]
fn redacted_secret_debug_never_renders_plaintext() {
    // Pins: the plaintext handoff cannot reach a log line, event, or model
    // payload through ordinary formatting.
    let secret = RedactedSecret::new("pk_live_task23_example_secret".to_string());

    assert_eq!(format!("{secret:?}"), "RedactedSecret(<redacted>)");
    assert!(!format!("{secret:?}").contains("pk_live"));
    assert_eq!(
        secret.expose_for_outbound_request(),
        "pk_live_task23_example_secret"
    );
}

#[test]
fn credential_kind_names_round_trip_and_reject_unknown() {
    // Pins: stored kind names are a closed set, so an unknown row value fails
    // closed instead of resolving as some default kind.
    for kind in [CredentialKind::ProviderApiKey, CredentialKind::OAuth] {
        assert_eq!(CredentialKind::from_str_exact(kind.as_str()), Some(kind));
    }
    // A retired address-style value and an empty value both fail closed. The
    // dead scheme is assembled rather than written out so this assertion does
    // not reintroduce the vocabulary the hard cut removed.
    let retired_address = format!("vault{}knowledge", "://");
    assert_eq!(CredentialKind::from_str_exact(&retired_address), None);
    assert_eq!(CredentialKind::from_str_exact(""), None);
}

#[test]
fn each_service_actor_permits_exactly_one_operation() {
    // Pins: the durable service-actor allowlist is not a general bypass. Each
    // actor is bound to one operation, so a knowledge workflow can never write
    // credential state and the purge actor can never read material.
    const ALL_OPERATIONS: [CredentialOperation; 5] = [
        CredentialOperation::Create,
        CredentialOperation::Resolve,
        CredentialOperation::Rotate,
        CredentialOperation::Revoke,
        CredentialOperation::Delete,
    ];
    let expected = [
        (
            CredentialServiceActor::KnowledgeSyncListing,
            CredentialOperation::Resolve,
        ),
        (
            CredentialServiceActor::KnowledgeContentFetch,
            CredentialOperation::Resolve,
        ),
        (
            CredentialServiceActor::TenantLifecyclePurge,
            CredentialOperation::Delete,
        ),
    ];

    for (actor, permitted) in expected {
        let principal = CredentialPrincipal::Service { actor };
        for operation in ALL_OPERATIONS {
            assert_eq!(
                principal.permits(operation),
                operation == permitted,
                "{actor:?} must permit only {permitted:?}, not {operation:?}"
            );
        }
        assert_eq!(principal.owner_identity(), None);
    }
}

#[test]
fn caller_principal_records_owner_and_delegation_separately() {
    // Pins: create stamps the acting identity as owner; acting under delegation
    // records the delegator without changing who the owner is.
    let identity_id = Uuid::from_u128(0x2301);
    let delegator = Uuid::from_u128(0x2302);

    let direct = CredentialPrincipal::Caller {
        identity_id,
        delegated_by: None,
    };
    let delegated = CredentialPrincipal::Caller {
        identity_id,
        delegated_by: Some(delegator),
    };

    assert_eq!(direct.owner_identity(), Some(identity_id));
    assert_eq!(delegated.owner_identity(), Some(identity_id));
    assert!(delegated.permits(CredentialOperation::Rotate));
}

#[test]
fn credential_reference_serializes_as_an_opaque_identifier() {
    // Pins: the durable reference persisted in connections, events, and API
    // payloads carries no parseable service/scope address a caller could edit
    // to reach another tenant's credential.
    let reference = CredentialRef::from_uuid(Uuid::from_u128(0x2303));
    let encoded = serde_json::to_string(&reference).expect("reference serializes");

    assert_eq!(
        encoded,
        format!("\"{}\"", Uuid::from_u128(0x2303)),
        "reference must serialize as a bare identifier"
    );
    // The dead address vocabulary is matched by shape rather than restated, so
    // this assertion does not itself reintroduce the retired scheme literal.
    assert!(!encoded.contains("://"));
    assert!(!encoded.contains("knowledge"));
}

#[test]
fn tenant_and_deployment_sources_are_distinct_variants() {
    // Pins: a deployment-owned transport secret and a tenant connection
    // credential are different types of thing, so a tenant resolution can never
    // silently fall back to a deployment credential.
    let tenant = CredentialSource::TenantConnection {
        reference: CredentialRef::from_uuid(Uuid::from_u128(0x2304)),
    };
    let deployment = CredentialSource::Deployment {
        secret: DeploymentSecret::PostmarkServerToken,
    };

    assert_ne!(tenant, deployment);
    assert!(matches!(
        deployment,
        CredentialSource::Deployment {
            secret: DeploymentSecret::PostmarkServerToken
        }
    ));
}
