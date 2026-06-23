//! Tests for canonical authorization-check helpers.

use httpmock::Method::POST;
use httpmock::prelude::*;
use moa_authz::{
    AuthzCheckError, FgaClient, FgaConfig, require_authz, require_authz_with_delegation,
};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::TenantId;
use moa_core::traits::{Identity, IdentityType};
use serde_json::json;
use uuid::Uuid;

fn fga_client(server: &MockServer) -> FgaClient {
    FgaClient::new(FgaConfig {
        url: server.url(""),
        preshared_key: "test-preshared".to_string(),
        store_id: "store-1".to_string(),
        model_id: "model-1".to_string(),
        timeout_ms: 5_000,
    })
    .expect("test FGA config should be valid")
}

fn tenant_id() -> Uuid {
    Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("valid tenant uuid")
}

fn user_identity(user_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::User,
        id: user_id,
        tenant_id: TenantId::from(tenant_id()),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn contact_identity(contact_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::Contact,
        id: contact_id,
        tenant_id: TenantId::from(tenant_id()),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn agent_identity(agent_id: Uuid, acting_on_behalf_of: Option<Uuid>) -> Identity {
    Identity {
        identity_type: IdentityType::Agent,
        id: agent_id,
        tenant_id: TenantId::from(tenant_id()),
        api_key_id: None,
        acting_on_behalf_of,
    }
}

fn check_body(user: &str, relation: &str, object: &str) -> serde_json::Value {
    json!({
        "authorization_model_id": "model-1",
        "tuple_key": {
            "user": user,
            "relation": relation,
            "object": object,
        },
    })
}

#[tokio::test]
async fn workspace_admin_administers_tenant() {
    // Pins: runtime tenant administration checks ask OpenFGA for tenant#admin.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let tenant_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let allowed = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "user:11111111-1111-1111-1111-111111111111",
                "admin",
                "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            ));
        then.status(200).json_body(json!({ "allowed": true }));
    });

    require_authz(
        &fga_client(&server),
        &user_identity(user_id),
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
    .expect("workspace-inherited tenant admin check should be allowed by FGA");

    allowed.assert_hits(1);
}

#[tokio::test]
async fn tenant_admin_participates_in_tenant_session() {
    // Pins: tenant admins read tenant sessions through session#participant.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let session_id = "22222222-2222-2222-2222-222222222222";
    let allowed = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "user:11111111-1111-1111-1111-111111111111",
                "participant",
                "session:22222222-2222-2222-2222-222222222222",
            ));
        then.status(200).json_body(json!({ "allowed": true }));
    });

    require_authz(
        &fga_client(&server),
        &user_identity(user_id),
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .expect("tenant admin session participant check should be allowed by FGA");

    allowed.assert_hits(1);
}

#[tokio::test]
async fn tenant_operator_cannot_admin_tenant() {
    // Pins: operator privileges are not admin privileges unless OpenFGA says admin.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let denied = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "user:11111111-1111-1111-1111-111111111111",
                "admin",
                "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            ));
        then.status(200).json_body(json!({ "allowed": false }));
    });

    let error = require_authz(
        &fga_client(&server),
        &user_identity(user_id),
        ObjectType::Tenant,
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        Relation::Admin,
    )
    .await
    .expect_err("tenant operator admin check should be forbidden");

    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            object_id,
            relation,
        } => {
            assert_eq!(subject, "user:11111111-1111-1111-1111-111111111111");
            assert_eq!(object_type, ObjectType::Tenant);
            assert_eq!(object_id, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
            assert_eq!(relation, Relation::Admin);
        }
        AuthzCheckError::Engine(engine) => {
            panic!("expected Forbidden, got Engine({engine})");
        }
    }
    denied.assert_hits(1);
}

