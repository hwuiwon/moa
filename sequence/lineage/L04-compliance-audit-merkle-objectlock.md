# Step L04 — Compliance audit tier (opt-in per workspace)

_Add `crates/moa-lineage/audit/` subcrate. Implement BLAKE3 hash chain on the lineage hypertable, periodic RFC 6962 Merkle root publishing via `ct-merkle` to S3 Object Lock (Compliance mode), Ed25519 signing, PII pseudonymization vault, and `moa lineage export` for DSAR. Per-workspace opt-in flag gates everything; engineering tier (L01–L03) keeps working unchanged when audit is off._

## 1 What this step is about

L01–L03 give MOA durable, queryable, observable lineage. L04 adds the **compliance bar**: a tamper-evident integrity chain over every lineage record, periodic Merkle roots published to immutable S3 storage, Ed25519 signatures, PII pseudonymization that survives crypto-shredding for GDPR right-to-erasure, and a DSAR (Data Subject Access Request) export.

This is the layer that maps to:
- **EU AI Act Article 12** (record-keeping for high-risk systems; applicable 2 Aug 2026)
- **EU AI Act Article 86** (right to explanation)
- **GDPR Article 22** (automated decision-making) + Article 17 (erasure)
- **NIST AI RMF + SP 800-53 control overlays** (the AI Agent Standards Initiative announced Feb 2026 — relevant to MOA's autonomous-agent posture)
- **ISO/IEC 42001 Annex A** (event logging, data lifecycle, explainability)
- **SEC 17a-4 / FINRA / CFTC** WORM requirements (S3 Object Lock Compliance mode is formally assessed)

L04 is **opt-in per workspace** — the engineering tier in L01–L03 is the design floor. Compliance is a strict superset enabled by a workspace flag. Workspaces that don't need it pay no extra cost.

**This prompt MUST end with a hard stop for external cryptographic review before any compliance certifications are claimed.** The `ct-merkle` crate is explicitly "not audited" by its authors. MOA's implementation must be reviewed by an external cryptographer or appsec firm before being represented as compliance evidence to a regulator or customer auditor.

## 2 Files to read

- L01–L03 (this prompt builds on all three)
- M22 prompt (pgaudit + S3 Object Lock infrastructure that L04 reuses)
- `crates/moa-lineage/sink/sql/schema.sql` (extend with compliance columns)
- `ct-merkle` crate docs (RFC 6962 implementation; **note "not audited"** disclaimer)
- `blake3` crate docs (the hash primitive)
- `ed25519-dalek` crate docs (signing the Merkle roots)
- `object_store` crate docs (Object Lock support via the S3 backend)
- EU AI Act Article 12 + Article 86 (current text)
- GDPR Articles 17 + 22 (current text)
- AWS S3 Object Lock docs (Governance vs Compliance modes)

## 3 Goal

After L04:

- `crates/moa-lineage/audit/` (package `moa-lineage-audit`) exists with the audit traits, hash chain, Merkle log, signing, PII vault, and DSAR exporter.
- `analytics.turn_lineage.prev_hash` is populated in compliance-tier workspaces (NULL elsewhere).
- A `compliance_workspaces` table tracks per-workspace opt-in.
- `analytics.audit_roots` table records each published Merkle root.
- A `moa-audit-roots` S3 bucket exists with Object Lock Compliance mode + 10-year retention.
- A periodic worker publishes Merkle roots every 5 minutes per opted-in workspace.
- `analytics.pii_vault` schema (separate logical DB or schema with separate connection pool, KMS-encrypted) holds the HMAC keys + reversible side-table for plaintext.
- Decision lineage (`LineageEvent::Decision`) captures every PII redaction event, ACL filter, scope enforcement, and policy version.
- `moa lineage export <subject-id>` produces a DSAR audit pack (zip with JSON+Parquet+inclusion proofs).
- `moa lineage verify <window>` validates the chain + Merkle root.
- `cargo build --workspace` clean.
- **External cryptographic review documented as a follow-up before claiming compliance certifications.**

## 4 Rules

- **Per-workspace opt-in.** A workspace flag in `compliance_workspaces` toggles the entire L04 surface for that workspace. Off → behaves as L01–L03. On → hash chain, Merkle, decision capture, vault.
- **Audit writes are at-least-once.** Same as L01. Idempotency via `(turn_id, record_kind, ts)`. Duplicate records get the same `prev_hash` (deterministic from canonicalized payload + previous hash).
- **No PII in the hash chain.** Plaintext PII never enters `analytics.turn_lineage.payload`. The redaction service redacts before write; the chain hashes redacted content. Plaintext lives only in the vault, keyed by per-subject HMAC.
- **Crypto-shredding for erasure.** GDPR Article 17 is honored by destroying the per-subject HMAC key in the vault. The chain remains verifiable; the subject is irreversibly anonymous.
- **Object Lock is real.** S3 bucket has Object Lock enabled at creation (cannot be added later). Compliance mode is irreversible — even AWS root cannot delete or shorten retention. Use Governance mode in development, Compliance in production. Test environments use a separate bucket.
- **`ct-merkle` is "not audited"** by its authors. Document this prominently in the audit crate's README. Block certification claims behind external review (a non-code deliverable). Use BLAKE3 as the primitive (faster than SHA-256; well-vetted).
- **Per-region buckets** if MOA operates across regions. Cross-region audit replication is out of scope for this prompt.
- **No new external service dependency for the basic path.** AWS S3 + KMS suffice. HashiCorp Vault is a future enhancement, not a prereq.

## 5 Tasks

### 5a Add the subcrate

```sh
mkdir -p crates/moa-lineage/audit/src
```

Add to workspace `Cargo.toml`:

```toml
"crates/moa-lineage/audit",
```

`crates/moa-lineage/audit/Cargo.toml`:

```toml
[package]
name = "moa-lineage-audit"
version = "0.1.0"
edition = "2024"

[dependencies]
moa-core         = { path = "../../moa-core" }
moa-lineage-core = { path = "../core" }
blake3           = "1"
ct-merkle        = "0.2"
ed25519-dalek    = { version = "2", features = ["pem", "pkcs8"] }
hmac             = "0.12"
sha2             = "0.10"
base64           = "0.22"
zstd             = "0.13"
zip              = "2"
tokio            = { workspace = true, features = ["fs", "rt", "macros", "sync", "time"] }
tokio-postgres   = { workspace = true }
deadpool-postgres = { workspace = true }
object_store     = { version = "0.11", features = ["aws"] }
arrow            = "53"
parquet          = { version = "53", features = ["async", "arrow"] }
serde            = { workspace = true, features = ["derive"] }
serde_json       = { workspace = true }
serde_canonical_json = "1"
uuid             = { workspace = true }
chrono           = { workspace = true }
tracing          = { workspace = true }
thiserror        = { workspace = true }
```

Update `crates/moa-lineage/README.md` to mark `audit/` as shipped.

### 5b Schema additions

Append to `crates/moa-lineage/sink/sql/schema.sql`:

```sql
-- Per-workspace compliance opt-in
CREATE TABLE IF NOT EXISTS analytics.compliance_workspaces (
    workspace_id      UUID PRIMARY KEY,
    enabled           BOOLEAN NOT NULL DEFAULT TRUE,
    enabled_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    retention_years   INT NOT NULL DEFAULT 10,
    s3_bucket         TEXT NOT NULL,
    kms_key_id        TEXT,
    signing_key_label TEXT NOT NULL,         -- handle into the signing key vault
    notes             TEXT
);

-- Audit roots: one row per published Merkle root
CREATE TABLE IF NOT EXISTS analytics.audit_roots (
    root_id           UUID PRIMARY KEY,
    workspace_id      UUID NOT NULL,
    window_start      TIMESTAMPTZ NOT NULL,
    window_end        TIMESTAMPTZ NOT NULL,
    record_count      BIGINT NOT NULL,
    merkle_root       BYTEA NOT NULL,        -- 32 bytes (BLAKE3-256)
    signature         BYTEA NOT NULL,        -- Ed25519 signature
    signing_key_label TEXT NOT NULL,
    s3_object_uri     TEXT NOT NULL,
    s3_object_etag    TEXT NOT NULL,
    object_lock_mode  TEXT NOT NULL,         -- 'GOVERNANCE' | 'COMPLIANCE'
    retain_until      TIMESTAMPTZ NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS ix_audit_roots_workspace_window
    ON analytics.audit_roots (workspace_id, window_end DESC);

-- PII vault — separate schema, separate connection pool, separate retention
CREATE SCHEMA IF NOT EXISTS pii_vault;

CREATE TABLE IF NOT EXISTS pii_vault.subject_keys (
    subject_pseudonym BYTEA PRIMARY KEY,        -- HMAC(workspace_secret, subject_natural_id)
    workspace_id      UUID NOT NULL,
    hmac_key_handle   TEXT NOT NULL,            -- KMS handle to the actual HMAC key
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    erased_at         TIMESTAMPTZ                -- set when crypto-shredded
);

CREATE TABLE IF NOT EXISTS pii_vault.plaintext_side (
    record_id         UUID PRIMARY KEY,
    subject_pseudonym BYTEA NOT NULL,
    workspace_id      UUID NOT NULL,
    field_name        TEXT NOT NULL,            -- 'email' | 'phone' | 'ssn' | ...
    ciphertext        BYTEA NOT NULL,           -- AES-256-GCM via KMS DEK
    encryption_context JSONB NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (subject_pseudonym) REFERENCES pii_vault.subject_keys(subject_pseudonym)
);

CREATE INDEX IF NOT EXISTS ix_plaintext_subject ON pii_vault.plaintext_side (subject_pseudonym);
CREATE INDEX IF NOT EXISTS ix_plaintext_workspace ON pii_vault.plaintext_side (workspace_id, created_at);
```

A retention policy on `pii_vault.plaintext_side` mirrors the workspace's PII retention rules; the `subject_keys` row stays forever (its erasure means crypto-shredding the row's `hmac_key_handle` in KMS, not the row itself).

