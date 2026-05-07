//! Privacy approval-token and manifest-signing helpers.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ApprovalClaims {
    pub(super) sub: String,
    pub(super) jti: String,
    pub(super) exp: i64,
    pub(super) op: String,
    pub(super) subject_user_id: String,
    #[serde(default)]
    pub(super) workspace_id: Option<String>,
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) roles: Vec<String>,
}

impl ApprovalClaims {
    fn has_platform_admin_role(&self) -> bool {
        self.role.as_deref() == Some("platform_admin")
            || self.roles.iter().any(|role| role == "platform_admin")
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct JwtHeader {
    alg: String,
}

pub(super) struct ApprovalTokenVerifier {
    pub(super) verifying_key: VerifyingKey,
}

impl ApprovalTokenVerifier {
    pub(super) fn from_env() -> Result<Self> {
        let raw = env::var(APPROVAL_PUBLIC_KEY_ENV)
            .or_else(|_| env::var(APPROVAL_PUBLIC_KEY_FALLBACK_ENV))
            .with_context(|| {
                format!(
                    "{APPROVAL_PUBLIC_KEY_ENV} or {APPROVAL_PUBLIC_KEY_FALLBACK_ENV} is required"
                )
            })?;
        Self::from_public_key_material(&raw)
    }

    pub(super) fn from_public_key_material(raw: &str) -> Result<Self> {
        let bytes = decode_key_material(raw)?;
        let key_bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("approval public key must be 32 bytes"))?;
        Ok(Self {
            verifying_key: VerifyingKey::from_bytes(&key_bytes)
                .context("invalid approval Ed25519 public key")?,
        })
    }

    pub(super) fn verify(
        &self,
        token: &str,
        expected_op: &str,
        subject_user_id: &str,
        workspace: Option<&str>,
    ) -> Result<ApprovalClaims> {
        let parts = token.split('.').collect::<Vec<_>>();
        if parts.len() != 3 {
            bail!("approval token must be a compact JWT");
        }

        let header: JwtHeader = serde_json::from_slice(&decode_base64url(parts[0])?)
            .context("decoding approval token header")?;
        if header.alg != "EdDSA" {
            bail!("approval token must use EdDSA");
        }

        let claims: ApprovalClaims = serde_json::from_slice(&decode_base64url(parts[1])?)
            .context("decoding approval token claims")?;
        validate_claims(&claims, expected_op, subject_user_id, workspace)?;

        let signature_bytes = decode_base64url(parts[2])?;
        let signature_bytes: [u8; 64] = signature_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("approval token signature must be 64 bytes"))?;
        let signature = Signature::from_bytes(&signature_bytes);
        let signed = format!("{}.{}", parts[0], parts[1]);
        self.verifying_key
            .verify(signed.as_bytes(), &signature)
            .context("approval token signature verification failed")?;
        Ok(claims)
    }
}

fn validate_claims(
    claims: &ApprovalClaims,
    expected_op: &str,
    subject_user_id: &str,
    workspace: Option<&str>,
) -> Result<()> {
    if claims.sub.trim().is_empty() {
        bail!("approval token missing sub");
    }
    if claims.jti.trim().is_empty() {
        bail!("approval token missing jti");
    }
    if claims.op != expected_op {
        bail!("approval token op must be `{expected_op}`");
    }
    if claims.subject_user_id != subject_user_id {
        bail!("approval token subject_user_id mismatch");
    }
    if !claims.has_platform_admin_role() {
        bail!("approval token requires platform_admin role");
    }
    let now = Utc::now().timestamp();
    if claims.exp <= now {
        bail!("approval token expired");
    }
    if let Some(token_workspace) = claims.workspace_id.as_deref()
        && Some(token_workspace) != workspace
    {
        bail!("approval token workspace_id mismatch");
    }
    Ok(())
}

pub(super) async fn consume_approval_jti(pool: &PgPool, claims: &ApprovalClaims) -> Result<()> {
    let expires_at = Utc
        .timestamp_opt(claims.exp, 0)
        .single()
        .ok_or_else(|| anyhow!("approval token exp is out of range"))?;
    let inserted = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO moa.audit_jti_used
            (jti, op, subject_user_id, approver_id, approval_claims, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (jti) DO NOTHING
        RETURNING jti
        "#,
    )
    .bind(&claims.jti)
    .bind(&claims.op)
    .bind(&claims.subject_user_id)
    .bind(&claims.sub)
    .bind(serde_json::to_value(claims)?)
    .bind(expires_at)
    .fetch_optional(pool)
    .await
    .context("recording approval token jti")?;
    ensure_jti_inserted(inserted.as_deref())
}

pub(super) fn ensure_jti_inserted(inserted: Option<&str>) -> Result<()> {
    if inserted.is_some() {
        Ok(())
    } else {
        bail!("approval token replayed")
    }
}

pub(super) struct Ed25519ManifestSigner {
    pub(super) key_id: String,
    pub(super) signing_key: SigningKey,
}

impl Ed25519ManifestSigner {
    pub(super) fn from_env() -> Result<Self> {
        let raw = env::var(EXPORT_SIGNING_KEY_ENV)
            .or_else(|_| env::var(EXPORT_SIGNING_KEY_FALLBACK_ENV))
            .with_context(|| {
                format!("{EXPORT_SIGNING_KEY_ENV} or {EXPORT_SIGNING_KEY_FALLBACK_ENV} is required")
            })?;
        let key_id = env::var(EXPORT_SIGNING_KEY_ID_ENV)
            .unwrap_or_else(|_| "moa-privacy-export-ops".to_string());
        Self::from_signing_key_material(key_id, &raw)
    }

    pub(super) fn from_signing_key_material(key_id: String, raw: &str) -> Result<Self> {
        let bytes = decode_key_material(raw)?;
        let seed = match bytes.len() {
            32 => bytes,
            64 => bytes[..32].to_vec(),
            len => bail!("export signing key must be 32 or 64 bytes, got {len}"),
        };
        let seed: [u8; 32] = seed
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("export signing key must be 32 bytes"))?;
        Ok(Self {
            key_id,
            signing_key: SigningKey::from_bytes(&seed),
        })
    }

    pub(super) fn key_id(&self) -> &str {
        &self.key_id
    }

    pub(super) fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    pub(super) fn sign(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(self.signing_key.sign(bytes).to_bytes().to_vec())
    }
}

fn decode_base64url(value: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value)
        .with_context(|| "invalid base64url value")
}

fn decode_key_material(raw: &str) -> Result<Vec<u8>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("key material is empty");
    }
    if trimmed.len().is_multiple_of(2)
        && trimmed
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return hex::decode(trimmed).context("invalid hex key material");
    }
    BASE64_STANDARD
        .decode(trimmed)
        .or_else(|_| URL_SAFE_NO_PAD.decode(trimmed))
        .context("key material must be hex or base64")
}
