# MOA Skills Authoring

This directory is only an authoring convenience. The runtime does not load
skills from disk.

Author or edit `*.md` / `*/SKILL.md` files here, then import them into
Postgres through the hosted Skills API:

```sh
curl -X POST "$MOA_API_URL/v1/skills/import" \
  -H "Authorization: Bearer $MOA_API_TOKEN" \
  -H "Content-Type: application/json" \
  --data @skill-import.json
```
