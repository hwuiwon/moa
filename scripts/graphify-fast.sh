#!/usr/bin/env bash
# graphify-fast: deterministic AST + cached-semantic rebuild of graphify-out/
#
# Skips LLM invocation. Pulls in any still-valid semantic cache entries
# (nodes, edges, hyperedges) from prior LLM runs and merges them with a
# fresh AST extraction. Runs in well under a second on typical edits.
#
# Persistent state lives in exactly these files under graphify-out/:
#   graph.json  graph.html  GRAPH_REPORT.md  manifest.json  cost.json  cache/
# No dotfiles, no scratch files. Anything else is a bug — trap cleans it up.
#
# Use /graphify-update (slash command) when non-code files changed and you
# want the LLM to refresh rationale/semantic/hyperedge data.
#
# Usage: scripts/graphify-fast.sh [path]    (default: .)

set -euo pipefail

TARGET="${1:-.}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

mkdir -p graphify-out

# Resolve Python interpreter inline — no cache file.
PY="$(command -v python3 || true)"
if [ -z "$PY" ]; then
    echo "[graphify-fast] ERROR: python3 not found on PATH" >&2
    exit 1
fi

# Clean up any stale dotfile state from previous versions of this script
# and any scratch files left behind by interrupted slash-command runs.
cleanup_scratch() {
    rm -f \
        graphify-out/.graphify_python \
        graphify-out/.graphify_labels.json \
        graphify-out/.graphify_detect.json \
        graphify-out/.graphify_extract.json \
        graphify-out/.graphify_ast.json \
        graphify-out/.graphify_semantic.json \
        graphify-out/.graphify_semantic_new.json \
        graphify-out/.graphify_cached.json \
        graphify-out/.graphify_uncached.txt \
        graphify-out/.graphify_analysis.json \
        graphify-out/.graphify_incremental.json \
        graphify-out/.graphify_old.json \
        graphify-out/.graphify_changed.json \
        graphify-out/.graphify_code_only.txt \
        graphify-out/.needs_update \
        graphify-out/.update_changed.json \
        graphify-out/.update_noncode.json \
        graphify-out/.update_semantic.json \
        graphify-out/.update_communities.json \
        graphify-out/.update_labels.json
}
trap cleanup_scratch EXIT
cleanup_scratch  # also wipe on entry, in case a previous run crashed

"$PY" - "$TARGET" <<'PYEOF'
import json, sys, time
from pathlib import Path
from datetime import datetime, timezone

from graphify.detect import detect_incremental, save_manifest
from graphify.extract import extract
from graphify.cache import check_semantic_cache
from graphify.build import build_from_json
from graphify.cluster import cluster, score_all
from graphify.analyze import god_nodes, surprising_connections, suggest_questions
from graphify.report import generate
from graphify.export import to_json, to_html

target = Path(sys.argv[1])
t0 = time.time()

# 1. Detect corpus + which files changed since last manifest
detect = detect_incremental(target)
all_files = [f for files in detect.get('files', {}).values() for f in files]
unchanged_set = {f for files in detect.get('unchanged_files', {}).values() for f in files}
changed = [f for f in all_files if f not in unchanged_set]

code_exts = {'.rs','.py','.ts','.js','.go','.java','.cpp','.c','.rb','.swift','.kt','.cs','.scala','.php','.cc','.cxx','.hpp','.h','.kts','.lua'}
all_code         = [Path(f) for f in all_files if Path(f).suffix.lower() in code_exts]
non_code_changed = [f for f in changed if Path(f).suffix.lower() not in code_exts]

# 2. AST extraction on the FULL code corpus. Fast (~10ms/file) and avoids
#    stale-merge bugs that come from incremental node pruning.
if all_code:
    ast_result = extract(all_code)
    print(f'[graphify-fast] AST: {len(ast_result["nodes"])} nodes, {len(ast_result["edges"])} edges from {len(all_code)} files')
