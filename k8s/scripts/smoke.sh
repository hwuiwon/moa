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
NETWORK_CHECK_POD="moa-runtime-restate-network-check"

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

readiness_probe_path() {
  awk '
    $1 == "readinessProbe:" { seen_readiness = 1; next }
    seen_readiness && $1 == "path:" { print $2; exit }
  ' <<<"$1"
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
  local local_bootstrap production_bootstrap local_bootstrap_sa production_bootstrap_sa
  local local_status_migrator production_status_migrator
  local local_orchestrator_service production_orchestrator_service
  local local_orchestrator_policy production_orchestrator_policy
  local local_edge_service production_edge_service
  local local_runtime_config production_runtime_config local_key_secret
  local local_snapshot_secret local_rustfs_policy local_rustfs_init
  local local_restate production_restate
  local local_orchestrator_readiness production_orchestrator_readiness
  local rewrap_job application_content
  work_dir="$(mktemp -d)"
  trap 'rm -rf -- "${work_dir}"' RETURN
  local_manifest="${work_dir}/local.yaml"
  production_manifest="${work_dir}/production.yaml"
  jobs_manifest="${work_dir}/jobs.yaml"

  command -v kustomize >/dev/null 2>&1 || die "kustomize is required"
  kustomize build "${REPO_ROOT}/k8s/overlays/local" >"${local_manifest}"
  kustomize build "${REPO_ROOT}/k8s/overlays/production" >"${production_manifest}"
  kustomize build "${REPO_ROOT}/k8s/jobs" >"${jobs_manifest}"

  local_orchestrator="$(manifest_document "${local_manifest}" RestateDeployment moa-orchestrator)"
  production_orchestrator="$(manifest_document "${production_manifest}" RestateDeployment moa-orchestrator)"
  local_edge="$(manifest_document "${local_manifest}" Deployment moa-edge)"
  production_edge="$(manifest_document "${production_manifest}" Deployment moa-edge)"
  local_bootstrap="$(manifest_document "${local_manifest}" Job moa-restate-bootstrap-image-revision)"
  production_bootstrap="$(manifest_document "${production_manifest}" Job moa-restate-bootstrap-image-revision)"
  local_status_migrator="$(manifest_document "${local_manifest}" Job moa-session-status-migrator-image-revision)"
  production_status_migrator="$(manifest_document "${production_manifest}" Job moa-session-status-migrator-image-revision)"
  local_bootstrap_sa="$(manifest_document "${local_manifest}" ServiceAccount moa-restate-bootstrap)"
  production_bootstrap_sa="$(manifest_document "${production_manifest}" ServiceAccount moa-restate-bootstrap)"
  local_orchestrator_readiness="$(readiness_probe_path "${local_orchestrator}")"
  production_orchestrator_readiness="$(readiness_probe_path "${production_orchestrator}")"
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
  local_snapshot_secret="$(manifest_document "${local_manifest}" Secret moa-restate-snapshots)"
  local_rustfs_policy="$(manifest_document "${local_manifest}" NetworkPolicy rustfs-s3-ingress)"
  local_rustfs_init="$(manifest_document "${local_manifest}" Job rustfs-init-snapshots-v1)"
  rewrap_job="$(manifest_document "${jobs_manifest}" Job moa-kms-rewrap)"
  for orchestrator in "${local_orchestrator}" "${production_orchestrator}"; do
    assert_contains "${orchestrator}" "name: moa-runtime-config" "orchestrator is missing the runtime ConfigMap"
    assert_contains "${orchestrator}" "secretName: moa-kms-root-keys" "orchestrator is missing the KMS Secret volume"
    assert_contains "${orchestrator}" "mountPath: /var/run/secrets/moa-kms/root-keys" "orchestrator is missing the KMS mount path"
    assert_contains "${orchestrator}" "readOnly: true" "orchestrator KMS mount is not read-only"
    assert_contains "${orchestrator}" "name: wait-status-cutover" \
      "orchestrator does not block on the completed raw-state cutover"
    assert_contains "${orchestrator}" "wait-status-cutover" \
      "orchestrator init container does not execute the cutover gate"
    assert_excludes "${orchestrator}" "key: admin-url" \
      "normal orchestrator pod still receives the database migration credential"
    assert_excludes "${orchestrator}" "MOA_DATABASE_ADMIN_URL" \
      "normal orchestrator pod still receives database admin authority"
    assert_excludes "${orchestrator}" "- migrate" \
      "normal orchestrator pod still applies migrations before serving"
    assert_excludes "${orchestrator}" "MOA_RESTATE_""ADMIN_URL" \
      "normal orchestrator replica still receives Restate Admin configuration"
    assert_excludes "${orchestrator}" "MOA_REQUIRE_RESTATE_""REGISTRATION_FOR_READINESS" \
      "normal orchestrator readiness still depends on deployment listing"
    assert_excludes "${orchestrator}" "MOA_DEREGISTER_""ON_SHUTDOWN" \
      "normal orchestrator still owns shutdown deregistration"
  done
  assert_occurrences "$(<"${local_manifest}")" 5 "image: moa/orchestrator:kind" \
    "local runtime, migration-only stage, and bootstrap must use the same orchestrator image"
  assert_occurrences "$(<"${production_manifest}")" 5 \
    "image: ghcr.io/hwuiwon/moa-orchestrator@sha256:0000000000000000000000000000000000000000000000000000000000000000" \
    "unrendered production runtime, migration-only stage, and bootstrap must use the same immutable sentinel"
  assert_occurrences "${production_edge}" 1 \
    "image: ghcr.io/hwuiwon/moa-edge@sha256:0000000000000000000000000000000000000000000000000000000000000000" \
    "unrendered production edge must use the immutable sentinel"
  [[ "${local_orchestrator_readiness}" == "/_health/ready" ]] \
    || die "local orchestrator readiness must remain local process readiness; found '${local_orchestrator_readiness}'"
  [[ "${production_orchestrator_readiness}" == "/_health/ready" ]] \
    || die "production orchestrator readiness must remain /_health/ready; found '${production_orchestrator_readiness}'"
  for edge in "${local_edge}" "${production_edge}"; do
    assert_contains "${edge}" "--post-data=" \
      "edge startup probe does not POST an empty JSON object"
    assert_contains "${edge}" "content-type: application/json" \
      "edge startup probe does not send the JSON content type"
    assert_contains "${edge}" "/restate/call/Health/check" \
      "edge startup probe does not use the public Restate health handler"
    assert_excludes "${edge}" "initContainers:" \
      "edge retains a local deployment-list readiness init poll"
  done
  for bootstrap in "${local_bootstrap}" "${production_bootstrap}"; do
    assert_contains "${bootstrap}" "serviceAccountName: moa-restate-bootstrap" \
      "bootstrap Job does not use its distinct service account"
    assert_contains "${bootstrap}" "automountServiceAccountToken: false" \
      "bootstrap Job mounts an unnecessary Kubernetes API token"
    assert_contains "${bootstrap}" "app.kubernetes.io/component: bootstrap" \
      "bootstrap Job lacks its narrow network identity label"
    assert_contains "${bootstrap}" "moa-orchestrator bootstrap" \
      "bootstrap Job does not execute the dedicated command"
    assert_contains "${bootstrap}" "--admin-url http://restate.moa-restate.svc.cluster.local:9070" \
      "bootstrap Job lacks its explicit Admin argument"
    assert_contains "${bootstrap}" "--ingress-url http://restate.moa-restate.svc.cluster.local:8080" \
      "bootstrap Job lacks its explicit ingress argument"
    assert_contains "${bootstrap}" \
      "--migration-deployment-uri http://moa-session-status-migrator.moa-system.svc.cluster.local:9080" \
      "bootstrap Job does not register the migration-only endpoint"
    assert_contains "${bootstrap}" "key: admin-url" \
      "bootstrap Job lacks the required database migration authority"
  done
  for migrator in "${local_status_migrator}" "${production_status_migrator}"; do
    assert_contains "${migrator}" "name: database-migrations" \
      "migration-only stage does not own database migrations"
    assert_contains "${migrator}" "key: admin-url" \
      "migration-only stage lacks its scoped database migration credential"
    assert_contains "${migrator}" "serve-status-migration" \
      "migration-only stage does not serve the raw Session handler"
    assert_contains "${migrator}" "wait-status-cutover" \
      "migration-only stage does not terminate from the durable receipt"
    assert_excludes "${migrator}" "MOA_RESTATE_ADMIN_URL" \
      "migration-only handler pod receives Restate Admin authority"
  done
  for service_account in "${local_bootstrap_sa}" "${production_bootstrap_sa}"; do
    assert_contains "${service_account}" "automountServiceAccountToken: false" \
      "bootstrap service account automounts a Kubernetes API token"
  done
  for runtime_config in "${local_runtime_config}" "${production_runtime_config}"; do
    assert_contains "${runtime_config}" "MOA_KMS_PROVIDER: postgres" "runtime config does not select Postgres KMS"
    assert_contains "${runtime_config}" "MOA_KMS_ROOT_KEY_DIR: /var/run/secrets/moa-kms/root-keys" "runtime config has the wrong keyring directory"
    assert_contains "${runtime_config}" "MOA_KMS_REQUIRED_GENERATION: primary" "runtime config does not require primary"
  done
  assert_contains "${local_key_secret}" "primary:" "local KMS Secret lacks the stable primary key"
  assert_contains "${local_snapshot_secret}" "namespace: moa-restate" \
    "local Restate snapshot credentials are not isolated in the Restate namespace"
  assert_contains "${local_snapshot_secret}" "access-key-id:" \
    "local Restate snapshot Secret lacks an access key"
  assert_contains "${local_snapshot_secret}" "secret-access-key:" \
    "local Restate snapshot Secret lacks a secret key"

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
  for restate in "${local_restate}" "${production_restate}"; do
    assert_contains "${restate}" "terminationGracePeriodSeconds: 660" \
      "Restate termination grace must exceed the ten-minute production shutdown timeout"
    assert_occurrences "${restate}" 2 "app.kubernetes.io/name: moa-restate-bootstrap" \
      "bootstrap identity must receive exactly ingress and Admin peer grants"
    assert_occurrences "${restate}" 1 "app.kubernetes.io/name: moa-orchestrator" \
      "normal orchestrator replicas must receive ingress, but no Admin, access"
  done
  assert_contains "${local_restate}" "replicas: 1" \
    "local Restate must remain an explicit single-node deployment"
  assert_contains "${local_restate}" "moa.hwuiwon.com/deployment-topology: local-single-node" \
    "local Restate does not label its single-node placement semantics"
  assert_excludes "${local_restate}" "topologyKey: topology.kubernetes.io/zone" \
    "local Restate inherits the production three-zone scheduling requirement"
  assert_excludes "${local_restate}" "minDomains: 3" \
    "local Restate inherits a production three-domain scheduling requirement"
  assert_excludes "${local_restate}" "requiredDuringSchedulingIgnoredDuringExecution" \
    "local Restate inherits production required pod anti-affinity"
  assert_excludes "${local_restate}" "[metadata-client]" \
    "single-node local Restate unexpectedly declares distributed metadata peers"
  assert_occurrences "${production_restate}" 1 "[metadata-client]" \
    "production Restate must declare exactly one metadata client section"
  for ordinal in 0 1 2; do
    assert_contains "${production_restate}" \
      "http://restate-${ordinal}.restate-cluster.moa-restate.svc.cluster.local:5122" \
      "production Restate is missing metadata peer restate-${ordinal}"
  done
  assert_occurrences "${production_restate}" 2 "minDomains: 3" \
    "production Restate must require three hostname and zone failure domains"
  assert_occurrences "${production_restate}" 2 "whenUnsatisfiable: DoNotSchedule" \
    "production Restate hostname and zone topology spreading must be mandatory"
  assert_occurrences "${production_restate}" 2 "topologyKey: kubernetes.io/hostname" \
    "production Restate must combine hostname spread with required pod anti-affinity"
  assert_occurrences "${production_restate}" 1 "topologyKey: topology.kubernetes.io/zone" \
    "production Restate must have exactly one mandatory zone spread constraint"
  assert_contains "${local_restate}" 'durability-mode = "balanced"' \
    "local Restate does not use snapshot-aware balanced durability"
  assert_contains "${local_restate}" \
    'destination = "s3://moa-restate-snapshots/moa-restate"' \
    "local Restate does not target the dedicated RustFS snapshot bucket"
  assert_contains "${local_restate}" "num-retained = 2" \
    "local Restate lacks explicit Restate 1.7 snapshot retention"
  assert_excludes "${local_restate}" "experimental-num-retained" \
    "local Restate uses the retired Restate 1.6 snapshot retention key"
  assert_contains "${local_restate}" \
    'aws-endpoint-url = "http://rustfs.moa-system.svc.cluster.local:9000"' \
    "local Restate does not use the in-cluster RustFS S3 endpoint"
  assert_contains "${local_restate}" 'aws-allow-http = true' \
    "local Restate does not explicitly allow its local-only HTTP S3 endpoint"
  assert_contains "${local_restate}" "name: RESTATE_WORKER__SNAPSHOTS__AWS_ACCESS_KEY_ID" \
    "local Restate snapshot access key is not sourced from an env override"
  assert_contains "${local_restate}" "name: RESTATE_WORKER__SNAPSHOTS__AWS_SECRET_ACCESS_KEY" \
    "local Restate snapshot secret key is not sourced from an env override"
  assert_occurrences "${local_restate}" 2 "name: moa-restate-snapshots" \
    "local Restate snapshot credential env overrides must use the same Secret"
  assert_contains "${local_restate}" "networkEgressRules:" \
    "local Restate CRD does not declare snapshot-store egress"
  assert_occurrences "${local_restate}" 1 "port: 9000" \
    "local Restate CRD must have one narrow RustFS egress port"
  assert_contains "${local_restate}" "protocol: TCP" \
    "local Restate snapshot-store egress is not restricted to TCP"
  assert_contains "${local_restate}" "app.kubernetes.io/name: rustfs" \
    "local Restate snapshot-store egress does not select RustFS"
  assert_contains "${local_restate}" "app.kubernetes.io/name: moa-session-status-migrator" \
    "local Restate egress cannot reach the migration-only handler Job"
  assert_occurrences "${local_restate}" 1 "port: 9080" \
    "local Restate must have one narrow migration-handler egress port"
  assert_contains "${local_rustfs_policy}" "namespace: moa-system" \
    "local RustFS ingress policy is not in RustFS's namespace"
  assert_contains "${local_rustfs_policy}" "kubernetes.io/metadata.name: moa-restate" \
    "local RustFS ingress policy does not admit the Restate namespace"
  assert_contains "${local_rustfs_policy}" "moa.hwuiwon.com/restate-cluster: moa-restate" \
    "local RustFS ingress policy does not restrict the Restate caller pods"
  assert_occurrences "${local_rustfs_policy}" 2 "port: 9000" \
    "local RustFS ingress must expose only S3 to application and Restate callers"
  assert_excludes "${local_rustfs_policy}" "port: 9001" \
    "local RustFS ingress policy exposes the management console"
  assert_contains "${local_rustfs_init}" \
    'rc bucket create --ignore-existing --region "${MOA_SESSION_ATTACHMENT_REGION}" "rustfs/moa-restate-snapshots"' \
    "local RustFS initializer does not create the dedicated Restate snapshot bucket"
  assert_contains "${production_restate}" 'durability-mode = "balanced"' \
    "production Restate does not use snapshot-aware balanced durability"
  assert_contains "${production_restate}" "snapshot-interval-num-records = 100000" \
    "production Restate lacks an explicit record-count snapshot trigger"
  assert_contains "${production_restate}" 'snapshot-interval = "60m"' \
    "production Restate lacks an explicit time snapshot trigger"
  assert_contains "${production_restate}" "num-retained = 2" \
    "production Restate lacks explicit snapshot retention"
  assert_excludes "${production_restate}" "experimental-num-retained" \
    "production Restate uses the retired Restate 1.6 snapshot retention key"
  assert_excludes "${production_restate}" "destination = \"gs://" \
    "unrendered production manifests must not contain a fake snapshot bucket"
  assert_excludes "${production_restate}" "iam.gke.io/gcp-service-account" \
    "unrendered production manifests must not contain a fake snapshot identity"
  for application in "${local_manifest}" "${production_manifest}"; do
    application_content="$(<"${application}")"
    assert_excludes "${application_content}" "name: moa-kms-rewrap" "application overlay installs the KMS rewrap Job"
  done

  assert_contains "${rewrap_job}" $'args:\n        - kms-rewrap\n        - --batch-size\n        - "100"' "KMS rewrap Job command is not exact"
  assert_contains "${rewrap_job}" \
    "image: ghcr.io/hwuiwon/moa-orchestrator@sha256:0000000000000000000000000000000000000000000000000000000000000000" \
    "unrendered maintenance Job must use the immutable orchestrator sentinel"
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
[[ "$#" -eq 2 && "${1}" == "--kind-context" ]] \
  || die "live usage: $0 --kind-context kind-<local-cluster-name>"
KUBE_CONTEXT="${2}"
[[ "${KUBE_CONTEXT}" =~ ^kind-[a-z0-9][a-z0-9-]*$ ]] \
  || die "live smoke requires an explicit local Kind context named kind-<cluster-name>"
KIND_CLUSTER_NAME="${KUBE_CONTEXT#kind-}"
case "${KIND_CLUSTER_NAME}" in
  *gke* | *prod* | *production* | *develop* | *development*)
    die "refusing context whose name resembles a shared/cloud environment: ${KUBE_CONTEXT}"
    ;;
