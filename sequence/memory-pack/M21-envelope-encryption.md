# Step M21 — KEK/DEK envelope encryption layer in `moa-security`

_Implement per-workspace KEK + per-fact DEK envelope encryption (AES-GCM) with pluggable key managers (`AwsKms` for cloud, `SoftHsm` for dev), so PHI/restricted facts can be crypto-shredded (M24) by destroying the per-fact DEK while ciphertext remains for audit._

## 1 What this step is about

HIPAA's "right to amend" + GDPR Art. 17 erasure require a deletion path that doesn't break audit chain. The envelope pattern: each fact has a per-fact DEK (Data Encryption Key); the DEK is encrypted ("wrapped") with a workspace-level KEK (Key Encryption Key); the KEK is held in KMS. Crypto-shredding = deleting the wrapped DEK. Without it, the ciphertext can never be decrypted. Audit row remains.

## 2 Files to read

- M00 stack-pin (AES-GCM, KMS/SoftHSM)
- M04 `moa.node_index` + M06 `moa.graph_changelog` (carry envelope columns)
- M24 (the consumer of crypto-shred)

## 3 Goal

1. New module `moa-security/src/envelope.rs` with `EnvelopeCipher`, `KeyManager` trait.
2. `AwsKms` impl (cloud) and `SoftHsm` impl (dev/local).
3. Migration: add envelope columns to `moa.node_index` and `moa.graph_changelog`.
4. Hooks in M08 write protocol so PHI/restricted facts are encrypted before INSERT.
5. Decryption helper for read-time (only when actor has clearance + RLS allowed it through).

## 4 Rules

- **AES-256-GCM** for both DEK encryption (of plaintext) and KEK wrapping (of DEK). KMS handles key rotation for KEK.
- **DEKs are per-fact**, never reused. Generated locally by `OsRng`.
- **Wrapped DEK and ciphertext** stored alongside each row; KEK lives only in KMS/HSM.
- **`pii_class='none'` rows are NOT encrypted**. Skipping non-PII keeps cost down.
- **`pii_class='restricted'` rows MUST be encrypted**, plus a stricter retention policy (M22).
- **Encryption is transparent to GraphStore**: it calls `EnvelopeCipher::encrypt_for(workspace, plaintext)` before INSERT and `decrypt_for(workspace, ciphertext, dek_wrap)` on read.

## 5 Tasks

### 5a Migration

`migrations/M21_envelope.sql`:

```sql
ALTER TABLE moa.node_index
  ADD COLUMN IF NOT EXISTS encryption_algorithm TEXT NOT NULL DEFAULT 'plaintext'
    CHECK (encryption_algorithm IN ('plaintext','AES-GCM-256','SHREDDED')),
  ADD COLUMN IF NOT EXISTS wrapped_dek BYTEA,
  ADD COLUMN IF NOT EXISTS dek_kek_version TEXT,
  ADD COLUMN IF NOT EXISTS ciphertext BYTEA,                   -- properties payload encrypted; properties_summary stays cleartext
  ADD COLUMN IF NOT EXISTS aad_hash BYTEA;                     -- additional data binding to (uid, label, scope)

ALTER TABLE moa.graph_changelog
  ADD COLUMN IF NOT EXISTS encryption_algorithm TEXT NOT NULL DEFAULT 'plaintext',
  ADD COLUMN IF NOT EXISTS wrapped_dek BYTEA,
  ADD COLUMN IF NOT EXISTS dek_kek_version TEXT;

CREATE TABLE IF NOT EXISTS moa.workspace_kek (
    workspace_id   UUID PRIMARY KEY,
    kek_arn        TEXT NOT NULL,                              -- AWS KMS key ARN, or local-mode soft-hsm key id
    kek_version    TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE moa.workspace_kek ENABLE ROW LEVEL SECURITY; ALTER TABLE moa.workspace_kek FORCE ROW LEVEL SECURITY;
CREATE POLICY ws ON moa.workspace_kek FOR SELECT TO moa_app
  USING (workspace_id = moa.current_workspace());
CREATE POLICY admin ON moa.workspace_kek FOR ALL TO moa_promoter USING (true) WITH CHECK (true);
GRANT SELECT ON moa.workspace_kek TO moa_app;
```

