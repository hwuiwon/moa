# ADR 0002 - Auth architecture

**Status:** Accepted
**Date:** 2026-05-11

## Context

MOA needs identity, authorization, and security audit as first-class platform
capabilities: who is calling, what they can do, and what they did. The Phase 1
surface covers API keys, SSO, SCIM, agent identity, fine-grained
authorization, async approvals, and OCSF security-event logs.

The deployment matrix is two-by-two: multi-tenant SaaS is the primary posture,
and self-hosted single-tenant is the secondary posture. Either posture can
optionally integrate Auth0 for SSO, SCIM, and Token Vault. Self-hosted
single-tenant deployments must work with zero external dependencies.

The decision matrix has two independent axes: authorization engine (OpenFGA vs
Auth0 FGA vs other) and identity provider (local API keys vs Auth0 vs generic
OIDC). These are resolved independently below.

## Decision

1. **Authorization engine: OpenFGA self-hosted (Postgres-backed).** MOA uses
   one FGA store per deployment, with tenants represented as relations in the
   authorization model. Auth0 FGA is a deferred swap-in option for Phase 4 with
   an identical API surface.
2. **Identity provider abstraction.** `AuthProvider`, `TokenVaultProvider`, and
   `AsyncAuthzProvider` traits live in `moa-core::traits`. Defaults are
   `LocalAuthProvider` for API keys, `NullTokenVaultProvider` returning
   `NotConfigured`, and `BuiltinAsyncAuthzProvider` for in-app approvals via
   Restate awakeables. Auth0 is an opt-in identity provider; its
   implementations live in a separate optional crate behind a Cargo feature.
3. **AI agents are first-class principals.** Agents have their own FGA subject
   identity in the form `agent:<uuid>`.
4. **`X-Moa-*` identity headers carry trusted edge identity.** `moa-edge`
   propagates identity to `moa-orchestrator` through these headers, and the
   orchestrator trusts them. The orchestrator's Restate handler port (`9080`)
   must not be exposed to the internet in production.
5. **Transactional outbox keeps Postgres and FGA consistent.** Handlers do not
   write FGA tuples directly; they write product state and outbox rows in the
   same Postgres transaction, and the outbox poller applies tuple changes to
   FGA.
6. **`make dev` brings up OpenFGA by default.** Contributors who explicitly
   want a leaner stack can set `MOA_SKIP_FGA=1`. Self-hosted single-tenant
   deployments ship embedded OpenFGA with its own Postgres database, usually a
   logical database on the same Postgres cluster.
7. **Naming and security-event format are fixed.** The public HTTP edge crate
   is `moa-edge`. The OCSF v1.3 security-event crate is `moa-ocsf`. These
   names avoid collision with the existing `moa-gateway` messaging-adapter
   crate and the existing `moa-lineage-audit` data-lineage Merkle audit crate.

## Consequences

**Positive:**
- Self-hosted deployments work without an Auth0 account or any external SaaS
  identity dependency.
- OpenFGA's API matches Auth0 FGA's, so migration to a managed engine is a
  config change, not a rewrite.
- Provider abstraction means Auth0 work is additive; no part of MOA is
  hard-coupled to a vendor.
- Agents as first-class principals avoid the "service account under a user"
  anti-pattern that complicates delegation auditing.
- Putting trait interfaces in `moa-core::traits` (consistent with the existing
  `CredentialVault` trait per `docs/architecture/type-placement.md`) keeps
  downstream crates from pulling in provider implementation dependencies they
  do not use.

**Negative:**
- Operating OpenFGA in self-hosted deployments adds an additional service to
  the local stack. Mitigated by `MOA_SKIP_FGA=1` for non-auth work and by the
  embedded-with-postgres bundling for production self-hosted.
- The orchestrator's header-trust model relies on network isolation of port
  `9080`. A misconfigured deployment that exposes `9080` to the internet would
  bypass authz entirely. This is documented in a runbook and enforced by
  deployment templates in Phase 1.
- Two audit pipelines now coexist: `moa-lineage-audit` for data lineage and
  `moa-ocsf` for security events. Naming discipline matters; both ship to S3
  via the same `audit-shipper` service, which is extended in P1.10.
- API keys, not OAuth tokens, are the default authentication. Keys cannot bind
  to a fine-grained user attribute set the way an OIDC ID token can. Mitigated
  by storing scopes as FGA tuples so keys narrow user permissions, and by
  encouraging Auth0 mode for multi-tenant production.

## Mitigations

- ADR-0002 supersedes any conflicting prose in
  `docs/01-architecture-overview.md` and `docs/08-security.md`; both documents
  are updated to match.
- The deployment runbook written in the Phase 1 auth prompts will require any
  production deployment template to bind the orchestrator handler port (`9080`)
  to an internal-only interface or service-mesh network.

## Revisit conditions

1. A customer requests a deployment posture where SSO is mandatory and API keys
   are forbidden. Auth0/OIDC providers already cover this; revisit only if the
   customer needs a third identity provider.
2. OpenFGA performance becomes a bottleneck at scale (Notion-class load).
   Either shard OpenFGA per region or migrate to Auth0 FGA managed.
3. A regulator requires identity propagation between services to be
   cryptographically signed, not merely header-trust over an isolated network.
   Add JWT signing between edge and orchestrator.
4. A third audit pipeline shape is requested, such as SIEM-native CEF/LEEF
   instead of OCSF. Re-examine the `moa-ocsf` naming and shipper architecture.

## Reversal sketch

Reversing this ADR would invalidate the assumptions behind P1.1 through P1.10.
The dependent prompts would need to be rewritten around a different default
authorization engine, identity-provider boundary, edge-to-orchestrator trust
model, crate naming scheme, and security-event audit pipeline.
