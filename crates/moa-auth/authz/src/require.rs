//! Canonical authorization-check helpers for MOA handlers.
//!
//! Handlers should call [`require_authz`] or [`require_authz_with_delegation`]
//! instead of invoking [`FgaClient`](crate::FgaClient) directly. These helpers
//! derive the canonical FGA subject, perform the check, and return a structured
//! error that handler shims can translate into a wire response.

use std::fmt;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use crate::client::SecurityAudit;
use crate::{AuthzError, FgaClient};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::error::FailureProvenance;
use moa_core::traits::{Identity, IdentityType};
use moka::future::Cache;
use thiserror::Error;

/// Default decision-cache TTL. Bounds how long a stale allow can outlive a
/// revocation before the next check re-consults OpenFGA.
const DEFAULT_DECISION_CACHE_TTL_MS: u64 = 2_000;
/// Upper bound on distinct cached positive decisions.
const DECISION_CACHE_CAPACITY: u64 = 100_000;
/// ASCII unit separator; safe delimiter for the composite cache key.
const KEY_SEP: char = '\u{1f}';

static DECISION_CACHE: OnceLock<Cache<String, ()>> = OnceLock::new();

/// Positive-decision cache. Only allows are cached; denials always re-check so a
/// revocation takes effect within the TTL. Keyed by `(subject, relation, object)`.
fn decision_cache() -> &'static Cache<String, ()> {
    DECISION_CACHE.get_or_init(|| {
        let ttl_ms = std::env::var("MOA_AUTHZ_DECISION_CACHE_TTL_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(DEFAULT_DECISION_CACHE_TTL_MS);
        Cache::builder()
            .max_capacity(DECISION_CACHE_CAPACITY)
            .time_to_live(Duration::from_millis(ttl_ms))
            .build()
    })
}

fn decision_key(subject: &str, relation: &str, object: &str) -> String {
    format!("{subject}{KEY_SEP}{relation}{KEY_SEP}{object}")
}

