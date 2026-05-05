# Step M21 — Envelope encryption: DEFERRED (decision log)

_Document the decision to defer KEK/DEK envelope encryption. Redaction at ingestion (the M09 PII service) is the v1 privacy boundary. No code changes required. The next step is M22._

## 1 What this step is about

The original M21 designed a per-workspace KEK + per-fact DEK envelope encryption layer with crypto-shred erasure. That design was paired with a spans-based PII model where the original PHI text persisted (encrypted) and could be selectively decrypted by authorized readers.

During reshape, the privacy model changed: the M09 PII service now redacts text at ingestion via the `openai/privacy-filter` HuggingFace model. The redacted text is what persists in the graph. There is no original PHI stored, so there is nothing to encrypt or crypto-shred.

This prompt records the decision and the conditions under which it should be revisited. It does not change any code.

## 2 Files to read

- `crates/moa-memory/pii/src/lib.rs` (current redaction-based PII contract)
- `crates/moa-memory/graph/src/write.rs` (M08 write protocol — note no encryption hooks)
- M22 prompt (next in sequence)

## 3 Goal

1. `docs/architecture/decisions/0001-envelope-encryption-deferred.md` records the decision in ADR format.
2. No schema changes (the M21 envelope columns described in the original prompt are NOT added).
3. No code changes in `moa-security`. The crate exists for future security plumbing but does not get the `EnvelopeCipher` / `KeyManager` traits in this version.
4. M22 (pgaudit + S3 Object Lock) proceeds unchanged.

## 4 Rules

- **No schema migration** for envelope columns (`encryption_algorithm`, `wrapped_dek`, `dek_kek_version`, `ciphertext`, `aad_hash`). Future re-introduction will add them via a fresh migration.
- **No `moa.workspace_kek` table.** Same reason.
- **`moa-security` crate keeps its current shape.** It contains whatever plumbing exists from earlier steps (e.g., approval-token verification used by M23/M24); no `EnvelopeCipher` is added.
- **`pii_class` on `node_index` is preserved.** The PII service returns it (per M09), and downstream retention policies use it. This is unrelated to encryption.

## 5 Tasks

### 5a Write the ADR

Create `docs/architecture/decisions/0001-envelope-encryption-deferred.md`:

