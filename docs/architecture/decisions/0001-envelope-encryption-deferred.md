# ADR 0001 - Envelope encryption deferred to v1.1

**Status:** Accepted
**Date:** 2026-05-05
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

Estimated effort: 2-3 weeks if all six steps are in scope.
