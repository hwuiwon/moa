# MOA Skills Authoring

This directory is only an authoring convenience. The runtime does not load
skills from disk.

Author or edit `*.md` / `*/SKILL.md` files here, then import them into
Postgres:

```sh
cargo run -p moa-cli -- skills import . --from skills --scope workspace
cargo run -p moa-cli -- skills bootstrap_global --from skills
```