```markdown
# ADR 0001 — Envelope encryption deferred to v1.1

**Status:** Accepted
**Date:** [today]
**Supersedes:** original M21 design

## Context

The original M21 specified per-workspace KEK + per-fact DEK envelope encryption,
intended to coexist with a spans-based PII model where original PHI text
persisted in encrypted form and could be selectively decrypted by authorized
readers. Crypto-shredding (deleting the wrapped DEK) was the GDPR/HIPAA
erasure path for high-sensitivity records.

During reshape between M19 and M20, the PII model changed. The M09 PII service
now redacts text at ingestion using the `openai/privacy-filter` HuggingFace
model. The redacted text is what the ingestion pipeline embeds and the graph
stores. Original PHI does not persist anywhere in the canonical store.

## Decision

Envelope encryption is deferred. v1 relies on redaction at ingestion as the
privacy boundary. Hard-purge via M08 `hard_purge` is the only erasure path.

## Consequences

**Positive:**
- Simpler ingestion pipeline (no per-fact key generation, no KMS round-trip per
  write).
- Simpler erasure (no crypto-shred mode in M24; hard-purge handles all cases).
- Reduced KMS dependency surface for v1.
- HIPAA Safe Harbor de-identification provides equivalent protection for the
  facts redaction handles.

**Negative:**
- No defense-in-depth if the PII service has a recall failure on a category
  not in its training distribution. A redaction miss means cleartext PHI lands
  in `properties_summary` / `name_tsv`.
- No mechanism to retain encrypted-but-unreadable PHI for compliance retention
  while denying read access. Hard-purge is irreversible.
- Workspaces that need stricter posture (e.g., `pii_class='restricted'` for
  financial data) cannot opt into encryption.

## Mitigations

- M25 adds a redaction-bypass pen-test that asserts no PHI patterns from
  ingestion input persist in the canonical store. CI fails on regression.
- The PII service contract (M09) requires a confidence threshold above which
  redaction is mandatory. The contract test fails the build if the service
  returns the original text unchanged when given known PHI patterns.
- The `pii_class` column is preserved on every node, so future encryption can
  be retrofitted to the `restricted` slice without a full re-classification.

## Revisit conditions

Any of these triggers a re-evaluation:

1. A workspace requests defense-in-depth encryption for `pii_class='restricted'`
   and the PII service alone is insufficient for its compliance posture.
2. A redaction-bypass incident reaches the canonical store (M25 attack catches
   it post-hoc, not pre-hoc).
3. A regulatory or customer audit requires per-record key destruction as the
   erasure mechanism (rather than hard-purge with audit redaction marker).
4. Multi-tenant cluster deployment requires per-workspace KMS key isolation
   for tenancy attestation purposes.

## Re-introduction sketch (when revisited)

Adding envelope encryption later requires:

1. New migration: add `encryption_algorithm`, `wrapped_dek`, `dek_kek_version`,
   `ciphertext`, `aad_hash` columns to `moa.node_index` and `moa.graph_changelog`;
   create `moa.workspace_kek` table.
2. New `moa-security::envelope` module: `EnvelopeCipher` + `KeyManager` trait
   + AWS KMS impl + SoftHSM dev impl.
3. Hook into M08 `create_node` / `supersede_node`: encrypt properties for
   `pii_class IN ('phi','restricted')` workspaces.
4. Hook into retrieval (`maybe_decrypt`): authorized readers see plaintext;
   unauthorized see ciphertext only.
5. Re-introduce the `crypto_shred` op into `moa.graph_changelog` op CHECK.
6. Re-introduce the `--mode crypto` branch in `moa privacy erase` (M24).
7. Re-introduce the KEK-substitution attack in M25.

Estimated effort: 2–3 weeks if all six steps are in scope.
```

### 5b Fill in `[today]` with the actual ADR creation date in YYYY-MM-DD format.

### 5c Add a top-level `docs/architecture/decisions/README.md` if it doesn't exist:

```markdown
# Architecture Decision Records

ADRs in this directory record significant architecture decisions. Format
follows the lightweight ADR convention (Status / Context / Decision /
Consequences). Each ADR is immutable once Accepted; supersession is recorded
explicitly.

| # | Title | Status |
|---|---|---|
| 0001 | Envelope encryption deferred to v1.1 | Accepted |
```

### 5d Confirm `moa-security` is unchanged

```sh
git status crates/moa-security/
```

Expected: clean. This prompt does not modify code in `moa-security`.

### 5e No migration

Confirm no migration file is added for M21:

```sh
ls migrations/ | grep -i M21
```

Expected: empty.

## 6 Deliverables

- `docs/architecture/decisions/0001-envelope-encryption-deferred.md`
- `docs/architecture/decisions/README.md` (if not already present)
- Zero code changes
- Zero schema changes

## 7 Acceptance criteria

1. ADR file exists at the path above and follows the format in §5a.
2. `git status` after this prompt shows only `docs/architecture/decisions/` additions.
3. `cargo build --workspace` clean (no-op since nothing changed).
4. `cargo test --workspace` green (no-op).

## 8 Tests

```sh
test -f docs/architecture/decisions/0001-envelope-encryption-deferred.md
git diff --name-only HEAD~1 HEAD | xargs -I{} dirname {} | sort -u
# expected output: only docs/architecture/decisions
cargo build --workspace
cargo test --workspace
```

## 9 Cleanup

Nothing to clean up — no prior envelope-encryption code exists.

## 10 What's next

**M22 — pgaudit configuration + S3 Object Lock shipping pipeline.** Unchanged from the original prompt. Audit trail and 6-year retention are independent of the encryption decision.
