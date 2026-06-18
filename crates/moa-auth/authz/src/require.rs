//! Canonical authorization-check helpers for MOA handlers.
//!
//! Handlers should call [`require_authz`] or [`require_authz_with_delegation`]
//! instead of invoking [`FgaClient`](crate::FgaClient) directly. These helpers
//! derive the canonical FGA subject, perform the check, and return a structured
//! error that handler shims can translate into a wire response.

use std::fmt;
use std::sync::OnceLock;

use crate::{AuthzError, FgaClient};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::{Identity, IdentityType};
use sqlx::PgPool;
use thiserror::Error;

static AUDIT: OnceLock<SecurityAuditConfig> = OnceLock::new();

#[derive(Clone)]
struct SecurityAuditConfig {
    pool: PgPool,
    emit_allows: bool,
}

/// Configure security-audit emission for authorization decisions.
///
/// Denied checks are always emitted. Allowed checks are emitted only when
/// `emit_allows` is true because allow decisions are high-volume.
pub fn configure_security_audit(pool: PgPool, emit_allows: bool) {
    let _ = AUDIT.set(SecurityAuditConfig { pool, emit_allows });
}

/// Failure returned by a required authorization check.
#[derive(Debug, Error)]
pub enum AuthzCheckError {
    /// The authorization engine returned a definitive deny.
    #[error("forbidden: identity {subject} not {relation} on {object_type}:{object_id}")]
    Forbidden {
        /// FGA subject string used for the denied check.
        subject: String,
        /// Object type used for the denied check.
        object_type: ObjectType,
        /// Object identifier used for the denied check.
        object_id: String,
        /// Relation used for the denied check.
        relation: Relation,
    },
    /// The authorization engine failed before returning a decision.
    #[error("authz engine error: {0}")]
    Engine(#[from] AuthzError),
}

/// Verify that `identity` has `relation` on `object_type:object_id`.
///
/// `Forbidden` is a definitive deny. `Engine` means the authorization engine
/// did not return a decision and callers must fail closed.
pub async fn require_authz(
    fga: &FgaClient,
    identity: &Identity,
    object_type: ObjectType,
    object_id: impl fmt::Display,
    relation: Relation,
) -> Result<(), AuthzCheckError> {
    let object_id = object_id.to_string();
    let subject = fga_subject(identity);
    let object = format!("{object_type}:{object_id}");
    let allowed = fga.check(&subject, &relation.to_string(), &object).await?;
    emit_authz_audit(identity, &object, object_type, &relation, allowed).await?;
    if !allowed {
        return Err(AuthzCheckError::Forbidden {
            subject,
            object_type,
            object_id,
            relation,
        });
    }
    Ok(())
}

async fn emit_authz_audit(
    identity: &Identity,
    object: &str,
    object_type: ObjectType,
    relation: &Relation,
    allowed: bool,
) -> Result<(), AuthzCheckError> {
    let Some(config) = AUDIT.get() else {
        return Ok(());
    };
    if allowed && !config.emit_allows {
        return Ok(());
    }
    moa_ocsf::emit_authz_decision(
        &config.pool,
        identity.tenant_id,
        identity,
        object,
        &object_type.to_string(),
        &relation.to_string(),
        allowed,
    )
    .await
    .map(|_| ())
    .map_err(|error| AuthzCheckError::Engine(AuthzError::Audit(error.to_string())))
}

/// Verify authorization and, for delegated agent calls, verify `can_act_as`.
///
/// Delegation does not borrow the underlying user's resource permissions. The
/// agent remains the resource-check subject and must be granted the requested
/// relation directly.
pub async fn require_authz_with_delegation(
    fga: &FgaClient,
    identity: &Identity,
    object_type: ObjectType,
    object_id: impl fmt::Display,
    relation: Relation,
) -> Result<(), AuthzCheckError> {
    if let Some(user_id) = identity.acting_on_behalf_of {
        let agent_object_id = identity.id.to_string();
        let agent_object = format!("{}:{agent_object_id}", ObjectType::Agent);
        if identity.identity_type != IdentityType::Agent {
            emit_authz_audit(
                identity,
                &agent_object,
                ObjectType::Agent,
                &Relation::CanActAs,
                false,
            )
            .await?;
            return Err(AuthzCheckError::Forbidden {
                subject: fga_subject(identity),
                object_type: ObjectType::Agent,
                object_id: agent_object_id,
                relation: Relation::CanActAs,
            });
        }

        let delegated_user = format!("user:{user_id}");
        let allowed = fga
            .check(
                &delegated_user,
                &Relation::CanActAs.to_string(),
                &agent_object,
            )
            .await?;
        emit_authz_audit(
            identity,
            &agent_object,
            ObjectType::Agent,
            &Relation::CanActAs,
            allowed,
        )
        .await?;
        if !allowed {
            return Err(AuthzCheckError::Forbidden {
                subject: format!("agent:{}", identity.id),
                object_type: ObjectType::Agent,
                object_id: agent_object_id,
                relation: Relation::CanActAs,
            });
        }
    }

    require_authz(fga, identity, object_type, object_id, relation).await
}

/// Return the canonical FGA subject for an authenticated identity.
///
/// API-key identity wins over the underlying owner identity. This is how API
/// key scopes narrow access: checks run as `api_key:<id>` and therefore only
/// see tuples granted to the key.
#[must_use]
pub fn fga_subject(identity: &Identity) -> String {
    if let Some(api_key_id) = identity.api_key_id {
        return format!("api_key:{api_key_id}");
    }

    match identity.identity_type {
        IdentityType::User => format!("user:{}", identity.id),
        IdentityType::Agent => format!("agent:{}", identity.id),
        IdentityType::Service => format!("service:{}", identity.id),
    }
}
