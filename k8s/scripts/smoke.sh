#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
SYSTEM_NAMESPACE="${SYSTEM_NAMESPACE:-moa-system}"
RESTATE_NAMESPACE="${RESTATE_NAMESPACE:-moa-restate}"
EDGE_PORT="${EDGE_PORT:-10000}"
RESTATE_INGRESS_PORT="${RESTATE_INGRESS_PORT:-10010}"
RESTATE_ADMIN_PORT="${RESTATE_ADMIN_PORT:-10011}"
PORT_FORWARD_PIDS=()

die() {
  echo "Smoke test failed: $*" >&2
  exit 1
}

assert_contains() {
  local content="$1"
  local expected="$2"
  local description="$3"
  [[ "${content}" == *"${expected}"* ]] || die "${description}"
}

assert_excludes() {
  local content="$1"
  local forbidden="$2"
  local description="$3"
  [[ "${content}" != *"${forbidden}"* ]] || die "${description}"
}

manifest_document() {
  local manifest="$1"
  local target_kind="$2"
  local target_name="$3"
  awk -v target_kind="${target_kind}" -v target_name="${target_name}" '
    function emit() {
      if (kind == target_kind && name == target_name) {
        printf "%s", document
      }
    }
    /^---$/ {
      emit()
      document = ""
      kind = ""
      name = ""
      in_metadata = 0
      next
    }
    {
      document = document $0 ORS
      if ($0 ~ /^kind: /) {
        kind = $2
      } else if ($0 == "metadata:") {
        in_metadata = 1
      } else if (in_metadata && $0 ~ /^  name: /) {
        name = $2
        in_metadata = 0
      }
    }
    END { emit() }
  ' "${manifest}"
}

validate_manifests() {
  local work_dir local_manifest production_manifest jobs_manifest
  local local_orchestrator production_orchestrator local_edge production_edge
  local local_runtime_config production_runtime_config local_key_secret
  local rewrap_job backfill_job application_content
  work_dir="$(mktemp -d)"
  trap 'rm -rf -- "${work_dir}"' RETURN
  local_manifest="${work_dir}/local.yaml"
  production_manifest="${work_dir}/production.yaml"
  jobs_manifest="${work_dir}/jobs.yaml"

  kubectl kustomize "${REPO_ROOT}/k8s/overlays/local" >"${local_manifest}"
  kubectl kustomize "${REPO_ROOT}/k8s/overlays/production" >"${production_manifest}"
  kubectl kustomize "${REPO_ROOT}/k8s/jobs" >"${jobs_manifest}"

  local_orchestrator="$(manifest_document "${local_manifest}" RestateDeployment moa-orchestrator)"
  production_orchestrator="$(manifest_document "${production_manifest}" RestateDeployment moa-orchestrator)"
  local_edge="$(manifest_document "${local_manifest}" Deployment moa-edge)"
  production_edge="$(manifest_document "${production_manifest}" Deployment moa-edge)"
  local_runtime_config="$(manifest_document "${local_manifest}" ConfigMap moa-runtime-config)"
  production_runtime_config="$(manifest_document "${production_manifest}" ConfigMap moa-runtime-config)"
  local_key_secret="$(manifest_document "${local_manifest}" Secret moa-kms-root-keys)"
  rewrap_job="$(manifest_document "${jobs_manifest}" Job moa-kms-rewrap)"
  backfill_job="$(manifest_document "${jobs_manifest}" Job moa-memory-sealing-backfill)"

  for orchestrator in "${local_orchestrator}" "${production_orchestrator}"; do
    assert_contains "${orchestrator}" "name: moa-runtime-config" "orchestrator is missing the runtime ConfigMap"
    assert_contains "${orchestrator}" "secretName: moa-kms-root-keys" "orchestrator is missing the KMS Secret volume"
    assert_contains "${orchestrator}" "mountPath: /var/run/secrets/moa-kms/root-keys" "orchestrator is missing the KMS mount path"
    assert_contains "${orchestrator}" "readOnly: true" "orchestrator KMS mount is not read-only"
  done
  for runtime_config in "${local_runtime_config}" "${production_runtime_config}"; do
    assert_contains "${runtime_config}" "MOA_KMS_PROVIDER: postgres" "runtime config does not select Postgres KMS"
    assert_contains "${runtime_config}" "MOA_KMS_ROOT_KEY_DIR: /var/run/secrets/moa-kms/root-keys" "runtime config has the wrong keyring directory"
    assert_contains "${runtime_config}" "MOA_KMS_REQUIRED_GENERATION: primary" "runtime config does not require primary"
  done
  assert_contains "${local_key_secret}" "primary:" "local KMS Secret lacks the stable primary key"

  for edge in "${local_edge}" "${production_edge}"; do
    assert_excludes "${edge}" "MOA_KMS_" "edge unexpectedly receives KMS configuration"
    assert_excludes "${edge}" "moa-kms-root-keys" "edge unexpectedly mounts the KMS Secret"
    assert_excludes "${edge}" "/var/run/secrets/moa-kms" "edge unexpectedly exposes the KMS keyring"
  done
  for application in "${local_manifest}" "${production_manifest}"; do
    application_content="$(<"${application}")"
    assert_excludes "${application_content}" "name: moa-kms-rewrap" "application overlay installs the KMS rewrap Job"
    assert_excludes "${application_content}" "name: moa-memory-sealing-backfill" "application overlay installs the sealing backfill Job"
  done

  assert_contains "${rewrap_job}" $'args:\n        - kms-rewrap\n        - --batch-size\n        - "100"' "KMS rewrap Job command is not exact"
  assert_contains "${backfill_job}" $'args:\n        - backfill-memory-sealed-content\n        - --batch-size\n        - "100"' "memory sealing Job command is not exact"
  for job in "${rewrap_job}" "${backfill_job}"; do
    assert_contains "${job}" "name: moa-postgres" "maintenance Job does not use the database Secret"
    assert_contains "${job}" "value: postgres" "maintenance Job does not use Postgres KMS"
    assert_contains "${job}" "secretName: moa-kms-root-keys" "maintenance Job is missing the KMS Secret"
    assert_contains "${job}" "mountPath: /var/run/secrets/moa-kms/root-keys" "maintenance Job is missing the KMS mount path"
    assert_contains "${job}" "readOnly: true" "maintenance Job KMS mount is not read-only"
  done

  echo "Manifest validation OK"
}

