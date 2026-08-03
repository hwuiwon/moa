# moa-auth

Authentication, authorization, and identity-provider crates for MOA. This
directory is a namespace folder, not a Rust package; its subdirectories are
independent crates.

## Subcrates

| Path | Crate name | Responsibility |
| --- | --- | --- |
| `authz-schema/` | `moa-authz-schema` | Typed OpenFGA object, relation, tuple, and model-version constants. |
| `authz/` | `moa-authz` | OpenFGA client, authorization checks, transactional outbox, and outbox poller. |
| `providers/` | `moa-auth-providers` | Local API-key auth, disabled auth, builtin approvals, tenant connector credentials, provider bundle construction, and optional Auth0/OIDC, CIBA, and JWKS modules. |
| `fga-bootstrap/` | `moa-fga-bootstrap` | OpenFGA store/model bootstrap binary. |

## Public Surface

Consumers depend on the package names, not these folder names:
`moa-authz-schema`, `moa-authz`, `moa-auth-providers`, and
`moa-fga-bootstrap`.

The shared authentication traits live in `moa-core::traits` so downstream
crates can depend on the contracts without pulling in provider implementation
dependencies.