else:
    ast_result = {'nodes': [], 'edges': [], 'input_tokens': 0, 'output_tokens': 0}

# 3. Pull every still-valid semantic cache entry (LLM-derived nodes/edges/hyperedges).
#    Cache is keyed by SHA256(file content) — stale entries silently drop out.
cached_nodes, cached_edges, cached_hyperedges, uncached = check_semantic_cache(all_files)
print(f'[graphify-fast] Semantic cache: {len(all_files)-len(uncached)} hits, {len(uncached)} files have no/stale cache')

# 4. Merge AST + cached semantic, dedup nodes by id (AST wins on conflicts)
seen = set()
merged_nodes = []
for n in ast_result['nodes']:
    if n['id'] not in seen:
        seen.add(n['id']); merged_nodes.append(n)
for n in cached_nodes:
    if n['id'] not in seen:
        seen.add(n['id']); merged_nodes.append(n)

extraction = {
    'nodes': merged_nodes,
    'edges': ast_result['edges'] + cached_edges,
    'hyperedges': cached_hyperedges,
    'input_tokens': 0, 'output_tokens': 0,
}

# 5. Build, cluster, analyze
G = build_from_json(extraction)
communities = cluster(G)
cohesion    = score_all(G, communities)
gods        = god_nodes(G)
surprises   = surprising_connections(G, communities)

# 6. Compute community labels. Strategy:
#    (a) Load existing NON-placeholder labels from manifest (preserve LLM-authored names).
#    (b) For everything else, auto-derive from source paths + most-central node.
#    (c) Stale "Community N" placeholder labels from older manifests are rejected on load
#        and rewritten on save, breaking any lock-in cycle.
import re
from os.path import commonpath, dirname
from collections import Counter

_PLACEHOLDER_LABEL = re.compile(r'^Community \d+$')

def auto_label(graph, member_ids):
    """Deterministic label from community content. Returns None if no source info."""
    files = Counter()
    for nid in member_ids:
        sf = graph.nodes[nid].get('source_file', '')
        if sf:
            files[sf] += 1
    if not files:
        return None

    unique = list(files.keys())
    try:
        prefix = commonpath(unique) if len(unique) > 1 else dirname(unique[0])
    except ValueError:
        prefix = ''
    if not prefix or prefix == '.':
        crates = Counter()
        for f, c in files.items():
            crates[f.split('/')[0]] += c
        prefix = crates.most_common(1)[0][0]

    # Readability: drop 'src' segments (moa-brain/src/pipeline -> moa-brain/pipeline)
    parts = [p for p in prefix.split('/') if p and p != 'src']
    cleaned = '/'.join(parts) if parts else prefix

    # Qualifier: most-connected node label — disambiguates communities in the same dir
    if len(member_ids) >= 3:
        best = max(member_ids, key=lambda n: graph.degree(n))
        top = graph.nodes[best].get('label', best).split('(')[0].strip(' .')
        if top and len(top) < 35 and top.lower() not in cleaned.lower():
            return f'{cleaned} · {top}'
    return cleaned

manifest_path = Path('graphify-out/manifest.json')
# Load stored labels WITH their content signatures. Cluster IDs from Louvain/
# Leiden are not stable across runs — the same cluster can be renumbered when
# the graph changes. So we match labels by *membership overlap*, not by raw ID.
# Legacy manifests (dict labels, no signatures) are dropped because their
# label→cluster mapping is known-unreliable after any graph edit.
stored_label_entries = []  # list of (label, frozenset(member_ids))
if manifest_path.exists():
    try:
        mf_prev = json.loads(manifest_path.read_text())
        raw_labels = mf_prev.get('community_labels', {})
        raw_sigs   = mf_prev.get('community_signatures', {})
        if isinstance(raw_labels, dict) and isinstance(raw_sigs, dict) and raw_sigs:
            for k, v in raw_labels.items():
                if not v or _PLACEHOLDER_LABEL.match(v):
                    continue
                sig = raw_sigs.get(str(k)) or raw_sigs.get(k)
                if sig:
                    stored_label_entries.append((v, frozenset(sig)))
    except (json.JSONDecodeError, ValueError, TypeError):
        pass

