# SCIM v2 provisioning

MOA exposes SCIM v2 endpoints from the orchestrator at `/scim/v2`.
In local development the SCIM listener defaults to port `10022`, so the
service-provider config is:

```sh
curl http://localhost:10022/scim/v2/ServiceProviderConfig
```

SCIM clients authenticate with a MOA API key whose OpenFGA scope grants
`scim_admin` on one tenant. Create and scope the key:

```sh
moa auth keys create --name="okta-scim" --env=prod
moa auth use-key <admin-key>
moa authz tuple-write \
  --user=api_key:<scim-key-id> \
  --relation=scim_admin \
  --object=tenant:<tenant-id>
```

Configure the IdP SCIM base URL as `https://<edge-or-orchestrator>/scim/v2`
and present the SCIM API key as `Authorization: Bearer <key>`.

## Supported operations

- `GET/POST /scim/v2/Users`
- `GET/PUT/PATCH/DELETE /scim/v2/Users/{id}`
- `GET/POST /scim/v2/Groups`
- `GET/PUT/PATCH/DELETE /scim/v2/Groups/{id}`
- `GET /scim/v2/ServiceProviderConfig`
- `GET /scim/v2/ResourceTypes`
- `GET /scim/v2/Schemas`

User filters support the common IdP forms:

```text
userName eq "alice@example.com"
emails.value eq "alice@example.com"
externalId eq "okta-12345"
```

Group filters support:

```text
displayName eq "engineers"
externalId eq "okta-group-12345"
```

## Deactivation cascade

`PATCH /Users/{id}` with `active=false` runs the deactivation cascade in one
Postgres transaction:

- marks the user inactive
- cancels active sessions
- revokes local API keys with reason `deactivation_cascade`
- enqueues OpenFGA tuple deletes for tenant, session, API-key, workspace, and
  agent-operator edges
- removes SCIM group memberships

Repeating the same `active=false` PATCH is a no-op; the cascade only runs for
users that are still active.

## Group mapping

SCIM groups enqueue FGA tuples when members are added or removed. The default
mapping is:

- any group name maps membership to `user:<U> member tenant:<T>`
- `tenant:<T>:<relation>` maps to `user:<U> <relation> tenant:<T>`
- `tenant:<T>:workspace:<W>:<relation>` maps to
  `user:<U> <relation> workspace:<W>`

## Okta SCIM Compliance Tester

1. Start the local stack and expose the SCIM listener to the tester.
2. Create a SCIM API key and grant `scim_admin` as shown above.
3. Open <https://oktadeveloper.github.io/scim-spec-test/>.
4. Set the base URL to `http://localhost:10022/scim/v2` or your tunnel URL.
5. Set the bearer token to the SCIM API key.
6. Run Create, List, Read, Patch, Delete, and Filter by `externalId`.

Live tests are ignored by default. Run them only with explicit credentials:

```sh
MOA_RUN_LIVE_SCIM_TESTS=1 \
MOA_TEST_SCIM_BASE_URL=http://localhost:10022/scim/v2 \
MOA_TEST_SCIM_TOKEN=<scim-api-key> \
cargo test -p moa-orchestrator scim_compliance -- --ignored
```
