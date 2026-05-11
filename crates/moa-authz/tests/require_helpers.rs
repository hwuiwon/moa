//! Tests for canonical authorization-check helpers.

use httpmock::Method::POST;
use httpmock::prelude::*;
use moa_authz::{
    AuthzCheckError, FgaClient, FgaConfig, require_authz, require_authz_with_delegation,
};
use moa_authz_schema::{ObjectType, Relation};
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

fn user_identity(user_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::User,
        id: user_id,
        tenant_id: Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
            .expect("valid tenant uuid"),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn agent_identity(agent_id: Uuid, acting_on_behalf_of: Option<Uuid>) -> Identity {
    Identity {
        identity_type: IdentityType::Agent,
        id: agent_id,
        tenant_id: Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
            .expect("valid tenant uuid"),
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
async fn require_authz_user_allowed() {
    // Pins: a true OpenFGA check lets the caller continue and issues exactly one check request.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let workspace_id = "workspace-alpha";
    let allowed = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "user:11111111-1111-1111-1111-111111111111",
                "editor",
                "workspace:workspace-alpha",
            ));
        then.status(200).json_body(json!({ "allowed": true }));
    });

    require_authz(
        &fga_client(&server),
        &user_identity(user_id),
        ObjectType::Workspace,
        workspace_id,
        Relation::Editor,
    )
    .await
    .expect("workspace editor check should be allowed");

    allowed.assert_hits(1);
}

#[tokio::test]
async fn require_authz_user_denied() {
    // Pins: a false OpenFGA check returns Forbidden with the exact subject and relation.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let workspace_id = "workspace-alpha";
    let denied = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "user:11111111-1111-1111-1111-111111111111",
                "editor",
                "workspace:workspace-alpha",
            ));
        then.status(200).json_body(json!({ "allowed": false }));
    });

    let error = require_authz(
        &fga_client(&server),
        &user_identity(user_id),
        ObjectType::Workspace,
        workspace_id,
        Relation::Editor,
    )
    .await
    .expect_err("workspace editor check should be forbidden");

    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            object_id,
            relation,
        } => {
            assert_eq!(subject, "user:11111111-1111-1111-1111-111111111111");
            assert_eq!(object_type, ObjectType::Workspace);
            assert_eq!(object_id, "workspace-alpha");
            assert_eq!(relation, Relation::Editor);
        }
        AuthzCheckError::Engine(engine) => {
            panic!("expected Forbidden, got Engine({engine})");
        }
    }
    denied.assert_hits(1);
}

#[tokio::test]
async fn require_authz_api_key_subject_wins_over_user_id() {
    // Pins: API-key-authenticated requests check api_key:<id>, not the owner user.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let api_key_id =
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("valid api key uuid");
    let mut identity = user_identity(user_id);
    identity.api_key_id = Some(api_key_id);
    let api_key_check = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "api_key:22222222-2222-2222-2222-222222222222",
                "member",
                "workspace:workspace-alpha",
            ));
        then.status(200).json_body(json!({ "allowed": true }));
    });

    require_authz(
        &fga_client(&server),
        &identity,
        ObjectType::Workspace,
        "workspace-alpha",
        Relation::Member,
    )
    .await
    .expect("api key workspace member check should be allowed");

    api_key_check.assert_hits(1);
}

#[tokio::test]
async fn require_authz_with_delegation_checks_can_act_as() {
    // Pins: delegated agent calls check can_act_as and then the resource relation.
    let server = MockServer::start();
    let agent_id =
        Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("valid agent uuid");
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let can_act_as = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "user:11111111-1111-1111-1111-111111111111",
                "can_act_as",
                "agent:33333333-3333-3333-3333-333333333333",
            ));
        then.status(200).json_body(json!({ "allowed": true }));
    });
    let resource = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "agent:33333333-3333-3333-3333-333333333333",
                "member",
                "workspace:workspace-alpha",
            ));
        then.status(200).json_body(json!({ "allowed": true }));
    });

    require_authz_with_delegation(
        &fga_client(&server),
        &agent_identity(agent_id, Some(user_id)),
        ObjectType::Workspace,
        "workspace-alpha",
        Relation::Member,
    )
    .await
    .expect("delegated agent workspace member check should be allowed");

    can_act_as.assert_hits(1);
    resource.assert_hits(1);
}

#[tokio::test]
async fn require_authz_with_delegation_rejects_when_can_act_as_denied() {
    // Pins: denied can_act_as stops before the resource check.
    let server = MockServer::start();
    let agent_id =
        Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("valid agent uuid");
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let can_act_as = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "user:11111111-1111-1111-1111-111111111111",
                "can_act_as",
                "agent:33333333-3333-3333-3333-333333333333",
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
        ObjectType::Workspace,
        "workspace-alpha",
        Relation::Member,
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
            assert_eq!(subject, "agent:33333333-3333-3333-3333-333333333333");
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
async fn require_authz_with_delegation_rejects_non_agent_with_behalf() {
    // Pins: acting_on_behalf_of on a non-agent identity is malformed and makes no FGA call.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let behalf_user_id =
        Uuid::parse_str("44444444-4444-4444-4444-444444444444").expect("valid delegated user uuid");
    let mut identity = user_identity(user_id);
    identity.acting_on_behalf_of = Some(behalf_user_id);
    let any_check = server.mock(|when, then| {
        when.method(POST).path("/stores/store-1/check");
        then.status(200).json_body(json!({ "allowed": true }));
    });

    let error = require_authz_with_delegation(
        &fga_client(&server),
        &identity,
        ObjectType::Workspace,
        "workspace-alpha",
        Relation::Member,
    )
    .await
    .expect_err("non-agent delegated identity should be forbidden");

    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            relation,
            ..
        } => {
            assert_eq!(subject, "user:11111111-1111-1111-1111-111111111111");
            assert_eq!(object_type, ObjectType::Agent);
            assert_eq!(relation, Relation::CanActAs);
        }
        AuthzCheckError::Engine(engine) => {
            panic!("expected Forbidden, got Engine({engine})");
        }
    }
    any_check.assert_hits(0);
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
                "editor",
                "workspace:workspace-alpha",
            ));
        then.status(500).body("boom");
    });

    let error = require_authz(
        &fga_client(&server),
        &user_identity(user_id),
        ObjectType::Workspace,
        "workspace-alpha",
        Relation::Editor,
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
