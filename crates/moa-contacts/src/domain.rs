//! Pure contact identity helpers.

use moa_core::{
    AgentSessionSelection, Channel, ContactId, ContactPointKind, ContactTokenClaims,
    ContactVerificationChallengeId, ContactVerificationState, TenantId,
};
use rand::Rng;
use uuid::Uuid;

use crate::{ContactError, Result};

const LOW_ASSURANCE_SCOPES: &[&str] = &[
    "agent:session:create",
    "contact:session:channel:update",
    "contact:session:message:send",
    "contact:verify:start",
    "contact:verify:complete",
    "memory:session:read",
    "memory:session:write",
];

const VERIFIED_SCOPES: &[&str] = &[
    "agent:session:create",
    "contact:session:channel:update",
    "contact:session:message:send",
    "contact:verify:start",
    "contact:verify:complete",
    "contact:self:update",
    "contact:session:promote",
    "memory:session:read",
    "memory:session:write",
    "memory:self:read",
    "memory:self:write",
];

/// Delivery route resolved from a contact point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactPointDelivery {
    /// Channel used for OTP delivery.
    pub channel: Channel,
    /// Normalized destination address or phone number.
    pub destination: String,
}

/// Returns low-assurance scopes bounded by the contact service allowlist.
#[must_use]
pub fn low_assurance_scopes(requested_scopes: &[String]) -> Vec<String> {
    bounded_scopes(requested_scopes, LOW_ASSURANCE_SCOPES)
}

/// Returns the full verified contact scope set.
#[must_use]
pub fn verified_scopes() -> Vec<String> {
    VERIFIED_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect()
}

/// Bounds requested scopes to an allowed static allowlist.
#[must_use]
pub fn bounded_scopes(requested_scopes: &[String], allowed: &[&str]) -> Vec<String> {
    if requested_scopes.is_empty() {
        return allowed.iter().map(|scope| (*scope).to_string()).collect();
    }
    requested_scopes
        .iter()
        .filter(|scope| allowed.iter().any(|allowed| allowed == &scope.as_str()))
        .cloned()
        .collect()
}

/// Resolves the delivery channel and normalized destination for an OTP contact point.
pub fn contact_point_delivery(
    kind: ContactPointKind,
    value: &str,
    requested_channel: Option<Channel>,
) -> Result<ContactPointDelivery> {
    let channel = match kind {
        ContactPointKind::Email => Channel::Email,
        ContactPointKind::Phone => Channel::Sms,
        ContactPointKind::ExternalId | ContactPointKind::AnonymousHandle => {
            return Err(ContactError::terminal(
                400,
                "contact verification supports email and phone delivery only",
            ));
        }
    };
    if let Some(requested_channel) = requested_channel
        && requested_channel != channel
    {
        return Err(ContactError::terminal(
            400,
            "delivery channel does not match contact point kind",
        ));
    }
    Ok(ContactPointDelivery {
        channel,
        destination: normalize_contact_point(kind, value)?,
    })
}

/// Returns whether a channel account can be used by the resolved contact.
#[must_use]
pub fn contact_allows_channel_contact(
    contact_id: ContactId,
    canonical_contact_id: Option<ContactId>,
    account_contact_id: ContactId,
) -> bool {
    contact_id == account_contact_id || canonical_contact_id == Some(account_contact_id)
}

/// Requires a contact token to contain the requested scope.
pub fn require_contact_scope(claims: &ContactTokenClaims, required_scope: &str) -> Result<()> {
    if claims.scopes.iter().any(|scope| scope == required_scope) {
        Ok(())
    } else {
        Err(ContactError::terminal(403, "contact token scope denied"))
    }
}

/// Requires a contact token to allow the requested session id.
pub fn require_contact_session_permission(
    claims: &ContactTokenClaims,
    session_id: Option<moa_core::SessionId>,
) -> Result<()> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    if claims.session_ids.is_empty() || claims.session_ids.contains(&session_id) {
        Ok(())
    } else {
        Err(ContactError::terminal(403, "contact token session denied"))
    }
}

/// Requires a contact token to allow the selected agent.
pub fn require_contact_agent_permission(
    claims: &ContactTokenClaims,
    agent: &AgentSessionSelection,
) -> Result<()> {
    if claims.agent_ids.is_empty() {
        validate_contact_agent_selection(agent).map(|_| ())
    } else {
        let selected_agent = validate_contact_agent_selection(agent)?;
        if claims
            .agent_ids
            .iter()
            .any(|agent_id| agent_id == &selected_agent)
        {
            Ok(())
        } else {
            Err(ContactError::terminal(403, "contact token agent denied"))
        }
    }
}

