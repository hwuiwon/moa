# moa-crypto

Envelope encryption, BYOK, and crypto-shred foundation for MOA's
defense-in-depth encryption of restricted (`pii_class`-tagged) data. Each
record is sealed with a fresh AES-256-GCM data-encryption key (DEK) wrapped by
a per-data-subject key-encryption key (KEK) held in a pluggable KMS; keys nest
tenant → data subject → record, so destroying one subject's KEK
cryptographically erases exactly that subject's records.

## Modules

- `envelope` — envelope encryption entry points: `encrypt` / `decrypt`
  (+ batch variants), `crypto_shred`, `crypto_shred_subject`
- `kms` — the `KeyManagementProvider` abstraction (AWS KMS, GCP KMS, Vault —
  bring-your-own-key)
- `local` — in-memory `LocalKmsProvider` for development and tests
- `key_wrap` — reviewed symmetric key-wrap framing shared by every KMS
  provider
- `aead` — AES-256-GCM seal/open primitives and CSPRNG helpers (private)
- `types` — core value types (`EncryptionContext`, `WrappedDek`, `KeyHandle`,
  `Ciphertext`, ...)
- `error` — the single crate `Error` type

## Rules

- `EncryptionContext` (tenant id, subject id, record id, `pii_class`) is bound
  as AEAD additional authenticated data at both the DEK-wrap and record-seal
  layers; `decrypt` will not open a payload sealed under a different context.
- A plaintext DEK seals exactly one record and is zeroized after use; only the
  `WrappedDek` is persisted next to the ciphertext.
- Unwrapping a destroyed key returns `Error::CryptoShredded`.
