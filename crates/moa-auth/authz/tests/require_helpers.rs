//! Tests for canonical authorization-check helpers.

use httpmock::Method::POST;
use httpmock::prelude::*;
use moa_authz::{
    AuthzCheckError, FgaClient, FgaConfig, require_authz, require_authz_with_delegation,
    require_authz_with_delegation_batch,
};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::identifiers::TenantId;
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
        identity_type: IdentityType::Operator,
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

fn check_body(operator: &str, relation: &str, object: &str) -> serde_json::Value {
    json!({
        "authorization_model_id": "model-1",
        "tuple_key": {
            "user": operator,
            "relation": relation,
            "object": object,
        },
    })
}

/// Expected OpenFGA `/batch-check` request body for an ordered list of tuples.
fn batch_check_body(items: &[(&str, &str, &str)]) -> serde_json::Value {
    let checks: Vec<serde_json::Value> = items
        .iter()
        .enumerate()
        .map(|(index, (user, relation, object))| {
            json!({
                "tuple_key": { "user": user, "relation": relation, "object": object },
                "correlation_id": format!("c{index}"),
            })
        })
        .collect();
    json!({ "authorization_model_id": "model-1", "checks": checks })
}

/// OpenFGA `/batch-check` response body keyed by correlation id.
fn batch_result(entries: &[(&str, bool)]) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    for (correlation_id, allowed) in entries {
        result.insert((*correlation_id).to_string(), json!({ "allowed": allowed }));
    }
    json!({ "result": result })
}

#[tokio::test]
async fn tenant_admin_administers_tenant() {
    // Pins: runtime tenant administration checks ask OpenFGA for tenant#admin.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let tenant_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let allowed = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "operator:11111111-1111-1111-1111-111111111111",
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
    .expect("tenant admin check should be allowed by FGA");

    allowed.assert_hits(1);
}

#[tokio::test]
async fn workspace_admin_resolution_still_checks_target_tenant_admin() {
    // Pins: workspace-admin super-admin behavior lives in OpenFGA inheritance;
    // handlers still ask for tenant#admin on the target tenant.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("99999999-9999-9999-9999-999999999999")
        .expect("valid workspace admin user uuid");
    let tenant_id = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    let allowed = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "operator:99999999-9999-9999-9999-999999999999",
                "admin",
                "tenant:bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
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
    .expect("workspace admin inherited tenant admin check should be allowed by FGA");

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
                "operator:11111111-1111-1111-1111-111111111111",
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
                "operator:11111111-1111-1111-1111-111111111111",
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
            assert_eq!(subject, "operator:11111111-1111-1111-1111-111111111111");
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
async fn tenant_operator_check_uses_operator_relation() {
    // Pins: product control-plane reads continue checking tenant#operator.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("12121212-1212-1212-1212-121212121212").expect("valid user uuid");
    let tenant_id = "cdcdcdcd-cdcd-cdcd-cdcd-cdcdcdcdcdcd";
    let allowed = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "operator:12121212-1212-1212-1212-121212121212",
                "operator",
                "tenant:cdcdcdcd-cdcd-cdcd-cdcd-cdcdcdcdcdcd",
            ));
        then.status(200).json_body(json!({ "allowed": true }));
    });

    require_authz(
        &fga_client(&server),
        &user_identity(user_id),
        ObjectType::Tenant,
        tenant_id,
        Relation::Operator,
    )
    .await
    .expect("tenant operator check should be allowed by FGA");

    allowed.assert_hits(1);
}

