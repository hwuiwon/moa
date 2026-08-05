//! Migration-only `Session` virtual object for the one-way idle-status cutover.

use moa_core::types::identifiers::SessionId;
use restate_sdk::prelude::*;
use std::time::Duration;

use crate::services::status_migration_dispatcher::StatusMigrationDispatcher;
use crate::services::status_migration_dispatcher::StatusMigrationDispatcherImpl;

const K_META: &str = "meta";
const K_STATUS: &str = "status";
const CUTOVER_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Exact input for the one-way Session VO status migration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusIdleMigrationRequest {
    /// Postgres-enumerated session expected to match the virtual-object key.
    pub session_id: SessionId,
}

/// Verifiable result of migrating one Session VO's raw lifecycle values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusIdleMigrationResponse {
    /// Session whose keyed state was inspected.
    pub session_id: SessionId,
    /// Whether the standalone `status` key was rewritten.
    pub status_rewritten: bool,
    /// Whether `meta.status` was rewritten.
    pub meta_status_rewritten: bool,
    /// Exact status value left in the standalone key, when the key exists.
    pub status: Option<String>,
    /// Exact status value left in the metadata mirror, when metadata exists.
    pub meta_status: Option<String>,
    /// Number of retired values remaining across both keys; always zero on a
    /// successful response.
    pub retired_values_remaining: u8,
}

/// Cutover-only shape of the `Session` virtual object.
///
/// This service is hosted only by the pre-runtime migration endpoint. The
/// product endpoint deliberately does not bind this handler, so the retired
/// lifecycle reader cannot leak into the steady-state deployment.
#[restate_sdk::object]
#[name = "Session"]
pub trait SessionStatusMigrator {
    /// Rewrites raw pre-cutover lifecycle values without decoding `SessionStatus`.
    async fn migrate_status_idle(
        request: Json<SessionStatusIdleMigrationRequest>,
    ) -> Result<Json<SessionStatusIdleMigrationResponse>, HandlerError>;
}

/// Stateless implementation of the migration-only raw Session state rewrite.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStatusMigratorImpl;

impl SessionStatusMigrator for SessionStatusMigratorImpl {
    #[tracing::instrument(skip(self, ctx, request), fields(session_id = %request.0.session_id))]
    // SAFETY: cutover-only handler invoked by the dedicated bootstrap process
    // from a Postgres-enumerated session id. The id must equal the VO key, and
    // the response exposes only the closed lifecycle values it just verified.
    async fn migrate_status_idle(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<SessionStatusIdleMigrationRequest>,
    ) -> Result<Json<SessionStatusIdleMigrationResponse>, HandlerError> {
        let session_id = parse_session_key(ctx.key())?;
        let request = request.into_inner();
        if request.session_id != session_id {
            return Err(TerminalError::new_with_code(
                409,
                format!(
                    "session status migration key {} does not match request {}",
                    session_id, request.session_id
                ),
            )
            .into());
        }

        // Reading raw JSON is the safety property of this endpoint. The hard
        // product enum has no retired spelling and is never constructed here.
        let mut status = ctx
            .get::<Json<serde_json::Value>>(K_STATUS)
            .await?
            .map(Json::into_inner);
        let mut meta = ctx
            .get::<Json<serde_json::Value>>(K_META)
            .await?
            .map(Json::into_inner);
        let migration = migrate_raw_session_status_values(status.as_mut(), meta.as_mut())?;

        if migration.status_rewritten {
            let value = status.as_ref().ok_or_else(|| {
                TerminalError::new("rewritten session status key unexpectedly disappeared")
            })?;
            ctx.set(K_STATUS, Json::from(value.clone()));
        }
        if migration.meta_status_rewritten {
            let value = meta.as_ref().ok_or_else(|| {
                TerminalError::new("rewritten session metadata key unexpectedly disappeared")
            })?;
            ctx.set(K_META, Json::from(value.clone()));
        }

        Ok(Json::from(SessionStatusIdleMigrationResponse {
            session_id,
            status_rewritten: migration.status_rewritten,
            meta_status_rewritten: migration.meta_status_rewritten,
            status: migration.status,
            meta_status: migration.meta_status,
            retired_values_remaining: 0,
        }))
    }
}

/// Builds the endpoint exposed only during the pre-runtime cutover stage.
#[must_use]
pub fn build_status_migration_endpoint() -> Endpoint {
    Endpoint::builder()
        .bind_with_options(
            StatusMigrationDispatcherImpl.serve(),
            cutover_service_options(),
        )
        .bind_with_options(
            SessionStatusMigratorImpl.serve(),
            cutover_service_options().handler(
                "migrate_status_idle",
                HandlerOptions::new().ingress_private(true),
            ),
        )
        .build()
}

fn cutover_service_options() -> ServiceOptions {
    ServiceOptions::new()
        .idempotency_retention(CUTOVER_RETENTION)
        .journal_retention(CUTOVER_RETENTION)
}

