# moa-fga-bootstrap

Binary crate: idempotent OpenFGA bootstrap for MOA. On each run it ensures the
configured store exists, writes the schema v1 authorization model embedded in
`moa-authz-schema`, runs smoke checks against a synthetic tenant chain, and
writes the resolved authz env values to `.env.fga` for shell sourcing.

## Structure

- `src/main.rs` — the `moa-fga-bootstrap` binary: clap CLI (all flags also
  read from env: `MOA_AUTHZ_OPENFGA_URL`, `MOA_AUTHZ_OPENFGA_PRESHARED_KEY`,
  `MOA_AUTHZ_OPENFGA_STORE_NAME`, `MOA_FGA_ENV_OUTPUT`,
  `MOA_FGA_BOOTSTRAP_SKIP_SMOKE`), store create-or-reuse, model write, and
  smoke checks.
- `src/http.rs` — minimal OpenFGA HTTP client used only by this binary (the
  production client lives in `moa-authz`).

## Usage

Safe to re-run; existing stores with the configured name are reused and the
model write is additive. Source the generated `.env.fga` so services and tests
pick up the store and model IDs. `--skip-smoke` is intended only for CI
bootstrapping.
