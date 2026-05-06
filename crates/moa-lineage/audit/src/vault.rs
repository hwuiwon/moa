//! PII pseudonymization vault helpers.
//!
//! Production deployments should back the workspace secret and data encryption
//! key with KMS. The local implementation keeps only redacted lineage payloads
//! outside the vault and stores reversible plaintext side data behind a separate
//! `pii_vault` schema when a Postgres pool is configured.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::error::{AuditError, Result};

type HmacSha256 = Hmac<Sha256>;

/// PII vault facade.
#[derive(Clone)]
pub struct PiiVault {
    pool: Option<sqlx::PgPool>,
    workspace_secret: Vec<u8>,
    key_handle: String,
}

/// Result of pseudonymizing one input text.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PseudonymizationOutcome {
    /// HMAC-SHA256 pseudonym for the natural subject identifier.
    pub subject_pseudonym: Vec<u8>,
    /// Text with detected PII replaced by stable tokens.
    pub redacted_text: String,
    /// Redaction events produced while transforming the text.
    pub redactions: Vec<RedactionEvent>,
}

/// One redacted field.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RedactionEvent {
    /// Field class.
    pub field: String,
    /// Detector name.
    pub detector: String,
    /// Detector confidence.
    pub confidence: f32,
    /// Token inserted into the redacted text.
    pub token: String,
}

impl PiiVault {
    /// Creates a vault that only computes pseudonyms and redacted text.
    #[must_use]
    pub fn new_dev(workspace_secret: Vec<u8>) -> Self {
        Self {
            pool: None,
            workspace_secret,
            key_handle: "local-dev".to_string(),
        }
    }

    /// Creates a vault backed by a separate Postgres pool.
    #[must_use]
    pub fn with_pool(
        pool: sqlx::PgPool,
        workspace_secret: Vec<u8>,
        key_handle: impl Into<String>,
    ) -> Self {
        Self {
            pool: Some(pool),
            workspace_secret,
            key_handle: key_handle.into(),
        }
    }

    /// Pseudonymizes a natural subject identifier and redacts obvious PII.
    pub async fn pseudonymize(
        &self,
        workspace_id: &str,
        subject_natural_id: &str,
        text: &str,
    ) -> Result<PseudonymizationOutcome> {
        let subject_pseudonym = self.subject_pseudonym(subject_natural_id)?;
        let (redacted_text, redactions, plaintexts) = redact_text(&subject_pseudonym, text);
        if let Some(pool) = &self.pool {
            self.store_plaintext(pool, workspace_id, &subject_pseudonym, &plaintexts)
                .await?;
        }
        Ok(PseudonymizationOutcome {
            subject_pseudonym,
            redacted_text,
            redactions,
        })
    }

