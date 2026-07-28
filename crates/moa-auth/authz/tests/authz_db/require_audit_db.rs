//! DB-backed authorization audit coverage for required authz checks.

use std::time::Duration;

use httpmock::Method::POST;
use httpmock::prelude::*;
use moa_authz::{AuthzCheckError, FgaClient, FgaConfig, SecurityAudit, require_authz};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::{
    traits::{Identity, IdentityType},
    types::identifiers::TenantId,
};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

const DEFAULT_DATABASE_URL: &str = "postgres://moa_owner:dev@localhost:10040/moa";

/// `(count, class_uid, activity_id, severity_id, actor_user_uid, target_resource_uid)`.
type AuditEventRow = (i64, i32, i32, i32, Option<String>, Option<String>);

fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn migrated_ocsf_pool() -> PgPool {
    let schema_name = format!("authz_audit_test_{}", Uuid::new_v4().simple());
    let search_path = format!("{}, public", quote_identifier(&schema_name));
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&test_database_url())
        .await
        .expect(
            "test Postgres should be reachable; start the compose Postgres or set MOA_DATABASE_URL",
        );
    moa_migrations::run_ocsf_schema(&pool, &schema_name)
        .await
        .expect("OCSF baseline should apply");
    pool
}

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

/// Builds a client whose authorization decisions are audited.
///
/// The audit runtime is returned alongside so the caller keeps it alive; a
/// dropped runtime aborts its writer, which is exactly the behaviour that makes
/// ownership visible.
fn audited_fga_client(
    server: &MockServer,
    pool: &PgPool,
    emit_allows: bool,
) -> (FgaClient, moa_ocsf::AuditRuntime) {
    let audit = moa_ocsf::AuditRuntime::start(pool.clone()).expect("audit runtime should start");
    let client = fga_client(server).with_security_audit(SecurityAudit {
        pool: pool.clone(),
        emitter: audit.emitter(),
        emit_allows,
    });
    (client, audit)
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
async fn denied_require_authz_persists_ocsf_security_event_db() {
    // Pins: a definitive FGA deny from require_authz records the configured OCSF
    // Authorization event before returning Forbidden, so denied decisions remain
    // fail-closed when security audit is configured.
    let pool = migrated_ocsf_pool().await;

    let server = MockServer::start();
    let tenant_id = Uuid::from_u128(0x7001);
    let user_id = Uuid::from_u128(0x7002);
    let session_id = Uuid::from_u128(0x7003);
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: user_id,
        tenant_id: TenantId::from(tenant_id),
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let denied = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store-1/check")
            .json_body(check_body(
                &format!("operator:{user_id}"),
                "participant",
                &format!("session:{session_id}"),
            ));
        then.status(200).json_body(json!({ "allowed": false }));
    });

    let (client, _audit) = audited_fga_client(&server, &pool, false);
    let error = require_authz(
        &client,
        &identity,
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .expect_err("denied FGA decision should be forbidden");
    assert!(
        matches!(
            error,
            AuthzCheckError::Forbidden {
                object_type: ObjectType::Session,
                relation: Relation::Participant,
                ..
            }
        ),
        "expected Forbidden session participant denial, got {error:?}"
    );
    denied.assert_hits(1);

    // The denial audit is written before the Forbidden result is returned.
    let mut row: Option<AuditEventRow> = None;
    for _ in 0..50 {
        row = sqlx::query_as(
            r#"
            SELECT COUNT(*) OVER (), class_uid, activity_id, severity_id,
                   actor_user_uid, target_resource_uid
            FROM security_events
            WHERE tenant_id = $1
            "#,
        )
        .bind(tenant_id)
        .fetch_optional(&pool)
        .await
        .expect("query denied authz security event row");
        if row.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let row = row.expect("denied authz security event is persisted asynchronously");
    assert_eq!(row.0, 1, "one denied authz event should be persisted");
    assert_eq!(row.1, 3003, "deny audit must use Authorization class");
    assert_eq!(row.2, 99, "deny audit maps to authz activity Other");
    assert_eq!(row.3, 2, "deny audit must be Low severity");
    assert_eq!(
        row.4.as_deref(),
        Some(format!("operator:{user_id}").as_str())
    );
    assert_eq!(
        row.5.as_deref(),
        Some(format!("session:{session_id}").as_str())
    );
}
