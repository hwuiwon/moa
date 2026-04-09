---
description: Smart graphify update — LLM only for changed non-code files, then deterministic AST rebuild
argument-hint: "[path]"
allowed-tools: Bash, Read, Write, Agent
---

# /graphify-update

Smart incremental refresh of `graphify-out/`. Runs the LLM **only when it can add something AST cannot** (rationale edges, hyperedges, semantic similarity from docs/markdown), then hands off to `scripts/graphify-fast.sh` for the deterministic rebuild.

Decision rule:
- **Code-only change set** → no LLM. Just run `scripts/graphify-fast.sh`. Cached hyperedges/semantic edges from prior runs survive automatically because the cache is content-hashed.
- **Any non-code change** (`.md`, `.txt`, `.pdf`, images, etc.) → dispatch **one** semantic-extraction subagent on just those files, save its result to `graphify-out/cache/`, then run `scripts/graphify-fast.sh`.

Path defaults to `.` (the repo root). Pass an explicit path to scope the run: `/graphify-update moa-brain`.

---

## Step 0 — Clean up scratch state from any prior interrupted run

```bash
rm -f graphify-out/.update_*.json graphify-out/.graphify_*.json graphify-out/.graphify_python graphify-out/.graphify_labels.json
```

This is idempotent. Safe to run even on a fresh checkout. The fast script's EXIT trap handles the same cleanup, but doing it here guards against a prior run that crashed before its trap fired.

## Step 1 — Detect what changed

Run this from the repo root. It writes a single scratch file listing the non-code changes, then Step 2 reads that file. Step 4 deletes it.

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

Read `graphify-out/.update_noncode.json`. If it is `[]` (empty list), **skip Step 2 entirely** and jump to Step 3 — no LLM needed.

---

## Step 2 — Single semantic-extraction subagent (only if non-code files changed)

Dispatch **exactly one** Agent call. Pass the file list inline (not via a temp file — keeps the agent self-contained). Use `subagent_type: general-purpose`. The agent must:

1. Read each file with the Read tool.
2. Extract nodes/edges/hyperedges in the schema below.
3. Output **only** the JSON object — no prose, no markdown fences.

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

Use relative paths from the repo root in source_file fields. Use the Read tool for each file.

Output exactly this JSON shape (no other text, no fences):
{"nodes":[{"id":"snake_case_id","label":"Human Readable","file_type":"document|paper|image","source_file":"relative/path","source_location":null,"source_url":null,"captured_at":null,"author":null,"contributor":null}],"edges":[{"source":"node_id","target":"node_id","relation":"references|cites|conceptually_related_to|rationale_for|semantically_similar_to","confidence":"EXTRACTED|INFERRED|AMBIGUOUS","confidence_score":1.0,"source_file":"relative/path","source_location":null,"weight":1.0}],"hyperedges":[{"id":"snake_case_id","label":"Human Readable","nodes":["id1","id2","id3"],"relation":"participate_in|implement|form","confidence":"EXTRACTED|INFERRED","confidence_score":0.85,"source_file":"relative/path"}],"input_tokens":0,"output_tokens":0}
```

Substitute `<FILE_LIST>` with the actual newline-joined paths from `graphify-out/.update_noncode.json` (use absolute paths so the subagent's Read tool calls work).

When the subagent returns, save its raw JSON to `graphify-out/.update_semantic.json` (use the Write tool). Then commit to cache:

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

If the subagent returned invalid JSON, log a warning and continue to Step 3 anyway — the fast script will still rebuild from whatever is cached.

---

## Step 3 — Deterministic rebuild via the fast script

```bash
./scripts/graphify-fast.sh "$TARGET"
```

This step always runs, regardless of whether Step 2 happened. It:
- runs AST extraction on the full code corpus
- merges in every still-valid cache entry (including whatever Step 2 just wrote)
- rebuilds `graph.json`, `graph.html`, `GRAPH_REPORT.md`, `manifest.json`
- preserves community labels from `.graphify_labels.json`

---

## Step 4 — Cleanup + report back to the user

```bash
rm -f graphify-out/.update_*.json graphify-out/.graphify_*.json graphify-out/.graphify_python graphify-out/.graphify_labels.json
```

After this runs, `graphify-out/` should contain exactly these entries and nothing else:

```
cache/  cost.json  GRAPH_REPORT.md  graph.html  graph.json  manifest.json
```

If `ls -la graphify-out/` shows anything starting with `.` other than `.` / `..`, something left scratch behind — investigate before declaring the command done.

Show the user the summary line printed by the fast script (node/edge/community/hyperedge counts) and tell them where the outputs live. If Step 2 ran, mention which non-code files were re-extracted by the LLM. Keep it short — they don't need a full report dump unless they ask.

If `graphify-out/GRAPH_REPORT.md` has new "Surprising Connections" or "Suggested Questions" worth surfacing, paste **only** those two sections.
