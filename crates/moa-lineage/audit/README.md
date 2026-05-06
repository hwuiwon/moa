# moa-lineage-audit

Compliance-tier lineage audit support for MOA. This crate is opt-in per
workspace and layers tamper-evidence, signed Merkle windows, PII
pseudonymization, DSAR export helpers, and verification utilities over the
engineering lineage tier.

## Attestation Gate

**DO NOT REPRESENT THIS AS COMPLIANCE EVIDENCE TO REGULATORS OR CUSTOMERS UNTIL
EXTERNAL CRYPTOGRAPHIC REVIEW IS COMPLETE.**

The `ct-merkle` crate is explicitly documented by its authors as not audited.
MOA's use of BLAKE3 canonical payload hashes, Ed25519 signatures,
Certificate-Transparency-style proof shapes, PII crypto-shredding semantics, and
S3 Object Lock handling must be reviewed by an external cryptographer or appsec
firm before any compliance certification or customer audit claim is made.

Engineering and internal-forensics use is allowed before that review.
Compliance representations are blocked until the review report is linked from
the release-engineering runbook.