### 5c Hash chain implementation

`crates/moa-lineage/audit/src/lib.rs`:

```rust
pub mod chain;
pub mod merkle;
pub mod signing;
pub mod vault;
pub mod decision;
pub mod export;

pub use chain::{HashChain, ChainHashError, canonical_payload_hash};
pub use merkle::{MerkleRootPublisher, RootPublisherConfig};
pub use signing::{SigningKey, SigningKeyVault};
pub use vault::{PiiVault, PseudonymizationOutcome};
pub use decision::{DecisionRecord, DecisionKind};
pub use export::{DsarExporter, DsarBundle};
```

`chain.rs` — the hash chain primitive:

```rust
use blake3::{Hash, Hasher};
use moa_lineage_core::LineageEvent;

pub fn canonical_payload_hash(payload: &serde_json::Value) -> Hash {
    // Canonicalize the JSON (sort keys, no whitespace, deterministic numbers).
    let canonical = serde_canonical_json::to_string(payload).expect("canonicalize");
    blake3::hash(canonical.as_bytes())
}

pub fn next_chain_hash(prev: Option<Hash>, payload_hash: Hash) -> Hash {
    let mut h = Hasher::new();
    if let Some(p) = prev {
        h.update(p.as_bytes());
    } else {
        h.update(b"\0\0\0\0moa-audit-genesis-v1\0\0\0\0");
    }
    h.update(payload_hash.as_bytes());
    h.finalize()
}

pub struct HashChain;

impl HashChain {
    /// Returns the integrity hash and prev_hash for an event in compliance mode.
    /// `prev` should be the previous chain hash for this workspace, fetched from
    /// `analytics.turn_lineage` in a transaction that selects the row with
    /// the highest sequence_id for this workspace.
    pub fn link(prev: Option<Hash>, payload: &serde_json::Value) -> (Hash, Option<Hash>) {
        let payload_hash = canonical_payload_hash(payload);
        let chain_hash = next_chain_hash(prev, payload_hash);
        (chain_hash, prev)
    }
}
```