### 5b Trait + types

`crates/moa-security/src/envelope.rs`:

```rust
use aes_gcm::{Aes256Gcm, KeyInit, aead::{Aead, AeadCore, OsRng}};
use async_trait::async_trait;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct WrappedRecord {
    pub algorithm: String,                // "AES-GCM-256"
    pub wrapped_dek: Vec<u8>,             // wrapped by KEK
    pub kek_version: String,              // tracks rotation
    pub ciphertext: Vec<u8>,
    pub aad_hash: Vec<u8>,
}

#[async_trait]
pub trait KeyManager: Send + Sync {
    async fn wrap_dek(&self, workspace: Uuid, dek: &[u8]) -> Result<(Vec<u8>, String)>;
    async fn unwrap_dek(&self, workspace: Uuid, wrapped: &[u8], kek_version: &str) -> Result<Vec<u8>>;
}

pub struct EnvelopeCipher { pub km: Arc<dyn KeyManager> }

impl EnvelopeCipher {
    pub async fn encrypt_for(&self, workspace: Uuid, plaintext: &[u8], aad: &[u8])
        -> Result<WrappedRecord>
    {
        let mut dek = vec![0u8; 32];
        rand::thread_rng().fill_bytes(&mut dek);
        let cipher = Aes256Gcm::new_from_slice(&dek)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let mut ct = nonce.to_vec();
        ct.extend(cipher.encrypt(&nonce, aes_gcm::aead::Payload { msg: plaintext, aad })?);
        let (wrapped_dek, kek_version) = self.km.wrap_dek(workspace, &dek).await?;
        Ok(WrappedRecord {
            algorithm: "AES-GCM-256".into(),
            wrapped_dek, kek_version, ciphertext: ct, aad_hash: blake3::hash(aad).as_bytes().to_vec(),
        })
    }

    pub async fn decrypt_for(&self, workspace: Uuid, w: &WrappedRecord, aad: &[u8])
        -> Result<Vec<u8>>
    {
        let dek = self.km.unwrap_dek(workspace, &w.wrapped_dek, &w.kek_version).await?;
        let cipher = Aes256Gcm::new_from_slice(&dek)?;
        let (nonce, ct) = w.ciphertext.split_at(12);
        let nonce = aes_gcm::Nonce::from_slice(nonce);
        cipher.decrypt(nonce, aes_gcm::aead::Payload { msg: ct, aad })
            .map_err(|e| anyhow!("decrypt: {}", e))
    }
}
```

### 5c AwsKms KeyManager

```rust
pub struct AwsKmsKeyManager { client: aws_sdk_kms::Client, key_arn_for: HashMap<Uuid, String> }
#[async_trait]
impl KeyManager for AwsKmsKeyManager {
    async fn wrap_dek(&self, ws: Uuid, dek: &[u8]) -> Result<(Vec<u8>, String)> {
        let arn = self.key_arn_for.get(&ws).ok_or(anyhow!("no kek for ws"))?;
        let resp = self.client.encrypt().key_id(arn).plaintext(Blob::new(dek)).send().await?;
        Ok((resp.ciphertext_blob.unwrap().into_inner(), arn.clone()))
    }
    async fn unwrap_dek(&self, _: Uuid, wrapped: &[u8], _kek_version: &str) -> Result<Vec<u8>> {
        let resp = self.client.decrypt().ciphertext_blob(Blob::new(wrapped)).send().await?;
        Ok(resp.plaintext.unwrap().into_inner())
    }
}
```

### 5d SoftHsm KeyManager (dev only)

