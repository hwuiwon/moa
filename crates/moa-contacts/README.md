# moa-contacts

Contact identity domain for MOA: contact records, channel accounts,
token grants, and verification state, with Postgres persistence and an OTP
verification service that delivers codes through `moa-messaging` connectors.

## Structure

- `domain` — pure contact identity helpers (token scopes, verification
  challenges).
- `error` — error types for contact identity operations.
- `repository` — contact repository operations grouped by persisted
  aggregate (contacts, channel accounts, token grants, verification).
- `verification_service` — application service for persisted contact
  verification and OTP delivery.
