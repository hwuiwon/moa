# 08 - Security

_Identity, authorization, credential isolation, sandbox policy, prompt
injection defenses, and audit._

## Default Posture

| Mode | Posture | Rationale |
|---|---|---|
| Local development API | Usable by default | Engineer controls the dev stack. |
| Cloud API and messaging | Secure by default | Agents run persistently and users may be offline. |

Usable local mode allows tool execution by default so local development keeps
moving. Secure cloud mode requires explicit tool enablement per tenant,
action-policy rules for deny or tenant-admin review, sandboxed code execution,
and host-side credential access only.

## Identity And Authorization

API keys are the zero-dependency default. Auth0 or generic OIDC can be enabled
for SSO, SCIM, Token Vault, and CIBA approvals. `auth.provider = "disabled"` is
for local development or isolated tests only.

OpenFGA is the default authorization engine. Handlers must call
`require_authz` or `require_authz_with_delegation` before protected reads. The
transactional outbox is the only supported way to synchronize product state and
OpenFGA tuples.

The public edge injects trusted `X-Moa-*` identity headers after stripping any
caller-provided values. The orchestrator trusts those headers, so production
deployments must keep the Restate handler port internal-only. See
[Auth Architecture](auth/README.md) and
[Architecture Policy](15-architecture-policy.md).

Agent-facing contacts are end users and use MOA-issued contact JWTs, not
trusted edge identity headers. Contact JWTs are bounded route/data credentials
with explicit scopes, structured permissions, tenant id, contact id, and
optional agent/session allowlists. Issuing a contact token is a tenant
admin/operator or authorized integration operation protected by normal caller
authz; presenting a contact JWT cannot call admin/operator APIs or become an
admin/operator user principal.

Identity verification can be initiated by workflows or skills, but the platform
contact service enforces challenge creation, OTP-style completion, token
upgrade, and session promotion. Low-assurance contact scopes can perform only
the operations explicitly granted before verification.
Contact-point lookup hashes use a separate 32-byte key from
`MOA_CONTACT_POINT_HASH_KEY_HEX`; raw emails and phone numbers must not be
stored in contact lookup columns.

## Credential Isolation

Credentials never enter the sandbox where generated code runs.

Supported patterns:

| Pattern | Use | Boundary |
|---|---|---|
| Bundled resource access | Git clone/push and tenant setup | Host prepares access without exposing raw token to the model. |
| MCP credential proxy | External tools and SaaS APIs | Host enriches MCP calls with real credentials. |
| Token Vault provider | User OAuth tokens | Provider retrieves user-approved tokens for trusted host-side calls. |
| Environment-backed provider keys | LLMs, embeddings, hand providers | Runtime constructs providers from env var names, not prompt-visible values. |

Local encrypted vault storage is no longer part of the active runtime. New
credential sources should implement `CredentialVault` or a typed provider vault
trait and stay behind the host-side credential boundary.

MCP credential proxy grants are private, process-local, and single-use: the
runtime creates an opaque grant, consumes it while enriching one MCP request,
and rejects reuse. Until MOA has a shared durable grant store, code must not
expose MCP credential grants across requests or depend on another Kubernetes
replica being able to resolve them.

## Sandbox Tiers

| Tier | Isolation | Default use |
|---|---|---|
| 0 | In-process trusted code | Built-in memory/search helpers |
| 1 | Container or managed workspace | Cloud code execution and normal hand tools |
| 2 | MicroVM | High-risk untrusted code |

Tier 1 containers should run non-root, with read-only root filesystems, narrow
workspace mounts, dropped capabilities, `no-new-privileges`, seccomp/AppArmor,
bounded process counts, and metadata-endpoint blocking. Local Docker currently
uses `--network none`, which is stricter than metadata-only blocking and means
networked local container tools need an explicit design change.

Every sandbox is ephemeral by default. Durable state belongs in the session
event log, memory, artifacts, or approved external systems, not in a leftover
container.

## Prompt Injection Defenses

MOA treats tool results, fetched content, and external files as untrusted.

Current defenses:

- The context pipeline preserves instruction precedence.
- Tool output is wrapped so lower-authority text cannot override system,
  tenant, contact/session, or skill instructions.
- A per-turn canary is injected into tool-enabled requests.
- Tool calls are blocked if they leak the active canary or any
  `moa_canary_*` marker.
- Suspicious output emits warning events.

If a model repeatedly emits malicious tool calls after receiving blocked-tool
feedback, the remaining control point is the turn retry/circuit-breaker policy.
Do not treat prompt filtering as a complete security boundary.

## Agent Guardrails

Configured agents may define optional input and output guardrail policy in the
DB-backed agent artifact JSON. At session creation, resolution pins that policy
into `session_agent_context` as part of the `AgentPolicySnapshot`.

V1 guardrails are LLM-judge text policies. Input guardrails run before
`UserMessage` is appended to the session event log. Output guardrails run after
the main model response text is buffered and before the visible `BrainResponse`
is appended. `GuardrailCheck` events record metadata such as direction, mode,
decision, model, and policy hash for audit; they do not store the raw guarded
text.

PII detection/redaction guardrails, response-schema guardrails, and tool
input/output guardrails are explicitly out of scope for V1. Guardrails are also
not a replacement for action or tool policy: tool visibility, authorization,
approval, and deny/review decisions remain enforced by the orchestrator
tool/action paths.

## Action Policy

Action-policy decisions are scoped to parsed tool intent, not raw command
strings. Shell matching splits command chains so a rule for one command does not
cover `&&`, `||`, `;`, or pipe-connected follow-up commands.

Default tool policy is auto-mode `allow`. Tenant-level policy rows and config
can return `allow`, `deny`, or `admin_review`.
`admin_review` persists a tenant action-review row plus event, returns a
pending-review tool result to the model, and does not block the root or
sub-agent workflow. Tenant admins clear or deny the stored action later through
the action-review service.

## Security Audit

MOA emits OCSF v1.3 security events for authentication, authorization,
API-key lifecycle, agent lifecycle, action reviews, and SCIM lifecycle changes.
Denied authorization decisions are always emitted when security audit is
configured. Allow decisions are high-volume and controlled by config.

Lineage audit and security-event audit are separate:

| Plane | Crate/service | Purpose |
|---|---|---|
| Lineage audit | `moa-lineage-audit` | Data lineage, Merkle roots, DSAR verification |
| Security audit | `moa-ocsf` and `services/audit-shipper` | OCSF event signing and tenant audit export |

The compliance lineage tier carries an explicit attestation caveat until
external cryptographic review covers canonicalization, hash chaining, signing,
Merkle proof construction, PII erase semantics, S3 Object Lock configuration,
timestamp discipline, and replay resistance.

## Build Rules

- Fail closed when identity or authz providers cannot make a decision.
- Keep secrets out of logs, fixtures, docs examples, and model-visible text.
- Use `tracing`, not stdout/stderr, for security-relevant events.
- Put security-sensitive provider dependencies behind feature flags when they
  are optional.
- Document any handler without resource-specific authz with the required
  one-line `// SAFETY:` justification.