if [[ "${1:-}" == "--validate-manifests" ]]; then
  [[ "$#" -eq 1 ]] || die "--validate-manifests accepts no additional arguments"
  validate_manifests
  exit 0
fi
[[ "$#" -eq 0 ]] || die "unknown argument: $1"

cleanup() {
  for pid in "${PORT_FORWARD_PIDS[@]}"; do
    if kill -0 "${pid}" 2>/dev/null; then
      kill "${pid}" 2>/dev/null || true
      wait "${pid}" 2>/dev/null || true
    fi
  done
}

trap cleanup EXIT

echo "Waiting for Restate cluster readiness..."
kubectl -n "${RESTATE_NAMESPACE}" wait --for=condition=Ready restatecluster/moa-restate --timeout=600s

echo "Waiting for orchestrator pods to become Ready..."
kubectl -n "${SYSTEM_NAMESPACE}" wait --for=condition=Ready restatedeployment/moa-orchestrator --timeout=600s
ORCHESTRATOR_SELECTOR="$(
  kubectl -n "${SYSTEM_NAMESPACE}" get restatedeployment/moa-orchestrator \
    -o jsonpath='{.status.labelSelector}'
)"
if [[ -z "${ORCHESTRATOR_SELECTOR}" ]]; then
  echo "Smoke test failed: RestateDeployment did not report a pod selector" >&2
  exit 1
fi
kubectl -n "${SYSTEM_NAMESPACE}" wait --for=condition=Ready pod \
  -l "${ORCHESTRATOR_SELECTOR}" \
  --timeout=600s

echo "Waiting for edge pods to become Ready..."
kubectl -n "${SYSTEM_NAMESPACE}" wait --for=condition=Ready pod \
  -l app.kubernetes.io/name=moa-edge \
  --timeout=600s

if kubectl -n "${SYSTEM_NAMESPACE}" get job/rustfs-init >/dev/null 2>&1; then
  echo "Waiting for local RustFS bucket initialization..."
  kubectl -n "${SYSTEM_NAMESPACE}" wait --for=condition=Complete job/rustfs-init --timeout=180s
fi

echo "Port-forwarding Restate ingress/admin and MOA edge..."
kubectl -n "${RESTATE_NAMESPACE}" port-forward svc/restate "${RESTATE_INGRESS_PORT}:8080" "${RESTATE_ADMIN_PORT}:9070" >/tmp/moa-k8s-smoke-restate-port-forward.log 2>&1 &
PORT_FORWARD_PIDS+=("$!")
kubectl -n "${SYSTEM_NAMESPACE}" port-forward svc/moa-edge "${EDGE_PORT}:8080" >/tmp/moa-k8s-smoke-edge-port-forward.log 2>&1 &
PORT_FORWARD_PIDS+=("$!")

for _attempt in $(seq 1 30); do
  if curl -sf "http://127.0.0.1:${EDGE_PORT}/healthz" >/dev/null && \
     curl -sf "http://127.0.0.1:${RESTATE_ADMIN_PORT}/health" >/dev/null; then
    break
  fi
  sleep 1
done

echo "Calling edge health endpoint..."
curl -sf "http://127.0.0.1:${EDGE_PORT}/healthz" >/dev/null

echo "Calling edge identity endpoint..."
curl -sf "http://127.0.0.1:${EDGE_PORT}/v1/whoami" | grep -q '"identity_type":"service"'

echo "Checking Restate service registration..."
DEPLOYMENT_ID="$(
  kubectl -n "${SYSTEM_NAMESPACE}" get restatedeployment/moa-orchestrator \
    -o jsonpath='{.status.deploymentId}'
)"
SERVICES_JSON="$(curl -sf "http://127.0.0.1:${RESTATE_ADMIN_PORT}/services")"
if [[ "${SERVICES_JSON}" != *"\"deployment_id\":\"${DEPLOYMENT_ID}\""* ]]; then
  echo "Smoke test failed: Restate services do not reference deployment ${DEPLOYMENT_ID}" >&2
  exit 1
fi
for service in SessionStore Session Contacts; do
  if [[ "${SERVICES_JSON}" != *"\"name\":\"${service}\""* ]]; then
    echo "Smoke test failed: Restate service ${service} is not registered" >&2
    exit 1
  fi
done

echo "Smoke test OK"
