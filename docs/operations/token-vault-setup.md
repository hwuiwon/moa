# Auth0 Token Vault setup

`MOA_TOKEN_VAULT_PROVIDER=auth0` lets MOA retrieve third-party access tokens
from Auth0 just in time. MOA stores only linked-connection metadata in
Postgres; it does not persist provider access tokens.

## Auth0 configuration

1. Enable Auth0 Token Vault for the tenant and the social or enterprise
   connection that should provide tokens, such as `google-oauth2` or `github`.
2. Create or reuse the MOA machine-to-machine application with access to the
   Auth0 Management API.
3. Use the same Auth0 app settings already configured for auth:

```sh
MOA_AUTH_PROVIDER=auth0
MOA_AUTH_AUTH0_DOMAIN=your-tenant.auth0.com
MOA_AUTH_AUTH0_AUDIENCE=https://api.moa.example.com
MOA_AUTH_AUTH0_CLIENT_ID=...
MOA_AUTH_AUTH0_CLIENT_SECRET=...
MOA_TOKEN_VAULT_PROVIDER=auth0
```

4. Set `MOA_AUTH_AUTH0_CLIENT_ID` and `MOA_AUTH_AUTH0_CLIENT_SECRET` in the orchestrator
   environment.

## Linked-connection webhook

Auth0 Actions should notify MOA when a user links a connection:

```json
{
  "auth0_sub": "auth0|abc123",
  "connection_name": "github",
  "scopes_granted": ["repo", "read:user"],
  "external_sub": "github-user-id"
}
```

Send the payload to:

```text
POST /v1/webhooks/auth0/connection-linked
Auth0-Signature: sha256=<hmac_sha256_hex>
```

The signature is HMAC-SHA256 over the raw request body using
`MOA_AUTH_AUTH0_WEBHOOK_SECRET`. The edge verifies it in constant time and
upserts `linked_connections`.

## Runtime behavior

Handlers call `providers.token_vault.get_token(user_id, connection_name)`.
The provider confirms the user has linked the connection, obtains a short-lived
Auth0 M2M token, exchanges it for the third-party access token, and returns the
token to the caller without storing it.