/// Returns the single selected agent id from a contact session request.
pub fn validate_contact_agent_selection(agent: &AgentSessionSelection) -> Result<String> {
    match (agent.installation_uid, agent.revision_uid) {
        (Some(installation_uid), None) => Ok(installation_uid.to_string()),
        (None, Some(revision_uid)) => Ok(revision_uid.to_string()),
        _ => Err(ContactError::terminal(
            400,
            "contact session requires exactly one agent installation_uid or revision_uid",
        )),
    }
}

/// Parses a contact id from verified token claims.
pub fn contact_id_from_claims(claims: &ContactTokenClaims) -> Result<ContactId> {
    Uuid::parse_str(&claims.sub)
        .map(ContactId)
        .map_err(|_| ContactError::terminal(400, "contact token subject is invalid"))
}

/// Normalizes one contact point for stable hashing and delivery.
pub fn normalize_contact_point(kind: ContactPointKind, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ContactError::terminal(
            400,
            "contact point value is required",
        ));
    }
    match kind {
        ContactPointKind::Email => {
            let normalized = trimmed.to_ascii_lowercase();
            if !normalized.contains('@') {
                return Err(ContactError::terminal(400, "invalid email contact point"));
            }
            Ok(normalized)
        }
        ContactPointKind::Phone => normalize_phone(trimmed),
        ContactPointKind::ExternalId | ContactPointKind::AnonymousHandle => Ok(trimmed.to_string()),
    }
}

/// Normalizes a phone number into the current E.164-like storage form.
pub fn normalize_phone(value: &str) -> Result<String> {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    if !(8..=15).contains(&digits.len()) {
        return Err(ContactError::terminal(400, "invalid phone contact point"));
    }
    Ok(format!("+{digits}"))
}

/// Hashes a normalized contact point using the configured keyed-hash secret.
pub fn hash_contact_point_from_env(
    tenant_id: TenantId,
    kind: ContactPointKind,
    normalized: &str,
    key_env: &str,
) -> Result<String> {
    let key_hex = std::env::var(key_env)
        .map_err(|_| ContactError::terminal(503, "contact point hash key is not configured"))?;
    hash_contact_point_with_key_hex(tenant_id, kind, normalized, key_hex.trim())
}

/// Hashes a normalized contact point using a hex-encoded keyed-hash secret.
pub fn hash_contact_point_with_key_hex(
    tenant_id: TenantId,
    kind: ContactPointKind,
    normalized: &str,
    key_hex: &str,
) -> Result<String> {
    let key_bytes = hex::decode(key_hex).map_err(|error| {
        ContactError::terminal(
            503,
            format!("contact point hash key must be hex-encoded: {error}"),
        )
    })?;
    let key: [u8; 32] = key_bytes.try_into().map_err(|bytes: Vec<u8>| {
        ContactError::terminal(
            503,
            format!(
                "contact point hash key must be 32 bytes, got {}",
                bytes.len()
            ),
        )
    })?;
    Ok(blake3::keyed_hash(
        &key,
        format!("{tenant_id}:{}:{normalized}", kind.as_str()).as_bytes(),
    )
    .to_hex()
    .to_string())
}

/// Generates a six-digit OTP verification code.
#[must_use]
pub fn verification_code() -> String {
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000))
}

/// Hashes an OTP verification code for storage.
#[must_use]
pub fn hash_verification_code(challenge_id: ContactVerificationChallengeId, code: &str) -> String {
    blake3::hash(format!("{challenge_id}:{}", code.trim()).as_bytes())
        .to_hex()
        .to_string()
}

/// Parses a persisted contact state.
pub fn parse_contact_state(value: &str) -> Result<ContactVerificationState> {
    value
        .parse::<ContactVerificationState>()
        .map_err(|_| ContactError::terminal(500, "invalid stored contact state"))
}

/// Parses a persisted contact point kind.
pub fn parse_contact_point_kind(value: &str) -> Result<ContactPointKind> {
    value
        .parse::<ContactPointKind>()
        .map_err(|_| ContactError::terminal(500, "invalid stored contact point kind"))
}

