//! Public sandbox-workspace route adapters.

use axum::body::Bytes;
use axum::http::{Method, Uri};
use moa_core::types::identifiers::{SandboxWorkspaceId, TenantId};
use moa_wire::sandbox_workspaces::{
    CreateSandboxWorkspaceRequest, RestoreSandboxWorkspaceRequest, SandboxWorkspaceIdRequest,
    SandboxWorkspaceListRequest, SandboxWorkspaceRestoreBody,
};
use serde::Serialize;
use uuid::Uuid;

use super::RouteTranslation;

const PUBLIC_ROOT: &str = "/v1/sandbox-workspaces";

pub(super) fn translate(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    _tenant_id: TenantId,
) -> Option<RouteTranslation> {
    let path = uri.path();
    if path != PUBLIC_ROOT && !path.starts_with("/v1/sandbox-workspaces/") {
        return None;
    }
    if uri.query().is_some() {
        return Some(RouteTranslation::BadRequest(
            "sandbox workspace routes do not accept query parameters",
        ));
    }
    if path == PUBLIC_ROOT {
        return Some(match *method {
            Method::POST => translate_create(body),
            Method::GET if body.is_empty() => serialize_command(
                "/SandboxWorkspaces/list",
                &SandboxWorkspaceListRequest::default(),
            ),
            Method::GET => {
                RouteTranslation::BadRequest("sandbox workspace list body must be empty")
            }
            _ => RouteTranslation::NotFound,
        });
    }

    let tail = path
        .strip_prefix("/v1/sandbox-workspaces/")
        .expect("sandbox workspace prefix was checked above");
    let mut segments = tail.split('/');
    let workspace_id = match segments.next().and_then(parse_workspace_id) {
        Some(workspace_id) => workspace_id,
        None => {
            return Some(RouteTranslation::BadRequest("invalid sandbox workspace id"));
        }
    };
    let children: Vec<_> = segments.collect();

    let translation = match children.as_slice() {
        [] if *method == Method::GET && body.is_empty() => {
            translate_id(workspace_id, "/SandboxWorkspaces/get")
        }
        [] if *method == Method::GET => {
            RouteTranslation::BadRequest("sandbox workspace get body must be empty")
        }
        [] if *method == Method::DELETE && body.is_empty() => {
            translate_id(workspace_id, "/SandboxWorkspaces/delete")
        }
        [] if *method == Method::DELETE => {
            RouteTranslation::BadRequest("sandbox workspace delete body must be empty")
        }
        [operation @ ("attach" | "checkpoint")] if *method == Method::POST && body.is_empty() => {
            let target = if *operation == "attach" {
                "/SandboxWorkspaces/attach"
            } else {
                "/SandboxWorkspaces/checkpoint"
            };
            translate_id(workspace_id, target)
        }
        ["attach" | "checkpoint"] if *method == Method::POST => {
            RouteTranslation::BadRequest("sandbox workspace action body must be empty")
        }
        ["restore"] if *method == Method::POST => translate_restore(body, workspace_id),
        _ => RouteTranslation::NotFound,
    };
    Some(translation)
}

fn translate_create(body: &Bytes) -> RouteTranslation {
    let request = match serde_json::from_slice::<CreateSandboxWorkspaceRequest>(body) {
        Ok(request) => request,
        Err(_) => {
            return RouteTranslation::BadRequest("invalid sandbox workspace create request");
        }
    };
    serialize_command("/SandboxWorkspaces/create", &request)
}

fn translate_id(workspace_id: SandboxWorkspaceId, target: &'static str) -> RouteTranslation {
    serialize_command(target, &SandboxWorkspaceIdRequest { workspace_id })
}

fn translate_restore(body: &Bytes, workspace_id: SandboxWorkspaceId) -> RouteTranslation {
    let request = match serde_json::from_slice::<SandboxWorkspaceRestoreBody>(body) {
        Ok(request) => request,
        Err(_) => {
            return RouteTranslation::BadRequest("invalid sandbox workspace restore request");
        }
    };
    serialize_command(
        "/SandboxWorkspaces/restore",
        &RestoreSandboxWorkspaceRequest {
            workspace_id,
            checkpoint_id: request.checkpoint_id,
        },
    )
}

fn serialize_command(target: &'static str, command: &impl Serialize) -> RouteTranslation {
    match serde_json::to_vec(command) {
        Ok(body) => RouteTranslation::Forward {
            method: Method::POST,
            path: target.to_string(),
            body,
        },
        Err(error) => {
            tracing::error!(%error, target, "serialize sandbox workspace command failed");
            RouteTranslation::BadRequest("invalid sandbox workspace request")
        }
    }
}