The L01 writer is extended: when the workspace is opted into compliance, before COPY, the writer:

1. SELECTs the most recent `integrity_hash` for this `workspace_id` (with `FOR UPDATE` on a per-workspace row in a small `analytics.compliance_workspace_state` table to serialize chain extension).
2. Computes `(integrity_hash, prev_hash)` from the canonical payload.
3. Inserts with both fields populated.

The serialization is per-workspace: chains do not interleave across workspaces. This means the lock contention is bounded to one workspace at a time, not the whole DB.

```sql
-- Per-workspace chain state, used to serialize chain extension
CREATE TABLE IF NOT EXISTS analytics.compliance_workspace_state (
    workspace_id      UUID PRIMARY KEY,
    last_integrity_hash BYTEA,
    last_ts           TIMESTAMPTZ,
    record_count      BIGINT NOT NULL DEFAULT 0,
    last_root_id      UUID
);
```

A simple Postgres advisory lock on `hashtext('compliance:' || workspace_id)` is sufficient to serialize per-workspace chain extension across multiple writer instances.

### 5d Merkle root publisher

`merkle.rs`:

```rust
use ct_merkle::{CtMerkleTree, RootHash};
use blake3::Hash;
use std::time::Duration;

pub struct RootPublisherConfig {
    pub publish_interval: Duration,    // default 5m
    pub max_window_records: usize,     // default 100K
    pub max_window_age:    Duration,   // default 15m
    pub object_lock_mode:  ObjectLockMode,    // GOVERNANCE in dev, COMPLIANCE in prod
}

pub struct MerkleRootPublisher {
    cfg: RootPublisherConfig,
    pool: deadpool_postgres::Pool,
    s3: std::sync::Arc<dyn object_store::ObjectStore>,
    signing: signing::SigningKey,
    workspace_id: uuid::Uuid,
}

impl MerkleRootPublisher {
    pub async fn run(self, cancel: tokio_util::sync::CancellationToken) {
        let mut interval = tokio::time::interval(self.cfg.publish_interval);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick()    => {}
            }
            if let Err(e) = self.publish_one_window().await {
                tracing::error!("merkle root publish failed: {e}");
            }
        }
    }

    async fn publish_one_window(&self) -> anyhow::Result<()> {
        // 1. Begin transaction; FOR UPDATE on compliance_workspace_state
        // 2. Read all (turn_id, record_kind, ts, integrity_hash) since last_root_id
        // 3. Build CtMerkleTree<blake3::Hash> of integrity_hashes in (workspace, ts, turn_id, record_kind) order
        // 4. Compute root hash
        // 5. Sign root_bytes || canonical(window_meta) with Ed25519
        // 6. Build the manifest (JSON):
        //      { version: "1", workspace_id, window_start, window_end, count,
        //        merkle_root: base64, signature: base64, signing_key_label, prev_root_id, ... }
        // 7. PUT to s3://moa-audit-roots/workspace=<uuid>/window=<ts>.json
        //      with ObjectLockMode + ObjectLockRetainUntilDate from cfg.
        // 8. INSERT into analytics.audit_roots
        // 9. UPDATE compliance_workspace_state.last_root_id
        // 10. COMMIT
        Ok(())
    }
}
```