#[cfg(test)]
mod tests {
    use moa_core::{
        AgentSessionSelection, Channel, ContactId, ContactPointKind, ContactTokenClaims,
        ContactVerificationState, TenantId,
    };

    use super::{
        contact_allows_channel_contact, contact_point_delivery, hash_contact_point_with_key_hex,
        low_assurance_scopes, normalize_contact_point, normalize_phone,
        require_contact_agent_permission,
    };
    use crate::ContactError;

    fn assert_terminal(error: &ContactError, code: u16, needle: &str) {
        assert_eq!(
            error.terminal_code(),
            Some(code),
            "unexpected terminal code: {error:?}"
        );
        assert!(
            format!("{error}").contains(needle),
            "unexpected message {error:?}, wanted substring {needle:?}"
        );
    }

    #[test]
    fn contact_agent_permission_allows_unbounded_token_with_single_selector() {
        // Pins: unbounded contact tokens may create sessions only when exactly one agent selector is provided.
        let installation_uid = uuid::Uuid::now_v7();
        let claims = contact_claims(Vec::new());
        let selection = AgentSessionSelection {
            installation_uid: Some(installation_uid),
            revision_uid: None,
        };

        require_contact_agent_permission(&claims, &selection)
            .expect("unbounded token should allow a single selected agent");
    }