#[tokio::test]
async fn contact_cannot_admin_tenant() {
    // Pins: contact credentials cannot satisfy tenant admin checks unless FGA
    // explicitly grants the impossible contact subject, which the model does not.
    let server = MockServer::start();
    let contact_id =
        Uuid::parse_str("34343434-3434-3434-3434-343434343434").expect("valid contact uuid");
    let tenant_id = "efefefef-efef-efef-efef-efefefefefef";
    let denied = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                "contact:34343434-3434-3434-3434-343434343434",
                "admin",
                "tenant:efefefef-efef-efef-efef-efefefefefef",
            ));
        then.status(200).json_body(json!({ "allowed": false }));
    });

    let error = require_authz(
        &fga_client(&server),
        &contact_identity(contact_id),
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
    .expect_err("contact tenant admin check should be forbidden");

    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            object_id,
            relation,
        } => {
            assert_eq!(subject, "contact:34343434-3434-3434-3434-343434343434");
            assert_eq!(object_type, ObjectType::Tenant);
            assert_eq!(object_id, "efefefef-efef-efef-efef-efefefefefef");
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
    // Pins: a denied can_act_as yields Forbidden on the agent's can_act_as
    // relation. The delegation and resource checks are resolved in a single
    // batch-check round trip; the denied delegation short-circuits the result
    // regardless of the resource decision.
    let server = MockServer::start();
    let agent_id =
        Uuid::parse_str("66666666-6666-6666-6666-666666666666").expect("valid agent uuid");
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let batch = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/batch-check")
            .json_body(batch_check_body(&[
                (
                    "operator:11111111-1111-1111-1111-111111111111",
                    "can_act_as",
                    "agent:66666666-6666-6666-6666-666666666666",
                ),
                (
                    "agent:66666666-6666-6666-6666-666666666666",
                    "participant",
                    "session:22222222-2222-2222-2222-222222222222",
                ),
            ]));
        then.status(200)
            .json_body(batch_result(&[("c0", false), ("c1", true)]));
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
    batch.assert_hits(1);
}

#[tokio::test]
async fn delegated_agent_acts_as_agent_subject_not_user() {
    // Pins: a granted can_act_as lets the delegated agent through, but the resource
    // check still runs as agent:<id> — delegation must NOT borrow the user's perms.
    // The batch-check body carries the agent subject for the resource tuple; a
    // regression that used the user subject would fail to match the mock body and
    // the call would error instead of succeeding.
    let server = MockServer::start();
    let agent_id =
        Uuid::parse_str("66666666-6666-6666-6666-666666666666").expect("valid agent uuid");
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let session_id = "88888888-8888-8888-8888-888888888888";

    let batch = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/batch-check")
            .json_body(batch_check_body(&[
                (
                    "operator:11111111-1111-1111-1111-111111111111",
                    "can_act_as",
                    "agent:66666666-6666-6666-6666-666666666666",
                ),
                (
                    "agent:66666666-6666-6666-6666-666666666666",
                    "participant",
                    "session:88888888-8888-8888-8888-888888888888",
                ),
            ]));
        then.status(200)
            .json_body(batch_result(&[("c0", true), ("c1", true)]));
    });

    require_authz_with_delegation(
        &fga_client(&server),
        &agent_identity(agent_id, Some(user_id)),
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .expect("granted can_act_as agent should pass the resource check as itself");

    batch.assert_hits(1);
}

#[tokio::test]
async fn batch_authorization_checks_multiple_objects_in_input_order() {
    // Pins: one public batch authorization call sends every requested object in
    // caller order and succeeds only when OpenFGA allows every decision.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("13131313-1313-1313-1313-131313131313").expect("valid user uuid");
    let object_ids = vec![
        "14141414-1414-1414-1414-141414141414".to_string(),
        "15151515-1515-1515-1515-151515151515".to_string(),
        "16161616-1616-1616-1616-161616161616".to_string(),
    ];
    let batch = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/batch-check")
            .json_body(batch_check_body(&[
                (
                    "operator:13131313-1313-1313-1313-131313131313",
                    "participant",
                    "session:14141414-1414-1414-1414-141414141414",
                ),
                (
                    "operator:13131313-1313-1313-1313-131313131313",
                    "participant",
                    "session:15151515-1515-1515-1515-151515151515",
                ),
                (
                    "operator:13131313-1313-1313-1313-131313131313",
                    "participant",
                    "session:16161616-1616-1616-1616-161616161616",
                ),
            ]));
        then.status(200)
            .json_body(batch_result(&[("c0", true), ("c1", true), ("c2", true)]));
    });

    require_authz_with_delegation_batch(
        &fga_client(&server),
        &user_identity(user_id),
        ObjectType::Session,
        &object_ids,
        Relation::Participant,
    )
    .await
    .expect("all allowed batch decisions should authorize every requested session");

    batch.assert_hits(1);
}