`ct-merkle` provides RFC 6962 inclusion proofs (item ∈ tree) and consistency proofs (T₂ extends T₁) — the auditor-friendly shape.

### 5e Signing key vault

`signing.rs`:

```rust
use ed25519_dalek::{Signer, SigningKey as Ed25519Signing, VerifyingKey, Signature};
use base64::Engine;

pub struct SigningKey {
    label: String,
    inner: Ed25519Signing,
    verifying: VerifyingKey,
}

#[async_trait::async_trait]
pub trait SigningKeyVault: Send + Sync {
    async fn get(&self, label: &str) -> anyhow::Result<SigningKey>;
    async fn rotate(&self, label: &str) -> anyhow::Result<SigningKey>;
    async fn list(&self) -> anyhow::Result<Vec<String>>;
}

/// Local dev impl — file-backed PKCS#8.
pub struct LocalSigningKeyVault { pub root: std::path::PathBuf }

/// Production impl — AWS KMS asymmetric SIGN_VERIFY keys (ECC_NIST_P256 or ED25519).
/// Note: AWS KMS supports Ed25519 for SIGN_VERIFY since 2024.
pub struct KmsSigningKeyVault { pub kms: aws_sdk_kms::Client }
```

For development, file-backed PKCS#8 is fine. For production, KMS Ed25519 keeps the private key in HSM-backed storage and audited via CloudTrail. Document both paths in `architecture.md`.

### 5f PII vault

`vault.rs`:

