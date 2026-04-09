---
description: Smart graphify update — LLM only where it pays off (semantic extraction + inline labeling), then deterministic rebuild
argument-hint: "[path]"
allowed-tools: Bash, Read, Write, Agent
---

# /graphify-update

Smart incremental refresh of `graphify-out/`. Uses the LLM only for the two things it's actually good at and AST cannot do:

1. **Semantic extraction** (hyperedges, rationale edges, cross-document similarity) — only for changed non-code files. Dispatched to a subagent so the main context stays clean.
2. **Community labeling** — done **inline by the main assistant** after the fast script has already clustered the graph. No subagent, no file reads, just reasoning over `graph.json`'s community structure. Typically ~5 seconds.

Everything else (AST extraction, clustering, rendering) is deterministic Python via `scripts/graphify-fast.sh` and runs in ~0.2s.

Expected cost:
- **Code-only change set** → no subagent, inline labeling only → **~5–10s total**
- **Any non-code change** → one subagent for the changed docs + inline labeling → **~30–45s total**
- Old `/graphify . --update` took ~130s regardless of change type.

Path defaults to `.` (the repo root). Pass an explicit path to scope the run: `/graphify-update moa-brain`.

---

## Step 0 — Clean up scratch state from any prior interrupted run

```bash
rm -f \
  graphify-out/.update_changed.json \
  graphify-out/.update_noncode.json \
  graphify-out/.update_semantic.json \
  graphify-out/.update_communities.json \
  graphify-out/.update_labels.json \
  graphify-out/.graphify_python \
  graphify-out/.graphify_labels.json
```

