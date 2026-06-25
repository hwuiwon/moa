# Credentials and Vault

How provider implementations get secrets, and how MCP servers proxy user-supplied credentials. The rule that runs through everything: providers do not read environment variables directly.

## The CredentialVault Indirection

`moa-security` exposes a `CredentialVault` trait. Provider constructors accept a vault handle, not a raw API key. At dispatch time, the provider looks up the credential by key, gets back a short-lived secret, and uses it for the call.

This indirection matters because:

- The same MOA process serves multiple tenants and users; one global env var is not enough.
- Test fixtures inject a fake vault that returns scripted secrets; without the indirection, tests would set process-wide env vars, which interleave badly under parallel `cargo test`.
- The vault rotates secrets and audits access; raw env vars cannot.

## Vault Implementations

The repo has multiple vault backends. Read the current set in `crates/moa-security/src/`. As of the audit:

- A file-backed vault for development.
- A KMS-backed vault for production deployments.
- A scripted vault for tests.

A provider implementation does not care which backend is in use; it accepts a `CredentialVault` (or an `Arc<dyn CredentialVault>`) at construction.

## Credential Keys

Each credential has a key string. The convention:

- LLM providers: `provider.<name>.api_key`. Example: `provider.anthropic.api_key`.
- Embedding providers: `embedding.<name>.api_key`.
- Hand sandbox providers: `hand.<name>.api_key` and any auxiliary keys (`hand.daytona.workspace_token`, etc.).
- Platform adapters: `platform.<name>.bot_token` and `platform.<name>.signing_secret`.
- MCP servers: see "MCP Credential Proxy" below.

When adding a new provider, register the credential keys in the vault's startup config or schema, and document the keys in a top-level table (search `docs/` for an existing credentials table; if none exists, add one).

## MCP Credential Proxy

MCP servers usually need credentials of their own (a Linear API token, a GitHub PAT, a Slack bot token). These do not belong in the MCP server's process environment; they flow through MOA at dispatch time.

The pattern:

1. The user adds an MCP server to their workspace via the desktop or hosted API; the server config includes a credential schema (named credential keys the server needs).
2. The user supplies values for those keys; values go into `CredentialVault` under `mcp.<server>.<key>`.
3. At dispatch time, the MCP router fetches the relevant credentials from the vault, injects them into the request to the MCP server (typically as headers), and the server uses them for the upstream call.
4. Credentials never persist on disk in the MCP server's process; the vault is the source of truth.

This means:

- MCP servers are stateless from a credential perspective.
- Rotating a credential requires updating the vault, not redeploying the MCP server.
- A user can have two MCP servers with different credentials for the same upstream service (e.g. two Linear workspaces).

## Test Patterns

For offline tests:

```rust
let vault = ScriptedVault::new()
    .with("provider.anthropic.api_key", "test-key");
let provider = AnthropicProvider::new(vault, &client_config);
```

For live tests, the test reads the credential from the real vault — usually because the test was started under a process that already has the vault populated, or because the test calls `CredentialVault::from_env()` as a development convenience. The double-gate pattern still applies: the test's `#[ignore]` reason names the credential keys it needs.

## Common Mistakes

- Calling `std::env::var("ANTHROPIC_API_KEY")` inside the provider. This was the original pattern but is no longer correct; the vault indirection is mandatory for new providers.
- Storing the credential in the provider struct at construction time. The vault is the source of truth; fetch on dispatch so rotation works.
- Logging the credential value, even at `DEBUG`. Use vault-aware redaction (the vault types should already implement `Debug` to redact).
- Hardcoding credential keys as string literals scattered across the codebase. Define them as constants in one place per provider.
- Using a single credential key for two providers. If two LLM providers happen to use the same vendor, give them distinct keys to keep rotation independent.