```rust
pub struct SoftHsmKeyManager { keys: dashmap::DashMap<Uuid, [u8; 32]> }
#[async_trait]
impl KeyManager for SoftHsmKeyManager {
    async fn wrap_dek(&self, ws: Uuid, dek: &[u8]) -> Result<(Vec<u8>, String)> {
        let kek = self.keys.entry(ws).or_insert_with(|| {
            let mut k = [0u8; 32]; rand::thread_rng().fill_bytes(&mut k); k
        }).clone();
        let cipher = Aes256Gcm::new_from_slice(&kek)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let mut wrapped = nonce.to_vec();
        wrapped.extend(cipher.encrypt(&nonce, dek)?);
        Ok((wrapped, "v1".into()))
    }
    async fn unwrap_dek(&self, ws: Uuid, wrapped: &[u8], _: &str) -> Result<Vec<u8>> {
        let kek = self.keys.get(&ws).ok_or(anyhow!("no kek"))?.clone();
        let cipher = Aes256Gcm::new_from_slice(&kek)?;
        let (nonce, ct) = wrapped.split_at(12);
        let nonce = aes_gcm::Nonce::from_slice(nonce);
        cipher.decrypt(nonce, ct).map_err(|e| anyhow!("decrypt: {}", e))
    }
}
```

### 5e Hook into M08 write protocol

Update `create_node` / `supersede_node` to:
- If `intent.pii_class >= Pii`, encrypt the `properties` JSON, store ciphertext + wrapped_dek + kek_version + aad_hash columns, blank `properties_summary` in node_index.
- Otherwise leave plaintext.

```rust
let aad = format!("{}|{}|{}", intent.uid, intent.label.as_str(), intent.scope);
if intent.pii_class >= PiiClass::Pii {
    let w = ctx.envelope.encrypt_for(intent.workspace_id.unwrap(), serde_json::to_vec(&intent.properties)?.as_slice(), aad.as_bytes()).await?;
    /* INSERT with ciphertext / wrapped_dek / dek_kek_version / aad_hash */
}
```

### 5f Decryption at retrieval

`moa-brain/src/retrieval/decrypt.rs`:

```rust
pub async fn maybe_decrypt(node: &NodeIndexRow, ctx: &Ctx) -> Result<Option<serde_json::Value>> {
    if node.encryption_algorithm.as_deref() == Some("plaintext") || node.ciphertext.is_none() { return Ok(None); }
    let aad = format!("{}|{}|{}", node.uid, node.label, node.scope);
    let w = WrappedRecord {
        algorithm: "AES-GCM-256".into(),
        wrapped_dek: node.wrapped_dek.clone().unwrap(),
        kek_version: node.dek_kek_version.clone().unwrap(),
        ciphertext: node.ciphertext.clone().unwrap(),
        aad_hash: node.aad_hash.clone().unwrap_or_default(),
    };
    let pt = ctx.envelope.decrypt_for(node.workspace_id.unwrap(), &w, aad.as_bytes()).await?;
    Ok(Some(serde_json::from_slice(&pt)?))
}
```

## 6 Deliverables

- `migrations/M21_envelope.sql`.
- `crates/moa-security/src/envelope.rs` (~400 lines).
- `crates/moa-security/src/kms_aws.rs` (~150 lines).
- `crates/moa-security/src/kms_soft.rs` (~120 lines).
- M08 hook updates.

## 7 Acceptance criteria

1. PHI fact created → ciphertext present in node_index, plaintext absent from properties_summary.
2. Authorized read returns plaintext via maybe_decrypt; unauthorized read sees ciphertext only.
3. KEK rotation: bump kek_version, decrypt-of-old still works (KMS supports versioned decrypt).
4. SoftHsm round-trips; AwsKms round-trips against a localstack image in CI.

## 8 Tests

```sh
cargo test -p moa-security envelope_round_trip
cargo test -p moa-security kek_rotation
cargo test -p moa-memory-graph encrypted_phi_write
```

## 9 Cleanup

- Remove any prior at-rest encryption helpers if they used a single static key.
- Remove any path that wrote PHI plaintext to `properties_summary` — that field is non-PHI summary only.

## 10 What's next

**M22 — pgaudit + S3 Object Lock shipping pipeline.**
