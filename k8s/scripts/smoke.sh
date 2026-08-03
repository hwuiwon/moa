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

assert_occurrences() {
  local content="$1"
  local expected="$2"
  local needle="$3"
  local description="$4"
  local observed
  observed="$(
    awk -v needle="${needle}" '
      index($0, needle) { count += 1 }
      END { print count + 0 }
    ' <<<"${content}"
  )"
  [[ "${observed}" -eq "${expected}" ]] \
    || die "${description}: expected ${expected}, found ${observed}"
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

# Pinned kubeconform. A different version can disagree about what strict mode
# accepts, so a local pass would not predict CI.
KUBECONFORM_VERSION="v0.7.0"

# Validates every rendered manifest against real schemas, including the CRDs.
#
# `-strict` rejects unknown fields, which is the only thing that catches a
# misspelled key: kustomize renders it happily and the API server drops it, so a
# typo'd `readinessProbe` field ships as a deployment with no readiness gate.
#
# There is deliberately NO `-ignore-missing-schemas`. With it, every custom
# resource - the Restate cluster and deployment, the alert rules, i.e. MOA's most
# structurally complex manifests - passes unchecked, and the summary still says
# valid. The vendored schemas under k8s/schemas exist so the flag is unnecessary.
validate_schemas() {
  local manifest_dir="$1" observed
  command -v kubeconform >/dev/null 2>&1 \
    || die "kubeconform is not on PATH. Install ${KUBECONFORM_VERSION} from https://github.com/yannh/kubeconform/releases"
  observed="$(kubeconform -v 2>&1 | head -1)"
  if [[ "${observed}" != *"${KUBECONFORM_VERSION}"* ]]; then
    if [[ "${OBSERVABILITY_TOOLS_ALLOW_UNPINNED:-0}" == "1" ]]; then
      echo "WARNING: kubeconform is ${observed}, pinned ${KUBECONFORM_VERSION}; continuing on request" >&2
    else
      die "kubeconform version mismatch: pinned '${KUBECONFORM_VERSION}', found '${observed}'. Install the pinned version, or set OBSERVABILITY_TOOLS_ALLOW_UNPINNED=1 to accept that a local pass may not predict CI."
    fi
  fi

  local rendered summary
  for rendered in "${manifest_dir}"/*.yaml; do
    summary="$(
      kubeconform -strict -summary \
        -schema-location default \
        -schema-location "${REPO_ROOT}/k8s/schemas/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json" \
        "${rendered}" 2>&1
    )" || die "$(printf 'kubeconform rejected %s:\n%s' "$(basename "${rendered}")" "${summary}")"
    # A skipped resource is an unvalidated resource. Without this the suite
    # reports success for a manifest whose schema is simply absent, which is the
    # exact failure the vendored schemas exist to prevent.
    assert_contains "${summary}" "Skipped: 0" \
      "$(printf 'kubeconform skipped a resource in %s, so something rendered without a schema:\n%s' "$(basename "${rendered}")" "${summary}")"
  done
  echo "Schema validation OK"
}

validate_manifests() {
  local work_dir local_manifest production_manifest jobs_manifest
  local local_orchestrator production_orchestrator local_edge production_edge
  local local_orchestrator_service production_orchestrator_service
  local local_orchestrator_policy production_orchestrator_policy
  local local_edge_service production_edge_service
  local local_runtime_config production_runtime_config local_key_secret
  local local_restate production_restate
  local rewrap_job application_content
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
  local_orchestrator_service="$(manifest_document "${local_manifest}" Service moa-orchestrator)"
  production_orchestrator_service="$(manifest_document "${production_manifest}" Service moa-orchestrator)"
  local_orchestrator_policy="$(manifest_document "${local_manifest}" NetworkPolicy moa-orchestrator-ingress)"
  production_orchestrator_policy="$(manifest_document "${production_manifest}" NetworkPolicy moa-orchestrator-ingress)"
  local_edge_service="$(manifest_document "${local_manifest}" Service moa-edge)"
  production_edge_service="$(manifest_document "${production_manifest}" Service moa-edge)"
  local_restate="$(manifest_document "${local_manifest}" RestateCluster moa-restate)"
  production_restate="$(manifest_document "${production_manifest}" RestateCluster moa-restate)"
  local_runtime_config="$(manifest_document "${local_manifest}" ConfigMap moa-runtime-config)"
  production_runtime_config="$(manifest_document "${production_manifest}" ConfigMap moa-runtime-config)"
  local_key_secret="$(manifest_document "${local_manifest}" Secret moa-kms-root-keys)"
  rewrap_job="$(manifest_document "${jobs_manifest}" Job moa-kms-rewrap)"

  for orchestrator in "${local_orchestrator}" "${production_orchestrator}"; do
    assert_contains "${orchestrator}" "name: moa-runtime-config" "orchestrator is missing the runtime ConfigMap"
    assert_contains "${orchestrator}" "secretName: moa-kms-root-keys" "orchestrator is missing the KMS Secret volume"
    assert_contains "${orchestrator}" "mountPath: /var/run/secrets/moa-kms/root-keys" "orchestrator is missing the KMS mount path"
    assert_contains "${orchestrator}" "readOnly: true" "orchestrator KMS mount is not read-only"
    assert_contains "${orchestrator}" "name: database-migrations" "orchestrator is missing the explicit database migration init container"
    assert_contains "${orchestrator}" "key: admin-url" "database migration init container is missing the admin database Secret key"
    assert_occurrences "${orchestrator}" 1 "name: MOA_DATABASE_ADMIN_URL" \
      "database admin authority must be scoped to the migration init container"
    assert_occurrences "${orchestrator}" 1 "- migrate" \
      "orchestrator must execute exactly one explicit migration init command"
  done
  assert_occurrences "${local_orchestrator}" 2 "image: moa/orchestrator:kind" \
    "local runtime and migration init must use the same orchestrator image"
  assert_occurrences "${production_orchestrator}" 2 "image: ghcr.io/hwuiwon/moa-orchestrator:latest" \
    "production runtime and migration init must use the same orchestrator image"
  for runtime_config in "${local_runtime_config}" "${production_runtime_config}"; do
    assert_contains "${runtime_config}" "MOA_KMS_PROVIDER: postgres" "runtime config does not select Postgres KMS"
    assert_contains "${runtime_config}" "MOA_KMS_ROOT_KEY_DIR: /var/run/secrets/moa-kms/root-keys" "runtime config has the wrong keyring directory"
    assert_contains "${runtime_config}" "MOA_KMS_REQUIRED_GENERATION: primary" "runtime config does not require primary"
  done
  assert_contains "${local_key_secret}" "primary:" "local KMS Secret lacks the stable primary key"

  # Each overlay must render exactly one explicit security posture. Local is the
  # development contract (host-local hands, permissive default); production is
  # the fail-closed cloud contract (deny default, credentialed cloud sandbox).
  assert_contains "${local_runtime_config}" "MOA_SECURITY_PROFILE: local" "local overlay does not select the local security profile"
  assert_contains "${local_runtime_config}" "MOA_PERMISSIONS_DEFAULT_EFFECT: allow" "local overlay does not render the permissive permission default"
  assert_contains "${local_runtime_config}" "MOA_CLOUD_HANDS_DEFAULT_PROVIDER: local" "local overlay does not select the local hand provider"
  assert_excludes "${local_runtime_config}" "MOA_SECURITY_PROFILE: cloud" "local overlay leaks the cloud security profile"
  assert_contains "${production_runtime_config}" "MOA_SECURITY_PROFILE: cloud" "production overlay does not select the cloud security profile"
  assert_contains "${production_runtime_config}" "MOA_PERMISSIONS_DEFAULT_EFFECT: deny" "production overlay does not render the deny permission default"
  assert_contains "${production_runtime_config}" "MOA_CLOUD_HANDS_DEFAULT_PROVIDER: e2b" "production overlay does not select the E2B sandbox backend"
  assert_excludes "${production_runtime_config}" "MOA_PERMISSIONS_DEFAULT_EFFECT: allow" "production overlay leaks a permissive permission default"

  # The cloud sandbox credential belongs to production only; base and local must
  # not reference it. The cloud profile refuses to serve without it.
  assert_contains "${production_orchestrator}" "MOA_CLOUD_HANDS_E2B_API_KEY" "production orchestrator is missing the E2B sandbox credential"
  assert_contains "${production_orchestrator}" "name: moa-hand-provider-keys" "production orchestrator is missing the hand-provider Secret"
  assert_excludes "${local_orchestrator}" "MOA_CLOUD_HANDS_E2B_API_KEY" "local orchestrator unexpectedly receives the E2B sandbox credential"
  assert_excludes "${local_orchestrator}" "moa-hand-provider-keys" "local orchestrator unexpectedly references the hand-provider Secret"

  # The deleted development opt-in has exactly one post-change contract: the
  # security profile. No manifest may reintroduce the removed key, whose name is
  # matched on its distinctive suffix so the dead name is not restated here.
  for application in "${local_manifest}" "${production_manifest}"; do
    assert_excludes "$(<"${application}")" "ALLOW_LOCAL" "overlay reintroduces the deleted local-hands opt-in key"
  done

  for edge in "${local_edge}" "${production_edge}"; do
    assert_excludes "${edge}" "MOA_KMS_" "edge unexpectedly receives KMS configuration"
    assert_excludes "${edge}" "moa-kms-root-keys" "edge unexpectedly mounts the KMS Secret"
    assert_excludes "${edge}" "/var/run/secrets/moa-kms" "edge unexpectedly exposes the KMS keyring"
    assert_contains "${edge}" "name: MOA_EDGE_CONNECTOR_CREDENTIAL_UPSTREAM" \
      "edge is missing the private connector credential upstream"
    assert_contains "${edge}" "http://moa-orchestrator.moa-system.svc.cluster.local:10023" \
      "edge connector credential upstream does not target the private orchestrator listener"
  done
  for orchestrator in "${local_orchestrator}" "${production_orchestrator}"; do
    assert_contains "${orchestrator}" "- --credential-port" \
      "orchestrator does not configure the private credential listener"
    assert_contains "${orchestrator}" "name: credentials" \
      "orchestrator pod does not declare its private credential port"
    assert_contains "${orchestrator}" "containerPort: 10023" \
      "orchestrator private credential listener is not on the expected port"
  done
  for service in "${local_orchestrator_service}" "${production_orchestrator_service}"; do
    assert_contains "${service}" "type: ClusterIP" \
      "orchestrator Service is not explicitly internal-only"
    assert_contains "${service}" "name: credentials" \
      "orchestrator Service does not route the private credential listener"
    assert_contains "${service}" "port: 10023" \
      "orchestrator Service has the wrong credential port"
    assert_contains "${service}" "targetPort: credentials" \
      "orchestrator Service does not target the named credential port"
  done
  for service in "${local_edge_service}" "${production_edge_service}"; do
    assert_excludes "${service}" "10023" \
      "edge Service publicly exposes the orchestrator credential port"
  done
  for policy in "${local_orchestrator_policy}" "${production_orchestrator_policy}"; do
    assert_contains "${policy}" "app.kubernetes.io/name: moa-edge" \
      "orchestrator NetworkPolicy does not select edge as the credential caller"
    assert_contains "${policy}" "port: 10023" \
      "orchestrator NetworkPolicy does not allow the private credential listener"
    assert_occurrences "${policy}" 1 "port: 10023" \
      "orchestrator NetworkPolicy must have one narrowly scoped credential allow rule"
  done
  for workload in \
    "${local_orchestrator}" \
    "${production_orchestrator}" \
    "${local_edge}" \
    "${production_edge}"; do
    assert_contains "${workload}" "name: MOA_SERVICE_INSTANCE_ID" \
      "MOA workload does not inject a per-pod telemetry identity"
    assert_contains "${workload}" "fieldPath: metadata.uid" \
      "MOA workload telemetry identity is not sourced from the pod UID"
  done
  for restate in "${local_restate}" "${production_restate}"; do
    assert_contains "${restate}" "node:" \
      "Restate networkPeers does not configure access to the node metrics port"
    assert_contains "${restate}" "kubernetes.io/metadata.name: observability" \
      "Restate networkPeers does not allow the observability namespace"
    assert_contains "${restate}" "app.kubernetes.io/name: alloy" \
      "Restate networkPeers does not allow the Alloy collector"
  done
  for application in "${local_manifest}" "${production_manifest}"; do
    application_content="$(<"${application}")"
    assert_excludes "${application_content}" "name: moa-kms-rewrap" "application overlay installs the KMS rewrap Job"
  done

  assert_contains "${rewrap_job}" $'args:\n        - kms-rewrap\n        - --batch-size\n        - "100"' "KMS rewrap Job command is not exact"
  assert_contains "${rewrap_job}" "name: moa-postgres" "maintenance Job does not use the database Secret"
  assert_contains "${rewrap_job}" "value: postgres" "maintenance Job does not use Postgres KMS"
  assert_contains "${rewrap_job}" "secretName: moa-kms-root-keys" "maintenance Job is missing the KMS Secret"
  assert_contains "${rewrap_job}" "mountPath: /var/run/secrets/moa-kms/root-keys" "maintenance Job is missing the KMS mount path"
  assert_contains "${rewrap_job}" "readOnly: true" "maintenance Job KMS mount is not read-only"

  # Termination grace periods, asserted by content because they CANNOT be
  # schema-validated. `spec.template.spec` in the RestateDeployment CRD carries
  # `x-kubernetes-preserve-unknown-fields: true`, so the entire pod spec - probes,
  # env, grace period - is free-form as far as every schema validator is
  # concerned. A misspelled `terminationGracePeriodSeconds` renders, applies, and
  # silently reverts the workload to the 30s default, which is shorter than the
  # drain both binaries perform on SIGTERM.
  for orchestrator in "${local_orchestrator}" "${production_orchestrator}"; do
    assert_contains "${orchestrator}" "terminationGracePeriodSeconds: 600" \
      "orchestrator lost its 600s termination grace period, so SIGKILL would arrive mid-drain"
  done
  for edge in "${local_edge}" "${production_edge}"; do
    assert_contains "${edge}" "terminationGracePeriodSeconds: 60" \
      "edge lost its 60s termination grace period, so SIGKILL would arrive mid-drain"
  done

  # The observability stack renders only in the production overlay, and the
  # deleted scrape surface has to stay deleted in what is actually applied - not
  # merely in the source file that produced it.
  assert_excludes "$(<"${production_manifest}")" "containerPort: 9090" \
    "production overlay reintroduces a MOA metrics scrape port"
  assert_excludes "$(<"${production_manifest}")" "grafana/alloy:latest" \
    "production overlay renders an unpinned Alloy image"
  assert_contains "${production_manifest_content:=$(<"${production_manifest}")}" "kind: PrometheusRule" \
    "production overlay renders no alert rules, so the rule synchronizer has nothing to synchronize"
  assert_contains "${production_manifest_content}" "MOA_METRICS_EXPORTER" \
    "production overlay does not select a metrics exporter"

  validate_schemas "${work_dir}"

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
