# moa-config

Runtime configuration for MOA, organized by sub-domain. This crate owns the
`MoaConfig` tree, its per-domain sub-configs, and the flat `EnvOverlay` used to
apply Kubernetes-style environment overrides. It is kept separate from
`moa-core` so config-knob changes do not force a rebuild of crates that never
touch configuration.

## Structure

- `lib.rs` — the `MoaConfig` root struct, validation, `model_for_task`
  routing, and re-exports of every sub-config type
- `loader.rs` — configuration loading (`MoaConfig::load`,
  `MoaConfig::load_from_env`)
- `env_overlay/` — flat single-underscore `MOA_*` environment overlay for
  Kubernetes runtime config (`EnvOverlay`)
- One module per configuration domain: `providers`, `database`, `auth`,
  `authz`, `async_authz`, `kms`, `token_vault`, `compliance`,
  `audit_security`, `llm_dlp`, `memory`, `knowledge`, `session`, `context`,
  `execution`, `learning`, `lineage`, `messaging`, `orchestrator`,
  `runtime_cache`, `sandbox`, `security`, `telemetry`, `analytics`,
  `clickhouse`

## Rules

- Environment overrides use canonical flat single-underscore names
  (`MOA_DATABASE_URL`, `MOA_AUTHZ_OPENFGA_URL`, ...) applied through
  `EnvOverlay`.
- Secret values loaded from runtime config go through
  `required_config_secret` / `optional_config_secret`, which trim and reject
  empty values.