#[tokio::test]
async fn delegated_batch_fails_closed_on_the_first_denied_object() {
    // Pins: delegated batches combine can_act_as and ordered resource checks in
    // one request, then reject the first denied resource even when other checks
    // are allowed.
    let server = MockServer::start();
    let agent_id =
        Uuid::parse_str("17171717-1717-1717-1717-171717171717").expect("valid agent uuid");
    let user_id = Uuid::parse_str("18181818-1818-1818-1818-181818181818").expect("valid user uuid");
    let object_ids = vec![
        "19191919-1919-1919-1919-191919191919".to_string(),
        "20202020-2020-2020-2020-202020202020".to_string(),
        "21212121-2121-2121-2121-212121212121".to_string(),
    ];
    let batch = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/batch-check")
            .json_body(batch_check_body(&[
                (
                    "operator:18181818-1818-1818-1818-181818181818",
                    "can_act_as",
                    "agent:17171717-1717-1717-1717-171717171717",
                ),
                (
                    "agent:17171717-1717-1717-1717-171717171717",
                    "participant",
                    "session:19191919-1919-1919-1919-191919191919",
                ),
                (
                    "agent:17171717-1717-1717-1717-171717171717",
                    "participant",
                    "session:20202020-2020-2020-2020-202020202020",
                ),
                (
                    "agent:17171717-1717-1717-1717-171717171717",
                    "participant",
                    "session:21212121-2121-2121-2121-212121212121",
                ),
            ]));
        then.status(200).json_body(batch_result(&[
            ("c0", true),
            ("c1", true),
            ("c2", false),
            ("c3", true),
        ]));
    });

    let error = require_authz_with_delegation_batch(
        &fga_client(&server),
        &agent_identity(agent_id, Some(user_id)),
        ObjectType::Session,
        &object_ids,
        Relation::Participant,
    )
    .await
    .expect_err("a denied resource in a mixed batch must fail closed");

    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            object_id,
            relation,
        } => {
            assert_eq!(subject, "agent:17171717-1717-1717-1717-171717171717");
            assert_eq!(object_type, ObjectType::Session);
            assert_eq!(object_id, "20202020-2020-2020-2020-202020202020");
            assert_eq!(relation, Relation::Participant);
        }
        AuthzCheckError::Engine(engine) => {
            panic!("expected Forbidden, got Engine({engine})");
        }
    }
    batch.assert_hits(1);
}

#[tokio::test]
async fn non_agent_identity_cannot_claim_delegation() {
    // Pins: only Agent identities may carry acting_on_behalf_of. A user smuggling a
    // delegation field is rejected before any FGA call is made.
    let server = MockServer::start();
    let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid user uuid");
    let other_user =
        Uuid::parse_str("77777777-7777-7777-7777-777777777777").expect("valid user uuid");
    let mut identity = user_identity(user_id);
    identity.acting_on_behalf_of = Some(other_user);
    // Any FGA traffic at all means the guard let a forged delegation through.
    let any_check = server.mock(|when, then| {
        when.method(POST).path("/stores/store-1/check");
        then.status(200).json_body(json!({ "allowed": true }));
    });

    let error = require_authz_with_delegation(
        &fga_client(&server),
        &identity,
        ObjectType::Session,
        "22222222-2222-2222-2222-222222222222",
        Relation::Participant,
    )
    .await
    .expect_err("a non-agent identity claiming delegation must be forbidden");

    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            object_id,
            relation,
        } => {
            assert_eq!(subject, "operator:11111111-1111-1111-1111-111111111111");
            assert_eq!(object_type, ObjectType::Agent);
            assert_eq!(object_id, user_id.to_string());
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
                "operator:11111111-1111-1111-1111-111111111111",
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
