#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
ORCHESTRATOR_NAME="ghcr.io/hwuiwon/moa-orchestrator"
EDGE_NAME="ghcr.io/hwuiwon/moa-edge"
DIGEST_PATTERN='^([a-z0-9]+([._-][a-z0-9]+)*)(:[0-9]+)?(/[a-z0-9]+([._-][a-z0-9]+)*)+@sha256:[a-f0-9]{64}$'

die() {
  echo "Production render failed: $*" >&2
  exit 1
}

require_value() {
  local name="$1"
  [[ -n "${!name:-}" ]] || die "${name} is required"
}

require_digest() {
  local name="$1"
  require_value "${name}"
  [[ "${!name}" =~ ${DIGEST_PATTERN} ]] \
    || die "${name} must be an exact lowercase name@sha256:<64 hex> reference"
}

[[ "$#" -eq 1 ]] || die "usage: $0 OUTPUT_DIRECTORY"
require_digest MOA_ORCHESTRATOR_IMAGE
require_digest MOA_EDGE_IMAGE
require_value RESTATE_SNAPSHOT_BUCKET
require_value RESTATE_SNAPSHOT_PREFIX
require_value RESTATE_SNAPSHOT_GSA
[[ "${RESTATE_SNAPSHOT_BUCKET}" != */* ]] \
  || die "RESTATE_SNAPSHOT_BUCKET must be a bare GCS bucket name"
[[ "${RESTATE_SNAPSHOT_PREFIX}" != /* && "${RESTATE_SNAPSHOT_PREFIX}" != *..* ]] \
  || die "RESTATE_SNAPSHOT_PREFIX must be a relative non-traversing object prefix"
[[ "${RESTATE_SNAPSHOT_GSA}" == *@*.iam.gserviceaccount.com ]] \
  || die "RESTATE_SNAPSHOT_GSA must be a Google service-account email"

command -v kustomize >/dev/null 2>&1 || die "kustomize is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"
command -v rg >/dev/null 2>&1 || die "ripgrep (rg) is required"

output_dir="$1"
[[ ! -e "${output_dir}" ]] || die "output path already exists: ${output_dir}"

work_dir="$(mktemp -d)"
trap 'rm -rf -- "${work_dir}"' EXIT
cp -R "${REPO_ROOT}/k8s" "${work_dir}/k8s"
cp -R "${REPO_ROOT}/ops" "${work_dir}/ops"

production_overlay="${work_dir}/k8s/overlays/production"
jobs_overlay="${work_dir}/k8s/jobs"
(
  cd "${production_overlay}"
  kustomize edit set image \
    "${ORCHESTRATOR_NAME}=${MOA_ORCHESTRATOR_IMAGE}" \
    "${EDGE_NAME}=${MOA_EDGE_IMAGE}"
)
(
  cd "${jobs_overlay}"
  kustomize edit set image "${ORCHESTRATOR_NAME}=${MOA_ORCHESTRATOR_IMAGE}"
)

revision="${MOA_ORCHESTRATOR_IMAGE##*@sha256:}"
revision="${revision:0:12}"
snapshot_destination="gs://${RESTATE_SNAPSHOT_BUCKET}/${RESTATE_SNAPSHOT_PREFIX}"
python3 - \
  "${production_overlay}/patches/restate-observability.yaml" \
  "${work_dir}/k8s/base/21-session-status-migrator.yaml" \
  "${work_dir}/k8s/base/22-restate-bootstrap-job.yaml" \
  "${snapshot_destination}" "${revision}" <<'PY'
from pathlib import Path
import sys

restate_patch = Path(sys.argv[1])
revision_files = [Path(sys.argv[2]), Path(sys.argv[3])]
destination = sys.argv[4]
revision = sys.argv[5]

text = restate_patch.read_text(encoding="utf-8")
marker = "    [worker.snapshots]\n"
if text.count(marker) != 1 or "    destination = " in text:
    raise SystemExit("production Restate snapshot table is not renderable")
restate_patch.write_text(
    text.replace(marker, marker + f'    destination = "{destination}"\n'),
    encoding="utf-8",
)

for path in revision_files:
    text = path.read_text(encoding="utf-8")
    if "image-revision" not in text:
        raise SystemExit(f"{path} has no image revision sentinel")
    path.write_text(text.replace("image-revision", revision), encoding="utf-8")
PY

cat >"${production_overlay}/patches/restate-render-inputs.yaml" <<EOF
- op: add
  path: /spec/security/serviceAccountAnnotations
  value:
    iam.gke.io/gcp-service-account: ${RESTATE_SNAPSHOT_GSA}
EOF
cat >>"${production_overlay}/kustomization.yaml" <<'EOF'
- path: patches/restate-render-inputs.yaml
  target:
    group: restate.dev
    version: v1
    kind: RestateCluster
    name: moa-restate
EOF

mkdir -p "${work_dir}/rendered"
kustomize build "${production_overlay}" >"${work_dir}/rendered/production.yaml"
kustomize build "${jobs_overlay}" >"${work_dir}/rendered/jobs.yaml"

rg -Fq "image: ${MOA_ORCHESTRATOR_IMAGE}" "${work_dir}/rendered/production.yaml" \
  || die "production manifest does not contain the requested orchestrator digest"
rg -Fq "image: ${MOA_EDGE_IMAGE}" "${work_dir}/rendered/production.yaml" \
  || die "production manifest does not contain the requested edge digest"
rg -Fq "image: ${MOA_ORCHESTRATOR_IMAGE}" "${work_dir}/rendered/jobs.yaml" \
  || die "maintenance manifest does not contain the requested orchestrator digest"
rg -Fq "destination = \"${snapshot_destination}\"" "${work_dir}/rendered/production.yaml" \
  || die "production manifest does not contain the requested snapshot destination"
if rg -q 'sha256:0{64}|image-revision' "${work_dir}/rendered"; then
  die "rendered manifests contain an unresolved sentinel"
fi

mkdir -- "${output_dir}"
mv "${work_dir}/rendered/production.yaml" "${output_dir}/production.yaml"
mv "${work_dir}/rendered/jobs.yaml" "${output_dir}/jobs.yaml"
echo "Rendered immutable production and maintenance manifests to ${output_dir}"
