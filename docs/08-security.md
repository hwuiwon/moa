# 08 - Security

_Identity, authorization, credential isolation, sandbox policy, prompt
injection defenses, and audit._

## Default Posture

The posture is selected explicitly by `security_profile`
(`MOA_SECURITY_PROFILE`), never inferred from the presence of other
configuration. It defaults to `local`.

| Profile | Posture | Rationale |
|---|---|---|
| `local` | Usable by default | Engineer controls the dev stack. |
| `cloud` | Secure by default | Agents run persistently and users may be offline. |

Usable local mode allows tool execution by default so local development keeps
moving, and it is the only profile under which host-local hands may run. Secure
cloud mode requires explicit tool enablement per tenant, action-policy rules for
deny or tenant-admin review, sandboxed code execution, and host-side credential
access only.

The `cloud` profile fails closed at construction and rejects all four of these
before serving a single request:

| Requirement | Rejected when |
|---|---|
| Deny-by-default permissions | `permissions.default_effect` is not `deny` |
| A persisted rule owner | no action-policy rule store is supplied |
| A non-local sandbox backend | the resolved hand route is local or absent |
| Present backend credentials | the selected sandbox has no credential |

Checked-in Kubernetes renders exactly one posture per overlay: base and local
render `local` with a permissive default and the local hand provider;
production renders `cloud` with a deny default and the E2B backend, and is the
only overlay that references the cloud sandbox credential Secret.

## Identity And Authorization

API keys are the zero-dependency default. Auth0 or generic OIDC can be enabled
for SSO, SCIM, Token Vault, and CIBA approvals. `auth.provider = "disabled"` is
for local development or isolated tests only.

OpenFGA is the default authorization engine. Handlers must call
`require_authz` or `require_authz_with_delegation` before protected reads. The
transactional outbox is the only supported way to synchronize product state and
OpenFGA tuples.

Workspace admins are first-class OpenFGA super-admin principals:
`workspace#admin` inherits into `tenant#admin` for every tenant attached to the
workspace, and `tenant#admin` inherits into `tenant#operator`. Handlers still
authorize against the target tenant/resource; they do not implement local
workspace-admin bypasses. Tenant remains the runtime, RLS, and data isolation
boundary.

The tenant-operations MCP protected resource is `/mcp`. Its tenant scope is
always taken from the verified `Identity`; tools have no `tenant_id` override,
and contact or agent identities are rejected before JSON-RPC dispatch. Exact
Host and Origin allowlists protect the Streamable HTTP endpoint. Caller access
tokens terminate at `moa-edge`: the proxy strips them and forwards only trusted
`X-Moa-*` identity headers to the internal Restate ingress.

A future dashboard OAuth flow may authorize MCP clients after customer login,
but it must still map the audience-bound token through the existing
`AuthProvider` and OpenFGA checks. The dashboard authorization server must use
Authorization Code with PKCE and RFC 8707 `resource`, publish RFC 9728
protected-resource metadata and RFC 8414/OIDC authorization-server metadata,
and issue short-lived tokens whose resource is the canonical `/mcp` endpoint.

The public edge injects trusted `X-Moa-*` identity headers after stripping any
caller-provided values. The orchestrator trusts those headers, so production
deployments must keep the Restate handler port internal-only. See
[Auth Architecture](auth/README.md) and
[Architecture Policy](15-architecture-policy.md).

Agent-facing contacts are end users and use MOA-issued contact JWTs, not
trusted edge identity headers. Contact JWTs are bounded route/data credentials
with explicit scopes, structured permissions, tenant id, contact id, and
agent/session allowlists. Issuing a contact token is a tenant admin/operator or
authorized integration operation protected by normal caller authz, and the
issuance request must include non-empty `requested_scopes` and `agent_ids`
rather than relying on implicit defaults or wildcard agent access. Presenting a
contact JWT cannot call admin/operator APIs or become an workspace,
tenant-admin, or tenant-operator principal.

Identity verification can be initiated by skills or execution runs, but the platform
contact service enforces challenge creation, OTP-style completion, token
upgrade, and session promotion. Low-assurance contact scopes can perform only
the operations explicitly granted before verification.
Contact-point lookup hashes use a separate 32-byte key from
`MOA_AUTH_CONTACT_TOKENS_CONTACT_POINT_HASH_KEY_HEX`; raw emails and phone numbers must not be
stored in contact lookup columns.

