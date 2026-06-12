//! PII pseudonymization vault helpers.
//!
//! Production deployments should back the workspace secret and data encryption
//! key with KMS. The local implementation keeps only redacted lineage payloads
//! outside the vault and stores reversible plaintext side data behind a separate
//! `pii_vault` schema when a Postgres pool is configured.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hmac::{Hmac, Mac};
use moa_memory_pii::classify_heuristic;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::error::{AuditError, Result};

type HmacSha256 = Hmac<Sha256>;
const CIPHERTEXT_VERSION_V2: u8 = 2;
const AES_GCM_NONCE_LEN: usize = 12;

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

    /// Marks a subject as erased and returns the number of subject-key rows touched.
    ///
    /// Production KMS key destruction happens behind the key handle represented
    /// by the `erased_at` marker.
    pub async fn erase_subject(&self, workspace_id: &str, subject_pseudonym: &[u8]) -> Result<u64> {
        if let Some(pool) = &self.pool {
            let affected = sqlx::query(
                r#"
                UPDATE pii_vault.subject_keys
                SET erased_at = now()
                WHERE workspace_id = $1 AND subject_pseudonym = $2
                "#,
            )
            .bind(workspace_id)
            .bind(subject_pseudonym)
            .execute(pool)
            .await?
            .rows_affected();
            return Ok(affected);
        }
        Ok(0)
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
            let ciphertext = self.encrypt_plaintext(plaintext.as_bytes())?;
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
                "version": 2,
                "nonce": "ciphertext_prefix",
            }))
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    fn encrypt_plaintext(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let key_hash = blake3::derive_key("moa-lineage-audit-pii-vault-v1", &self.workspace_secret);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_hash));
        let mut nonce_bytes = [0_u8; AES_GCM_NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher
            .encrypt(nonce, plaintext)
            .map_err(|_| AuditError::Invalid("PII encryption failed".to_string()))?;
        let mut framed = Vec::with_capacity(1 + AES_GCM_NONCE_LEN + encrypted.len());
        framed.push(CIPHERTEXT_VERSION_V2);
        framed.extend_from_slice(&nonce_bytes);
        framed.extend_from_slice(&encrypted);
        Ok(framed)
    }
}

fn redact_text(
    subject_pseudonym: &[u8],
    text: &str,
) -> (String, Vec<RedactionEvent>, Vec<(String, String)>) {
    let result = classify_heuristic(text);
    let mut spans = result
        .spans
        .iter()
        .filter(|span| {
            span.start < span.end
                && span.end <= text.len()
                && text.is_char_boundary(span.start)
                && text.is_char_boundary(span.end)
        })
        .collect::<Vec<_>>();
    spans.sort_by_key(|span| span.start);

    let mut redacted = String::with_capacity(text.len());
    let mut events = Vec::new();
    let mut plaintexts = Vec::new();
    let mut cursor = 0;
    for span in spans {
        if span.start < cursor {
            continue;
        }
        redacted.push_str(&text[cursor..span.start]);
        let source = &text[span.start..span.end];
        let (trim_start, trim_end) = trim_ascii_punctuation_range(source);
        if trim_start >= trim_end {
            redacted.push_str(source);
            cursor = span.end;
            continue;
        }
        let plaintext = &source[trim_start..trim_end];
        let field = span.category.field_name();
        let stable = blake3::hash(
            [subject_pseudonym, plaintext.as_bytes()]
                .concat()
                .as_slice(),
        );
        let replacement = format!("PII:{field}:{}", &stable.to_hex().to_string()[..8]);
        redacted.push_str(&source[..trim_start]);
        redacted.push_str(&replacement);
        redacted.push_str(&source[trim_end..]);
        events.push(RedactionEvent {
            field: field.to_string(),
            detector: result.model_version.clone(),
            confidence: span.confidence,
            token: replacement,
        });
        plaintexts.push((field.to_string(), plaintext.to_string()));
        cursor = span.end;
    }
    redacted.push_str(&text[cursor..]);
    (redacted, events, plaintexts)
}

fn trim_ascii_punctuation_range(source: &str) -> (usize, usize) {
    let mut start = 0;
    for (index, character) in source.char_indices() {
        if character.is_ascii_punctuation() {
            start = index + character.len_utf8();
        } else {
            break;
        }
    }

    let mut end = source.len();
    for (index, character) in source.char_indices().rev() {
        if index < start {
            break;
        }
        if character.is_ascii_punctuation() {
            end = index;
        } else {
            break;
        }
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::{AES_GCM_NONCE_LEN, CIPHERTEXT_VERSION_V2, PiiVault};

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

    #[tokio::test]
    async fn pii_vault_uses_shared_classifier_but_keeps_audit_token_format() {
        let vault = PiiVault::new_dev(b"workspace-secret".to_vec());
        let first = vault
            .pseudonymize(
                "workspace",
                "alice@example.com",
                "Email alice@example.com phone 555-123-4567 ssn 123-45-6789",
            )
            .await
            .expect("pseudonymize PII text");
        let second = vault
            .pseudonymize(
                "workspace",
                "alice@example.com",
                "Email alice@example.com phone 555-123-4567 ssn 123-45-6789",
            )
            .await
            .expect("pseudonymize PII text again");

        assert_eq!(first.redacted_text, second.redacted_text);
        assert!(!first.redacted_text.contains("alice@example.com"));
        assert!(!first.redacted_text.contains("555-123-4567"));
        assert!(!first.redacted_text.contains("123-45-6789"));
        assert_eq!(
            first
                .redactions
                .iter()
                .map(|event| event.field.as_str())
                .collect::<Vec<_>>(),
            vec!["email", "phone", "ssn"]
        );
        assert_eq!(
            first
                .redactions
                .iter()
                .map(|event| event.confidence)
                .collect::<Vec<_>>(),
            vec![0.80, 0.90, 0.90]
        );
        for event in &first.redactions {
            assert_eq!(event.detector, "moa-heuristic:v1");
            assert!(
                event.token.starts_with(&format!("PII:{}:", event.field)),
                "{event:?}"
            );
        }
    }

    #[test]
    fn pii_encryption_uses_fresh_nonce_per_field() {
        let vault = PiiVault::new_dev(b"workspace-secret".to_vec());

        let first = vault
            .encrypt_plaintext(b"alice@example.com")
            .expect("first encryption should succeed");
        let second = vault
            .encrypt_plaintext(b"+15551234567")
            .expect("second encryption should succeed");

        assert_eq!(first[0], CIPHERTEXT_VERSION_V2);
        assert_eq!(second[0], CIPHERTEXT_VERSION_V2);
        assert_ne!(
            &first[1..1 + AES_GCM_NONCE_LEN],
            &second[1..1 + AES_GCM_NONCE_LEN],
            "AES-GCM nonce must be unique for each encrypted PII field"
        );
    }
}