```rust
use blake3::Hash;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub struct PiiVault {
    pool: deadpool_postgres::Pool,
    workspace_secret: Vec<u8>,    // KMS-encrypted at rest, in-memory only
}

#[derive(Debug, Clone)]
pub struct PseudonymizationOutcome {
    pub subject_pseudonym: Vec<u8>,    // HMAC-SHA256(workspace_secret, subject_natural_id)
    pub redacted_text: String,         // text with PII replaced by tokens
    pub redactions: Vec<RedactionEvent>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RedactionEvent {
    pub field: String,        // 'email' | 'phone' | 'ssn' | ...
    pub detector: String,     // 'openai/privacy-filter' | 'manual'
    pub confidence: f32,
    pub token: String,        // 'PII:email:01234' — stable per (subject, field) within a session
}

impl PiiVault {
    /// Pseudonymize: emit (pseudonym, redacted_text, events) and store ciphertext.
    pub async fn pseudonymize(
        &self,
        workspace_id: uuid::Uuid,
        subject_natural_id: &str,
        text: &str,
    ) -> anyhow::Result<PseudonymizationOutcome> { /* ... */ Ok(todo!()) }

    /// Crypto-shred: schedule the per-subject KMS key for deletion.
    /// After deletion, the chain remains verifiable but plaintext is unrecoverable.
    pub async fn erase_subject(
        &self,
        workspace_id: uuid::Uuid,
        subject_pseudonym: &[u8],
    ) -> anyhow::Result<()> { /* ... */ Ok(()) }
}
```

The redaction itself is delegated to the existing `moa-memory/pii/` filter (the openai/privacy-filter sidecar from M09). The vault wraps that filter and adds the HMAC pseudonym + ciphertext side-table.

### 5g Decision lineage

`decision.rs` — flesh out `LineageEvent::Decision`:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub ts: DateTime<Utc>,
    pub kind: DecisionKind,
    pub policy_version: String,
    pub integrity_hash: Vec<u8>,       // BLAKE3 of canonical payload (mirror, for cross-check)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum DecisionKind {
    PiiRedaction(PiiRedactionDecision),
    AclFilter(AclFilterDecision),
    ScopeEnforcement(ScopeEnforcementDecision),
    PrivacyExport(PrivacyExportDecision),
    PrivacyErase(PrivacyEraseDecision),
}
```

Each kind carries the policy that applied, the inputs evaluated (without raw PII), and the outcome. The retriever, ingestion VO, ACL middleware, and DSAR tooling each emit relevant `DecisionRecord`s through the same `LineageSink`. In compliance-tier workspaces, the writer attaches `prev_hash` like any other record.

### 5h DSAR exporter

`export.rs`:

```rust
pub struct DsarExporter {
    pool: deadpool_postgres::Pool,
    s3: std::sync::Arc<dyn object_store::ObjectStore>,
    vault: PiiVault,
    signing: signing::SigningKey,
}

pub struct DsarBundle {
    pub subject_pseudonym: Vec<u8>,
    pub bundle_uri: String,
    pub manifest_signature: Vec<u8>,
    pub record_count: u64,
    pub windows: Vec<RootWindow>,        // Merkle windows the bundle pulls from
}

impl DsarExporter {
    pub async fn export(
        &self,
        workspace_id: uuid::Uuid,
        subject_pseudonym: &[u8],
        out_path: &std::path::Path,
    ) -> anyhow::Result<DsarBundle> {
        // 1. Find every lineage record where payload references subject_pseudonym
        //    (via the GIN index on payload jsonb_path_ops).
        // 2. For each Merkle window touched, fetch the audit_roots row + S3 manifest.
        // 3. For each record, compute the inclusion proof in its window.
        // 4. Optionally fetch the original plaintext from pii_vault.plaintext_side
        //    (legal-hold gated; default off).
        // 5. Bundle as zip:
        //      manifest.json
        //      records/lineage.parquet
        //      records/scores.parquet
        //      records/decisions.parquet
        //      proofs/window-{ts}.json   (one per window, with inclusion proofs)
        //      verifying-keys/<label>.pub
        //      README.txt — auditor instructions: how to verify the chain + Merkle proofs
        // 6. Sign manifest.json hash with the same Ed25519 signing key.
        Ok(todo!())
    }
}
```

CLI: `moa lineage export --subject=<pseudonym|natural-id> --workspace=<uuid> --out=<path.zip>`. If `natural-id` is given (rather than the pseudonym), the exporter computes the pseudonym via the vault and proceeds.

### 5i Verification CLI

`moa lineage verify <window-uri>`:

1. Download the manifest.json from S3 (via `object_store`).
2. Verify the Ed25519 signature over `(merkle_root || window_meta)` with the published verifying key.
3. SELECT the records in the window from the hypertable (or from a provided Parquet file).
4. Recompute the BLAKE3 chain and the Merkle tree.
5. Compare against the published root.
6. Print a structured report.

`moa lineage verify` returns 0 on success, non-zero on any mismatch. CI can call this against a recent window as a smoke test.

### 5j Spawn the publisher and the vault from orchestrator startup

In the orchestrator startup (the same construction site touched in L01–L02), behind a per-workspace check:

```rust
for ws in load_compliance_enabled_workspaces(&pool).await? {
    let cfg = RootPublisherConfig::from_workspace(&ws);
    let signing = signing_vault.get(&ws.signing_key_label).await?;
    let publisher = MerkleRootPublisher::new(pool.clone(), s3.clone(), signing, ws.workspace_id, cfg);
    let handle = tokio::spawn(publisher.run(shutdown_token.clone()));
    self.audit_publisher_handles.push(handle);
}
```

Publishers are per-workspace, lightweight (single tokio task), and idempotent.

### 5k Object Lock bucket bootstrap

`scripts/bootstrap-audit-bucket.sh` creates the bucket with Object Lock at creation time:

```bash
aws s3api create-bucket \
  --bucket "${MOA_AUDIT_BUCKET}" \
  --region "${AWS_REGION}" \
  --object-lock-enabled-for-bucket