Tenant knowledge content is admitted under the source system's own permissions,
not merely the tenant boundary. A connection is `TenantPublic` or
`ProviderManaged`, and the connector's declared `ProviderAclCapability` — never a
caller or operator choice — decides which; a re-link cannot widen an existing
`ProviderManaged` connection. `ProviderManaged` content is admitted only through
an immutable snapshot that is complete and revision-matched, with a matching
allow entry and no matching deny entry. Missing, incomplete, stale, or
revision-drifted ACLs deny, an empty caller principal set denies, and tenant role
or operator status grants no content bypass.

Principals are persisted only as keyed opaque fingerprints (HMAC-SHA256 under a
KMS-wrapped, versioned per-tenant ACL key, with the key version encoded into the
value), so no email address, phone number, or provider label reaches a row, log,
trace, or cache key. The caller's principal set is resolved once per turn from
authenticated identity plus verified bindings, never from request JSON and never
re-fetched inside a retrieval leg.

One shared SQL predicate enforces this on every path that can surface source
content — lexical, pgvector, every recursive graph hop, hydration, and each
context-window neighbour — with a single batched check for external vector
candidates before fusion. Tenant RLS remains underneath as defense in depth. The
tenant ACL epoch and the aggregate principal-set fingerprint are part of
retrieval cache identity, so a revocation cannot be served from a warm cache. See
[Tenant Knowledge Base](21-tenant-knowledge-base.md) for the full contract.

## Credential Isolation

Credentials never enter the sandbox where generated code runs.

Supported patterns:

| Pattern | Use | Boundary |
|---|---|---|
| Bundled resource access | Git clone/push and tenant setup | Host prepares access without exposing raw token to the model. |
| MCP credential proxy | External tools and SaaS APIs | Host enriches MCP calls with real credentials. |
| Token Vault provider | User OAuth tokens | Provider retrieves user-approved tokens for trusted host-side calls. |
| Environment-backed provider keys | LLMs, embeddings, hand providers | Runtime loads directly injected secrets into typed host-side config, not prompt-visible values. |

Local encrypted vault storage is no longer part of the active runtime. New
credential sources should implement `CredentialVault` or a typed provider vault
trait and stay behind the host-side credential boundary.

MOA-managed tenant connector material lives in one durable owner. A credential's
persistence identity is `(tenant, owning connection, kind, version)`; resolution
separately carries the acting principal — a caller that passed
`(Tenant, tenant_id, Operator)`, or a closed service actor bound to exactly one
operation — plus a replay-stable operation id and canonical request hash. The
audit row commits before any plaintext is returned, so a resolution cannot be
observed without a durable record, and reusing an operation id with different
inputs is a typed conflict rather than a silent overwrite. Both tables force
row-level security with strict tenant isolation and no control-plane branch: a
missing or wrong `moa.tenant_id` denies rather than widening. The audit is
append-only twice over (no UPDATE policy, no UPDATE grant), and deletion is
reachable only from a transaction that explicitly sets `moa.credential_purge`.

Plaintext leaves the owner only as a non-serializable, redacted carrier that
cannot be cloned into a model payload, serialized into Restate state or an
event, or persisted on a knowledge row. Deployment-owned operator transport
secrets are a separate typed source, so a tenant connection can never fall back
to an operator credential.

Outbound MCP calls carry one owner's credential, decided by the server's
required `credential_scope`. A deployment-owned server uses one operator secret
read from deployment environment; a tenant-owned server is served only through a
forced-RLS connection binding that names the tenant's own connection, the exact
stored credential version, and a closed operation allowlist. Delegated
`(Tenant, tenant_id, Operator)` authorization runs before the first binding
read, the stored version's identity is checked against the binding before
anything is decrypted, and plaintext is shaped into headers immediately before
`tools/call` and dropped. No tenant-owned failure falls back to the deployment
branch, and environment credentials are unreachable from a tenant-owned
connector.

The proxy mints no grant token: it resolves and consumes the credential inside
one host function, so an opaque grant would add cache, expiry, and reuse risk
without crossing an isolation boundary. Reintroduce a single-use grant only when
a real remote proxy boundary sits between the resolver and the MCP transport.

## Encryption And Key Management

Persisted restricted/PHI memory and self-hosted token-vault values use envelope
encryption. Postgres owns shared generation metadata and per-subject wrapped
KEKs; Kubernetes supplies generation-named root-key files through the
externally provisioned `moa-kms-root-keys` Secret. Root keys are never stored in
Postgres, configuration values, logs, or model-visible context.

All orchestrator replicas and opt-in encryption maintenance Jobs mount the same
keyring read-only at `/var/run/secrets/moa-kms/root-keys` and select the
Postgres KMS provider. The edge has no KMS responsibility and must not receive
that Secret. Startup/readiness fails when the configured required generation is
not database-active or a generation referenced by a live KEK is absent from the
mounted ring; there is no production fallback to per-process keys.