    /// Marks a subject as erased. Production KMS key destruction happens behind
    /// the key handle represented by the `erased_at` marker.
    pub async fn erase_subject(&self, workspace_id: &str, subject_pseudonym: &[u8]) -> Result<()> {
        if let Some(pool) = &self.pool {
            sqlx::query(
                r#"
                UPDATE pii_vault.subject_keys
                SET erased_at = now()
                WHERE workspace_id = $1 AND subject_pseudonym = $2
                "#,
            )
            .bind(workspace_id)
            .bind(subject_pseudonym)
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    /// Computes the deterministic subject pseudonym.
    pub fn subject_pseudonym(&self, subject_natural_id: &str) -> Result<Vec<u8>> {
        let mut mac =
            <HmacSha256 as Mac>::new_from_slice(&self.workspace_secret).map_err(|_| {
                AuditError::Invalid("workspace secret is not valid HMAC material".to_string())
            })?;
        mac.update(subject_natural_id.as_bytes());
        Ok(mac.finalize().into_bytes().to_vec())
    }

    async fn store_plaintext(
        &self,
        pool: &sqlx::PgPool,
        workspace_id: &str,
        subject_pseudonym: &[u8],
        plaintexts: &[(String, String)],
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO pii_vault.subject_keys (
                subject_pseudonym, workspace_id, hmac_key_handle
            )
            VALUES ($1, $2, $3)
            ON CONFLICT (subject_pseudonym) DO UPDATE
            SET hmac_key_handle = EXCLUDED.hmac_key_handle
            "#,
        )
        .bind(subject_pseudonym)
        .bind(workspace_id)
        .bind(&self.key_handle)
        .execute(pool)
        .await?;

        for (field_name, plaintext) in plaintexts {
            let ciphertext = self.encrypt_plaintext(subject_pseudonym, plaintext.as_bytes())?;
            sqlx::query(
                r#"
                INSERT INTO pii_vault.plaintext_side (
                    record_id,
                    subject_pseudonym,
                    workspace_id,
                    field_name,
                    ciphertext,
                    encryption_context
                )
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (record_id) DO NOTHING
                "#,
            )
            .bind(Uuid::now_v7())
            .bind(subject_pseudonym)
            .bind(workspace_id)
            .bind(field_name)
            .bind(ciphertext)
            .bind(serde_json::json!({
                "key_handle": self.key_handle,
                "algorithm": "AES-256-GCM",
            }))
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    fn encrypt_plaintext(&self, subject_pseudonym: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let key_hash = blake3::derive_key("moa-lineage-audit-pii-vault-v1", &self.workspace_secret);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_hash));
        let nonce_hash = blake3::hash(subject_pseudonym);
        let nonce = Nonce::from_slice(&nonce_hash.as_bytes()[..12]);
        cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| AuditError::Invalid("PII encryption failed".to_string()))
    }
}

fn redact_text(
    subject_pseudonym: &[u8],
    text: &str,
) -> (String, Vec<RedactionEvent>, Vec<(String, String)>) {
    let mut redacted = text.to_string();
    let mut events = Vec::new();
    let mut plaintexts = Vec::new();
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|ch: char| ch.is_ascii_punctuation());
        let field = classify_token(trimmed);
        if let Some(field) = field {
            let stable = blake3::hash([subject_pseudonym, trimmed.as_bytes()].concat().as_slice());
            let replacement = format!("PII:{field}:{}", &stable.to_hex().to_string()[..8]);
            redacted = redacted.replace(trimmed, &replacement);
            events.push(RedactionEvent {
                field: field.to_string(),
                detector: "moa-lineage-audit-local".to_string(),
                confidence: 0.9,
                token: replacement,
            });
            plaintexts.push((field.to_string(), trimmed.to_string()));
        }
    }
    (redacted, events, plaintexts)
}

fn classify_token(token: &str) -> Option<&'static str> {
    if token.contains('@') && token.contains('.') {
        return Some("email");
    }
    let digits = token.chars().filter(|ch| ch.is_ascii_digit()).count();
    if digits >= 10
        && token
            .chars()
            .all(|ch| ch.is_ascii_digit() || "+-().".contains(ch))
    {
        return Some("phone");
    }
    if digits == 9
        && token.len() == 11
        && token.as_bytes().get(3) == Some(&b'-')
        && token.as_bytes().get(6) == Some(&b'-')
    {
        return Some("ssn");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::PiiVault;

    #[tokio::test]
    async fn pseudonym_is_deterministic_and_redacts_email() {
        let vault = PiiVault::new_dev(b"workspace-secret".to_vec());
        let first = vault
            .pseudonymize(
                "workspace",
                "alice@example.com",
                "Email alice@example.com now",
            )
            .await
            .expect("pseudonymize");
        let second = vault
            .pseudonymize(
                "workspace",
                "alice@example.com",
                "Email alice@example.com now",
            )
            .await
            .expect("pseudonymize");

        assert_eq!(first.subject_pseudonym, second.subject_pseudonym);
        assert!(first.redacted_text.contains("PII:email:"));
        assert_eq!(first.redactions.len(), 1);
    }
}
