# Token Vault Setup

MOA supports an Auth0-managed vault and a self-hosted Postgres vault. The
Postgres provider always encrypts access and refresh tokens through the runtime
KMS before persistence; plaintext token storage is not a supported mode. Every
orchestrator replica therefore needs the same Postgres KMS configuration and
root-key Secret described in [KMS Root-Key Rotation](kms-root-key-rotation.md).

## Self-hosted Postgres vault

Select the provider and supply OAuth refresh clients as one typed JSON value:

```sh
MOA_TOKEN_VAULT_PROVIDER=postgres
MOA_TOKEN_VAULT_REFRESH_JSON='{
  "github": {
    "token_endpoint": "https://github.com/login/oauth/access_token",
    "client_id": "...",
    "client_secret": "..."
  }
}'
MOA_KMS_PROVIDER=postgres
MOA_KMS_ROOT_KEY_DIR=/var/run/secrets/moa-kms/root-keys
MOA_KMS_REQUIRED_GENERATION=primary
```

`client_secret` is direct secret material, not the name of another environment
variable. Put the entire `MOA_TOKEN_VAULT_REFRESH_JSON` value in a Kubernetes
Secret and inject it only into the orchestrator. Token endpoint URLs and all
required nonempty fields validate at startup. Public OAuth clients may omit
`client_secret`.

The vault stores ciphertext and KMS envelope metadata in Postgres. It uses the
same shared KMS provider as memory encryption, so token reads continue across
pod restarts and non-sticky replica routing. A missing or incompatible keyring
fails startup/readiness instead of falling back to plaintext or process-local
keys.

## Auth0-managed vault

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

The orchestrator constructs and owns one configured token-vault provider at
startup using the same shared KMS handle as graph memory. The provider confirms
that a user linked the requested connection and returns a current third-party
access token without exposing a global provider registry or plaintext storage
path.
