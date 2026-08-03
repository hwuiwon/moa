# moa-auth-providers

First-party authentication provider implementations for MOA: API keys, local
users and sessions, contact JWTs, tenant connector credentials, and the
first-party OAuth 2.1 authorization server. Also builds the runtime provider
bundle (auth, contact tokens, approvals) from config.

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
- `postgres_credential_vault.rs` — durable staged tenant connector credential
  storage and audited resolution.
- `builtin_authz.rs` — builtin async approvals backed by Postgres and Restate
  awakeables.
- `auth0/` — optional Auth0/OIDC identity, CIBA approvals, JWKS caching, and
  user provisioning.
- `disabled.rs` — provider for explicitly unauthenticated deployments.
- `bundle.rs` — independent builders wiring the configured authentication,
  contact-token, and approvals providers.

## Features

- `auth0` — compiles the Auth0/OIDC identity and CIBA approval modules and lets
  `bundle.rs` construct them. Off by default.
