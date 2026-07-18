# Execution evaluation corpora

`manifest.toml` pins the byte hashes and record counts for three JSONL files:

- `routing-v1.jsonl` contains 320 adjudicated route cases and scripted provider
  behavior that drives the production async classifier without a live model:
  60 Respond, 240 Execute (140 Inline and 100 Durable), and 20 NeedsInput.
- `contract-recorded-v1.jsonl` contains 80 strict planner candidates paired
  with independently stated goal-contract expectations.
- `task-quality-v1.jsonl` contains 20 paid-lane cases with exact public-route
  and optional strategy expectations.

Regenerate the machine-owned routing and contract corpora plus the complete
manifest from the workspace root with:

```bash
cargo run -p moa-eval --example generate_execution_corpus --locked
```

The offline gate scores public-route cost separately from Execute-strategy
cost. It treats Execute-to-Respond as catastrophic, requires perfect Durable
strategy recall, and rejects contract omission or false completion. Review
semantic changes in the generator and sampled JSONL rows. Never edit a manifest
hash independently of its corpus bytes.
