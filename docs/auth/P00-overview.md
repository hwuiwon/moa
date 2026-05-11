# P00 — Auth Pack Overview

## Purpose

This prompt pack adds identity, authorization, and security audit to MOA.
Execute prompts P1.0–P1.10 in sequence. Each prompt is self-contained and can
be worked independently once its prerequisites are satisfied.

Do not execute any code in this prompt. Read, understand, commit.

## What you are about to build

A layered auth system that defaults to **self-hosted OpenFGA** for authorization
and **local API keys** for identity, with **Auth0 as an opt-in upgrade** for
SSO/SCIM/Token Vault. AI agents are first-class principals alongside users.
OCSF v1.3 security events flow to per-tenant S3 buckets under Object Lock.

The system must run end-to-end with zero external dependencies (no Auth0, no
managed FGA) so that self-hosted single-tenant deployments work cleanly.

## Prompt sequence — Phase 1

| # | Title | What it ships |
|---|---|---|
| P1.0 | Naming ADR + architecture refresh | ADR-0002 locks crate names, OpenFGA-by-default decision, Auth0-optional posture; `docs/01-architecture-overview.md` updated |
| P1.1 | OpenFGA infra + authz schema v1 | OpenFGA in compose, `moa-authz-schema` crate, `moa-fga-bootstrap` bin |
| P1.2 | `moa-authz` crate + transactional outbox + cross-crate traits | FGA client wrapper, `authz_outbox` poller, `AuthProvider`/`TokenVaultProvider`/`AsyncAuthzProvider` traits in `moa-core` |
| P1.3 | Identity context + `moa-edge` HTTP edge + client surface | `X-Moa-*` header trust, new `moa-edge` crate, `OrchestratorClient::with_identity()` |
| P1.4 | `require_authz()` helpers + first handler enforcement | Authz checks wired into session create, turn dispatch, agent registration |
| P1.5 | API keys: first-class identity surface | `moa_<env>_<random>_<crc32>` format, argon2id, FGA-tuple scopes, GitHub secret-scanning partner |
| P1.6 | Local auth provider stack (zero-deps defaults) | `LocalAuthProvider` + `NullTokenVaultProvider` + `BuiltinAsyncAuthzProvider` |
| P1.7 | Auth0 + OIDC provider implementations (optional) | `moa-auth-providers-auth0` crate behind `auth0` feature flag |
| P1.8 | Token Vault + agent-as-principal + Auth0 CIBA | Agent identity lifecycle, third-party token exchange |
| P1.9 | SCIM v2 endpoints + deactivation cascade | SCIM in orchestrator, cascading cleanup |
| P1.10 | OCSF v1.3 audit + per-tenant buckets + Object Lock | New `moa-ocsf` crate, extends `services/audit-shipper` |

## Key decisions locked in for all prompts

These are the architectural invariants. They do not get re-litigated inside
individual prompts. If a prompt seems to contradict one, the prompt is wrong;
update it.

1. **OpenFGA self-hosted is the default authorization engine.** `make dev`
   brings it up alongside Postgres and Restate. Auth0 FGA (managed) is a
   future swap-in option (Phase 4); the API is identical.
2. **Local API keys are the default identity mechanism.** A self-hosted MOA
   deployment must work end-to-end with API keys alone. No SSO requirement.
3. **Auth0 is opt-in for identity.** `MOA__AUTH__PROVIDER=auth0` switches in
   the Auth0 provider; default is `local`. The `auth0` Cargo feature gates
   the dependency.
4. **AI agents are first-class principals**, not service accounts under
   users. Agents have FGA subject identity (`agent:<id>`) and can be granted
   permissions directly, including `can_act_as` for delegation.
5. **One FGA store per deployment**, not per tenant. Tenants are a relation
   in the schema. Matches Notion / Linear / Vercel deployment patterns.
6. **Provider abstraction traits live in `moa-core::traits`**, alongside
   `CredentialVault`. Downstream crates do not depend on `moa-auth-providers`
   directly.