> **Why explicit filenames?** zsh errors on non-matching globs (`.update_*.json` blows up when there's nothing to match). Listing every known scratch file makes this work on both bash and zsh with no shell-option gymnastics.

Idempotent. Guards against a prior run that crashed before its EXIT trap fired.

---

## Step 1 — Detect what changed, split code vs non-code

```bash
TARGET="${1:-.}"
python3 - "$TARGET" <<'PYEOF'
import json, sys
from pathlib import Path
from graphify.detect import detect_incremental

target = Path(sys.argv[1])
detect = detect_incremental(target)

unchanged = {f for files in detect.get('unchanged_files', {}).values() for f in files}
all_files = [f for files in detect.get('files', {}).values() for f in files]
changed   = [f for f in all_files if f not in unchanged]

code_exts = {'.rs','.py','.ts','.js','.go','.java','.cpp','.c','.rb','.swift','.kt','.cs','.scala','.php','.cc','.cxx','.hpp','.h','.kts','.lua'}
non_code_changed = [f for f in changed if Path(f).suffix.lower() not in code_exts]

Path('graphify-out/.update_noncode.json').write_text(json.dumps(non_code_changed))
print(f'CHANGED={len(changed)} NONCODE={len(non_code_changed)}')
for f in non_code_changed[:10]:
    print(f'  noncode: {f}')
PYEOF
```

Read `graphify-out/.update_noncode.json`. If it is `[]`, **skip Step 2** and jump to Step 3.

---

## Step 2 — Single semantic-extraction subagent (only if non-code changed)

Dispatch **exactly one** `Agent` call with `subagent_type: general-purpose`. The subagent reads each changed non-code file with the `Read` tool, extracts nodes/edges/hyperedges, and returns **JSON only** — no prose, no fences.

### Subagent prompt template

```
You are a graphify semantic-extraction subagent. Read the files listed and extract a knowledge graph fragment focused on what AST extraction CANNOT see:
- rationale_for edges (docs that explain a design decision in code)
- semantically_similar_to edges (cross-cutting conceptual matches)
- hyperedges (3+ nodes participating in one shared flow/pattern)
- named concepts and entities from prose

Files:
<FILE_LIST>

Rules:
- EXTRACTED: explicit in source (citation, "see §3.2", inline link)
- INFERRED: reasonable inference from prose (most edges here)
- AMBIGUOUS: uncertain — include with confidence 0.1–0.3, do not omit

Doc/markdown files: extract named concepts, sections, design rationales. If a doc explains WHY a code module exists, emit a `rationale_for` edge from the doc-section node to the code-entity node (use the code entity's snake_case ID — e.g. `lib_file_memory_store`, `pipeline_context_pipeline`).

Hyperedges: only when 3+ nodes form a shared concept that pairwise edges miss (e.g. "Stage 5 memory preload flow" linking the pipeline + retriever + data struct + store impl). Max 3 per file.

confidence_score is REQUIRED on every edge:
- EXTRACTED → 1.0
- INFERRED → 0.6–0.9 based on evidence strength
- AMBIGUOUS → 0.1–0.3

Use relative paths from the repo root in source_file fields.

Output exactly this JSON shape (no other text, no fences):
{"nodes":[{"id":"snake_case_id","label":"Human Readable","file_type":"document|paper|image","source_file":"relative/path","source_location":null,"source_url":null,"captured_at":null,"author":null,"contributor":null}],"edges":[{"source":"node_id","target":"node_id","relation":"references|cites|conceptually_related_to|rationale_for|semantically_similar_to","confidence":"EXTRACTED|INFERRED|AMBIGUOUS","confidence_score":1.0,"source_file":"relative/path","source_location":null,"weight":1.0}],"hyperedges":[{"id":"snake_case_id","label":"Human Readable","nodes":["id1","id2","id3"],"relation":"participate_in|implement|form","confidence":"EXTRACTED|INFERRED","confidence_score":0.85,"source_file":"relative/path"}],"input_tokens":0,"output_tokens":0}
```

Substitute `<FILE_LIST>` with absolute paths from `graphify-out/.update_noncode.json` (absolute so the subagent's `Read` tool works regardless of its CWD).

When the subagent returns, save its raw JSON to `graphify-out/.update_semantic.json` via the `Write` tool, then commit to cache:

```bash
python3 - <<'PYEOF'
import json
from pathlib import Path
from graphify.cache import save_semantic_cache

result = json.loads(Path('graphify-out/.update_semantic.json').read_text())
n = save_semantic_cache(result.get('nodes', []), result.get('edges', []), result.get('hyperedges', []))
print(f'Cached semantic data for {n} files')
PYEOF
```

If the subagent returned invalid JSON: log a warning and continue to Step 3 anyway — the fast script will rebuild from whatever is still cached.

---

## Step 3 — First fast rebuild (produces auto-labeled graph)

```bash
./scripts/graphify-fast.sh "$TARGET"
```

This runs AST extraction on the full code corpus, merges in every still-valid cache entry (including whatever Step 2 just wrote), and produces `graph.json` / `graph.html` / `GRAPH_REPORT.md` / `manifest.json`.

Any community that doesn't already have a stored label gets a **path-based auto-label** from the fast script's deterministic labeler (e.g. `moa-brain/pipeline · CacheOptimizer`). These are the baseline the next step will refine.

---

## Step 4 — Inline community labeling (main assistant, no subagent)

**This is where the LLM earns its keep without the subagent overhead.** You (the main assistant) read `graph.json` directly, look at each community's member labels, and propose a plain-English 2–5 word name that reflects what the cluster actually represents.

Load the clustering data:

```bash
python3 - <<'PYEOF'
import json
from pathlib import Path
from collections import Counter

g = json.loads(Path('graphify-out/graph.json').read_text())
nodes = g.get('nodes', [])
mf = json.loads(Path('graphify-out/manifest.json').read_text())
current_labels = mf.get('community_labels', {})

# Group nodes by community
by_community = {}
for n in nodes:
    cid = n.get('community')
    if cid is None:
        continue
    by_community.setdefault(str(cid), []).append(n)

# Emit a compact summary: community id, current label, top 8 node labels, dominant source path
rows = []
for cid in sorted(by_community.keys(), key=lambda x: int(x)):
    members = by_community[cid]
    labels = [m.get('label', m.get('id', '?')) for m in members]
    sources = Counter(m.get('source_file', '') for m in members if m.get('source_file'))
    top_source = sources.most_common(1)[0][0] if sources else ''
    rows.append({
        'id': cid,
        'size': len(members),
        'current_label': current_labels.get(cid, ''),
        'top_nodes': labels[:8],
        'top_source': top_source,
        'all_sources': [s for s, _ in sources.most_common(3)],
    })

Path('graphify-out/.update_communities.json').write_text(json.dumps(rows, indent=2))
print(f'Wrote {len(rows)} communities to graphify-out/.update_communities.json')
PYEOF
```

Then use the `Read` tool to read `graphify-out/.update_communities.json`. For each community, propose a label following these rules:

- **2–5 words**, plain English. No snake_case, no dots.
- Capture the **concept**, not the directory. "Context Pipeline" > "moa-brain/pipeline".
- For communities where the current auto-label is already good (e.g., `moa-core · WorkingContext` → "Working Context"), just humanize it.
- For thin communities (1–2 nodes), use the top node's label as-is (e.g. "Compaction", "TUI Entry Point").
- **Preserve** any existing label that is **not** a path-style auto-label and **not** the `Community N` placeholder — the user may have set it manually. Detect path-style by looking for `·` or `/` in the stored label.

Do this purely in-context — **do not dispatch a subagent, do not use `Agent`**. Your reasoning budget here should be roughly 1–2 sentences per community internally; the output is just a single `{cid: label}` dict.

Write your label dict to `graphify-out/.update_labels.json` using the `Write` tool. The structure must be:

```json
{"0": "Core Domain Types", "1": "Anthropic Provider", "2": "Hands Router", ...}
```

Then merge those labels into `manifest.json`:

```bash
python3 - <<'PYEOF'
import json
from pathlib import Path

mf_path = Path('graphify-out/manifest.json')
mf = json.loads(mf_path.read_text())
new_labels = json.loads(Path('graphify-out/.update_labels.json').read_text())

# Overwrite: these are fresher + human-authored
mf['community_labels'] = {str(k): v for k, v in new_labels.items()}
mf_path.write_text(json.dumps(mf, indent=2))
print(f'Merged {len(new_labels)} labels into manifest.json')
PYEOF
```

---

## Step 5 — Second fast rebuild (re-export with the new labels)

```bash
./scripts/graphify-fast.sh "$TARGET"
```

Same script, same ~0.2s cost. The script reads the freshly-written labels from `manifest.json` via its `stored_labels` path and re-renders `graph.json`, `graph.html`, and `GRAPH_REPORT.md` so the new labels show up everywhere (node tooltips, community sidebar, report sections).

Expected output:
```
[graphify-fast] Graph: ... communities (0 auto-labeled, N preserved), ... hyperedges
```

The `0 auto-labeled, N preserved` line is the success signal: every community now has a human-readable label loaded from manifest, no fallback to `Community N`.

---

## Step 6 — Cleanup + report back to the user

```bash
rm -f \
  graphify-out/.update_changed.json \
  graphify-out/.update_noncode.json \
  graphify-out/.update_semantic.json \
  graphify-out/.update_communities.json \
  graphify-out/.update_labels.json \
  graphify-out/.graphify_python \
  graphify-out/.graphify_labels.json
```

> **Why explicit filenames?** zsh errors on non-matching globs (`.update_*.json` blows up when there's nothing to match). Listing every known scratch file makes this work on both bash and zsh with no shell-option gymnastics.

After this, `graphify-out/` must contain exactly:

```
cache/  cost.json  GRAPH_REPORT.md  graph.html  graph.json  manifest.json
```

If `ls -lA graphify-out/` shows anything else, something left scratch behind — investigate before declaring done.

Tell the user (keep it terse):
- Final node/edge/community/hyperedge counts from the second fast rebuild
- Which non-code files (if any) the subagent re-extracted
- How long the whole thing took

If `GRAPH_REPORT.md` gained interesting new "Surprising Connections" or "Suggested Questions", paste **only** those two sections. Do not dump the whole report.