Root-key rotation is additive and generation-aware: mount the new key alongside
all referenced historical keys, activate and rewrap in bounded resumable work,
retire an unreferenced old generation, then remove its file. See
[KMS Root-Key Rotation](operations/kms-root-key-rotation.md) for the required
rolling-deployment order and maintenance Jobs.

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

The root coordinator is sandbox-free, and each worker owns one isolated
sandbox keyed by `worker_id` (lease key `(session_id, worker_id,
provider)`). Parallel workers therefore do not share a compute environment, so
one delegated task's untrusted code or files cannot reach a sibling's sandbox. A
worker's sandbox is released when it self-cleans, and any remainder is released
at session teardown.

Conversational workers are available only as interactive delegation in `act`;
they are not the bulk DAG primitive. Sandbox-using `ExecutionTask` instances
receive equivalent isolation under a stable run/task identity and generation
fence. Execution maps submit every budget-admitted logical task without an
application fan-out cap; Restate concurrency and provider pacing control
physical pressure.

## Prompt Injection Defenses

MOA treats tool results, fetched content, and external files as untrusted.

Current defenses:

- The context pipeline preserves instruction precedence.
- Tool output is wrapped so lower-authority text cannot override system,
  tenant, contact/session, or skill instructions.
- A per-turn canary is injected into tool-enabled requests.
- Tool calls are blocked if they leak the active canary or any
  `moa_canary_*` marker.
- Every tool output is classified by the typed security circuit below before it
  reaches any durable or model-facing surface.

### The typed prompt-injection circuit

Wrapping untrusted output is containment, not a control: it still delivers the
attacker's text to the model. The circuit is the control.

**One classifier, at the raw-output source.** `moa_security::classify_tool_output`
runs immediately after every built-in, Hand, and MCP provider return, on
recovery-created error output, and in the trusted-file branch that bypasses the
router — always *before* output budgeting, artifactization, telemetry,
persistence, tracing, or any logging of provider text. It is carrier-aware: it
scans text blocks, JSON blocks, the structured payload, process stdout/stderr and
error carriers, and collapses byte-identical bodies first, so one malicious
paragraph echoed into several carriers scores once. Nothing downstream
reclassifies.

**One envelope.** The classifier returns `SecuredToolOutput { safe_output,
assessment, capability, hand_id }`, and that is the only shape a classified
output travels in. Router and executor APIs return it; `Event::tool_result`
consumes it whole. Security metadata is never optional, so no surface can hold
output whose provenance through the detector is unknown.

**Typed classes with an additive score.** `Safe = 0`,
`SuspiciousInstruction = 1`, `ConfirmedInjection = 2`, `CanaryLeak = 4`,
`RestrictedOrSecretOutput = 4`. Suspicious matched spans are redacted in place;
the three higher classes destroy every raw carrier — content, structured payload,
and artifact reference — and substitute one fixed safe string, regardless of the
capability's current score.

**Per-owner, per-capability accumulation.** Score 1 warns, 2 disables the
capability, 3 suspends for user input, 4 or more halts the owner. Only the first
highest stage reached transitions, so a clear-to-4 canary leak emits one halt
rather than walking the intermediate stages. The circuit is keyed by the exact
generation-fenced owner plus the *router-resolved* canonical capability
(`builtin:<tool>`, `mcp:<server>:<tool>`, or one logical Hand capability
independent of which sandbox provider served it). State resets only for a
genuinely new owner generation — never for a new input fingerprint, new tool
arguments, a fallback Hand provider, or a workflow replay. That is what makes the
circuit hold while an attacker varies the payload, and why it catches what a
generic repetition cap does not.

**Neutral, replay-stable facts.** Crossing a stage boundary appends one
`Event::PromptInjectionCircuitTransition` carrying no output — only the safe
class, detector revision, owner and capability identifiers, the transition, and
counts. Its dedupe key is the transition digest itself
(`prompt_injection_circuit:v1:<64 lowercase blake3 hex>` over domain-separated
canonical JSON of the schema version, session, owner, capability, tool-call id,
prior stage, and reached stage), so a replayed or retried owner collapses onto one
Session fact. The Session and Worker virtual objects own the read-score-write step,
which makes it atomic against concurrent tool results in the same turn.

Do not treat prompt filtering as a complete security boundary. The circuit bounds
the blast radius of a successful injection; it does not make untrusted output
trustworthy.

