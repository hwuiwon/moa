//! Live SCIM smoke tests. Ignored unless explicitly enabled.

use reqwest::StatusCode;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_SCIM_TESTS=1 and a running SCIM endpoint"]
async fn scim_user_lifecycle_smoke() {
    if std::env::var("MOA_RUN_LIVE_SCIM_TESTS").as_deref() != Ok("1") {
        return;
    }

    let base_url = required_env("MOA_TEST_SCIM_BASE_URL");
    let token = required_env("MOA_TEST_SCIM_TOKEN");
    let client = reqwest::Client::new();
    let external_id = format!("codex-{}", Uuid::new_v4());
    let email = format!("{external_id}@example.com");

    let created = client
        .post(format!("{base_url}/Users"))
        .bearer_auth(&token)
        .json(&json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
            "externalId": external_id,
            "userName": email,
            "name": { "givenName": "SCIM", "familyName": "Smoke" },
            "displayName": "SCIM Smoke",
            "emails": [{ "value": email, "primary": true, "type": "work" }],
            "active": true
        }))
        .send()
        .await
        .expect("create SCIM user");
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body: serde_json::Value = created.json().await.expect("created user json");
    let id = created_body["id"].as_str().expect("created id").to_string();
    assert_eq!(created_body["externalId"], external_id);
    assert_eq!(created_body["active"], true);

    let listed = client
        .get(format!(
            "{base_url}/Users?filter=externalId%20eq%20%22{external_id}%22"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .expect("list SCIM users");
    assert_eq!(listed.status(), StatusCode::OK);
    let list_body: serde_json::Value = listed.json().await.expect("list json");
    assert_eq!(list_body["totalResults"], 1);
    assert_eq!(list_body["Resources"][0]["id"], id);

    let patched = client
        .patch(format!("{base_url}/Users/{id}"))
        .bearer_auth(&token)
        .json(&json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
            "Operations": [{ "op": "replace", "path": "active", "value": false }]
        }))
        .send()
        .await
        .expect("patch SCIM user inactive");
    assert_eq!(patched.status(), StatusCode::OK);
    let patched_body: serde_json::Value = patched.json().await.expect("patched user json");
    assert_eq!(patched_body["id"], id);
    assert_eq!(patched_body["active"], false);

    let deleted = client
        .delete(format!("{base_url}/Users/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("delete SCIM user");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is required when MOA_RUN_LIVE_SCIM_TESTS=1"))
}
