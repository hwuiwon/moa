#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
ORCHESTRATOR_NAME="moa/orchestrator"
EDGE_NAME="moa/edge"
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
require_value SANDBOX_CHECKPOINT_BUCKET
require_value SANDBOX_CHECKPOINT_PREFIX
require_value SANDBOX_WORKSPACE_GSA
require_value SANDBOX_PROVIDER_ACCOUNT_ID
require_value SANDBOX_PROVIDER_ACCOUNT_GENERATION
require_value SANDBOX_PROVIDER_ISOLATION_CELL
require_value SANDBOX_PROVIDER_PROJECT_FINGERPRINT
require_value SANDBOX_CANARY_TENANT_ID
require_value SANDBOX_PROVIDER_CREDENTIAL_SECRET
require_value SANDBOX_MAINTENANCE_DATABASE_SECRET
require_value OPENFGA_URL
require_value OPENFGA_STORE_ID
require_value OPENFGA_MODEL_ID
require_value OPENFGA_PRESHARED_KEY_SECRET
[[ "${RESTATE_SNAPSHOT_BUCKET}" != */* ]] \
  || die "RESTATE_SNAPSHOT_BUCKET must be a bare GCS bucket name"
[[ "${RESTATE_SNAPSHOT_PREFIX}" != /* && "${RESTATE_SNAPSHOT_PREFIX}" != *..* ]] \
  || die "RESTATE_SNAPSHOT_PREFIX must be a relative non-traversing object prefix"
[[ "${RESTATE_SNAPSHOT_GSA}" == *@*.iam.gserviceaccount.com ]] \
  || die "RESTATE_SNAPSHOT_GSA must be a Google service-account email"
[[ "${SANDBOX_CHECKPOINT_BUCKET}" =~ ^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$ ]] \
  || die "SANDBOX_CHECKPOINT_BUCKET must be a bare DNS-compatible bucket name"
[[ "${SANDBOX_CHECKPOINT_PREFIX}" != /* \
  && "${SANDBOX_CHECKPOINT_PREFIX}" != *..* \
  && "${SANDBOX_CHECKPOINT_PREFIX}" != "" ]] \
  || die "SANDBOX_CHECKPOINT_PREFIX must be a relative non-traversing reserved prefix"
[[ "${SANDBOX_WORKSPACE_GSA}" == *@*.iam.gserviceaccount.com ]] \
  || die "SANDBOX_WORKSPACE_GSA must be a Google service-account email"
[[ "${SANDBOX_PROVIDER_ACCOUNT_ID}" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
  || die "SANDBOX_PROVIDER_ACCOUNT_ID must be a lowercase UUID"
[[ "${SANDBOX_PROVIDER_ACCOUNT_GENERATION}" =~ ^[1-9][0-9]*$ ]] \
  || die "SANDBOX_PROVIDER_ACCOUNT_GENERATION must be positive"
[[ "${SANDBOX_PROVIDER_ISOLATION_CELL}" =~ ^[A-Za-z0-9._:-]+$ ]] \
  || die "SANDBOX_PROVIDER_ISOLATION_CELL contains unsupported characters"
[[ "${SANDBOX_PROVIDER_PROJECT_FINGERPRINT}" =~ ^[A-Za-z0-9._:/@-]+$ ]] \
  || die "SANDBOX_PROVIDER_PROJECT_FINGERPRINT contains unsupported characters"
[[ "${SANDBOX_CANARY_TENANT_ID}" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
  || die "SANDBOX_CANARY_TENANT_ID must be a lowercase UUID"
[[ "${SANDBOX_PROVIDER_CREDENTIAL_SECRET}" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] \
  || die "SANDBOX_PROVIDER_CREDENTIAL_SECRET must be a Kubernetes DNS label"
[[ "${SANDBOX_MAINTENANCE_DATABASE_SECRET}" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] \
  || die "SANDBOX_MAINTENANCE_DATABASE_SECRET must be a Kubernetes DNS label"
[[ "${OPENFGA_URL}" =~ ^https://[^/[:space:]]+(:[0-9]+)?$ ]] \
  || die "OPENFGA_URL must be a canonical HTTPS origin"
[[ "${OPENFGA_STORE_ID}" =~ ^[A-Za-z0-9._:-]+$ ]] \
  || die "OPENFGA_STORE_ID contains unsupported characters"
[[ "${OPENFGA_MODEL_ID}" =~ ^[A-Za-z0-9._:-]+$ ]] \
  || die "OPENFGA_MODEL_ID contains unsupported characters"
[[ "${OPENFGA_PRESHARED_KEY_SECRET}" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] \
  || die "OPENFGA_PRESHARED_KEY_SECRET must be a Kubernetes DNS label"

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

python3 - \
  "${production_overlay}/patches/runtime-security-profile.yaml" \
  "${production_overlay}/sandbox-workspace-object-store-service-account.yaml" \
  "${production_overlay}/patches/orchestrator-security-profile.yaml" \
  "${work_dir}/k8s/base/20-orchestrator-deployment.yaml" \
  "${SANDBOX_CHECKPOINT_BUCKET}" "${SANDBOX_CHECKPOINT_PREFIX}" \
  "${SANDBOX_WORKSPACE_GSA}" "${SANDBOX_PROVIDER_ACCOUNT_ID}" \
  "${SANDBOX_PROVIDER_ACCOUNT_GENERATION}" "${SANDBOX_PROVIDER_ISOLATION_CELL}" \
  "${SANDBOX_PROVIDER_PROJECT_FINGERPRINT}" "${SANDBOX_CANARY_TENANT_ID}" \
  "${SANDBOX_PROVIDER_CREDENTIAL_SECRET}" "${SANDBOX_MAINTENANCE_DATABASE_SECRET}" \
  "${OPENFGA_URL}" \
  "${OPENFGA_STORE_ID}" "${OPENFGA_MODEL_ID}" \
  "${OPENFGA_PRESHARED_KEY_SECRET}" <<'PY'
from pathlib import Path
import sys

runtime_path, service_account_path, orchestrator_path, base_orchestrator_path = map(Path, sys.argv[1:5])
(
    bucket,
    prefix,
    gsa,
    account_id,
    generation,
    isolation_cell,
    fingerprint,
    tenant_id,
    provider_secret,
    maintenance_database_secret,
    openfga_url,
    openfga_store_id,
    openfga_model_id,
    openfga_secret,
) = sys.argv[5:]

runtime = runtime_path.read_text(encoding="utf-8")
replacements = {
    "sandbox-checkpoint-bucket-render-input": bucket,
    "sandbox-checkpoint-prefix-render-input": prefix,
    "sandbox-cell-render-input": isolation_cell,
    "sandbox-project-fingerprint-render-input": fingerprint,
    "5df222fb-c303-5ae4-a494-8ae4de622e2d": account_id,
    "ae88b9a9-35e8-5ce4-a4de-8f5172c17115": tenant_id,
    "111111": generation,
    "openfga-url-render-input": openfga_url,
    "openfga-store-render-input": openfga_store_id,
    "openfga-model-render-input": openfga_model_id,
}
for sentinel, value in replacements.items():
    if sentinel not in runtime:
        raise SystemExit(f"missing sandbox render sentinel {sentinel!r}")
    runtime = runtime.replace(sentinel, value)
runtime_path.write_text(runtime, encoding="utf-8")

service_account = service_account_path.read_text(encoding="utf-8")
sentinel = "sandbox-workspace-gsa-render-input"
if service_account.count(sentinel) != 1:
    raise SystemExit("sandbox workspace service-account sentinel drifted")
service_account_path.write_text(service_account.replace(sentinel, gsa), encoding="utf-8")

orchestrator = orchestrator_path.read_text(encoding="utf-8")
for sentinel, value in {
    "sandbox-provider-secret-render-input": provider_secret,
    "openfga-secret-render-input": openfga_secret,
}.items():
    if orchestrator.count(sentinel) != 1:
        raise SystemExit(f"production Secret sentinel drifted: {sentinel}")
    orchestrator = orchestrator.replace(sentinel, value)
orchestrator_path.write_text(orchestrator, encoding="utf-8")

base_orchestrator = base_orchestrator_path.read_text(encoding="utf-8")
sentinel = "moa-postgres-maintenance"
if base_orchestrator.count(sentinel) != 1:
    raise SystemExit("maintenance database Secret sentinel drifted")
base_orchestrator_path.write_text(
    base_orchestrator.replace(sentinel, maintenance_database_secret),
    encoding="utf-8",
)
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
rg -Fq "name: ${SANDBOX_CHECKPOINT_BUCKET}" "${work_dir}/rendered/production.yaml" \
  || rg -Fq "MOA_SANDBOX_CHECKPOINT_BUCKET: ${SANDBOX_CHECKPOINT_BUCKET}" "${work_dir}/rendered/production.yaml" \
  || die "production manifest does not contain the external checkpoint bucket"
rg -Fq "iam.gke.io/gcp-service-account: ${SANDBOX_WORKSPACE_GSA}" "${work_dir}/rendered/production.yaml" \
  || die "production manifest does not bind the sandbox workspace workload identity"
if rg -q 'sha256:0{64}|image-revision' "${work_dir}/rendered"; then
  die "rendered manifests contain an unresolved sentinel"
fi

mkdir -- "${output_dir}"
mv "${work_dir}/rendered/production.yaml" "${output_dir}/production.yaml"
mv "${work_dir}/rendered/jobs.yaml" "${output_dir}/jobs.yaml"
echo "Rendered immutable production and maintenance manifests to ${output_dir}"