Execution plans do not bypass these controls. The compiler accepts only
registered capabilities with schema, policy, authorization, risk, idempotency,
and provenance metadata. `Capability` and `Agent` tasks invoke the same governed
boundary as root tools. An agent task is autonomous only inside its declared
skills, capabilities, turn count, and budget. It cannot mutate durable state or
the graph invisibly: unexpected conditions return typed `NeedsInput` or
`NeedsReplan`, and every amendment is compiler-validated, replayable, and unable
to broaden authorization.

Progress narration treats child summaries and tool output as untrusted input.
The per-session narrator summarizes that material into neutral, user-facing prose
that is never executed, respects the same privacy/PII boundaries as other visible
output, and must not widen what the user can already see. Its
`tokens_used`/cost are attributed to a system/overhead bucket in observability,
not to the user's task budget, and a narration failure is a warning rather than a
turn failure.

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

Execution resource envelopes are not capability grants. Cost, token, task,
tool-call, retrieved-byte, deadline, and unattended-spend limits govern how much
work may run. Skills, capabilities, node shapes, strategies, and data access are
restricted separately by the pinned agent policy, execution capability catalog,
OpenFGA/RLS, action review, and node declarations. Raising a resource limit does
not widen permission.

## Action Policy

Action-policy decisions are scoped to parsed tool intent, not raw command
strings. Shell matching splits command chains so a rule for one command does not
cover `&&`, `||`, `;`, or pipe-connected follow-up commands.

Default tool policy is auto-mode `allow`. Tenant-level policy rows and config
can return `allow`, `deny`, or `admin_review`.

Every decision combines authorities by taking the strictest outcome, and reports
a typed source (`deployment_default`, `tool_definition`, `persisted_rule`,
`configured_review`, or `configured_deny`) that names the deciding authority
without carrying the invocation input or the matched pattern.

Restrictions come in two tiers, which differ in whether a tenant grant can lift
them. A tool author's intrinsic `admin_review` is a **cautious default**: it
applies whenever nothing has deliberately granted the operation, and a matched
persisted rule for that exact tenant/contact and operation lifts it. Command
execution and MCP tools both declare it, so this is what makes them grantable at
all. An **unliftable floor** is either an intrinsic `deny` (an inherent
restriction of the operation) or a configured `permissions.always_deny` /
`permissions.admin_review` override — floors belong to the deployment operator,
not the tool author, so a deployment that wants an operation review-locked
regardless of tenant rules configures the override rather than relying on a tool
default. Above both sits the profile: `security_profile = cloud` sets a
deny-by-default posture, so an unmatched request is denied outright, which is
stricter than any review gate.

Concretely:

- An unmatched request combines the deployment default with the tool
  definition's own intrinsic default.
- A matched persisted rule is not capped by the deployment default, so an
  explicit scoped grant works under a deny-by-default deployment. It lifts an
  intrinsic `admin_review` but never an intrinsic `deny` or a configured
  override.
- A rule never makes a filtered or unregistered tool visible; policy is
  evaluated only after the tool resolves in the registry.

`admin_review` persists a tenant action-review row plus event, registers the
review on its one typed owner, returns a pending-review tool result to the model,
and does not block the root or worker workflow. Tenant admins clear or deny the
stored action later through the action-review service.

A cleared action executes as a new MOA-owned invocation: fresh internal tool-call
id, no reused provider tool-use id, and a canary-screened stored request. Its
conversational owner is resumed only after the decision and the executed tool's
terminal event are durable, and only through a bounded no-tools continuation turn,
so an approval cannot silently re-open tool access or planning. A review that
times out fails closed and produces no continuation.

## Security Audit

MOA emits OCSF v1.3 security events for authentication, authorization,
API-key lifecycle, agent lifecycle, action reviews, and SCIM lifecycle changes.
Denied authorization decisions are always emitted when security audit is
configured. Allow decisions are high-volume and controlled by config.

Lineage audit and security-event audit are separate:

| Plane | Crate/service | Purpose |
|---|---|---|
| Lineage audit | `moa-lineage-audit` | Data lineage, Merkle roots, DSAR verification |
| Security audit | `moa-ocsf` | OCSF event signing and Postgres persistence |

Tool execution spans do not attach raw serialized tool input, raw tool output,
or raw error output by default. Spans keep correlation-safe metadata instead:
tool name, duration, success, input byte length and hash, and output byte length
and hash when output exists. `MOA_TRACE_TOOL_OUTPUT=1` or `true` is an
operator-only diagnostic opt-in for bounded raw tool input/output bodies,
mirroring `MOA_TRACE_PROMPT_SAMPLE`; it must not be enabled for normal
production telemetry.

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
