#!/usr/bin/env bash
#
# rag-scorecard.sh — consolidated RAG accuracy scorecard from existing report JSONs.
#
# Phase 3.3 of docs/engineering-discipline/plans/2026-07-11-rag-accuracy-plan.md.
# Reads whatever eval reports are already on disk (nothing is rebuilt here),
# prints one markdown scorecard table to stdout, appends a dated snapshot to
# docs/eval/scorecards/rag-scorecard.md, and prints regression hints against the
# previous snapshot section.
#
# Inputs are all optional; missing report families are skipped with a note.
#   - Memory hermetic: newest target/memory-eval/pr-marked*.json and natural*.json
#   - Memory live:     newest target/memory-eval/live-*.json
#   - External lanes:  newest .moa/wixqa/reports/{multihoprag,synthetic,financebench}-*.json
#
# CI wiring is deliberately OUT OF SCOPE: an operator runs this manually or via
# cron. It shells out to python3 only (no jq dependency) and touches no crates.
#
# Usage: scripts/rag-scorecard.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCORECARD_FILE="${REPO_ROOT}/docs/eval/scorecards/rag-scorecard.md"

mkdir -p "$(dirname "${SCORECARD_FILE}")"

python3 - "${REPO_ROOT}" "${SCORECARD_FILE}" <<'PY'
import glob
import json
import os
import sys
from datetime import datetime, timezone

repo_root, scorecard_file = sys.argv[1], sys.argv[2]

# Columns rendered in the scorecard table, in order. The first column is a
# STABLE source key (independent of the exact report filename) so a later run
# can line rows up against an earlier snapshot for delta hints.
COLUMNS = [
    "Source",
    "Recall",
    "Hit@k",
    "nDCG",
    "MRR",
    "Prec@4",
    "AbstainFP",
    "p95 ms",
    "Report",
]
# Numeric metric columns eligible for regression deltas.
METRIC_COLUMNS = ["Recall", "Hit@k", "nDCG", "MRR", "Prec@4", "AbstainFP", "p95 ms"]

notes = []


def newest(pattern):
    matches = glob.glob(pattern)
    if not matches:
        return None
    return max(matches, key=os.path.getmtime)


def load(path):
    with open(path) as handle:
        return json.load(handle)


def fmt_score(value):
    return "—" if value is None else f"{value:.3f}"


def fmt_ms(value):
    return "—" if value is None else f"{int(round(value))}"


def memory_metric(metrics, name):
    """Memory-eval metrics are {numerator, denominator, value} objects."""
    entry = metrics.get(name)
    if isinstance(entry, dict):
        return entry.get("value")
    return entry


def memory_row(source_key, path):
    report = load(path)
    metrics = report.get("metrics", {})
    return {
        "Source": source_key,
        "Recall": fmt_score(memory_metric(metrics, "recall_at_4")),
        "Hit@k": "—",
        "nDCG": fmt_score(memory_metric(metrics, "ndcg_at_4")),
        "MRR": fmt_score(memory_metric(metrics, "mrr")),
        "Prec@4": fmt_score(memory_metric(metrics, "precision_at_4")),
        "AbstainFP": fmt_score(memory_metric(metrics, "abstention_false_positive_rate")),
        "p95 ms": fmt_ms(metrics.get("p95_retrieval_latency_ms")),
        "Report": os.path.basename(path),
    }


def external_row(source_key, path):
    report = load(path)
    metrics = report.get("metrics", {})
    p95 = None
    latency = report.get("latency", {})
    if isinstance(latency, dict):
        retrieval = latency.get("retrieval", {})
        if isinstance(retrieval, dict):
            p95 = retrieval.get("p95_ms")
    return {
        "Source": source_key,
        "Recall": fmt_score(metrics.get("recall_at_k")),
        "Hit@k": fmt_score(metrics.get("hit_at_k")),
        "nDCG": fmt_score(metrics.get("ndcg_at_k")),
        "MRR": fmt_score(metrics.get("mrr")),
        "Prec@4": "—",
        "AbstainFP": "—",
        "p95 ms": fmt_ms(p95),
        "Report": os.path.basename(path),
    }


rows = []

# Memory hermetic lanes: newest pr-marked* and natural* (natural* must not match
# the live-natural-* reports, so it is anchored at the basename start).
mem_dir = os.path.join(repo_root, "target", "memory-eval")
for source_key, pattern in [
    ("memory-hermetic/pr-marked", os.path.join(mem_dir, "pr-marked*.json")),
    ("memory-hermetic/natural", os.path.join(mem_dir, "natural*.json")),
    ("memory-live", os.path.join(mem_dir, "live-*.json")),
]:
    path = newest(pattern)
    if path is None:
        notes.append(f"no report for {source_key} (pattern: {os.path.relpath(pattern, repo_root)})")
        continue
    rows.append(memory_row(source_key, path))

