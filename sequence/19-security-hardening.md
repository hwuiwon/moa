# Step 19: Security Hardening

## What this step is about
Credential vault implementations, prompt injection detection, canary tokens, and sandbox hardening.

## Files to read
- `docs/08-security.md` — Full security architecture

## Goal
Credentials are encrypted at rest and never reachable from sandboxes. Prompt injection is detected before it reaches the brain. Docker containers are hardened with seccomp and read-only filesystems.

## Tasks
1. **`moa-security/src/vault.rs`**: `FileVault` (local, encrypted with `age` crate) and `CredentialVault` trait. Support `get/set/delete/list` operations.
2. **`moa-security/src/injection.rs`**: Prompt injection detection — heuristic classifier for untrusted content, canary token injection and checking.
3. **Update `moa-hands/src/local.rs`**: Docker container hardening — seccomp profile, read-only root, dropped capabilities, blocked metadata endpoints.
4. **Update `moa-brain/src/harness.rs`**: Before processing tool results, run injection classifier. Wrap untrusted content in explicit tags.
5. **Instruction hierarchy enforcement**: System prompt content cannot be overridden by tool results.

## Deliverables
`moa-security/src/vault.rs`, `moa-security/src/injection.rs`, updated `local.rs` and `harness.rs`

## Acceptance criteria
1. FileVault encrypts credentials on disk, decrypts on read
2. Injection classifier flags "ignore previous instructions" with high risk score
3. Canary tokens injected into context are detected if they appear in tool calls
4. Docker containers run with seccomp profile and read-only root
5. Tool results are wrapped in `<untrusted_tool_output>` tags

## Tests
- Unit test: FileVault encrypt → decrypt roundtrip
- Unit test: Injection classifier scores known attack patterns correctly
- Unit test: Canary token detection works
- Unit test: Instruction hierarchy — tool result content doesn't override system prompt
- Integration test: Docker container runs with hardening (verify seccomp active, metadata endpoint blocked)

---