#[tokio::test]
async fn contact_participates_only_in_own_session() {
    // Pins: contact credentials use contact:<id> and rely on session#participant grants.
    let server = MockServer::start();
    let contact_id =
        Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("valid contact uuid");
    let own_session = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "contact:33333333-3333-3333-3333-333333333333",
                "participant",
                "session:22222222-2222-2222-2222-222222222222",
            ));
        then.status(200).json_body(json!({ "allowed": true }));
    });
    let other_session = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "contact:33333333-3333-3333-3333-333333333333",
                "participant",
                "session:44444444-4444-4444-4444-444444444444",
            ));
        then.status(200).json_body(json!({ "allowed": false }));
    });

    require_authz(
        &fga_client(&server),
        &contact_identity(contact_id),
        ObjectType::Session,
        "22222222-2222-2222-2222-222222222222",
        Relation::Participant,
    )
    .await
    .expect("contact should participate in its granted session");

    let error = require_authz(
        &fga_client(&server),
        &contact_identity(contact_id),
        ObjectType::Session,
        "44444444-4444-4444-4444-444444444444",
        Relation::Participant,
    )
    .await
    .expect_err("contact should not participate in another session");

    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            object_id,
            relation,
        } => {
            assert_eq!(subject, "contact:33333333-3333-3333-3333-333333333333");
            assert_eq!(object_type, ObjectType::Session);
            assert_eq!(object_id, "44444444-4444-4444-4444-444444444444");
            assert_eq!(relation, Relation::Participant);
        }
        AuthzCheckError::Engine(engine) => {
            panic!("expected Forbidden, got Engine({engine})");
        }
    }
    own_session.assert_hits(1);
    other_session.assert_hits(1);
}

#[tokio::test]
async fn api_key_subject_still_narrows_access() {
    // Pins: API-key-authenticated requests check api_key:<id>, not the owner user.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let api_key_id =
        Uuid::parse_str("55555555-5555-5555-5555-555555555555").expect("valid api key uuid");
    let mut identity = user_identity(user_id);
    identity.api_key_id = Some(api_key_id);
    let api_key_check = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "api_key:55555555-5555-5555-5555-555555555555",
                "operator",
                "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            ));
        then.status(200).json_body(json!({ "allowed": true }));
    });

    require_authz(
        &fga_client(&server),
        &identity,
        ObjectType::Tenant,
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        Relation::Operator,
    )
    .await
    .expect("api key tenant operator check should be allowed");

    api_key_check.assert_hits(1);
}

#[tokio::test]
async fn delegated_agent_still_requires_can_act_as() {
    // Pins: denied can_act_as stops before the resource check.
    let server = MockServer::start();
    let agent_id =
        Uuid::parse_str("66666666-6666-6666-6666-666666666666").expect("valid agent uuid");
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let can_act_as = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "user:11111111-1111-1111-1111-111111111111",
                "can_act_as",
                "agent:66666666-6666-6666-6666-666666666666",
            ));
        then.status(200).json_body(json!({ "allowed": false }));
    });
    let resource = server.mock(|when, then| {
        when.method(POST).path("/stores/store-1/check");
        then.status(200).json_body(json!({ "allowed": true }));
    });

    let error = require_authz_with_delegation(
        &fga_client(&server),
        &agent_identity(agent_id, Some(user_id)),
        ObjectType::Session,
        "22222222-2222-2222-2222-222222222222",
        Relation::Participant,
    )
    .await
    .expect_err("delegated agent without can_act_as should be forbidden");

    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            relation,
            ..
        } => {
            assert_eq!(subject, "agent:66666666-6666-6666-6666-666666666666");
            assert_eq!(object_type, ObjectType::Agent);
            assert_eq!(relation, Relation::CanActAs);
        }
        AuthzCheckError::Engine(engine) => {
            panic!("expected Forbidden, got Engine({engine})");
        }
    }
    can_act_as.assert_hits(1);
    resource.assert_hits(0);
}

#[tokio::test]
async fn require_authz_engine_error_propagates() {
    // Pins: OpenFGA transport/server errors become Engine errors, not allows.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let failing = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "user:11111111-1111-1111-1111-111111111111",
                "operator",
                "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            ));
        then.status(500).body("boom");
    });

    let error = require_authz(
        &fga_client(&server),
        &user_identity(user_id),
        ObjectType::Tenant,
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        Relation::Operator,
    )
    .await
    .expect_err("server error should propagate as Engine");

    match error {
        AuthzCheckError::Engine(engine) => {
            assert_eq!(engine.to_string(), "FGA HTTP error: 500: boom");
        }
        AuthzCheckError::Forbidden { subject, .. } => {
            panic!("expected Engine, got Forbidden for {subject}");
        }
    }
    failing.assert_hits(1);
}
