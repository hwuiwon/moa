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
only overlay that references the cloud sandbox credential Secret. Production
also authors the `production-e2b-v1` sandbox policy: deny-all egress, 900-second
idle expiry, 3600-second hard lifetime, and otherwise explicit unbounded
resources.

## Identity And Authorization

API keys are the zero-dependency default. Auth0 or generic OIDC can be enabled
for SSO, SCIM, and CIBA approvals. `auth.provider = "disabled"` is
for local development or isolated tests only.

OpenFGA is the default authorization engine. Handlers must call
`require_authz` or `require_authz_with_delegation` before protected reads. The
transactional outbox is the only supported way to synchronize product state and
OpenFGA tuples.

`SandboxWorkspace` is a tenant-owned resource. Every list, get, attach,
checkpoint, restore, and delete authorizes before its protected read and derives
tenant plus worker/execution-task scope from verified durable state. A local
lifecycle/delete fence is checked on every operation in addition to OpenFGA, so
a short positive-cache lifetime cannot reopen a deleting workspace. Provider
identifiers, paths, labels, and object keys are never authorization tokens. See
[Sandbox Workspaces](25-sandbox-workspaces.md).

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
not merely the tenant boundary. Every connection is governed by source ACLs;
there is no connection-level or operator override. Content is admitted only through an
immutable snapshot that is complete and revision-matched, with a matching allow
entry and no matching deny entry. Missing, incomplete, stale, or
revision-drifted ACLs deny, an empty caller principal set denies, and tenant role
or operator status grants no content bypass.

Principals are persisted only as connection-scoped keyed opaque fingerprints
(HMAC-SHA256 under a KMS-wrapped, versioned per-tenant ACL key, with the key
version encoded into the value), so no email address, phone number, or provider
label reaches a row, log, trace, or cache key. The caller's principal set is
resolved once per turn from authenticated identity plus verified bindings,
never from request JSON and never re-fetched inside a retrieval leg. The current
production bridge binds provider `anyone` grants only; user, group, and domain
principals remain fail-closed until a verified identity bridge writes them.

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
| Host-side connector injection | Reviewed constrained HTTP operations | Trusted runtime resolves one named slot only after authorization and destination admission. |
| Environment-backed provider keys | LLMs, embeddings, hand providers | Runtime loads directly injected secrets into typed host-side config, not prompt-visible values. |

Per-user OAuth token brokering is not part of the runtime. Connector credential
sources implement `CredentialVault` and stay behind the host-side credential
boundary.

`CredentialVault` deliberately exposes only the connector lifecycle MOA uses:
stage a candidate, activate or roll it back against an exact predecessor,
resolve the active version with an append-only audit, report secret-free slot
status/description, revoke one version or an owning connection, and perform a
bounded tenant purge. It has no generic credential-source resolver, deployment
secret accessor, create/rotate shortcut, or ordinary connection-delete method.

MOA-managed tenant connector material lives in one durable owner. A credential
series identity is `(tenant, owning connection, slot, kind)` with append-only
versions; resolution
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
event, or persisted on a knowledge row. Deployment-owned operator MCP transport
secrets are read from operator-selected environment configuration while the tool
router is constructed; they are never stored in or resolved through the tenant
credential vault and are never available to tenant HTTP connector connections.

Tenant connector credential writes take a dedicated boundary:
`moa-edge` authenticates and bounds
`PUT /v1/connectors/connections/{connection_id}/credentials/{slot_name}`, then
forwards the opaque body to the orchestrator's private
`/internal/v1/connectors/credentials/write` listener on port 10023. The listener
derives identity from edge-injected headers and performs delegated `Manage`
authorization before staging material. Plaintext never enters a Restate
request, journal, public response, or caller-selected identity/reference field.

The private write stages an inactive candidate, advances the connection's
secret-free generation fence, and activates only against the exact predecessor.
Concurrent rotation leaves one winner. Exact rollback revokes only that
candidate and restores only its recorded non-revoked predecessor. Ordinary
disconnect revokes all connection versions while preserving history and audit;
only the bounded tenant-purge actor may delete them.

Operator-owned deployment MCP may use one deployment credential read from an
operator-selected environment variable at router construction. Missing
configured material fails startup. The MCP client marks authentication headers
sensitive and applies them to the complete protocol exchange: initialize,
initialized notification, discovery, and `tools/call`. Non-success response
bodies are not copied into errors, so an upstream cannot reflect credentials
into logs.

