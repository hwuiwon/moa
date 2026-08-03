# moa-auth-providers-auth0

Optional Auth0 and generic OIDC authentication plug-in for MOA. This crate is
compiled only when the parent `moa-auth-providers` crate's `auth0` feature is
enabled (or when testing this crate directly); the provider bundle then wires
these implementations in place of the local providers.

## Structure

- `auth0_provider.rs` — `Auth0AuthProvider`: validates Auth0-issued RS256
  bearer JWTs against the tenant JWKS, with static provisioning support.
- `oidc_provider.rs` — `OidcAuthProvider`: generic OIDC RS256 JWT validation
  against a configured issuer.
- `jwks_cache.rs` — in-memory JWKS cache serving last-known-good keys and
  refreshing on unknown `kid`s.
- `ciba.rs` — `Auth0AsyncAuthzProvider`: Client-Initiated Backchannel
  Authentication for human approvals.
