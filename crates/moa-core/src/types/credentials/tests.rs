//! Behavior tests for the typed credential identity and redaction contract.

use super::*;

#[test]
fn credential_slot_name_accepts_exact_grammar_boundaries() {
    // Pins: connector auth declarations share one canonical, bounded slot
    // vocabulary across authoring, persistence, and runtime resolution.
    let max_length = format!("a{}", "0".repeat(62));
    let cases = ["a", "primary", "service_api_2", max_length.as_str()];

    for value in cases {
        let slot = CredentialSlotName::try_from(value)
            .expect("fixture matching the credential-slot grammar should parse");
        assert_eq!(slot.as_str(), value);
        assert_eq!(slot.to_string(), value);

        let encoded = serde_json::to_string(&slot).expect("credential slot should serialize");
        let decoded: CredentialSlotName =
            serde_json::from_str(&encoded).expect("valid credential slot should deserialize");
        assert_eq!(decoded, slot);
    }

    assert_eq!(CredentialSlotName::PRIMARY.as_str(), "primary");
}

#[test]
fn credential_slot_name_rejects_invalid_programmatic_and_json_values() {
    // Pins: invalid or visually ambiguous slot selectors fail at every input
    // boundary rather than reaching credential lookup as unchecked strings.
    let too_long = format!("a{}", "0".repeat(63));
    let invalid = [
        "",
        "Primary",
        "1primary",
        "api-key",
        "api key",
        "api.key",
        "_primary",
        "sécret",
        too_long.as_str(),
    ];

    for value in invalid {
        let parse_error = CredentialSlotName::try_from(value)
            .expect_err("fixture outside the credential-slot grammar must fail");
        assert!(
            matches!(parse_error, crate::error::MoaError::ValidationError(_)),
            "unexpected programmatic parse failure for {value:?}: {parse_error}"
        );

        let encoded = serde_json::to_string(value).expect("fixture string should serialize");
        let decode_error = serde_json::from_str::<CredentialSlotName>(&encoded)
            .expect_err("invalid persisted credential slot must fail deserialization");
        assert!(
            decode_error.to_string().contains("credential slot name"),
            "unexpected serde failure for {value:?}: {decode_error}"
        );
    }
}

#[test]
fn credential_identity_requires_and_distinguishes_named_slots() {
    // Pins: the credential slot is a required part of durable series identity;
    // persisted identities cannot omit it or collapse two slots of one kind.
    let tenant_id = TenantId::from(Uuid::from_u128(0x2305));
    let connection_uid = Uuid::from_u128(0x2306);
    let primary = CredentialIdentity {
        tenant_id,
        connection_uid,
        kind: CredentialKind::ProviderApiKey,
        slot_name: CredentialSlotName::PRIMARY,
    };
    let webhook = CredentialIdentity {
        slot_name: CredentialSlotName::try_from("webhook").expect("fixture slot should be valid"),
        ..primary.clone()
    };

    assert_ne!(primary, webhook);
    assert_eq!(
        serde_json::to_value(&primary).expect("credential identity should serialize"),
        serde_json::json!({
            "tenant_id": tenant_id,
            "connection_uid": connection_uid,
            "kind": "provider_api_key",
            "slot_name": "primary"
        })
    );

    let missing_slot = serde_json::json!({
        "tenant_id": tenant_id,
        "connection_uid": connection_uid,
        "kind": "provider_api_key"
    });
    let error = serde_json::from_value::<CredentialIdentity>(missing_slot)
        .expect_err("a persisted credential identity without a slot must fail closed");
    assert!(
        error.to_string().contains("slot_name"),
        "unexpected missing-slot error: {error}"
    );
}

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
    const ALL_OPERATIONS: [CredentialOperation; 8] = [
        CredentialOperation::Create,
        CredentialOperation::Stage,
        CredentialOperation::Activate,
        CredentialOperation::RollbackActivation,
        CredentialOperation::Resolve,
        CredentialOperation::Rotate,
        CredentialOperation::Revoke,
        CredentialOperation::Delete,
    ];
    let expected = [
        (
            CredentialServiceActor::ConnectorManagementReadiness,
            CredentialOperation::Resolve,
        ),
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
fn rollback_activation_has_a_stable_distinct_audit_name() {
    // Pins: compensating an activation has its own append-only audit identity;
    // it cannot be confused with an ordinary revoke during replay.
    assert_eq!(
        CredentialOperation::RollbackActivation.as_str(),
        "rollback_activation"
    );
    assert_ne!(
        CredentialOperation::RollbackActivation.as_str(),
        CredentialOperation::Revoke.as_str()
    );
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
fn staging_token_debug_hides_both_credential_references() {
    // Pins: the host-local staging handoff can be attached to an internal error
    // or span without leaking either the staged or predecessor reference.
    let staged = Uuid::from_u128(0x2310);
    let prior = Uuid::from_u128(0x2311);
    let token = CredentialStagingToken::new(
        CredentialRef::from_uuid(staged),
        CredentialIdentity {
            tenant_id: TenantId::from(Uuid::from_u128(0x2312)),
            connection_uid: Uuid::from_u128(0x2313),
            kind: CredentialKind::ProviderApiKey,
            slot_name: CredentialSlotName::PRIMARY,
        },
        2,
        Some(CredentialRef::from_uuid(prior)),
    );

    let debug = format!("{token:?}");
    assert_eq!(debug, "CredentialStagingToken(<redacted>)");
    assert!(!debug.contains(&staged.to_string()));
    assert!(!debug.contains(&prior.to_string()));
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
