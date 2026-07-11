//! Tenant signup, settings, users, invitations, and deletion routes.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::tenant_accounts::ApplicationError;

mod deletion;
mod invitations;
mod registration;
mod users;

pub(super) use deletion::{delete_tenant, tenant_purge_status};
pub(super) use invitations::{accept_invitation, invite_user};
pub(super) use registration::signup;
pub(super) use users::{create_user, get_tenant, list_users, patch_tenant};

fn application_error_response(error: ApplicationError) -> Response {
    match error {
        ApplicationError::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
        ApplicationError::Conflict(message) => (StatusCode::CONFLICT, message).into_response(),
        ApplicationError::NotFound(message) => (StatusCode::NOT_FOUND, message).into_response(),
        ApplicationError::Internal(error) => super::auth_accounts::internal_error(error),
    }
}

fn looks_like_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.ends_with('.')
}

#[cfg(test)]
mod tests {
    use crate::tenant_accounts::{TenantUserRole, application::invitation_token_hash};

    use super::*;

    #[test]
    fn tenant_signup_email_validation_requires_domain_dot() {
        // Pins: tenant signup rejects obvious non-email login IDs before credential creation.
        assert!(looks_like_email("admin@example.com"));
        assert!(!looks_like_email("admin"));
        assert!(!looks_like_email("admin@example"));
    }

    #[test]
    fn tenant_user_roles_round_trip_openfga_relations() {
        // Pins: invitation role strings are exactly the tenant relations written to OpenFGA.
        assert_eq!(TenantUserRole::Admin.relation(), "admin");
        assert_eq!(TenantUserRole::Operator.relation(), "operator");
        assert_eq!(
            TenantUserRole::from_relation("admin"),
            Some(TenantUserRole::Admin)
        );
        assert_eq!(
            TenantUserRole::from_relation("operator"),
            Some(TenantUserRole::Operator)
        );
        assert_eq!(TenantUserRole::from_relation("workspace_admin"), None);
    }

    #[test]
    fn invitation_token_hash_is_not_the_raw_token() {
        // Pins: invitation tokens are stored as a deterministic digest, never as the bearer value.
        let token = "tenant_invite_example";
        let digest = invitation_token_hash(token);
        assert_ne!(digest, token);
        assert_eq!(digest.len(), 64);
        assert_eq!(digest, invitation_token_hash(token));
    }
}
