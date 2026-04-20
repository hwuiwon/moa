---
description: Smart graphify update — deterministic rebuild plus inline community labeling
argument-hint: "[path]"
allowed-tools: Bash, Read, Write
---

# /graphify-update

Smart incremental refresh of `graphify-out/`. Uses the LLM only for one thing AST cannot do: **community labeling** — done inline by the main assistant after the fast script has already clustered the graph. No subagent, no file reads on non-code, just reasoning over `graph.json`'s community structure. Typically ~5 seconds.

Everything else (AST extraction, clustering, rendering) is deterministic Python via `scripts/graphify-fast.sh`.

Non-code files (markdown, docs) are intentionally **not** re-extracted by an LLM subagent — deterministic AST + clustering is enough for this repo, and the subagent overhead isn't worth it.

Expected cost: **~5–10s total** regardless of change type. Old `/graphify . --update` took ~130s.

Path defaults to `.` (the repo root). Pass an explicit path to scope the run: `/graphify-update moa-brain`.

---

## Step 0 — Clean up scratch state from any prior interrupted run

```bash
rm -f \
  graphify-out/.update_changed.json \
  graphify-out/.update_communities.json \
  graphify-out/.update_labels.json \
  graphify-out/.graphify_python \
  graphify-out/.graphify_labels.json
```

> **Why explicit filenames?** zsh errors on non-matching globs (`.update_*.json` blows up when there's nothing to match). Listing every known scratch file makes this work on both bash and zsh with no shell-option gymnastics.

Idempotent. Guards against a prior run that crashed before its EXIT trap fired.

---

## Step 1 — Detect what changed (informational only)

```bash
TARGET="${1:-.}"
python3 - "$TARGET" <<'PYEOF'
import sys
from pathlib import Path
from graphify.detect import detect_incremental

target = Path(sys.argv[1])
detect = detect_incremental(target)

unchanged = {f for files in detect.get('unchanged_files', {}).values() for f in files}
all_files = [f for files in detect.get('files', {}).values() for f in files]
changed   = [f for f in all_files if f not in unchanged]

print(f'CHANGED={len(changed)}')
for f in changed[:10]:
    print(f'  changed: {f}')
PYEOF
```

This is informational. The fast rebuild in Step 3 handles changed files through the cache; we don't branch on file type.

---

## Step 1b — Prune orphaned cache entries + stub binary files

Run immediately after Step 1 every time (regardless of whether non-code changed). This keeps the cache lean and eliminates "N files have no/stale cache" noise from binary assets.

```bash
python3 - <<'PYEOF'
import json
from pathlib import Path
from graphify.detect import detect_incremental
from graphify.cache import file_hash, cache_dir, load_cached, save_cached

detect = detect_incremental(Path('.'))
all_files = [f for files in detect.get('files', {}).values() for f in files]

# Compute valid hashes for all current files
valid_hashes = set()
for f in all_files:
    p = Path(f)
    if p.exists():
        try:
            valid_hashes.add(file_hash(p))
        except OSError:
            pass

# Prune orphaned entries (old hash versions of changed/deleted files)
cdir = cache_dir()
pruned = 0
for c in cdir.glob('*.json'):
    if c.stem not in valid_hashes:
        c.unlink()
        pruned += 1

# Stub binary/image files with empty cache so fast script stops counting them
binary_exts = {'.png', '.svg', '.ico', '.jpg', '.jpeg', '.gif', '.webp', '.pdf', '.woff', '.woff2', '.ttf', '.eot'}
stubbed = 0
for f in all_files:
    if Path(f).suffix.lower() in binary_exts:
        p = Path(f)
        if p.exists() and load_cached(p) is None:
            save_cached(p, {"nodes": [], "edges": [], "hyperedges": []})
            stubbed += 1

print(f'Cache: pruned {pruned} orphaned entries, stubbed {stubbed} binary files, {len(list(cdir.glob("*.json")))} entries remain')
PYEOF
```

> **Why here?** `file_hash` needs the current file contents, so pruning must happen after we have the live file list from `detect_incremental`. Doing it before the fast rebuild means Step 3 starts with a clean cache and the "N files have no/stale cache" count is accurate.

---

## Step 2 — First fast rebuild (produces auto-labeled graph)

```bash
./scripts/graphify-fast.sh "$TARGET"
```

This runs AST extraction on the full code corpus, merges in every still-valid cache entry, and produces `graph.json` / `graph.html` / `GRAPH_REPORT.md` / `manifest.json`.

Any community that doesn't already have a stored label gets a **path-based auto-label** from the fast script's deterministic labeler (e.g. `moa-brain/pipeline · CacheOptimizer`). These are the baseline the next step will refine.

The script may report "N files have no/stale cache" for non-code files that changed — that's expected and harmless; we don't refresh them.

---

## Step 3 — Inline community labeling (main assistant, no subagent)

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

## Step 4 — Second fast rebuild (re-export with the new labels)

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

## Step 5 — Cleanup + report back to the user

```bash
rm -f \
  graphify-out/.update_changed.json \
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
- How long the whole thing took

If `GRAPH_REPORT.md` gained interesting new "Surprising Connections" or "Suggested Questions", paste **only** those two sections. Do not dump the whole report.
