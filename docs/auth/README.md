# Auth Architecture

MOA's auth layer answers three questions before a handler touches protected
data:

1. Who is calling?
2. What relation does that caller have to the target object?
3. What security event should be written for the decision?

ADR-0002 records the durable decision. This file is the compact build map for
the current implementation.

## Runtime Boundary

`moa-edge` is the public HTTP boundary. It validates the presented credential,
strips any inbound `X-Moa-*` headers, injects trusted identity headers, and
forwards the request to the orchestrator. The orchestrator trusts those headers,
so the Restate handler port must stay internal-only in production.

Trusted headers:

| Header | Meaning |
|---|---|
| `x-moa-identity-type` | `operator`, `contact`, `agent`, or `service` |
| `x-moa-identity-id` | principal UUID |
| `x-moa-tenant-id` | tenant UUID |
| `x-moa-api-key-id` | API key UUID when a local key authenticated the call |
| `x-moa-acting-on-behalf-of` | delegating user UUID for agent calls |

## Defaults

| Layer | Default | Optional upgrade |
|---|---|---|
| Identity | Local API keys | Auth0 or generic OIDC |
| Authorization | Self-hosted OpenFGA | Auth0 FGA-compatible engine later |
| Async approvals | Builtin approvals | Auth0 CIBA |
| Security audit | Signed, immutable OCSF rows in Postgres | Deployment-owned external archival |

Self-hosted single-tenant deployments must work without Auth0 or any managed
identity provider. `auth.provider = "disabled"` is only for isolated local
tests and must not be exposed.

MOA also ships a first-party OAuth 2.1 authorization server (authorization
code + PKCE) in `crates/moa-auth/providers/src/oauth_as/`, exposed by
`moa-edge` at `/oauth/authorize`, `/oauth/token`, `/oauth/introspect`, and
`/oauth/revoke`.

## Authorization Rules

Handlers that touch caller-owned data must call
`moa_authz::require_authz` or `require_authz_with_delegation` before reading
the protected resource.

The canonical subject is derived by `fga_subject`:

- API-key identity wins: `api_key:<id>`.
- Operator identities authorize through the `operator:<id>` OpenFGA subject
  namespace.
- Agents are `agent:<id>`.
- Services are `service:<id>`.

API keys narrow permissions because checks run as the key subject, not the
owner subject. Agent delegation is two checks:

1. The operator has `can_act_as` on the agent.
2. The agent subject has the requested relation on the target resource.

Delegation does not borrow the user's resource permissions.

Workspace admins are represented as `workspace#admin` in OpenFGA. Tenants are
linked to the single deployment workspace
`workspace:00000000-0000-0000-0000-000000000001` with the `workspace` relation
on `tenant:<tenant-id>`, and the model inherits `workspace#admin` into
`tenant#admin`, then into `tenant#operator`. Handlers must keep checking the
target tenant/resource relation instead of adding local workspace-admin bypass
logic.

Contacts use MOA-issued contact JWTs for bounded agent/session routes. A
contact token cannot become a workspace admin, tenant admin, or tenant
operator identity and cannot call platform-internal control-plane APIs. Contact
token issuance requires explicit non-empty `requested_scopes` and `agent_ids`;
empty values are rejected so public contact credentials do not inherit implicit
low-assurance scopes or wildcard agent access.

## Tuple Consistency

Product handlers do not write OpenFGA tuples directly. They write product state
and authz outbox rows in the same Postgres transaction. The outbox poller then
applies writes and deletes to OpenFGA.

Create and delete paths must stay symmetric. If creating a resource enqueues
tuples, deleting or deactivating that resource must enqueue inverse tuple
deletes in the same transaction so stale FGA grants do not survive.

## Crate Map

| Path | Crate | Responsibility |
|---|---|---|
| `crates/moa-auth/authz-schema` | `moa-authz-schema` | OpenFGA object, relation, tuple, and model constants |
| `crates/moa-auth/authz` | `moa-authz` | FGA client, `require_authz`, transactional outbox, poller |
| `crates/moa-auth/providers` | `moa-auth-providers` | Local API keys, disabled auth, builtin approvals, tenant connector credentials, first-party OAuth 2.1 authorization server (`oauth_as`), and feature-gated Auth0/OIDC, CIBA, and JWKS modules |
| `crates/moa-auth/fga-bootstrap` | `moa-fga-bootstrap` | Idempotent OpenFGA store/model bootstrap |
| `crates/moa-edge` | `moa-edge` | Public authn/proxy edge and identity header injection |
| `crates/moa-ocsf` | `moa-ocsf` | OCSF security events, signing, verification, persistence |

Shared traits live in `moa-core::traits`; downstream crates depend on the
contracts without pulling in provider implementations.

Agent-facing contacts are separate from authenticated MOA operators. SSO, OIDC,
API keys, SCIM, and OpenFGA protect tenant-admin-or-higher control-plane access;
contacts use MOA-issued bounded contact JWTs for agent sessions and are managed
through the contact/privacy APIs unless a future product flow promotes a contact
into an operator identity.

## Operational Docs

| Topic | Document |
|---|---|
| Edge header trust and network isolation | [operations/edge-network-isolation.md](../operations/edge-network-isolation.md) |
| Builtin approval flow | [operations/builtin-approvals.md](../operations/builtin-approvals.md) |
| Agent identity and deactivation | [operations/agent-lifecycle.md](../operations/agent-lifecycle.md) |
| Workspace admin bootstrap | [operations/workspace-admin-bootstrap.md](../operations/workspace-admin-bootstrap.md) |
| Auth0/OIDC setup | [operations/auth0-setup.md](../operations/auth0-setup.md) |
| SCIM provisioning | [scim.md](scim.md) |
| OCSF security audit | [operations/ocsf-audit.md](../operations/ocsf-audit.md) |

## Build Checklist

- Add auth checks before protected reads.
- Use `require_authz_with_delegation` for handlers callable by agents.
- Enqueue FGA tuple writes/deletes through the transactional outbox.
- Emit or preserve OCSF events for authn, authz, API-key, agent, approval, and
  SCIM lifecycle decisions.
- Keep the orchestrator handler port internal-only whenever `moa-edge` is the
  public boundary.