7. **Transactional outbox** is the only correct way to keep Postgres state
   and FGA tuples consistent. No direct FGA writes from handlers.
8. **`X-Moa-*` identity headers from `moa-edge` are trusted by the
   orchestrator.** The orchestrator's Restate handler port (`9080`) must be
   network-isolated in production. `moa-edge` is the only public-facing
   surface.
9. **OCSF v1.3 is the security-event audit format.** Distinct from
   `moa-lineage-audit` (which is data-lineage Merkle audit) and
   `services/audit-shipper` (which is the pgaudit S3 shipper that the new
   pack extends, not replaces).
10. **Naming**: HTTP edge crate is `moa-edge` (not `moa-auth-gateway` — that
    name collides with the existing `moa-gateway` messaging-adapter crate).
    OCSF crate is `moa-ocsf` (not `moa-audit` — that name collides with the
    existing `moa-lineage-audit` and the `audit-shipper` service).

## Before P1.0

1. Read this overview end to end.
2. Read `docs/01-architecture-overview.md` (current state, will be updated by P1.0).
3. Read `docs/08-security.md` (current security posture; auth pack extends).
4. Read `docs/architecture/decisions/0001-envelope-encryption-deferred.md` to
   see the ADR style used in this repo.
5. Skim `docs/12-restate-architecture.md` for the orchestrator handler shape.
6. Read `AGENTS.md` for testing rules — `assert!(result.is_ok())` is banned;
   weak-assertion tests are deleted; live integration tests are
   `#[ignore]`-gated behind explicit env flags.

Then proceed to P1.0.

## Milestone gates

Do not start a later P-prompt until the earlier one's acceptance criteria are
met. The sequence is designed so each prompt's tests pass before the next
begins.

- **Gate after P1.2**: `moa-authz` outbox poller drains tuples to FGA
  end-to-end; `moa-core::traits` exposes the three auth-provider traits.
- **Gate after P1.5**: an API key can authenticate to `moa-edge`, identity
  headers reach the orchestrator, `require_authz()` allows/denies the
  session-create handler based on a real FGA check.
- **Gate after P1.6**: `make dev` produces a working stack where a user can
  hit `moa-edge`, present an API key, create a session, and the workflow
  pauses on a builtin approval that the user resolves via `moa-cli approvals
  approve`.
- **Gate after P1.10**: every authz check (allow + deny), every login, every
  approval, every API key event emits an OCSF v1.3 event to
  `security_events`; per-tenant signing verifies; events ship to S3 under
  Object Lock.

## What is NOT in Phase 1

Deferred to Phase 2+:

- RAG and knowledge_base authz integration (Phase 2)
- Connector framework (Drive, GitHub, Confluence, S3) with per-user credential
  isolation (Phase 2)
- Three-layer tool authz with RFC 8693 Token Exchange (Phase 2)
- Conditional FGA relationships (time-bound, ABAC attributes) (Phase 3)
- MCP server endpoints with OAuth 2.1 + RFC 8707 (Phase 3)
- BYOK/CMEK customer-managed keys (Phase 4)
- Managed Auth0 FGA migration option (Phase 4)
- SOC 2 / HIPAA / ISO 42001 evidence pipelines (Phase 4)
- GDPR hard-delete (Phase 4; current deactivation cascade preserves user row)
- MFA in local mode (not supported by design; OIDC mode delegates to the IdP)
- Admin UI (deferred indefinitely)

## Cross-references

- `AGENTS.md` — testing rules, workspace conventions, `MOA__SECTION__KEY`
  env var format
- `docs/architecture/decisions/README.md` — ADR index, where ADR-0002 will land
- `docs/architecture/type-placement.md` — guides where new types live
  (newtypes in `moa-core::types`, trait interfaces in `moa-core::traits`)
- `services/audit-shipper/` — existing pgaudit S3 shipper that P1.10 extends
- `crates/moa-lineage/audit/` — existing data-lineage Merkle audit (do not
  conflate with OCSF security-event audit added by P1.10)
