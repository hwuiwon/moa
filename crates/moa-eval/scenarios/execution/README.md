# Execution evaluation corpora

`manifest.toml` pins the byte hashes and record counts for three JSONL files:

- `routing.jsonl` contains 328 adjudicated route cases and scripted provider
  behavior that drives the production async classifier without a live model:
  60 Respond, 248 Execute (144 Inline and 104 Durable), and 20 NeedsInput.
  Four Execute/Durable cases pin enumerated parallel workstreams that
  forward-reference not-yet-provided user material to Act rather than clarify.
  Four Execute/Inline cases carry covering installed skills (`available_skills`)
  so borderline requests route to Act rather than clarify (session S016).
- `contract-recorded.jsonl` contains 80 strict planner candidates
  with explicit cancellation policy and node compensation fields, paired with
  independently stated goal-contract expectations.
- `task-quality.jsonl` contains 20 paid-lane cases with exact public-route
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
