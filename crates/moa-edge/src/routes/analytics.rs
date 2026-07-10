//! Public analytics, experiments, admin, lineage, and privacy routes.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use moa_analytics::{AnalyticsError, AnalyticsService};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::TenantId;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::analytics::AnalyticsQueryRequest;
use serde::Deserialize;
use uuid::Uuid;

use super::{
    AppState, RouteTranslation, authenticate_direct_request, parse_json_body, require_direct_authz,
    route_error, translate_json_object_with_tenant_id,
};

#[derive(Debug, Deserialize)]
pub(super) struct AnalyticsTargetQuery {
    tenant_id: Option<Uuid>,
}

/// Handles analytics catalog reads at the edge.
#[tracing::instrument(skip(state, headers))]
pub(super) async fn handle_catalog(
    State(state): State<AppState>,
    Query(query): Query<AnalyticsTargetQuery>,
    headers: HeaderMap,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/analytics/catalog").await {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    let tenant_id = match analytics_target_tenant(&identity, query.tenant_id.map(TenantId::from)) {
        Ok(tenant_id) => tenant_id,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    Json(AnalyticsService::new().catalog()).into_response()
}

/// Handles tenant-scoped analytics queries at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub(super) async fn handle_query(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/analytics/query").await
    {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let mut request: AnalyticsQueryRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let tenant_id = match analytics_target_tenant(&identity, request.tenant_id) {
        Ok(tenant_id) => tenant_id,
        Err(response) => return response,
    };
    request.tenant_id = Some(tenant_id);
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    // The edge request path is read-only for both backends. Materialized-view
    // refresh is owned by the durable maintenance cron (single-flighted under a
    // Postgres advisory lock), not triggered per request; the query serves the
    // current read-model state and reports its freshness.
    let result = if let Some(clickhouse) = state.clickhouse_analytics.as_deref() {
        let response = AnalyticsService::clickhouse()
            .query_clickhouse(clickhouse, request)
            .await;
        match response {
            Ok(mut response) => {
                response.metadata.read_model_updated_at =
                    clickhouse_read_model_updated_at(&state.pool).await;
                Ok(response)
            }
            Err(error) => Err(error),
        }
    } else {
        let response = AnalyticsService::new()
            .with_statement_timeout_ms(state.config.analytics.statement_timeout_ms)
            .query(&state.pool, request)
            .await;
        match response {
            Ok(mut response) => {
                response.metadata.read_model_updated_at =
                    postgres_read_model_updated_at(&state.pool).await;
                Ok(response)
            }
            Err(error) => Err(error),
        }
    };
    match result {
        Ok(response) => Json(response).into_response(),
        Err(error) => analytics_error_response(error),
    }
}

/// Freshness of the ClickHouse read models: the most-stale export cursor
/// across all exported tables. `None` when the exporter has not run yet (or
/// the cursor table is missing) — absence of freshness data must not fail the
/// query.
async fn clickhouse_read_model_updated_at(
    pool: &sqlx::PgPool,
) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT MIN(cursor_ts) FROM analytics.clickhouse_export_state")
        .fetch_one(pool)
        .await
        .ok()
        .flatten()
}

/// Freshness of the Postgres materialized-view read models: the last successful
/// maintenance refresh. `None` when no refresh has succeeded yet (or the state
/// table is missing) — absence of freshness data must not fail the query.
async fn postgres_read_model_updated_at(
    pool: &sqlx::PgPool,
) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar(
        "SELECT last_success_at FROM analytics.materialized_view_refresh_state WHERE id",
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten()
}

fn analytics_target_tenant(
    identity: &Identity,
    tenant_id: Option<TenantId>,
) -> Result<TenantId, Response> {
    if identity.identity_type == IdentityType::Contact {
        return Err((StatusCode::FORBIDDEN, "forbidden").into_response());
    }
    Ok(tenant_id.unwrap_or(identity.tenant_id))
}

