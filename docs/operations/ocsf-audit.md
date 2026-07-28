# OCSF Security Audit

MOA emits OCSF v1.3 security events for authentication, authorization,
API-key lifecycle, agent lifecycle, approval decisions, SCIM
provisioning/deactivation, and prompt-injection security-circuit transitions.
A bounded background sink signs most events with a per-tenant HMAC key and
writes them to Postgres without blocking request handling. Detection Findings
are the exception and are written synchronously; see below.

## Event Volume

Authentication and denied authorization events are emitted by default.
Allowed authorization decisions are high volume and are controlled by:

```sh
MOA_AUDIT_SECURITY_EMIT_AUTHZ_ALLOWS=true
```

For most tenants, keep allow-emission off unless a compliance posture requires
full allow/deny audit trails.

## Prompt-Injection Detection Findings

Every prompt-injection circuit stage change emits one OCSF Detection Finding:
`class_uid=2004`, `category_uid=2` (Findings), `activity_id=1` (Create),
`type_uid=200401`. These are written **synchronously and fail closed**, unlike
every other event on this page — a halt must never take effect with no audit
record explaining why, so the owning agent blocks until the finding is durable.
They do not pass through the background sink and are never dropped.

Identity is UUIDv5 over the transition key, so a crashed-and-replayed agent
writes the same primary key instead of a second row. `finding_info.uid` is that
key, shaped `prompt_injection_circuit:v1:<64 lowercase blake3 hex>`, and it is
also the dedupe key of the matching `PromptInjectionCircuitTransition` session
event — join on it to line a finding up with its session history.

Queryable columns: `actor_session_uid` is the owning session and
`target_resource_uid` is the canonical capability that tripped
(`builtin:<tool>`, `mcp:<server>:<tool>`, or `hand:<tool>`). `actor_user_uid` is
NULL because a circuit transition has no human actor, and
`retrieval_operation_id` is NULL because these findings deliberately do not
borrow the data-access uniqueness contract.

Severity is a pure function of the stage reached, so it is stable across
replays and safe to alert on directly:

| Stage reached | `severity_id` | Meaning |
|---|---|---|
| `warned` | 2 (Low) | One suspicious output; the capability still dispatches. |
| `disabled` | 3 (Medium) | The capability cannot dispatch again under this owner. |
| `suspended_for_input` | 4 (High) | The owner is suspended awaiting user input. |
| `halted` | 5 (Critical) | The owner is halted. |

The payload is content-free by construction: fixed title and description,
closed-vocabulary detector signals, and MOA-minted identifiers only. No tool
output, no matched span, and no attacker-supplied byte reaches a finding, so
findings are safe to ship to an external SIEM verbatim.

**Alert on replay conflicts.** If a write returns a replay conflict, two
genuinely different transitions collided on one deterministic identity, or a
stored row's canonical payload drifted from what the owner re-derived. Either
means the audit trail disagrees with itself and the derivation or the row needs
investigation; the emitter surfaces it as a terminal error rather than
absorbing it. Verification of a conflicting row uses the key that row was
signed with, resolved from its own `signing_key_id`, so a routine key rotation
never produces a spurious conflict.

## Signing Keys

Each tenant has one active HMAC-SHA256 signing key. Create or rotate keys with:

```sh
curl -X POST http://localhost:10010/Tenants/ensure_signing_key \
  -H "Content-Type: application/json" \
  --data '"<tenant_uuid>"'
curl -X POST http://localhost:10010/Tenants/rotate_signing_key \
  -H "Content-Type: application/json" \
  --data '"<tenant_uuid>"'
```

Old key rows stay in `tenant_signing_keys` so historical events remain
verifiable after rotation.

## Verify An Event

```sh
curl -X POST http://localhost:10000/v1/audit/verify \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <edge-token>" \
  --data '{"event_id":"<event_uuid>","tenant_id":"<tenant_uuid>"}'
```

`valid: true` means the stored canonical JSON bytes still match
`signature_hex` for the event's `signing_key_id`. `valid: false` means the row
was tampered with or the wrong signing key was used. This is a direct
`moa-edge` read route with tenant-admin authorization, not a Restate read
service.

## Persistence And Failure Behavior

The in-process sink batches inserts into `security_events`. A full queue,
signing failure, or insert failure drops the affected event rather than failing
the caller and increments `moa_ocsf_audit_events_dropped_total`. Alert on any
non-zero increase because it means the Postgres audit trail is incomplete.

This drop-rather-than-fail behavior covers only the background sink, which is
used where an audit write must not gate a response (authentication,
authorization denials). Synchronous emitters — Detection Findings and the
transaction-scoped `emit_*_tx` helpers — fail closed instead: the caller gets an
error and rolls back the action rather than proceeding without a record.