# External retrieval lanes.
wixqa_dir = os.path.join(repo_root, ".moa", "wixqa", "reports")
for dataset in ["multihoprag", "synthetic", "financebench"]:
    pattern = os.path.join(wixqa_dir, f"{dataset}-*.json")
    path = newest(pattern)
    source_key = f"external/{dataset}"
    if path is None:
        notes.append(f"no report for {source_key} (pattern: {os.path.relpath(pattern, repo_root)})")
        continue
    rows.append(external_row(source_key, path))


def render_table(table_rows):
    lines = ["| " + " | ".join(COLUMNS) + " |", "|" + "|".join(["---"] * len(COLUMNS)) + "|"]
    for row in table_rows:
        lines.append("| " + " | ".join(row[column] for column in COLUMNS) + " |")
    return "\n".join(lines)


def parse_snapshot_sections(text):
    """Splits an existing scorecard into (header, body-text) sections by '## '."""
    sections = []
    current_header = None
    current_lines = []
    for line in text.splitlines():
        if line.startswith("## "):
            if current_header is not None:
                sections.append((current_header, "\n".join(current_lines)))
            current_header = line[3:].strip()
            current_lines = []
        elif current_header is not None:
            current_lines.append(line)
    if current_header is not None:
        sections.append((current_header, "\n".join(current_lines)))
    return sections


def parse_table_metrics(body):
    """Maps Source -> {metric: float} from a rendered snapshot table body."""
    result = {}
    for line in body.splitlines():
        stripped = line.strip()
        if not stripped.startswith("|"):
            continue
        cells = [cell.strip() for cell in stripped.strip("|").split("|")]
        if len(cells) != len(COLUMNS):
            continue
        if cells[0] in ("Source", "---") or set(cells[0]) == {"-"}:
            continue
        record = dict(zip(COLUMNS, cells))
        metrics = {}
        for column in METRIC_COLUMNS:
            raw = record.get(column, "—")
            try:
                metrics[column] = float(raw)
            except (TypeError, ValueError):
                metrics[column] = None
        result[record["Source"]] = metrics
    return result


today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
generated_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

existing_text = ""
if os.path.exists(scorecard_file):
    with open(scorecard_file) as handle:
        existing_text = handle.read()

sections = parse_snapshot_sections(existing_text)
# Drop any trailing section already dated today so same-day reruns replace rather
# than duplicate; the "previous" section for deltas is the last one before today.
prior_sections = [section for section in sections if section[0] != today]
previous_metrics = parse_table_metrics(prior_sections[-1][1]) if prior_sections else {}

table = render_table(rows)

# Regression hints against the previous snapshot section (informational only,
# no gating). The arrow reflects the raw direction of change (⬆ value rose, ⬇
# value fell, ≈ unchanged); which direction is "good" depends on the metric —
# up is better for recall/nDCG/MRR/precision, down is better for latency and the
# abstention false-positive rate.
hint_lines = []
for row in rows:
    source = row["Source"]
    baseline = previous_metrics.get(source)
    if not baseline:
        hint_lines.append(f"  {source}: no prior snapshot to compare")
        continue
    parts = []
    for column in METRIC_COLUMNS:
        try:
            current = float(row[column])
        except ValueError:
            continue
        before = baseline.get(column)
        if before is None:
            continue
        delta = current - before
        if abs(delta) < 5e-4:
            marker = "≈"
        elif delta > 0:
            marker = "⬆"
        else:
            marker = "⬇"
        parts.append(f"{column} {marker}{delta:+.3f}")
    hint_lines.append(f"  {source}: " + ("; ".join(parts) if parts else "no comparable metrics"))

# Compose the dated snapshot section.
snapshot_lines = [f"## {today}", "", f"_generated {generated_at}_", ""]
snapshot_lines.append(table)
if notes:
    snapshot_lines.append("")
    for note in notes:
        snapshot_lines.append(f"> skipped: {note}")
snapshot = "\n".join(snapshot_lines)

# Rebuild the file: fixed title + all non-today sections + the new section.
header = "# RAG accuracy scorecard\n\nConsolidated retrieval metrics across memory and external lanes. Generated by `scripts/rag-scorecard.sh` (phase 3.3). Newest section is most recent.\n"
body_sections = ["## " + h + "\n" + b.rstrip("\n") for h, b in prior_sections]
body_sections.append(snapshot)
new_text = header + "\n" + "\n\n".join(body_sections) + "\n"

with open(scorecard_file, "w") as handle:
    handle.write(new_text)

# stdout: the table, skip notes, and regression hints.
print(f"RAG scorecard — {generated_at}")
print()
print(table)
if notes:
    print()
    for note in notes:
        print(f"skipped: {note}")
print()
print("regression vs previous snapshot"
      + (f" ({prior_sections[-1][0]})" if prior_sections else " (none on file)")
      + ":")
for line in hint_lines:
    print(line)
print()
print(f"snapshot appended to {os.path.relpath(scorecard_file, repo_root)}")
PY