fn analytics_error_response(error: AnalyticsError) -> Response {
    match error {
        AnalyticsError::ConflictingTenantFilter => {
            (StatusCode::FORBIDDEN, error.to_string()).into_response()
        }
        AnalyticsError::Execution(_) => route_error(error),
        other => (StatusCode::BAD_REQUEST, other.to_string()).into_response(),
    }
}

pub(super) fn translate(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> Option<RouteTranslation> {
    if *method != Method::POST {
        return None;
    }

    let translation = match uri.path() {
        "/v1/experiments/generate-plan" => {
            translate_json_object_with_tenant_id(body, "/Experiments/generate_plan", tenant_id)
        }
        "/v1/experiments/run-plan" => {
            translate_json_object_with_tenant_id(body, "/Experiments/run", tenant_id)
        }
        "/v1/experiments/status" => {
            translate_json_object_with_tenant_id(body, "/Experiments/status", tenant_id)
        }
        "/v1/experiments/list" => {
            translate_json_object_with_tenant_id(body, "/Experiments/list", tenant_id)
        }
        "/v1/experiments/plans/list" => {
            translate_json_object_with_tenant_id(body, "/Experiments/list_plans", tenant_id)
        }
        "/v1/experiments/trials" => {
            translate_json_object_with_tenant_id(body, "/Experiments/trials", tenant_id)
        }
        "/v1/experiments/trial-status" => {
            translate_json_object_with_tenant_id(body, "/Experiments/trial_status", tenant_id)
        }
        "/v1/experiments/cancel" => {
            translate_json_object_with_tenant_id(body, "/Experiments/cancel", tenant_id)
        }
        "/v1/experiments/propose-improvements" => translate_json_object_with_tenant_id(
            body,
            "/Experiments/propose_improvements",
            tenant_id,
        ),
        "/v1/experiments/scores" => {
            translate_json_object_with_tenant_id(body, "/Experiments/scores", tenant_id)
        }
        "/v1/experiments/compare" => {
            translate_json_object_with_tenant_id(body, "/Experiments/compare", tenant_id)
        }
        "/v1/experiments/agent-revision-simulations" => translate_json_object_with_tenant_id(
            body,
            "/Experiments/run_agent_revision_simulation",
            tenant_id,
        ),
        "/v1/experiments/agent-revision-simulations/compare" => {
            translate_json_object_with_tenant_id(
                body,
                "/Experiments/compare_agent_revision_simulation",
                tenant_id,
            )
        }
        "/v1/admin-maintenance/vector/promote" => translate_json_object_with_tenant_id(
            body,
            "/AdminMaintenance/promote_tenant_vector",
            tenant_id,
        ),
        "/v1/admin-maintenance/vector/rollback-promotion" => translate_json_object_with_tenant_id(
            body,
            "/AdminMaintenance/rollback_promotion",
            tenant_id,
        ),
        "/v1/admin-maintenance/vector/finalize-promotion" => translate_json_object_with_tenant_id(
            body,
            "/AdminMaintenance/finalize_promotion",
            tenant_id,
        ),
        "/v1/privacy/export" => {
            translate_json_object_with_tenant_id(body, "/Privacy/export", tenant_id)
        }
        "/v1/privacy/erase" => {
            translate_json_object_with_tenant_id(body, "/Privacy/erase", tenant_id)
        }
        _ => return None,
    };
    Some(translation)
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};

    use crate::routes::RouteTranslation;
    use crate::routes::test_support::{test_tenant_json, translate};

    #[test]
    fn analytics_public_routes_do_not_translate_to_restate_handlers() {
        // Pins: hosted analytics routes are direct edge handlers, not Restate forwards.
        let cases = [
            (Method::GET, "/v1/analytics/catalog"),
            (Method::POST, "/v1/analytics/query"),
        ];

        for (method, public_path) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&method, &uri, &body);

            match translation {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward { method, path, .. } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through direct routing, got: {message}")
                }
            }
        }
    }

    #[test]
    fn eval_public_routes_do_not_translate_to_product_handlers() {
        // Pins: hosted eval is not part of the default public product edge surface.
        let paths = [
            "/v1/evals/plan",
            "/v1/evals/suites/list",
            "/v1/evals/run",
            "/v1/evals/run-status",
            "/v1/evals/datasets/register",
            "/v1/evals/datasets/list",
            "/v1/evals/replay",
            "/v1/evals/scores",
            "/v1/evals/compare",
        ];

        for public_path in paths {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward {
                    method,
                    path,
                    body: _,
                } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through unchanged, got: {message}")
                }
            }
        }
    }

    #[test]
    fn experiments_public_routes_translate_to_restate_handlers() {
        // Pins: hosted experiment edge routes forward to the internal Experiments service paths.
        let cases = [
            (
                "/v1/experiments/generate-plan",
                "/Experiments/generate_plan",
            ),
            ("/v1/experiments/run-plan", "/Experiments/run"),
            ("/v1/experiments/status", "/Experiments/status"),
            ("/v1/experiments/list", "/Experiments/list"),
            ("/v1/experiments/plans/list", "/Experiments/list_plans"),
            ("/v1/experiments/trials", "/Experiments/trials"),
            ("/v1/experiments/trial-status", "/Experiments/trial_status"),
            ("/v1/experiments/cancel", "/Experiments/cancel"),
            (
                "/v1/experiments/propose-improvements",
                "/Experiments/propose_improvements",
            ),
            ("/v1/experiments/scores", "/Experiments/scores"),
            ("/v1/experiments/compare", "/Experiments/compare"),
            (
                "/v1/experiments/agent-revision-simulations",
                "/Experiments/run_agent_revision_simulation",
            ),
            (
                "/v1/experiments/agent-revision-simulations/compare",
                "/Experiments/compare_agent_revision_simulation",
            ),
        ];

        for (public_path, internal_path) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded.get("tenant_id"), Some(&test_tenant_json()));
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn admin_maintenance_vector_public_routes_translate_to_restate_handlers() {
        // Pins: hosted vector maintenance stays tenant-scoped through the public edge translator.
        let cases = [
            (
                "/v1/admin-maintenance/vector/promote",
                "/AdminMaintenance/promote_tenant_vector",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "target_backend": "turbopuffer",
                    "validate_percent": 5,
                    "dual_read_hours": 24
                }),
            ),
            (
                "/v1/admin-maintenance/vector/rollback-promotion",
                "/AdminMaintenance/rollback_promotion",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "action": "rollback"
                }),
            ),
            (
                "/v1/admin-maintenance/vector/finalize-promotion",
                "/AdminMaintenance/finalize_promotion",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "action": "finalize"
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            if let Some(object) = input_body.as_object_mut() {
                object.remove("tenant_id");
            }
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn admin_maintenance_checkpoint_public_routes_do_not_translate() {
        // Pins: deployment-global checkpoint maintenance is not exposed through tenant public routes.
        let paths = [
            "/v1/admin-maintenance/checkpoints/create",
            "/v1/admin-maintenance/checkpoints/list",
            "/v1/admin-maintenance/checkpoints/rollback",
            "/v1/admin-maintenance/checkpoints/cleanup",
        ];

        for public_path in paths {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward { method, path, .. } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through unchanged, got: {message}")
                }
            }
        }
    }

    #[test]
    fn lineage_public_routes_do_not_translate_to_restate_handlers() {
        // Pins: hosted lineage routes stay on direct edge handlers.
        let paths = [
            "/v1/lineage/explain",
            "/v1/lineage/query",
            "/v1/lineage/verify",
        ];

        for public_path in paths {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward { method, path, .. } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through direct routing, got: {message}")
                }
            }
        }
    }

    #[test]
    fn privacy_public_routes_translate_to_restate_handlers() {
        // Pins: privacy operations are still durable Restate service calls.
        let cases = [
            (
                "/v1/privacy/export",
                "/Privacy/export",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject_user_id": "22222222-2222-2222-2222-222222222222",
                    "reason": "GDPR",
                    "approval_token": "token"
                }),
            ),
            (
                "/v1/privacy/erase",
                "/Privacy/erase",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject_user_id": "22222222-2222-2222-2222-222222222222",
                    "reason": "GDPR",
                    "approval_token": "token"
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.remove("tenant_id");
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }
}
