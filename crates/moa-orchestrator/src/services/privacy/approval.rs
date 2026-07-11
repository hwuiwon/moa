//! Approval-token validation for privacy operations.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};
use chrono::Utc;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use moa_core::config::ComplianceConfig;
use moa_core::types::identifiers::TenantId;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use super::handler_error;

/// Signed approval-token claims required before privacy operations touch data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalClaims {
    /// Approving administrator subject identifier.
    pub sub: String,
    /// Unique token identifier used for replay protection.
    pub jti: String,
    /// Expiration timestamp as Unix seconds.
    pub exp: i64,
    /// Approved operation, such as `export` or `erase`.
    pub op: String,
    /// Subject user identifier covered by the approval.
    pub subject_user_id: String,
    /// Tenant identifier covered by the approval.
    pub tenant_id: TenantId,
    /// Optional single role claim.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional role list claim.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl ApprovalClaims {
    fn has_platform_admin_role(&self) -> bool {
        self.role.as_deref() == Some("platform_admin")
            || self.roles.iter().any(|role| role == "platform_admin")
    }
}

#[derive(Debug, Deserialize)]
struct JwtHeader {
    alg: String,
}

/// Verifies compact EdDSA approval JWTs used for privacy operations.
pub struct ApprovalTokenVerifier {
    /// Ed25519 public key that verifies approval tokens.
    pub verifying_key: VerifyingKey,
}

impl ApprovalTokenVerifier {
    /// Builds a verifier from the configured approval public key material.
    pub fn from_config(config: &ComplianceConfig) -> Result<Self, HandlerError> {
        let raw = config
            .privacy_approval_public_key_hex
            .as_deref()
            .ok_or_else(|| TerminalError::new("MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX is required"))?;
        Self::from_public_key_material(raw)
    }

    /// Builds a verifier from hex or base64 public key material.
    pub fn from_public_key_material(raw: &str) -> Result<Self, HandlerError> {
        let bytes = decode_key_material(raw)?;
        let key_bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            TerminalError::new_with_code(400, "approval public key must be 32 bytes")
        })?;
        Ok(Self {
            verifying_key: VerifyingKey::from_bytes(&key_bytes).map_err(handler_error)?,
        })
    }

    /// Verifies an approval JWT for one operation, subject, and tenant.
    pub fn verify(
        &self,
        token: &str,
        expected_op: &str,
        subject_user_id: &str,
        tenant_id: TenantId,
    ) -> Result<ApprovalClaims, HandlerError> {
        let parts = token.split('.').collect::<Vec<_>>();
        if parts.len() != 3 {
            return Err(
                TerminalError::new_with_code(400, "approval token must be a compact JWT").into(),
            );
        }

        let header: JwtHeader =
            serde_json::from_slice(&decode_base64url(parts[0])?).map_err(handler_error)?;
        if header.alg != "EdDSA" {
            return Err(TerminalError::new_with_code(400, "approval token must use EdDSA").into());
        }

        let claims_payload = decode_base64url(parts[1])?;
        let claims: ApprovalClaims =
            serde_json::from_slice(&claims_payload).map_err(handler_error)?;
        validate_claims(&claims, expected_op, subject_user_id, tenant_id)?;

        let signature_bytes = decode_base64url(parts[2])?;
        let signature_bytes: [u8; 64] = signature_bytes.as_slice().try_into().map_err(|_| {
            TerminalError::new_with_code(400, "approval token signature must be 64 bytes")
        })?;
        let signature = Signature::from_bytes(&signature_bytes);
        let signed = format!("{}.{}", parts[0], parts[1]);
        self.verifying_key
            .verify(signed.as_bytes(), &signature)
            .map_err(handler_error)?;
        Ok(claims)
    }
}

/// Ensures an approval-token JTI insert actually inserted a new row.
pub fn ensure_jti_inserted(inserted: Option<&str>) -> Result<(), HandlerError> {
    if inserted.is_some() {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(409, "approval token replayed").into())
    }
}

fn validate_claims(
    claims: &ApprovalClaims,
    expected_op: &str,
    subject_user_id: &str,
    tenant_id: TenantId,
) -> Result<(), HandlerError> {
    if claims.sub.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "approval token missing sub").into());
    }
    if claims.jti.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "approval token missing jti").into());
    }
    if claims.op != expected_op {
        return Err(TerminalError::new_with_code(
            400,
            format!("approval token op must be `{expected_op}`"),
        )
        .into());
    }
    if claims.subject_user_id != subject_user_id {
        return Err(
            TerminalError::new_with_code(400, "approval token subject_user_id mismatch").into(),
        );
    }
    if claims.tenant_id != tenant_id {
        return Err(TerminalError::new_with_code(400, "approval token tenant_id mismatch").into());
    }
    if !claims.has_platform_admin_role() {
        return Err(TerminalError::new_with_code(
            403,
            "approval token requires platform_admin role",
        )
        .into());
    }
    let now = Utc::now().timestamp();
    if claims.exp <= now {
        return Err(TerminalError::new_with_code(401, "approval token expired").into());
    }
    Ok(())
}

fn decode_base64url(value: &str) -> Result<Vec<u8>, HandlerError> {
    URL_SAFE_NO_PAD.decode(value).map_err(|error| {
        TerminalError::new_with_code(400, format!("invalid base64url value: {error}")).into()
    })
}

/// Decodes hex, standard-base64, or URL-safe-base64 key material.
pub(super) fn decode_key_material(raw: &str) -> Result<Vec<u8>, HandlerError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TerminalError::new_with_code(400, "key material is empty").into());
    }
    if trimmed.len().is_multiple_of(2)
        && trimmed
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return hex::decode(trimmed).map_err(handler_error);
    }
    BASE64_STANDARD
        .decode(trimmed)
        .or_else(|_| URL_SAFE_NO_PAD.decode(trimmed))
        .map_err(handler_error)
}
