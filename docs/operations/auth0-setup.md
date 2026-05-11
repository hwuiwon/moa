# Auth0 setup

Auth0 support is optional. Local API keys remain available even when
`auth.provider = "auth0"` because MOA routes `moa_...` bearer values to the
local API-key provider and JWT bearer values to Auth0.

## Auth0 tenant setup

1. Create an Auth0 API named `MOA API`.
2. Set the API identifier to the audience MOA will validate, for example
   `https://api.moa.example.com`.
3. Use RS256 signing.
4. Create a Native Application for CLI device-code login.
5. Enable Device Authorization Grant for that application.
6. Add a post-login Auth0 Action that copies MOA metadata into the access
   token:

```js
exports.onExecutePostLogin = async (event, api) => {
  const tid = event.user.app_metadata.tenant_id;
  if (tid) api.accessToken.setCustomClaim('https://moa/tenant_id', tid);
  const t = event.user.app_metadata.identity_type || 'user';
  api.accessToken.setCustomClaim('https://moa/identity_type', t);
};
```

Set `app_metadata.tenant_id` on each Auth0 user to the MOA tenant UUID.

## MOA configuration

```toml
[auth]
provider = "auth0"

[auth.auth0]
domain = "your-tenant.auth0.com"
audience = "https://api.moa.example.com"
client_id_env = "MOA_AUTH0_CLIENT_ID"
client_secret_env = "MOA_AUTH0_CLIENT_SECRET"
```

Build binaries that need Auth0 with the feature enabled:

```sh
cargo build --release -p moa-orchestrator --features auth0
cargo build --release -p moa-edge --features auth0
```

Log in from the CLI:

```sh
moa auth login --issuer=https://your-tenant.auth0.com/ --client-id="$MOA_AUTH0_CLIENT_ID"
```

The command stores an access token, refresh token, token endpoint, client id,
issuer, and access-token expiry in `~/.moa/credentials.json` with mode `0600`
on Unix. Subsequent CLI requests refresh the access token when it is within
60 seconds of expiry.

## Common failures

- Missing `https://moa/tenant_id` claim: the Auth0 Action is not attached or
  the user lacks `app_metadata.tenant_id`.
- `credential rejected by provider`: check audience, issuer, and that the API
  uses RS256.
- JWKS key miss after key rotation: retry once after the provider refreshes
  the JWKS cache; the cache refreshes on `kid` miss and after one hour.