## Connector Destination Admission

Production connector HTTP uses one permanently strict outbound policy. It
accepts one canonical HTTPS origin only when the complete current DNS answer
set is publicly routable. Every attempt gets a fresh DNS-pinned client with
system/environment proxies, redirects, and automatic retries disabled.
Authorization and destination admission run before credential resolution and
header injection, and every retry repeats the full sequence.

Destination failures expose bounded stable codes, never origins, hosts, IP
addresses, resolver text, upstream bodies, JSON-RPC errors, or credential-derived
material. Plain HTTP loopback admission exists only under `cfg(test)` or the
explicit `test-support` feature for isolated fixtures. The runtime local profile
does not weaken this policy.

For the complete connection authorization, catalog, idempotency, and
unknown-outcome contract, see
[Connectors And Connections](24-connectors-and-connections.md).

## Encryption And Key Management

Persisted restricted/PHI memory and tenant connector credential values use
envelope encryption. Postgres owns shared generation metadata and per-subject wrapped
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
bounded process counts, and metadata-endpoint blocking.

The network posture is no longer a provider constant. Local Docker maps the
resolved profile's egress mode onto `--network none` (`DenyAll`) or
`--network bridge` (`Unrestricted`), and refuses an egress allowlist outright
because `docker run` has no per-destination filter to enforce one with. The bare
host tier can enforce no network posture at all, so it admits only
`Unrestricted` — a deployment that needs deny-all egress must not route to host
hands. See `docs/06-hands-and-mcp.md` for the full effective-profile contract.

Every sandbox compute instance is ephemeral by default, and that is enforced
rather than assumed. Each lease carries an immutable hard maximum lifetime that
renewal cannot extend, and the durable hand-lease reaper destroys expired, idle,
and abandoned compute without waiting for traffic that may never arrive.
Tenant-created filesystem state is different: an independently retained
`SandboxWorkspace` stores only its filesystem-only mutable root through
encrypted, verified portable checkpoints. Restate/session state and a leftover
container do not make arbitrary files durable.

The root coordinator is sandbox-free. Each worker or sandbox-using execution
task owns one typed workspace and one writable attachment under independent
writer and compute-instance fences. Parallel owners therefore do not share a
compute environment or workspace root. Self-cleanup checkpoints according to
policy and releases compute without deleting retained workspace state. The
architecture and runtime have no session-level workspace admission: sandbox
dispatch requires a typed worker or execution-task owner and rejects its
absence before workspace reads or provider I/O.

Conversational workers are available only as interactive delegation in `act`;
they are not the bulk DAG primitive. Sandbox-using `ExecutionTask` instances
receive equivalent isolation under a stable run/task identity and generation
fence. Execution maps materialize every budget-admitted logical row, but only
the positive `execution.max_in_flight_tasks` window owns live task invocations;
pending rows remain storage-only. Provider pacing and governed capability or
hand capacity impose independent bounds inside that run-owned window.

Only the reserved mutable tenant-data root is checkpointed. Trusted files,
credentials, tokens, authorization, policy, runtime controls, network state,
and process memory stay outside it and are rebuilt from current authority after
restore. Provider working state is never committed by itself. Mutating tool
success is durable only after quiescence, bounded encrypted checkpoint upload,
manifest/digest verification, and a fenced head compare-and-set. Daytona uses
tenant-dedicated volumes with opaque workspace subpaths as working state; E2B
exports the reserved root, publishes a portable checkpoint, and kills compute
because its public pause and snapshot contracts retain process memory;
local/Docker uses isolated scratch. All use the portable checkpoint as recovery
authority.

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
(`builtin:<byte-length>:<tool>`, `mcp:<server-byte-length>:<server>:<tool-byte-length>:<tool>`,
or the equivalent framed Hand identity independent of which sandbox provider
served it). Length framing keeps the identity injective even when names contain
separators. State resets only for a genuinely new owner generation — never for
a new input fingerprint, new tool arguments, a fallback Hand provider, or a
workflow replay. That is what makes the circuit hold while an attacker varies
the payload, and why it catches what a generic repetition cap does not.

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

## Learning Privacy