fn parse_workspace_id(value: &str) -> Option<SandboxWorkspaceId> {
    Uuid::parse_str(value).ok().map(SandboxWorkspaceId)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_support::translate as translate_public;
    use serde_json::{Value, json};

    const WORKSPACE_ID: &str = "11111111-1111-1111-1111-111111111111";
    const CHECKPOINT_ID: &str = "33333333-3333-3333-3333-333333333333";

    fn uri(path: &str) -> Uri {
        path.parse().expect("fixture URI should parse")
    }

    fn assert_forward(translation: RouteTranslation, expected_path: &str, expected_body: Value) {
        let RouteTranslation::Forward { method, path, body } = translation else {
            panic!("valid sandbox workspace route should forward")
        };
        assert_eq!(method, Method::POST);
        assert_eq!(path, expected_path);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("forwarded command should be JSON"),
            expected_body
        );
    }

    #[test]
    fn sandbox_workspace_routes_translate_exact_typed_commands() {
        // Pins: every public management route maps to one exact Restate handler,
        // derives workspace identity from the path, and forwards no tenant or provider data.
        let create = json!({
            "scope": {
                "kind": "worker",
                "session_id": "22222222-2222-2222-2222-222222222222",
                "worker_id": "44444444-4444-4444-4444-444444444444"
            },
            "durability_class": "portable_filesystem"
        });
        assert_forward(
            translate_public(
                &Method::POST,
                &uri(PUBLIC_ROOT),
                &Bytes::from(create.to_string()),
            ),
            "/SandboxWorkspaces/create",
            create,
        );
        assert_forward(
            translate_public(&Method::GET, &uri(PUBLIC_ROOT), &Bytes::new()),
            "/SandboxWorkspaces/list",
            json!({}),
        );

        for (method, suffix, target) in [
            (Method::GET, "", "/SandboxWorkspaces/get"),
            (Method::DELETE, "", "/SandboxWorkspaces/delete"),
            (Method::POST, "/attach", "/SandboxWorkspaces/attach"),
            (Method::POST, "/checkpoint", "/SandboxWorkspaces/checkpoint"),
        ] {
            assert_forward(
                translate_public(
                    &method,
                    &uri(&format!("{PUBLIC_ROOT}/{WORKSPACE_ID}{suffix}")),
                    &Bytes::new(),
                ),
                target,
                json!({"workspace_id": WORKSPACE_ID}),
            );
        }

        assert_forward(
            translate_public(
                &Method::POST,
                &uri(&format!("{PUBLIC_ROOT}/{WORKSPACE_ID}/restore")),
                &Bytes::from(json!({"checkpoint_id": CHECKPOINT_ID}).to_string()),
            ),
            "/SandboxWorkspaces/restore",
            json!({
                "workspace_id": WORKSPACE_ID,
                "checkpoint_id": CHECKPOINT_ID
            }),
        );
    }

    #[test]
    fn sandbox_workspace_routes_reject_untyped_or_ambiguous_inputs() {
        // Pins: callers cannot inject tenant/provider state, override a path id,
        // attach bodies to no-body actions, or reach unlisted methods and children.
        let forged_create = Bytes::from(
            json!({
                "scope": {
                    "kind": "worker",
                    "session_id": "22222222-2222-2222-2222-222222222222",
                    "worker_id": "44444444-4444-4444-4444-444444444444"
                },
                "durability_class": "portable_filesystem",
                "tenant_id": "55555555-5555-5555-5555-555555555555",
                "provider": "daytona",
                "provider_subpath": "/tenant/private"
            })
            .to_string(),
        );
        assert_eq!(
            translate_public(&Method::POST, &uri(PUBLIC_ROOT), &forged_create),
            RouteTranslation::BadRequest("invalid sandbox workspace create request")
        );
        assert_eq!(
            translate_public(
                &Method::POST,
                &uri(&format!("{PUBLIC_ROOT}/{WORKSPACE_ID}/restore")),
                &Bytes::from(
                    json!({
                        "workspace_id": "99999999-9999-9999-9999-999999999999",
                        "checkpoint_id": CHECKPOINT_ID
                    })
                    .to_string()
                ),
            ),
            RouteTranslation::BadRequest("invalid sandbox workspace restore request")
        );
        assert_eq!(
            translate_public(
                &Method::POST,
                &uri(&format!("{PUBLIC_ROOT}/{WORKSPACE_ID}/checkpoint")),
                &Bytes::from_static(b"{}"),
            ),
            RouteTranslation::BadRequest("sandbox workspace action body must be empty")
        );
        assert_eq!(
            translate_public(
                &Method::GET,
                &uri(&format!("{PUBLIC_ROOT}?tenant_id=forged")),
                &Bytes::new(),
            ),
            RouteTranslation::BadRequest("sandbox workspace routes do not accept query parameters")
        );
        assert_eq!(
            translate_public(
                &Method::GET,
                &uri(&format!("{PUBLIC_ROOT}/not-a-uuid")),
                &Bytes::new(),
            ),
            RouteTranslation::BadRequest("invalid sandbox workspace id")
        );
        assert_eq!(
            translate_public(
                &Method::PATCH,
                &uri(&format!("{PUBLIC_ROOT}/{WORKSPACE_ID}")),
                &Bytes::new(),
            ),
            RouteTranslation::NotFound
        );
        assert_eq!(
            translate_public(
                &Method::POST,
                &uri(&format!("{PUBLIC_ROOT}/{WORKSPACE_ID}/provider")),
                &Bytes::new(),
            ),
            RouteTranslation::NotFound
        );
    }
}
