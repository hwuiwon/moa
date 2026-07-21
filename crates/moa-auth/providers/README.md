# moa-auth-providers

First-party authentication provider implementations for MOA: API keys, local
users and sessions, contact JWTs, the self-hosted token vault, and the
first-party OAuth 2.1 authorization server. Also builds the runtime provider
bundle (auth, token vault, contact tokens, approvals) from config.

## Structure

- `api_keys.rs` — API key format (`moa_<env>_<random>_<crc32>`), generation,
  hashing, validation, and storage.
- `passwords.rs` — Argon2 password hashing for first-party local users.
- `user_sessions.rs` — opaque local user-session token generation, storage,
  and validation.
- `local.rs` — `LocalAuthProvider` backed by API keys and user sessions.
- `oauth_as/` — first-party OAuth 2.1 authorization server core
  (authorization code + PKCE, opaque hashed tokens, introspection).
- `oauth_access_token.rs` — one-pass authentication for MOA-issued OAuth
  access tokens.
- `contact_tokens.rs` — issuance and verification of MOA contact JWTs.
- `postgres_vault.rs` — self-hosted Postgres-backed `TokenVaultProvider` for
  third-party tokens, with refresh support.
- `null_vault.rs` — null token vault for zero-dependency local deployments.
- `builtin_authz.rs` — builtin async approvals backed by Postgres and Restate
  awakeables.
- `disabled.rs` — provider for explicitly unauthenticated deployments.
- `bundle.rs` — independent builders wiring the configured authentication,
  token vault, contact-token, and approvals providers.

## Features

- `auth0` — pulls in `moa-auth-providers-auth0` and lets `bundle.rs` construct
  the Auth0/OIDC providers and Auth0 Token Vault. Off by default.
