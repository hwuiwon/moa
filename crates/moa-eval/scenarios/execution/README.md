# Execution evaluation corpora

`manifest.toml` pins the byte hashes and record counts for two JSONL files:

- `routing-v1.jsonl` contains 320 adjudicated route cases and scripted provider
  behavior that drives the production async classifier without a live model.
- `contract-recorded-v1.jsonl` contains 80 strict planner candidates paired
  with independently stated goal-contract expectations.

Regenerate all three machine-owned files from the workspace root with:

```bash
cargo run -p moa-eval --example generate_execution_corpus --locked
```

Review semantic changes in the generator and sampled JSONL rows. Never edit a
manifest hash independently of its corpus bytes.