/// Resolve a single `(subject, relation, object)` decision, serving cached
/// positives and caching new positives. Denials are never cached, so `check` is
/// re-run for them and revocations are seen at once.
async fn cached_decision<F, Fut>(
    subject: &str,
    relation: &str,
    object: &str,
    check: F,
) -> Result<bool, AuthzError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<bool, AuthzError>>,
{
    let key = decision_key(subject, relation, object);
    if decision_cache().get(&key).await.is_some() {
        return Ok(true);
    }
    let allowed = check().await?;
    if allowed {
        decision_cache().insert(key, ()).await;
    }
    Ok(allowed)
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

impl AuthzCheckError {
    /// Returns the typed provenance of the failed authorization check.
    #[must_use]
    pub fn failure_provenance(&self) -> FailureProvenance {
        match self {
            Self::Forbidden { .. } => FailureProvenance::Permanent,
            Self::Engine(error) => error.failure_provenance(),
        }
    }
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
    let relation_str = relation.to_string();
    let allowed = cached_decision(&subject, &relation_str, &object, || {
        fga.check(&subject, &relation_str, &object)
    })
    .await?;
    emit_authz_audit(fga, identity, &object, object_type, &relation, allowed).await?;
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

/// Record an authorization decision to the audit trail.
///
/// Denials are emitted synchronously and fail closed, preserving the security
/// audit contract. Allows are high-volume and stay best-effort when configured.
async fn emit_authz_audit(
    fga: &FgaClient,
    identity: &Identity,
    object: &str,
    object_type: ObjectType,
    relation: &Relation,
    allowed: bool,
) -> Result<(), AuthzCheckError> {
    let Some(audit): Option<&SecurityAudit> = fga.security_audit() else {
        return Ok(());
    };
    if allowed && !audit.emit_allows {
        return Ok(());
    }
    if !allowed {
        // Denials stay synchronous and fail closed. A denial the audit trail did
        // not record is a denial nobody can prove happened, so it must fail the
        // check rather than be enqueued and hoped for.
        moa_ocsf::emit_authz_decision(
            &audit.pool,
            identity.tenant_id,
            identity,
            object,
            &object_type.to_string(),
            &relation.to_string(),
            false,
        )
        .await
        .map_err(|error| AuthzCheckError::Engine(AuthzError::Audit(error)))?;
        return Ok(());
    }
    moa_ocsf::spawn_authz_decision(
        &audit.emitter,
        identity.tenant_id,
        identity,
        object,
        &object_type.to_string(),
        &relation.to_string(),
        allowed,
    );
    Ok(())
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
    if identity.acting_on_behalf_of.is_none() {
        return require_authz(fga, identity, object_type, object_id, relation).await;
    }

    require_authz_with_delegation_batch(
        fga,
        identity,
        object_type,
        &[object_id.to_string()],
        relation,
    )
    .await
}

/// Verifies one relation across several objects in one OpenFGA batch request.
///
/// Delegated agents add their single `can_act_as` tuple to the same request.
/// Positive cache entries are omitted from the batch, denials remain uncached,
/// and every decision retains the normal security-audit behavior.
pub async fn require_authz_with_delegation_batch(
    fga: &FgaClient,
    identity: &Identity,
    object_type: ObjectType,
    object_ids: &[String],
    relation: Relation,
) -> Result<(), AuthzCheckError> {
    if object_ids.is_empty() {
        return Ok(());
    }
    let subject = fga_subject(identity);
    let relation_str = relation.to_string();
    let objects = object_ids
        .iter()
        .map(|object_id| format!("{object_type}:{object_id}"))
        .collect::<Vec<_>>();
    let mut allowed = vec![false; objects.len()];
    let mut checks = Vec::new();
    let mut check_targets = Vec::new();

    let delegation = if let Some(user_id) = identity.acting_on_behalf_of {
        let agent_object_id = identity.id.to_string();
        let agent_object = format!("{}:{agent_object_id}", ObjectType::Agent);
        if identity.identity_type != IdentityType::Agent {
            emit_authz_audit(
                fga,
                identity,
                &agent_object,
                ObjectType::Agent,
                &Relation::CanActAs,
                false,
            )
            .await?;
            return Err(AuthzCheckError::Forbidden {
                subject,
                object_type: ObjectType::Agent,
                object_id: agent_object_id,
                relation: Relation::CanActAs,
            });
        }
        let delegated_operator = format!("operator:{user_id}");
        let can_act_as = Relation::CanActAs.to_string();
        let key = decision_key(&delegated_operator, &can_act_as, &agent_object);
        let cached = decision_cache().get(&key).await.is_some();
        if !cached {
            checks.push((delegated_operator, can_act_as, agent_object.clone()));
            check_targets.push(None);
        }
        Some((agent_object_id, agent_object, key, cached))
    } else {
        None
    };

    for (index, object) in objects.iter().enumerate() {
        let key = decision_key(&subject, &relation_str, object);
        if decision_cache().get(&key).await.is_some() {
            allowed[index] = true;
        } else {
            checks.push((subject.clone(), relation_str.clone(), object.clone()));
            check_targets.push(Some((index, key)));
        }
    }

    let mut delegation_allowed = delegation.as_ref().is_none_or(|entry| entry.3);
    if !checks.is_empty() {
        let results = fga.batch_check(&checks).await?;
        if results.len() != checks.len() {
            return Err(AuthzCheckError::Engine(AuthzError::Ambiguous(
                "BatchCheck returned a different result count".to_string(),
            )));
        }
        for (target, decision) in check_targets.into_iter().zip(results) {
            match target {
                None => {
                    delegation_allowed = decision;
                    if decision && let Some((_, _, key, _)) = delegation.as_ref() {
                        decision_cache().insert(key.clone(), ()).await;
                    }
                }
                Some((index, key)) => {
                    allowed[index] = decision;
                    if decision {
                        decision_cache().insert(key, ()).await;
                    }
                }
            }
        }
    }

    if let Some((agent_object_id, agent_object, _, _)) = delegation {
        emit_authz_audit(
            fga,
            identity,
            &agent_object,
            ObjectType::Agent,
            &Relation::CanActAs,
            delegation_allowed,
        )
        .await?;
        if !delegation_allowed {
            return Err(AuthzCheckError::Forbidden {
                subject: format!("agent:{}", identity.id),
                object_type: ObjectType::Agent,
                object_id: agent_object_id,
                relation: Relation::CanActAs,
            });
        }
    }

    for ((object_id, object), decision) in object_ids.iter().zip(&objects).zip(allowed) {
        emit_authz_audit(fga, identity, object, object_type, &relation, decision).await?;
        if !decision {
            return Err(AuthzCheckError::Forbidden {
                subject: subject.clone(),
                object_type,
                object_id: object_id.clone(),
                relation,
            });
        }
    }
    Ok(())
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

    format!("{}:{}", identity.identity_type.as_str(), identity.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn cached_decision_caches_positive_and_always_rechecks_denials() {
        // Pins: an allow is cached so the second identical check skips OpenFGA,
        // while a denial is never cached so each check re-consults the engine —
        // bounding how long a revoked grant can be served.
        let calls = AtomicUsize::new(0);
        // Unique per-test tuples so a shared process-global cache cannot pollute.
        let subject = format!("operator:{}", uuid::Uuid::new_v4());
        let denied_object = format!("session:{}", uuid::Uuid::new_v4());
        let allowed_object = format!("session:{}", uuid::Uuid::new_v4());

        let allow = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        };
        let deny = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        };

        assert!(
            cached_decision(&subject, "viewer", &allowed_object, allow)
                .await
                .unwrap()
        );
        assert!(
            cached_decision(&subject, "viewer", &allowed_object, allow)
                .await
                .unwrap()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second allow is served from the decision cache"
        );

        assert!(
            !cached_decision(&subject, "viewer", &denied_object, deny)
                .await
                .unwrap()
        );
        assert!(
            !cached_decision(&subject, "viewer", &denied_object, deny)
                .await
                .unwrap()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "denials are never cached and always re-check"
        );
    }
}
