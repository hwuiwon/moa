# moa-kms

Persistent, self-hosted key management for MOA envelope encryption. Provides a
Postgres-backed implementation of `moa_crypto::KeyManagementProvider` so
encrypted data and crypto-shred survive process restarts — the production
counterpart to `moa_crypto`'s in-memory `LocalKmsProvider`.

## Structure

- `provider.rs` — `PostgresKmsProvider`: per-subject KEKs stored in `moa.kek`
  wrapped under the deployment root key (AES-256-GCM, AAD binds
  tenant|subject|kek id); per-record DEKs are wrapped under their subject's
  KEK.
- `root_key.rs` — `RootKeyRing`: deployment root-key generations loaded from a
  mounted directory; root keys never land in the database.
- `error.rs` — `KmsError`.

## Notes

- Crypto-shred: `destroy_subject_key` tombstones the subject's `moa.kek` row
  and zeroes the wrapped KEK, making every DEK sealed under it permanently
  un-unwrappable. This is the erasure primitive the privacy erase path calls.
- Root-key rotation: shared Postgres state selects the active generation for
  new KEKs; bounded restart-safe jobs rewrap historical KEKs while pods keep
  all referenced generations mounted.
- Consumers depend on `Arc<dyn KeyManagementProvider>` from `moa_crypto`; the
  concrete provider is injected at the composition root.