fn parse_session_key(key: &str) -> Result<SessionId, HandlerError> {
    uuid::Uuid::parse_str(key).map(SessionId).map_err(|error| {
        TerminalError::new_with_code(400, format!("invalid session id `{key}`: {error}")).into()
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawSessionStatusMigration {
    status_rewritten: bool,
    meta_status_rewritten: bool,
    status: Option<String>,
    meta_status: Option<String>,
}

fn migrate_raw_session_status_values(
    status: Option<&mut serde_json::Value>,
    meta: Option<&mut serde_json::Value>,
) -> Result<RawSessionStatusMigration, HandlerError> {
    let (status_rewritten, status) = match status {
        Some(value) => rewrite_raw_session_status(value, K_STATUS)?,
        None => (false, None),
    };
    let (meta_status_rewritten, meta_status) = match meta {
        Some(value) => {
            let status = value
                .as_object_mut()
                .and_then(|meta| meta.get_mut("status"))
                .ok_or_else(|| {
                    TerminalError::new("session metadata migration requires a status field")
                })?;
            rewrite_raw_session_status(status, "meta.status")?
        }
        None => (false, None),
    };

    Ok(RawSessionStatusMigration {
        status_rewritten,
        meta_status_rewritten,
        status,
        meta_status,
    })
}

fn rewrite_raw_session_status(
    value: &mut serde_json::Value,
    field: &str,
) -> Result<(bool, Option<String>), HandlerError> {
    let status = value.as_str().ok_or_else(|| {
        TerminalError::new(format!(
            "session status migration requires a string at {field}"
        ))
    })?;
    let rewritten = status == "paused";
    if rewritten {
        *value = serde_json::Value::String("idle".to_string());
    } else if !matches!(
        status,
        "created" | "running" | "idle" | "completed" | "cancelled" | "failed"
    ) {
        return Err(TerminalError::new(format!(
            "session status migration found unknown value `{status}` at {field}"
        ))
        .into());
    }
    Ok((rewritten, value.as_str().map(str::to_string)))
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};

    use super::*;

    #[test]
    fn raw_session_status_migration_rewrites_both_keys_exactly_once() {
        // Pins: the cutover reads legacy state as raw JSON and rewrites both the
        // hot key and metadata mirror without constructing `SessionStatus`.
        let mut status = serde_json::json!("paused");
        let mut meta = serde_json::json!({"status": "paused", "title": "paused"});

        let first = migrate_raw_session_status_values(Some(&mut status), Some(&mut meta))
            .expect("legacy raw state should migrate");
        assert!(first.status_rewritten);
        assert!(first.meta_status_rewritten);
        assert_eq!(first.status.as_deref(), Some("idle"));
        assert_eq!(first.meta_status.as_deref(), Some("idle"));
        assert_eq!(status, serde_json::json!("idle"));
        assert_eq!(meta["status"], serde_json::json!("idle"));
        assert_eq!(meta["title"], serde_json::json!("paused"));

        let second = migrate_raw_session_status_values(Some(&mut status), Some(&mut meta))
            .expect("the forward migration should be idempotent");
        assert!(!second.status_rewritten);
        assert!(!second.meta_status_rewritten);
    }

    #[test]
    fn raw_session_status_migration_fails_closed_on_malformed_state() {
        // Pins: unknown or non-string lifecycle state cannot be reported as a
        // successful zero-old-value migration.
        let mut unknown = serde_json::json!("sleeping");
        let mut malformed_meta = serde_json::json!({"status": 7});
        assert!(migrate_raw_session_status_values(Some(&mut unknown), None).is_err());
        assert!(migrate_raw_session_status_values(None, Some(&mut malformed_meta)).is_err());
    }

    #[tokio::test]
    async fn migration_endpoint_exposes_no_product_health_or_turn_handler() {
        // Pins: the pre-runtime endpoint cannot satisfy edge startup or admit
        // product turns before the cutover receipt exists.
        let response = build_status_migration_endpoint().handle(
            Request::builder()
                .uri("/discover")
                .header("accept", "application/vnd.restate.endpointmanifest.v4+json")
                .body(Body::empty())
                .expect("discovery request should build"),
        );
        let bytes = axum::body::to_bytes(Body::new(response.into_body()), usize::MAX)
            .await
            .expect("discovery response should read");
        let manifest: serde_json::Value =
            serde_json::from_slice(&bytes).expect("discovery response should decode");
        let services = manifest["services"]
            .as_array()
            .expect("services should be an array");
        assert_eq!(services.len(), 2);
        assert!(services.iter().all(|service| service["name"] != "Health"));
        let session = services
            .iter()
            .find(|service| service["name"] == "Session")
            .expect("migration endpoint should expose Session");
        assert_eq!(session["handlers"].as_array().map(Vec::len), Some(1));
        assert_eq!(session["handlers"][0]["name"], "migrate_status_idle");
    }
}
