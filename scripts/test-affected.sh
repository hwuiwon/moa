#!/usr/bin/env bash
# Runs only the tests that can be affected by the current change set.
#
# Computes changed files (vs. the merge base with main, plus uncommitted and
# untracked files), maps them to workspace crates via `cargo metadata` (no
# compilation), expands to reverse dependents, and runs
# `cargo nextest run -p <crate>...` for that closure. This gives Bazel-style
# "skip unaffected tests" behavior at crate granularity without leaving cargo.
#
# Usage: scripts/test-affected.sh [--base <rev>] [--profile <nextest-profile>]
#                                 [--dry-run] [-- <extra nextest args>]
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

BASE=""
PROFILE="fast-pr"
DRY_RUN=0
EXTRA_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)
      BASE="$2"
      shift
      ;;
    --profile)
      PROFILE="$2"
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      ;;
    --)
      shift
      EXTRA_ARGS=("$@")
      break
      ;;
    -h|--help)
      sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
  shift
done

if [[ -z "${BASE}" ]]; then
  for candidate in origin/main main; do
    if git rev-parse --verify --quiet "${candidate}" >/dev/null; then
      BASE="$(git merge-base HEAD "${candidate}")"
      break
    fi
  done
fi
if [[ -z "${BASE}" ]]; then
  echo "could not resolve a base revision; pass --base <rev>" >&2
  exit 2
fi

CHANGED_FILES="$(
  {
    git diff --name-only "${BASE}"
    git diff --name-only HEAD
    git ls-files --others --exclude-standard
  } | sort -u
)"

CRATES="$(python3 - "${CHANGED_FILES}" <<'PY'
import json
import os
import subprocess
import sys

changed = [line for line in sys.argv[1].splitlines() if line]

# Paths that invalidate the whole build graph rather than one crate.
GLOBAL_PREFIXES = (
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/",
    ".config/nextest.toml",
    "rust-toolchain",
)

metadata = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
        check=True,
        capture_output=True,
    ).stdout
)
root = os.getcwd()
crate_dirs = {}
for package in metadata["packages"]:
    crate_dir = os.path.relpath(os.path.dirname(package["manifest_path"]), root)
    crate_dirs[crate_dir + os.sep] = package["name"]

member_names = set(crate_dirs.values())

changed_crates = set()
for path in changed:
    if path.startswith(GLOBAL_PREFIXES):
        print("ALL")
        sys.exit(0)
    # Longest prefix wins: nested crates (crates/moa-eval/core under
    # crates/moa-eval) must map to the inner crate.
    matches = [d for d in crate_dirs if path.startswith(d)]
    if matches:
        changed_crates.add(crate_dirs[max(matches, key=len)])

if not changed_crates:
    sys.exit(0)

# Reverse edges among workspace members. Normal and build deps propagate
# transitively (they change the dependent's compiled lib); dev-deps affect
# only the dependent's own tests, so they are applied as a final single hop.
reverse_lib = {}
dev_dependents = {}
for package in metadata["packages"]:
    for dep in package["dependencies"]:
        if dep["name"] not in member_names:
            continue
        if dep["kind"] == "dev":
            dev_dependents.setdefault(dep["name"], set()).add(package["name"])
        else:
            reverse_lib.setdefault(dep["name"], set()).add(package["name"])

affected = set(changed_crates)
frontier = list(changed_crates)
while frontier:
    crate = frontier.pop()
    for dependent in reverse_lib.get(crate, ()):
        if dependent not in affected:
            affected.add(dependent)
            frontier.append(dependent)
for crate in list(affected):
    affected.update(dev_dependents.get(crate, ()))

print(" ".join(sorted(affected)))
PY
)"

if [[ -z "${CRATES}" ]]; then
  echo "no workspace crates affected; nothing to test"
  exit 0
fi

CMD=(cargo nextest run --locked --profile "${PROFILE}")
if [[ "${CRATES}" != "ALL" ]]; then
  for crate in ${CRATES}; do
    CMD+=(-p "${crate}")
  done
else
  echo ">> workspace-level file changed; running the full ${PROFILE} lane"
fi
if [[ ${#EXTRA_ARGS[@]} -gt 0 ]]; then
  CMD+=("${EXTRA_ARGS[@]}")
fi

if [[ "${CRATES}" != "ALL" ]]; then
  echo ">> affected crates: ${CRATES}"
fi
echo ">> ${CMD[*]}"
if [[ "${DRY_RUN}" -eq 1 ]]; then
  exit 0
fi
exec "${CMD[@]}"