Automatic learning is an egress path: it sends transcript-derived content to a
model provider and writes it into durable draft rows before any human sees it.
Every one of those boundaries takes `moa_skills::evidence::SanitizedLearningEvidence`,
a type with private fields, no raw-string or raw-event constructor, and no
`Deserialize`. Raw transcript evidence is unrepresentable there, not merely
discouraged.

Sanitization is **irreversible** and is the opposite mechanism from the
request-scoped tokenization in `moa-providers`' provider-governance layer. A DLP
token is a placeholder that a later restoration step turns back into the
original value, scoped to one request. Learning sanitization replaces the
original bytes with a category placeholder and keeps no way back. Mixing them
would be a real leak, so text carrying the reserved DLP delimiters (`⟦`/`⟧`) is
refused before the classifier is consulted: a restorable token inside a durable
learning artifact would let the original value be reconstructed long after the
request ended. The delimiters are defined once in `moa-memory-pii`, and the
provider-governance DLP implementation references them from there.

PII and PHI may proceed, but only after irreversible redaction. These refuse
outright:

- `Restricted` classification, and any secret/credential-category span. A
  redacted credential still reached the learning boundary, so redaction is not
  an acceptable remedy for one.
- Classifier error or abstention. An unavailable detector must never degrade
  into an implicit "no sensitive content found".
- Spans that cannot be applied exactly as detected — empty, inverted,
  out-of-range, non-UTF-8-boundary, or overlapping. Applying a partial span
  would leave the original bytes in place while the result claimed to be
  sanitized.
- Residual sensitivity after re-classifying the redacted text, which catches a
  detector that found one of two occurrences.

A refusal produces zero provider calls and zero derived writes for that segment.
Sibling and recurrence paths gate each member independently.

Rejections are reported as a stable carrier label plus a stable reason code, and
never carry the refused text or the classifier's own error message. Derived rows
carry identifiers, the detector version, the redacted category vocabulary, and
one constant policy revision — enough for a reviewer to trace a draft back to
its exact source events without the content being copied forward. The raw
session event log stays the single source-of-truth owner of unredacted
transcript, so erasure and retention have exactly one place to act.

See `docs/09-skills-and-learning.md` for the carrier list and the learning-loop
view of the same gate.

## Learning-Derived Erasure

Sanitization governs what learning may *read*. This governs what happens to
learning already *written* when the subject behind it exercises a right to
erasure.

Deleting a subject's memories while a skill distilled from those memories keeps
serving is not erasure. It removes the evidence and leaves the conclusion. MOA
previously could not do better, because provenance was a bare `UUID[]` with no
foreign key and no declared referent type: nothing in the database could
enumerate a derivation, so nothing could reverse one.

Provenance is now normalized and typed. Each referent kind has its own column
with a composite foreign key that carries the partition, so a cross-tenant
source is rejected by the constraint rather than by a query someone has to
remember to write. The privacy-erasure decision and provenance tables force
tenant RLS in addition to these scoped joins. The closure runs
`contact/session/event/task_segment -> experience/attribution -> candidate ->
learning_log -> artifact revision/file -> generated or accumulated suite
contribution`, recursively following promoted-candidate dependencies and
artifact-revision contributions into dependent candidates. Erasure walks it in
reverse through typed joins — never JSON containment, array membership, or
`LIKE`, all of which silently both over- and under-match.

Four rules decide dispositions, and each exists because its absence produces a
confident lie:

- **A legal hold mutates nothing.** The blocked path still enumerates read-only
  and records one idempotent `retained_legal_hold` decision per record. The
  database refuses to mark such a decision `applied`, so "the hold was honored"
  is a checkable per-record fact rather than an absence of evidence.
- **A dry run is a plan.** Dispositions persist with `applied = false`. A dry run
  that recorded deletions would later be read as proof the data is gone. Ledger
  identity includes tenant, subject, erase attempt, record kind, and record id,
  so a dry run or legal-hold attempt cannot mask a later applied attempt.
- **Fused model output is non-subtractable.** A revision's `definition` and
  `source_text` may have been written from several people's transcripts at
  once, so every attributable revision is archived and cleared in place:
  definition, source, files, and serving state are removed while the revision
  identity remains for pinned foreign keys. It is never partially rewritten or
  deleted.