esac

for tool in kind kubectl rg; do
  command -v "${tool}" >/dev/null 2>&1 || die "${tool} is required for live smoke"
done
kind get clusters | rg -Fxq "${KIND_CLUSTER_NAME}" \
  || die "Kind does not report the explicitly allowed local cluster ${KIND_CLUSTER_NAME}"
KUBECTL=(kubectl --context "${KUBE_CONTEXT}")
kube_identity="$(
  "${KUBECTL[@]}" config view --minify --raw \
    -o jsonpath='{.contexts[0].context.cluster}{"\n"}{.contexts[0].context.user}{"\n"}{.clusters[0].cluster.server}{"\n"}'
)" || die "cannot inspect explicit context ${KUBE_CONTEXT}"
expected_identity="${KUBE_CONTEXT}"$'\n'"${KUBE_CONTEXT}"$'\n'
[[ "${kube_identity}" == "${expected_identity}"https://127.0.0.1:* \
  || "${kube_identity}" == "${expected_identity}"https://localhost:* ]] \
  || die "context ${KUBE_CONTEXT} is not a local Kind cluster identity with a loopback API endpoint"
"${KUBECTL[@]}" cluster-info >/dev/null \
  || die "explicit local Kind context ${KUBE_CONTEXT} is unavailable"

cleanup() {
  for pid in "${PORT_FORWARD_PIDS[@]:-}"; do
    [[ -n "${pid}" ]] || continue
    if kill -0 "${pid}" 2>/dev/null; then
      kill "${pid}" 2>/dev/null || true
      wait "${pid}" 2>/dev/null || true
    fi
  done
  "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" delete pod "${NETWORK_CHECK_POD}" \
    --ignore-not-found --wait=false >/dev/null 2>&1 || true
}

trap cleanup EXIT

echo "Waiting for Restate cluster readiness..."
"${KUBECTL[@]}" -n "${RESTATE_NAMESPACE}" wait --for=condition=Ready restatecluster/moa-restate --timeout=600s

echo "Waiting for orchestrator pods to become Ready..."
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" wait --for=condition=Ready restatedeployment/moa-orchestrator --timeout=600s
ORCHESTRATOR_SELECTOR="$(
  "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" get restatedeployment/moa-orchestrator \
    -o jsonpath='{.status.labelSelector}'
)"
if [[ -z "${ORCHESTRATOR_SELECTOR}" ]]; then
  echo "Smoke test failed: RestateDeployment did not report a pod selector" >&2
  exit 1
fi
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" wait --for=condition=Ready pod \
  -l "${ORCHESTRATOR_SELECTOR}" \
  --timeout=600s

echo "Waiting for revisioned Restate bootstrap completion..."
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" wait --for=condition=Complete job \
  -l app.kubernetes.io/name=moa-restate-bootstrap \
  --timeout=900s

echo "Waiting for edge pods to become Ready..."
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" wait --for=condition=Ready pod \
  -l app.kubernetes.io/name=moa-edge \
  --timeout=600s

echo "Proving normal replicas can reach ingress but not Restate Admin..."
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" delete pod "${NETWORK_CHECK_POD}" \
  --ignore-not-found --wait=true >/dev/null
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" run "${NETWORK_CHECK_POD}" \
  --image=curlimages/curl:8.10.1 \
  --labels=app.kubernetes.io/name=moa-orchestrator,app.kubernetes.io/part-of=moa \
  --restart=Never \
  --command -- /bin/sh -ec '
    curl -sf --max-time 10 \
      -H "content-type: application/json" \
      --data "{}" \
      http://restate.moa-restate.svc.cluster.local:8080/restate/call/Health/check \
      >/dev/null
    if curl -sf --max-time 3 \
      http://restate.moa-restate.svc.cluster.local:9070/health >/dev/null; then
      echo "normal replica identity unexpectedly reached Restate Admin" >&2
      exit 1
    fi
  ' >/dev/null
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" wait --for=jsonpath='{.status.phase}'=Succeeded \
  "pod/${NETWORK_CHECK_POD}" --timeout=60s \
  || {
    "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" logs "${NETWORK_CHECK_POD}" >&2 || true
    die "normal runtime ingress/Admin network proof failed"
  }
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" delete pod "${NETWORK_CHECK_POD}" --wait=true >/dev/null

if "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" get job/rustfs-init-snapshots-v1 >/dev/null 2>&1; then
  echo "Waiting for local RustFS bucket initialization..."
  "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" wait --for=condition=Complete job/rustfs-init-snapshots-v1 --timeout=180s
fi

echo "Port-forwarding Restate ingress/admin and MOA edge..."
"${KUBECTL[@]}" -n "${RESTATE_NAMESPACE}" port-forward svc/restate "${RESTATE_INGRESS_PORT}:8080" "${RESTATE_ADMIN_PORT}:9070" >/tmp/moa-k8s-smoke-restate-port-forward.log 2>&1 &
PORT_FORWARD_PIDS+=("$!")
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" port-forward svc/moa-edge "${EDGE_PORT}:8080" >/tmp/moa-k8s-smoke-edge-port-forward.log 2>&1 &
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
  "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" get restatedeployment/moa-orchestrator \
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