aws s3api put-object-lock-configuration \
  --bucket "${MOA_AUDIT_BUCKET}" \
  --object-lock-configuration '{
    "ObjectLockEnabled": "Enabled",
    "Rule": {
      "DefaultRetention": {
        "Mode": "COMPLIANCE",
        "Years": 10
      }
    }
  }'
```

Document this in `architecture.md` with a warning that **Object Lock cannot be added after bucket creation** and **Compliance mode cannot be reduced or bypassed**.

For test environments, a separate bucket with Governance mode + 1-day retention is used. Tests must never run against the production bucket.

### 5l External cryptographic review

Add a hard-stop gate in `architecture.md` and the audit crate's README:

> **ATTESTATION GATE — DO NOT REPRESENT THIS AS COMPLIANCE EVIDENCE TO REGULATORS OR
> CUSTOMERS UNTIL EXTERNAL CRYPTOGRAPHIC REVIEW IS COMPLETE.**
>
> The `ct-merkle` crate is explicitly "not audited" by its authors. MOA's
> implementation in `moa-lineage-audit` must be reviewed by an external
> cryptographer or appsec firm and that review must be linked here before
> the implementation is represented as compliance-grade evidence.
>
> Engineering use (internal debugging, internal forensics) is not gated by
> this review. Compliance use (DSAR exports, regulator response, audit
> attestations, certifications) is.
>
> Review checklist:
> - BLAKE3 usage (canonicalization, chain extension, no length-extension misuse)
> - Ed25519 key handling (HSM-backed in production, no plaintext private keys)
> - ct-merkle inclusion + consistency proof construction
> - PII vault crypto-shredding semantics (key destruction in KMS, not row deletion)
> - S3 Object Lock configuration (Compliance mode in production, Governance mode in dev)
> - Time-stamping of windows (NTP discipline, monotonicity guarantees)
> - Replay attack resistance on the verify path

This is a non-code deliverable but it's part of the prompt's scope.

### 5m Tests

In `crates/moa-lineage/audit/tests/`:

1. `chain_extension.rs` — extend the chain with N records, recompute end-to-end, assert match.
2. `chain_tamper_detection.rs` — modify one record's payload, assert the chain verification fails.
3. `merkle_inclusion_proof.rs` — build a tree of N hashes, generate inclusion proof for index i, verify the proof.
4. `merkle_consistency_proof.rs` — extend tree T₁ → T₂, generate consistency proof, verify.
5. `signing_roundtrip.rs` — sign a Merkle root, verify with the public key, assert tampered root fails.
6. `pii_pseudonymize.rs` — pseudonymize a fixture, assert deterministic pseudonym across calls, assert plaintext stored in vault, assert ciphertext decrypts.
7. `pii_crypto_shred.rs` — store fixtures, schedule erasure, assert subsequent vault reads return `Erased`, assert chain verification still passes.
8. `dsar_export_roundtrip.rs` — seed records for a subject, export, unzip, verify manifest signature, verify Merkle proofs.
9. `verify_cli.rs` — `moa lineage verify` against a freshly published window returns success.
10. `verify_cli_tampered.rs` — manually modify a hypertable row's `integrity_hash`, run verify, assert non-zero exit.
11. `object_lock_smoke.rs` — write a manifest to a local Object-Lock-enabled bucket emulator (or LocalStack), assert deletion attempts fail.

In `crates/moa-cli/tests/`:

12. `lineage_export_e2e.rs` — end-to-end DSAR export against a populated test DB.

## 6 Deliverables

- `crates/moa-lineage/audit/` subcrate (chain, merkle, signing, vault, decision, export modules).
- Schema additions for compliance_workspaces, audit_roots, compliance_workspace_state, pii_vault.
- Writer worker extended to populate `prev_hash` for compliance-tier rows.
- Per-workspace MerkleRootPublisher spawned at startup.
- Decision lineage emission at retriever / ingestion / ACL / privacy boundaries.
- DSAR exporter + `moa lineage export` CLI.
- Verification CLI `moa lineage verify`.
- Bootstrap script for the Object Lock bucket.
- External cryptographic review checklist + attestation gate in `architecture.md` and audit crate README.
- Tests above.

## 7 Acceptance criteria

1. `cargo build --workspace` clean.
2. `cargo test --workspace` green.
3. With a workspace flagged in `compliance_workspaces`, every `analytics.turn_lineage` row for that workspace has non-NULL `prev_hash`. Workspaces not flagged have NULL `prev_hash`.
4. After 5 minutes of operation, `analytics.audit_roots` has at least one row for the flagged workspace with a corresponding S3 object.
5. The S3 object's response to `head_object` shows `ObjectLockMode=COMPLIANCE` (production) or `GOVERNANCE` (dev).
6. `moa lineage verify <window-uri>` exits 0 against a freshly published window.
7. Manually corrupting one record's payload and running verify exits non-zero with a localized error message identifying the failing record.
8. `moa lineage export --subject=<pseudonym>` produces a zip whose manifest signature verifies and whose inclusion proofs validate.
9. After `moa lineage erase --subject=<pseudonym>`, the vault's `plaintext_side` rows for that subject return decryption failure (KMS key destroyed), but `moa lineage verify` over the affected windows still passes.
10. `cargo run -p xtask --bin audit_legacy_memory` (the C06 guardrail) still passes.
11. The attestation gate is present and prominent in `architecture.md` and the audit crate's README.

## 8 Tests

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings

# Bootstrap dev bucket
MOA_AUDIT_BUCKET=moa-audit-dev AWS_REGION=us-east-1 \
  scripts/bootstrap-audit-bucket.sh

# Enable a test workspace
psql -c "INSERT INTO analytics.compliance_workspaces \
         (workspace_id, s3_bucket, signing_key_label) \
         VALUES ('00000000-0000-0000-0000-000000000001', \
                 'moa-audit-dev', 'dev-signing-1')" $TEST_DB

# Run a session
moa "Tell me about OAuth"
sleep 360    # wait for Merkle publish
psql -c "SELECT count(*) FROM analytics.audit_roots WHERE workspace_id = '...'" $TEST_DB

# Verify
moa lineage verify s3://moa-audit-dev/workspace=.../window=...json
echo "Exit: $?"   # 0

# DSAR export
moa lineage export --subject=<pseudonym> --workspace=<uuid> --out=/tmp/dsar.zip
unzip -l /tmp/dsar.zip
moa lineage verify /tmp/dsar.zip   # also validates the bundled proofs

# Tamper test
psql -c "UPDATE analytics.turn_lineage SET integrity_hash = '\\x00...' \
         WHERE turn_id = '<some-turn-uuid>'" $TEST_DB
moa lineage verify s3://moa-audit-dev/workspace=.../window=...json
echo "Exit: $?"   # non-zero, with localized error
```

## 9 Cleanup

- Confirm the dev bucket and prod bucket are different. Test runs MUST NOT touch the prod bucket.
- Confirm `serde_canonical_json` produces identical bytes for equivalent JSON across machines (cross-arch determinism test).
- Confirm `blake3` and `ct-merkle` versions are pinned in `Cargo.toml`.
- Document the rotation procedure for signing keys (new key, sign new windows; old key remains valid for verifying old windows).
- Document the attestation gate one more time in the release-engineering runbook.

## 10 What's next

**HARD STOP — external cryptographic review before claiming compliance certifications.** That is a non-code deliverable scheduled outside this prompt.

Engineering and operational use of L04 starts immediately. Compliance representations to regulators or customers wait for the external review.

After the review lands and any findings are addressed, MOA's two-tier observability and explainability layer is complete. The remaining roadmap items (additional embedders, additional providers, cross-region replication, multi-cloud audit) are independent enhancements.