- **Concurrent learning is fenced before enumeration.** The erase claims its
  operation and destruction fence *before* it enumerates, and contribution
  inserts are refused while a fence is in progress. Without that, a turn
completing mid-erase could file derived learning between enumeration and
deletion and survive the run.

The public decision vocabulary is closed. Record kinds are
`learning_candidate`, `learning_log`, `artifact_revision`,
`artifact_suite_contribution`, `experience_record`, and
`experience_attribution`; dispositions are `erased`, `invalidated_revision`,
and `retained_legal_hold`.

Ordering is load-bearing: the reverse-derived learning and artifact stages run
**before** the vault, graph, digest, and lineage stages, because the closure walk
needs the source memories to still exist in order to find what was derived from
them.

### Scope fence — read this before assuming coverage

**Learning-derived erasure does not claim raw session-event, attachment, blob, or
archive erasure.** Those are separate stores with separate owners and separate
lifecycles.

The guarantee is bounded to exactly this: no *active learning-derived
contribution or source byte* survives outside a legal hold. It must never be read
as "the subject's data is gone." The raw session event log remains the
single source-of-truth owner of unredacted transcript, and it is erased by its
own path, not this one.

`experience_records.task_summary` is now written redacted at rest. Every
candidate for it comes from sanitized evidence: the classifier-approved task
summary, else the sanitized first user message, else a constant. The raw query
rewrite's `task_summary` is deliberately *not* a fallback, and that is the whole
fix — it is the exact text handed to sanitization, so reaching for it when
sanitization produced nothing would reinstate the value the classifier rejected
or abstained on, turning the one case where redaction mattered most into the one
case where it did not happen. It also keeps the stored summary and the embedded
summary the same bytes; a raw stored summary would fork them, so a row's vector
would describe text the row does not contain and semantic routing would degrade
with no failure signal.

The fresh-only schema admits no pre-redaction compatibility state. Every
`experience_records.task_summary` write follows the sanitized path above; the
erasure path remains responsible for subject deletion.

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
times out fails closed and produces no continuation; its durable release delivery
removes the Session or Worker lifecycle hold without invoking the model.

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

## Telemetry Collector Access And Supply Chain

The Alloy collector holds cluster-wide read access, so its permissions are
enumerated rather than granted broadly. `k8s/observability/15-alloy-rbac.yaml`
grants `get`/`list`/`watch` on `pods`, `pods/log` and `namespaces` for log
collection, and the same three verbs on `monitoring.coreos.com/prometheusrules`
for rule synchronization. Read-only on rules is the correct ceiling: the
component's job is to copy git-authored rules into Mimir, so it has no reason to
be able to modify a rule inside the cluster. Backend credentials reach it only
through the `grafana-cloud` Secret; none appear in a manifest, a config file, or
a command line.

CRD schemas used for manifest validation are vendored under `k8s/schemas/`,
pinned by upstream release tag **and** content checksum in `sources.json`.
`refresh.sh` verifies every checksum before it regenerates anything and refuses
on a mismatch. The tag alone is not a pin — a tag can be moved, and a schema
fetched from a moved tag would quietly widen or narrow what manifest validation
accepts while continuing to look like it works. CI installs `kubeconform`,
`alloy` and `promtool` at pinned versions verified by `sha256sum -c` for the
same reason.

`k8s/scripts/observability-smoke.sh` mutates a live cluster: it rotates
Deployments, applies and deletes a temporary `PrometheusRule`, and starts a real
billed turn. It is gated on `MOA_RUN_LIVE_OBSERVABILITY_SMOKE=1` **and** on an
explicitly named kube context, because a developer's current context is
routinely some unrelated cluster and every mutating command would otherwise be
aimed at it. Backend credentials are passed to `curl` through a `0600` config
file rather than a command line, where `ps` and shell tracing can read them, and
every temporary resource is removed by an `EXIT` trap on failure as well as
success — a canary alert rule left in Mimir is a permanently firing alert nobody
owns.

## Build Rules

- Fail closed when identity or authz providers cannot make a decision.
- Keep secrets out of logs, fixtures, docs examples, and model-visible text.
- Use `tracing`, not stdout/stderr, for security-relevant events.
- Put security-sensitive provider dependencies behind feature flags when they
  are optional.
- Document any handler without resource-specific authz with the required
  one-line `// SAFETY:` justification.
