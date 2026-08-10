# Implementation Caveats To Fix

This file tracks current implementation caveats that still need a code or design
fix before later work builds on top of them. It intentionally does not include
deliberate trade-offs, future-only extension points, or stale caveats already
addressed by the current code.

## E2B Workspace Persistence

E2B's public pause/resume and reusable-snapshot contracts preserve process
memory as well as the filesystem. They are therefore not valid implementations
of MOA's filesystem-only durability class. Production E2B workspace routing
sets automatic pause/resume off, inspects the exact sandbox and ownership
metadata before obtaining an access token, exports only
`/home/user/moa-data` through a bounded host-side temporary root, publishes the
validated encrypted portable checkpoint, and kills the source sandbox. Restore
decrypts into a new permission-restricted host temporary root and uploads the
validated entries into a separately provisioned fresh hand. E2B volumes are
not a selectable storage mode.

---

## Security And Tool Execution

### Prompt-injection feedback needs a security circuit breaker

MOA already injects per-turn canaries when tools are available, blocks tool
inputs that leak canaries, wraps tool outputs as untrusted content, and stops
identical repeated tool calls through the general tool budget. There is still
no security-specific circuit for varied malicious calls that keep changing
fingerprints after blocked-tool feedback.

Why this needs fixing:

- General max-tool-call and repeated-fingerprint budgets prevent unbounded
  loops, but they do not distinguish ordinary tool churn from repeated security
  violations.
- A model that keeps adapting malicious tool calls after `ToolError` or
  `Warning` feedback should trip a clearer policy response than "eventually hit
  the generic turn cap."

Long-term fix:

- Add security circuit state beside the tool budget. Inputs should include
  canary leaks, suspicious tool-output classifications, repeated policy-denied
  dangerous actions, and ignored blocked-tool feedback.
- Make the output explicit: terminate the turn, disable tools for the rest of
  the turn, require user/admin confirmation, or record a typed security event.
