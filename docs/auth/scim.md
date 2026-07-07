# SCIM v2 provisioning

MOA exposes SCIM v2 endpoints from the orchestrator at `/scim/v2`.
In local development the SCIM listener defaults to port `10022`, so the
service-provider config is:

```sh
curl http://localhost:10022/scim/v2/ServiceProviderConfig
```

SCIM clients authenticate with a MOA API key whose OpenFGA scope grants
`tenant#admin` on one tenant. Create and scope the key:

```sh
curl -X POST http://localhost:10010/ApiKeys/create \
  -H "Content-Type: application/json" \
  -H "x-moa-identity-type: operator" \
  -H "x-moa-identity-id: <admin-user-id>" \
  -H "x-moa-tenant-id: <tenant-id>" \
  --data '{"name":"okta-scim","env":"prod","description":null,"for_agent_id":null}'
curl -X POST http://localhost:10000/v1/authz/api-key-tenant-roles \
  -H "Authorization: Bearer <admin-key>" \
  -H "Content-Type: application/json" \
  --data '{"operation":"grant_api_key_tenant_role","api_key_id":"<scim-key-id>","tenant_id":"<tenant-id>","relation":"admin"}'
```

Configure the IdP SCIM base URL as `https://<edge-or-orchestrator>/scim/v2`
and present the SCIM API key as `Authorization: Bearer <key>`.

The public authz administration endpoint is intentionally typed. It can grant
or revoke only `api_key:<id> admin|operator tenant:<tenant-id>` after verifying
the API key belongs to that tenant; it cannot write raw OpenFGA tuple strings.
Those API-key tenant-role grants are manual grants. Revoking or rotating an API
key deletes the old key's `admin` and `operator` tenant-role tuples; rotation
does not copy them to the replacement key.

SCIM users are MOA operator/admin identities: tenant admins and service users
that need authenticated access to MOA control-plane APIs.
Agent-facing contacts are not SCIM users. Contact JWTs use separate bounded
`contact:<id>` OpenFGA subjects for agent/session interaction and cannot become
tenant or workspace control-plane identities. Contacts are created, verified,
linked, exported, and erased through the contact and privacy APIs; deleting or
deactivating a SCIM user does not silently delete unrelated contacts.

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
- enqueues OpenFGA tuple deletes for tenant, session, API-key, and
  agent-operator edges, including API-key `admin` and `operator` tenant-role
  grants
- removes SCIM group memberships
- leaves agent-facing contacts unchanged unless a separate privacy erasure
  request explicitly targets those contacts

Repeating the same `active=false` PATCH is a no-op; the cascade only runs for
users that are still active.

## Group mapping

SCIM groups enqueue FGA tuples when members are added or removed only for
schema-backed tenant role groups. The mapping is:

- ordinary group names persist membership as SCIM product data without OpenFGA tuples
- `tenant:<T>:admin` maps to `operator:<U> admin tenant:<T>`
- `tenant:<T>:operator` maps to `operator:<U> operator tenant:<T>`

Other `tenant:<T>:<relation>` group names are rejected.

## Okta SCIM Compliance Tester

1. Start the local stack and expose the SCIM listener to the tester.
2. Create a SCIM API key and grant tenant admin as shown above.
3. Open <https://oktadeveloper.github.io/scim-spec-test/>.
4. Set the base URL to `http://localhost:10022/scim/v2` or your tunnel URL.
5. Set the bearer token to the SCIM API key.
6. Run Create, List, Read, Patch, Delete, and Filter by `externalId`.

Live tests are ignored by default. Run them only with explicit credentials:

```sh
MOA_RUN_LIVE_SCIM_TESTS=1 \
MOA_SCIM_BASE_URL=http://localhost:10022/scim/v2 \
MOA_TEST_SCIM_TOKEN=<scim-api-key> \
cargo test -p moa-orchestrator --test scim_compliance_live -- --ignored
```