# Match stored labels to new communities by greedy max-overlap assignment.
# Each stored label can attach to at most one new community, and each new
# community gets at most one carried-over label. A minimum coverage threshold
# (half of the old cluster must still live in the new one) prevents a handful
# of leftover nodes from stealing an unrelated label.
new_community_sets = {cid: set(members) for cid, members in communities.items()}
candidates = []  # (intersection_size, coverage, entry_idx, cid)
for idx, (_label, old_set) in enumerate(stored_label_entries):
    if not old_set:
        continue
    for cid, new_set in new_community_sets.items():
        inter = len(old_set & new_set)
        if inter == 0:
            continue
        coverage = inter / len(old_set)
        candidates.append((inter, coverage, idx, cid))
candidates.sort(reverse=True)

carryover = {}          # cid -> label
used_entries = set()
MIN_COVERAGE = 0.5
for inter, coverage, idx, cid in candidates:
    if idx in used_entries or cid in carryover:
        continue
    if coverage < MIN_COVERAGE:
        continue
    carryover[cid] = stored_label_entries[idx][0]
    used_entries.add(idx)

labels = {}
auto_count = 0
preserved_count = 0
for cid, members in communities.items():
    if cid in carryover:
        labels[cid] = carryover[cid]
        preserved_count += 1
    else:
        derived = auto_label(G, members)
        labels[cid] = derived or f'Community {cid}'
        if derived:
            auto_count += 1

questions = suggest_questions(G, communities, labels)

# 7. Write outputs
report = generate(G, communities, cohesion, labels, gods, surprises, detect,
                  {'input': 0, 'output': 0}, str(target), suggested_questions=questions)
Path('graphify-out/GRAPH_REPORT.md').write_text(report)
to_json(G, communities, 'graphify-out/graph.json')
if G.number_of_nodes() <= 5000:
    to_html(G, communities, 'graphify-out/graph.html', community_labels=labels)

# 8. Persist manifest with labels + signatures (one state file, no dotfiles).
#    community_signatures records the exact member set of each cluster so the
#    next run can match labels by content overlap even if Louvain renumbers
#    the communities. Keyed by current cid so /graphify-update's inline
#    labeling step (which writes into community_labels[cid]) still aligns.
save_manifest(detect['files'])
mf = json.loads(manifest_path.read_text())  # re-read after save_manifest wrote it
mf['community_labels'] = {str(k): v for k, v in labels.items()}
mf['community_signatures'] = {
    str(cid): sorted(members) for cid, members in communities.items()
}
manifest_path.write_text(json.dumps(mf, indent=2))

# 9. Append to cost tracker
cost_path = Path('graphify-out/cost.json')
cost = json.loads(cost_path.read_text()) if cost_path.exists() else {'runs': [], 'total_input_tokens': 0, 'total_output_tokens': 0}
cost['runs'].append({'date': datetime.now(timezone.utc).isoformat(), 'input_tokens': 0, 'output_tokens': 0, 'files': detect.get('total_files', 0), 'mode': 'fast'})
cost_path.write_text(json.dumps(cost, indent=2))

# 10. Summary
print(f'[graphify-fast] Graph: {G.number_of_nodes()} nodes, {G.number_of_edges()} edges, {len(communities)} communities ({auto_count} auto-labeled, {preserved_count} preserved), {len(cached_hyperedges)} hyperedges')
if non_code_changed:
    print(f'[graphify-fast] {len(non_code_changed)} non-code file(s) changed since last LLM run:')
    for f in non_code_changed[:5]:
        print(f'    {f}')
    print(f'[graphify-fast] Run /graphify-update to refresh semantic edges for those files.')
print(f'[graphify-fast] Done in {time.time()-t0:.2f}s')
PYEOF
