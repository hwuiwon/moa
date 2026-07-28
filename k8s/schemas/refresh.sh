#!/usr/bin/env bash
# Regenerates the vendored kubeconform JSON schemas from pinned upstream CRDs.
#
# Run this ONLY when deliberately bumping a pin in sources.json. CI and
# validate-observability.sh read the committed .json files and never call this,
# so validation stays offline and reproducible.
#
# Every download is checksum-verified before it is used. A mismatch is fatal: a
# schema fetched from a moved tag would quietly widen or narrow what manifest
# validation accepts, which is worse than not validating at all because it looks
# like it is working.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf -- "${WORK_DIR}"' EXIT

command -v python3 >/dev/null || { echo "refresh: python3 is required" >&2; exit 1; }
python3 -c 'import yaml' 2>/dev/null || {
  echo "refresh: PyYAML is required (pip install pyyaml)" >&2
  exit 1
}

count="$(python3 -c 'import json,sys;print(len(json.load(open(sys.argv[1]))["crds"]))' \
  "${SCRIPT_DIR}/sources.json")"

for index in $(seq 0 $((count - 1))); do
  read -r name url expected <<<"$(
    python3 -c '
import json, sys
crd = json.load(open(sys.argv[1]))["crds"][int(sys.argv[2])]
print(crd["name"], crd["url"], crd["sha256"])
' "${SCRIPT_DIR}/sources.json" "${index}"
  )"
  target="${WORK_DIR}/${name}.yaml"
  echo "Fetching ${name} from ${url}"
  curl -sSL --fail --max-time 60 -o "${target}" "${url}"
  actual="$(shasum -a 256 "${target}" | cut -d' ' -f1)"
  if [[ "${actual}" != "${expected}" ]]; then
    echo "refresh: checksum mismatch for ${name}" >&2
    echo "  expected ${expected}" >&2
    echo "  actual   ${actual}" >&2
    exit 1
  fi
done

python3 "${SCRIPT_DIR}/crd_to_jsonschema.py" \
  --sources "${SCRIPT_DIR}/sources.json" \
  --downloads "${WORK_DIR}" \
  --out "${SCRIPT_DIR}"

echo "Schemas regenerated under ${SCRIPT_DIR}"