    #[test]
    fn contact_agent_permission_rejects_token_agent_allowlist_miss() {
        // Pins: bounded contact tokens cannot create sessions for agents outside their allowlist.
        let allowed_installation_uid = uuid::Uuid::now_v7();
        let denied_installation_uid = uuid::Uuid::now_v7();
        let claims = contact_claims(vec![allowed_installation_uid.to_string()]);
        let selection = AgentSessionSelection {
            installation_uid: Some(denied_installation_uid),
            revision_uid: None,
        };

        let error = require_contact_agent_permission(&claims, &selection)
            .expect_err("unlisted agent should be denied");

        assert!(
            format!("{error:?}").contains("contact token agent denied"),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn contact_message_send_scope_is_grantable_only_within_low_assurance_allowlist() {
        // Pins: low-assurance contact tokens can hold the send scope but cannot smuggle a verified-only scope.
        let requested = vec![
            "contact:session:message:send".to_string(),
            // `memory:self:write` is a verified-only scope, absent from LOW_ASSURANCE_SCOPES.
            "memory:self:write".to_string(),
        ];

        let granted = low_assurance_scopes(&requested);

        assert_eq!(granted, vec!["contact:session:message:send".to_string()]);
    }

    #[test]
    fn normalize_contact_point_rejects_invalid_email_and_empty_value() {
        // Pins: email contact points require an '@' and a non-empty value before hashing or delivery.
        let no_at = normalize_contact_point(ContactPointKind::Email, "foo")
            .expect_err("email without @ must reject");
        assert_terminal(&no_at, 400, "invalid email contact point");

        let empty = normalize_contact_point(ContactPointKind::Email, "   ")
            .expect_err("empty contact point value must reject");
        assert_terminal(&empty, 400, "contact point value is required");

        // The happy path still trims and lowercases.
        let normalized = normalize_contact_point(ContactPointKind::Email, "  User@Example.COM ")
            .expect("valid email should normalize");
        assert_eq!(normalized, "user@example.com");
    }

    #[test]
    fn normalize_phone_enforces_eight_to_fifteen_digit_bounds() {
        // Pins: phone normalization accepts 8-15 digit E.164-like numbers and rejects out-of-range lengths.
        let too_short =
            normalize_phone("1234567").expect_err("7-digit phone must reject as too short");
        assert_terminal(&too_short, 400, "invalid phone contact point");

        let too_long = normalize_phone("1234567890123456")
            .expect_err("16-digit phone must reject as too long");
        assert_terminal(&too_long, 400, "invalid phone contact point");

        assert_eq!(
            normalize_phone("12345678").expect("8-digit lower bound is valid"),
            "+12345678"
        );
        assert_eq!(
            normalize_phone("(123) 456-789-012-345").expect("15-digit upper bound is valid"),
            "+123456789012345"
        );
    }

    #[test]
    fn hash_contact_point_with_key_hex_is_stable_and_validates_key() {
        // Pins: contact-point hashing is a deterministic keyed BLAKE3 over `tenant:kind:value` and rejects malformed keys.
        let tenant_id = TenantId::from(uuid::Uuid::from_u128(1));
        let key_bytes = [7u8; 32];
        let key_hex = hex::encode(key_bytes);

        let hashed = hash_contact_point_with_key_hex(
            tenant_id,
            ContactPointKind::Email,
            "user@example.com",
            &key_hex,
        )
        .expect("a valid 32-byte hex key should hash");

        // Independently recompute the keyed hash over the documented message layout.
        let expected = blake3::keyed_hash(
            &key_bytes,
            format!("{tenant_id}:email:user@example.com").as_bytes(),
        )
        .to_hex()
        .to_string();
        assert_eq!(hashed, expected);
        assert_eq!(hashed.len(), 64, "BLAKE3-256 hex digest is 64 chars");

        // A different normalized value yields a different digest.
        let other = hash_contact_point_with_key_hex(
            tenant_id,
            ContactPointKind::Email,
            "other@example.com",
            &key_hex,
        )
        .expect("hash should succeed");
        assert_ne!(hashed, other);

        // Non-hex keys are rejected as terminal configuration errors.
        let bad_hex = hash_contact_point_with_key_hex(
            tenant_id,
            ContactPointKind::Email,
            "user@example.com",
            "zzzz",
        )
        .expect_err("non-hex key must reject");
        assert_terminal(&bad_hex, 503, "hex-encoded");

        // Hex keys that decode to the wrong length are rejected.
        let short_key = hash_contact_point_with_key_hex(
            tenant_id,
            ContactPointKind::Email,
            "user@example.com",
            &hex::encode([7u8; 31]),
        )
        .expect_err("31-byte key must reject");
        assert_terminal(&short_key, 503, "must be 32 bytes");
    }

    fn contact_claims(agent_ids: Vec<String>) -> ContactTokenClaims {
        ContactTokenClaims {
            iss: "moa-test".to_string(),
            aud: "moa-contact".to_string(),
            sub: ContactId::new().to_string(),
            exp: 1,
            iat: 0,
            nbf: 0,
            jti: uuid::Uuid::now_v7().to_string(),
            tenant_id: TenantId::from(uuid::Uuid::now_v7()),
            state: ContactVerificationState::Unverified,
            scopes: vec!["agent:session:create".to_string()],
            permissions: serde_json::Value::Null,
            agent_ids,
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
            linked_contact_ids: Vec::new(),
        }
    }

    #[test]
    fn contact_point_delivery_routes_email_and_phone_only() {
        // Pins: OTP delivery supports email and SMS contact points, not external ids or anonymous handles.
        let email = contact_point_delivery(ContactPointKind::Email, "USER@EXAMPLE.COM", None)
            .expect("email contact point should support delivery");
        assert_eq!(email.channel, Channel::Email);
        assert_eq!(email.destination, "user@example.com");

        let phone = contact_point_delivery(
            ContactPointKind::Phone,
            "(500) 555-0006",
            Some(Channel::Sms),
        )
        .expect("phone contact point should support SMS delivery");
        assert_eq!(phone.channel, Channel::Sms);
        assert_eq!(phone.destination, "+5005550006");

        let mismatch = contact_point_delivery(
            ContactPointKind::Email,
            "user@example.com",
            Some(Channel::Sms),
        )
        .expect_err("email contact point should reject SMS delivery");
        let mismatch = format!("{mismatch:?}");
        assert!(
            mismatch.contains("delivery channel"),
            "unexpected mismatch error: {mismatch}"
        );

        let external = contact_point_delivery(ContactPointKind::ExternalId, "customer-123", None)
            .expect_err("external id should not support OTP delivery");
        let external = format!("{external:?}");
        assert!(
            external.contains("email and phone"),
            "unexpected external-id error: {external}"
        );
    }

    #[test]
    fn contact_allows_channel_accounts_for_self_and_canonical_contacts_only() {
        // Pins: channel-account validation does not follow linked contacts by default.
        let contact_id = ContactId::new();
        let canonical_id = ContactId::new();
        let linked_id = ContactId::new();
        let unrelated_id = ContactId::new();

        assert!(contact_allows_channel_contact(
            contact_id,
            Some(canonical_id),
            contact_id
        ));
        assert!(contact_allows_channel_contact(
            contact_id,
            Some(canonical_id),
            canonical_id
        ));
        assert!(!contact_allows_channel_contact(
            contact_id,
            Some(canonical_id),
            linked_id
        ));
        assert!(!contact_allows_channel_contact(
            contact_id,
            Some(canonical_id),
            unrelated_id
        ));
    }
}
