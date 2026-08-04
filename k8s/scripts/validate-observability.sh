#!/usr/bin/env bash
# Offline contract checks for the observability stack.
#
# Everything here is a link that no compiler, no type system and no YAML parser
# checks: a metric name lives in Rust, in an alert expression, in a dashboard
# query and in a doc, and nothing connects them. A rename that misses one class
# is invisible until an alert stops firing, which is the moment nobody is
# looking.
#
# Deliberately NOT a YAML-parses check. If the honest answer to "what fails if
# this file were empty" is "nothing", the check is documentation. Every
# assertion below fails on an empty or gutted input.
#
# Requires `alloy` and `promtool`. Both are fetched by CI at pinned versions;
# see .github/workflows/ci.yml. This script never downloads anything, so it
# behaves identically offline and in CI.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
ALLOY_CONFIG="${REPO_ROOT}/k8s/observability/config.alloy"
ALLOY_DEPLOYMENT="${REPO_ROOT}/k8s/observability/20-alloy-deployment.yaml"
ALLOY_PVC="${REPO_ROOT}/k8s/observability/25-alloy-pvc.yaml"
ALLOY_RBAC="${REPO_ROOT}/k8s/observability/15-alloy-rbac.yaml"
ALERTS_DIR="${REPO_ROOT}/ops/prometheus/alerts"
LIVE_SMOKE="${REPO_ROOT}/k8s/scripts/observability-smoke.sh"
CANONICAL_DASHBOARD_DIR="${REPO_ROOT}/dashboards/grafana"
LOCAL_PHASE0_REPORT="${REPO_ROOT}/k8s/overlays/local/phase0-observability-report.sh"

# Pinned tool versions. A validator that accepts whatever binary happens to be on
# PATH cannot tell "this config is valid" from "this version of the checker did
# not understand the construct", so a local pass would not predict CI.
ALLOY_VERSION="v1.18.0"
PROMTOOL_VERSION="3.13.1"

# Every alert this deployment ships. A curated list, not a derived one, and that
# is the point: the set of alerts is a contract, so adding or removing one is a
# deliberate two-place edit. Without it, emptying a rule file passes every
# structural check that exists - the file still parses, still renders, still
# synchronizes, and silences every alert in it.
EXPECTED_ALERTS=(
  MOAActionReviewBacklogAge
  MOAAnalyticsExportLag
  MOAAuthzOutboxBacklogAge
  MOAAuthzOutboxDeadLetters
  MOABuiltinApprovalBacklogAge
  MOALLMFailoverElevated
  MOALineageDeadLettering
  MOALineageDrainTimeout
  MOALineageDropping
  MOALineageJournalBacklogAge
  MOALineageJournalDeferralsElevated
  MOALineageJournalDepthGrowing
  MOALineageRecordFailures
  MOALineageWriterDrainStuck
  MOALineageWriterFailed
  MOAOCSFAuditEventsDropped
  MOAProviderConcurrencySaturated
)

WORK_DIR="$(mktemp -d)"
trap 'rm -rf -- "${WORK_DIR}"' EXIT

die() {
  echo "Observability validation failed: $*" >&2
  exit 1
}

require_tool() {
  local tool="$1" expected="$2" install_hint="$3" observed
  command -v "${tool}" >/dev/null 2>&1 || die "${tool} is not on PATH. ${install_hint}"
  observed="$("${tool}" --version 2>&1 | head -1)"
  if [[ "${observed}" != *"${expected}"* ]]; then
    if [[ "${OBSERVABILITY_TOOLS_ALLOW_UNPINNED:-0}" == "1" ]]; then
      echo "WARNING: ${tool} is ${observed}, pinned ${expected}; continuing on request" >&2
    else
      die "${tool} version mismatch: pinned '${expected}', found '${observed}'. \
Install the pinned version, or set OBSERVABILITY_TOOLS_ALLOW_UNPINNED=1 to accept \
that a local pass may not predict CI."
    fi
  fi
}

assert_contains() {
  local haystack="$1" needle="$2" description="$3"
  [[ "${haystack}" == *"${needle}"* ]] || die "${description}"
}

assert_excludes() {
  local haystack="$1" needle="$2" description="$3"
  [[ "${haystack}" != *"${needle}"* ]] || die "${description}"
}

require_tool alloy "${ALLOY_VERSION}" \
  "Install Grafana Alloy ${ALLOY_VERSION} from https://github.com/grafana/alloy/releases."
require_tool promtool "${PROMTOOL_VERSION}" \
  "Install promtool ${PROMTOOL_VERSION} from https://github.com/prometheus/prometheus/releases."

echo "Checking the local Phase 0 telemetry contract..."
LOCAL_PHASE0_REPORT_TEXT="$(<"${LOCAL_PHASE0_REPORT}")"
assert_contains "${LOCAL_PHASE0_REPORT_TEXT}" \
  'count by (service_name, __name__) ({__name__=~"moa_.+|gen_ai_.+"})' \
  "the local report does not inventory active MOA series by service and metric"
assert_contains "${LOCAL_PHASE0_REPORT_TEXT}" \
  'otelcol_receiver_accepted_log_records' \
  "the local report does not verify direct OTLP log acceptance"
assert_contains "${LOCAL_PHASE0_REPORT_TEXT}" \
  'otelcol_exporter_send_failed_log_records' \
  "the local report does not surface OTLP log export failures"
assert_contains "${LOCAL_PHASE0_REPORT_TEXT}" \
  'count_over_time({service_name=~"moa-.+"}[1h])' \
  "the local report does not measure stored MOA logs by service"
assert_contains "${LOCAL_PHASE0_REPORT_TEXT}" \
  'sum by (service) (increase({__name__=~"traces_span.*_calls_total"}[1h]))' \
  "the local report groups Tempo span metrics by a label Tempo does not emit"
assert_excludes "${LOCAL_PHASE0_REPORT_TEXT}" "phase0_" \
  "the local report still depends on deleted custom-collector count connectors"
assert_excludes "${LOCAL_PHASE0_REPORT_TEXT}" "loki_distributor_bytes_received_total" \
  "the local report still depends on a custom backend self-scrape"

echo "Validating the Alloy collector configuration..."
# Catches broken component references and unrecognized argument names - the two
# ways an Alloy config is wrong in a manner YAML validation cannot see, because
# until this file was extracted from its ConfigMap nothing could parse it at all.
alloy validate "${ALLOY_CONFIG}" || die "alloy validate rejected ${ALLOY_CONFIG}"

echo "Checking the Alloy pipeline contract..."
ALLOY_CONFIG_TEXT="$(<"${ALLOY_CONFIG}")"
assert_contains "${ALLOY_CONFIG_TEXT}" "otelcol.exporter.prometheus" \
  "Alloy does not export OTLP metrics to Prometheus remote-write; MOA pushes metrics over OTLP and they would be received and discarded"
assert_contains "${ALLOY_CONFIG_TEXT}" "add_metric_suffixes = false" \
  "Alloy would append type/unit suffixes to MOA metric names, renaming every series out from under the alert rules and dashboards"
assert_contains "${ALLOY_CONFIG_TEXT}" "resource_to_telemetry_conversion = true" \
  "Alloy would drop service.name/deployment.environment from metric labels, leaving no way to scope a query to a service"
assert_contains "${ALLOY_CONFIG_TEXT}" 'otelcol.processor.attributes "loki_labels"' \
  "direct OTLP logs do not promote bounded resource identity into queryable Loki labels"
assert_contains "${ALLOY_CONFIG_TEXT}" 'value  = "service.name, deployment.environment, service.version"' \
  "direct OTLP logs do not expose the bounded service identity needed for log/trace joins"
assert_contains "${ALLOY_CONFIG_TEXT}" 'discovery.relabel "pod_logs"' \
  "production pod-log discovery has no application-log deduplication stage"
assert_contains "${ALLOY_CONFIG_TEXT}" '"__meta_kubernetes_pod_container_name"' \
  "production application-log deduplication is pod-wide and would drop init-container logs"
assert_contains "${ALLOY_CONFIG_TEXT}" 'regex  = "moa-edge;edge|moa-orchestrator;orchestrator"' \
  "production tails edge/orchestrator runtime stdout in addition to their direct OTLP logs"
assert_contains "${ALLOY_CONFIG_TEXT}" 'targets    = discovery.relabel.pod_logs.output' \
  "the Kubernetes log source bypasses the MOA application-log deduplication stage"
assert_contains "${ALLOY_CONFIG_TEXT}" "mimir.rules.kubernetes" \
  "no PrometheusRule synchronizer is configured, so the checked-in alert rules reach Mimir by no path at all"
assert_contains "${ALLOY_CONFIG_TEXT}" "moa.dev/rule-sync" \
  "the rule synchronizer has no rule selector; an unselected sync adopts and can overwrite rules this deployment does not own"
# The fake scrape endpoints are gone from the MOA side. A scrape target pointed at
# a port nothing binds fails silently and forever.
assert_excludes "${ALLOY_CONFIG_TEXT}" "moa-orchestrator.moa-system.svc.cluster.local:9090" \
  "Alloy still scrapes a MOA orchestrator metrics port that no longer exists"
assert_excludes "${ALLOY_CONFIG_TEXT}" "moa-edge.moa-system.svc.cluster.local:9090" \
  "Alloy still scrapes a MOA edge metrics port that no longer exists"
assert_excludes "${ALLOY_CONFIG_TEXT}" "restate.moa-restate.svc.cluster.local:5122" \
  "Alloy scrapes Restate through a load-balanced Service and blends per-node counters"
assert_contains "${ALLOY_CONFIG_TEXT}" 'discovery.relabel "restate"' \
  "Alloy does not derive Restate scrape targets from Kubernetes pod discovery"
assert_contains "${ALLOY_CONFIG_TEXT}" "__meta_kubernetes_pod_label_moa_hwuiwon_com_restate_cluster" \
  "Restate pod discovery is not restricted to the labeled MOA cluster"
assert_contains "${ALLOY_CONFIG_TEXT}" "__meta_kubernetes_pod_container_port_number" \
  "Restate pod discovery does not select one metrics target per pod"
assert_contains "${ALLOY_CONFIG_TEXT}" 'replacement   = "$1:5122"' \
  "Restate pod discovery does not target each pod's metrics port"
assert_contains "${ALLOY_CONFIG_TEXT}" 'otelcol.receiver.otlp "restate"' \
  "Restate traces do not have a dedicated OTLP receiver"
assert_contains "${ALLOY_CONFIG_TEXT}" 'endpoint = "0.0.0.0:4319"' \
  "the dedicated Restate OTLP receiver is not listening on port 4319"
assert_contains "${ALLOY_CONFIG_TEXT}" 'otelcol.processor.probabilistic_sampler "restate"' \
  "Restate traces bypass the dedicated probabilistic sampler"
assert_contains "${ALLOY_CONFIG_TEXT}" 'sampling_percentage = 1' \
  "Restate trace sampling is not fixed at one percent"
assert_contains "${ALLOY_CONFIG_TEXT}" 'forward_to      = [prometheus.relabel.restate.receiver]' \
  "Restate metrics bypass their ingestion keep-list"
assert_contains "${ALLOY_CONFIG_TEXT}" 'source_labels = ["__name__"]' \
  "the Restate metric keep-list does not filter by metric name"
RESTATE_METRIC_KEEP='up|restate_ingress_requests_total|restate_invoker_invocation_tasks_total|restate_ingress_request_duration_seconds|restate_rocksdb_estimate_live_data_size_bytes'
assert_contains "${ALLOY_CONFIG_TEXT}" "regex         = \"${RESTATE_METRIC_KEEP}\"" \
  "the Restate metric keep-list drifted from the dashboard and smoke contract"

echo "Checking the Alloy deployment contract..."
ALLOY_DEPLOYMENT_TEXT="$(<"${ALLOY_DEPLOYMENT}")"
assert_contains "${ALLOY_DEPLOYMENT_TEXT}" "replicas: 1" \
  "Alloy must run exactly one replica; a second replica splits the write-ahead log and duplicates rule reconciliation"
assert_contains "${ALLOY_DEPLOYMENT_TEXT}" "type: Recreate" \
  "Alloy must use the Recreate strategy; a RollingUpdate deadlocks on the ReadWriteOnce volume"
assert_contains "${ALLOY_DEPLOYMENT_TEXT}" "claimName: alloy-data" \
  "Alloy does not mount its persistent write-ahead log"
assert_excludes "${ALLOY_DEPLOYMENT_TEXT}" "emptyDir" \
  "Alloy buffers telemetry on an emptyDir, so every pod restart destroys undelivered data"
assert_excludes "${ALLOY_DEPLOYMENT_TEXT}" "grafana/alloy:latest" \
  "Alloy image is unpinned; a collector picking up a new version on an unrelated restart changes pipeline semantics silently"
assert_contains "${ALLOY_DEPLOYMENT_TEXT}" "image: grafana/alloy:v" \
  "Alloy image is not pinned to an exact release tag"
assert_contains "${ALLOY_DEPLOYMENT_TEXT}" "name: restate-otlp" \
  "Alloy does not expose the dedicated Restate OTLP receiver"
assert_contains "${ALLOY_DEPLOYMENT_TEXT}" "containerPort: 4319" \
  "Alloy's dedicated Restate OTLP container port is not 4319"
assert_contains "${ALLOY_DEPLOYMENT_TEXT}" "port: 4319" \
  "Alloy's Service does not expose the dedicated Restate OTLP port"

ALLOY_PVC_TEXT="$(<"${ALLOY_PVC}")"
assert_contains "${ALLOY_PVC_TEXT}" "ReadWriteOnce" "Alloy PVC is not ReadWriteOnce"
assert_contains "${ALLOY_PVC_TEXT}" "storage: 20Gi" "Alloy PVC is not the expected 20Gi buffer"

ALLOY_RBAC_TEXT="$(<"${ALLOY_RBAC}")"
assert_contains "${ALLOY_RBAC_TEXT}" "monitoring.coreos.com" \
  "Alloy has no RBAC for PrometheusRule resources, so the rule synchronizer silently syncs nothing"
assert_contains "${ALLOY_RBAC_TEXT}" "prometheusrules" \
  "Alloy has no RBAC for PrometheusRule resources, so the rule synchronizer silently syncs nothing"

RESTATE_CLUSTER_TEXT="$(<"${REPO_ROOT}/k8s/base/10-restate-cluster.yaml")"
assert_contains "${RESTATE_CLUSTER_TEXT}" "node:" \
  "Restate networkPeers does not configure access to the node metrics port"
assert_contains "${RESTATE_CLUSTER_TEXT}" "kubernetes.io/metadata.name: observability" \
  "Restate networkPeers blocks Alloy from reaching the discovered pods"
assert_contains "${RESTATE_CLUSTER_TEXT}" "app.kubernetes.io/name: alloy" \
  "Restate networkPeers does not select the Alloy collector"

echo "Checking the live smoke contract..."
LIVE_SMOKE_TEXT="$(<"${LIVE_SMOKE}")"
assert_contains "${LIVE_SMOKE_TEXT}" \
  'rotate_workload_pods "app.kubernetes.io/name=moa-edge"' \
  "the live smoke no longer rotates edge pods"
assert_contains "${LIVE_SMOKE_TEXT}" \
  '"app.kubernetes.io/name=moa-orchestrator"' \
  "the live smoke no longer rotates orchestrator pods"
assert_contains "${LIVE_SMOKE_TEXT}" '"DELETED"' \
  "the live smoke does not verify the terminal event of each old pod identity"
assert_contains "${LIVE_SMOKE_TEXT}" 'count(up{job="restate"} == 1) == 3' \
  "the live smoke does not require all three Restate pod targets"
assert_contains "${LIVE_SMOKE_TEXT}" 'PGSERVICEFILE="${DATABASE_SERVICE_FILE}"' \
  "the live smoke does not use a libpq service file"
assert_contains "${LIVE_SMOKE_TEXT}" 'PGPASSFILE="${DATABASE_PASSWORD_FILE}"' \
  "the live smoke does not use a protected pgpass file"
assert_excludes "${LIVE_SMOKE_TEXT}" 'psql "${SMOKE_DATABASE_URL}"' \
  "the live smoke exposes the database credential in psql process arguments"

echo "Checking that the exporters and the collector agree on transport..."
# MOA's endpoint-only contract uses its gRPC default. The endpoint and Alloy
# receiver live in different files, so keep their ports linked explicitly.
python3 - "${REPO_ROOT}" <<'PY' || exit 1
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
patches = {
    "orchestrator": root / "k8s/overlays/production/patches/orchestrator-observability.yaml",
    "edge": root / "k8s/overlays/production/patches/edge-observability.yaml",
}

configured = {}
for workload, path in patches.items():
    text = path.read_text(encoding="utf-8")
    endpoint = re.search(
        r"name:\s*MOA_OBSERVABILITY_OTLP_ENDPOINT\s*\n\s*value:\s*(\S+)", text
    )
    if not endpoint:
        raise SystemExit(
            f"{path} does not set MOA_OBSERVABILITY_OTLP_ENDPOINT; without an "
            "endpoint the MOA exporter remains disabled"
        )
    for redundant in (
        "MOA_OBSERVABILITY_ENABLED",
        "MOA_OBSERVABILITY_OTLP_PROTOCOL",
        "OTEL_METRIC_EXPORT_INTERVAL",
    ):
        if redundant in text:
            raise SystemExit(
                f"{path} still sets redundant {redundant}; endpoint-only defaults "
                "must stay single-sourced in MOA"
            )
    port = re.search(r":(\d+)/?$", endpoint.group(1))
    if not port:
        raise SystemExit(f"{path} OTLP endpoint {endpoint.group(1)!r} names no port")
    configured[workload] = port.group(1)

if len(set(configured.values())) != 1:
    raise SystemExit(
        "production workloads disagree about the OTLP transport: "
        f"{configured}. One of them is pointed at a receiver that is not there."
    )
port = next(iter(configured.values()))
protocol = "grpc"

alloy = (root / "k8s/observability/config.alloy").read_text(encoding="utf-8")
receiver = re.search(
    r'otelcol\.receiver\.otlp\s+"[^"]+"\s*\{(.*?)\n\}', alloy, re.DOTALL
)
if not receiver:
    raise SystemExit(
        "config.alloy declares no otelcol.receiver.otlp block, so MOA's pushed "
        "telemetry reaches nothing"
    )
block = re.search(
    rf'\b{re.escape(protocol)}\s*\{{\s*endpoint\s*=\s*"[^"]*:(\d+)"', receiver.group(1)
)
if not block:
    raise SystemExit(
        f"production exports over {protocol}, but config.alloy's OTLP receiver "
        f"declares no {protocol} listener"
    )
if block.group(1) != port:
    raise SystemExit(
        f"production exports {protocol} to port {port}, but the collector's "
        f"{protocol} receiver listens on {block.group(1)}"
    )
print(f"  OK production uses the MOA {protocol} default at :{port} and Alloy listens there")
PY

echo "Checking the production telemetry controls..."
ORCHESTRATOR_BASE_TEXT="$(<"${REPO_ROOT}/k8s/base/20-orchestrator-deployment.yaml")"
EDGE_BASE_TEXT="$(<"${REPO_ROOT}/k8s/base/50-edge-deployment.yaml")"
SCOPED_RUST_LOG='value: warn,moa_orchestrator=info,moa_brain=info,moa_edge=info,async_openai::error=off'
assert_contains "${ORCHESTRATOR_BASE_TEXT}" "${SCOPED_RUST_LOG}" \
  "orchestrator logging is not scoped to MOA targets"
assert_contains "${EDGE_BASE_TEXT}" "${SCOPED_RUST_LOG}" \
  "edge logging is not scoped to MOA targets"
assert_excludes "${ORCHESTRATOR_BASE_TEXT}" $'name: RUST_LOG\n              value: info' \
  "orchestrator still enables unscoped INFO logging"
assert_excludes "${EDGE_BASE_TEXT}" $'name: RUST_LOG\n              value: info' \
  "edge still enables unscoped INFO logging"

ORCHESTRATOR_PATCH_TEXT="$(<"${REPO_ROOT}/k8s/overlays/production/patches/orchestrator-observability.yaml")"
EDGE_PATCH_TEXT="$(<"${REPO_ROOT}/k8s/overlays/production/patches/edge-observability.yaml")"
RESTATE_PATCH_TEXT="$(<"${REPO_ROOT}/k8s/overlays/production/patches/restate-observability.yaml")"
LOCAL_ORCHESTRATOR_PATCH_TEXT="$(<"${REPO_ROOT}/k8s/overlays/local/patches/orchestrator.yaml")"
LOCAL_EDGE_PATCH_TEXT="$(<"${REPO_ROOT}/k8s/overlays/local/patches/edge.yaml")"
for patch in \
  "${ORCHESTRATOR_PATCH_TEXT}" \
  "${EDGE_PATCH_TEXT}" \
  "${LOCAL_ORCHESTRATOR_PATCH_TEXT}" \
  "${LOCAL_EDGE_PATCH_TEXT}"; do
  assert_contains "${patch}" "name: MOA_OBSERVABILITY_OTLP_ENDPOINT" \
    "a MOA workload does not configure its OTLP endpoint"
  assert_contains "${patch}" ":4317" \
    "a MOA workload does not use the default OTLP/gRPC receiver"
  assert_excludes "${patch}" "name: MOA_OBSERVABILITY_ENABLED" \
    "a MOA workload still duplicates endpoint-derived enablement"
  assert_excludes "${patch}" "name: MOA_OBSERVABILITY_OTLP_PROTOCOL" \
    "a MOA workload still duplicates the default OTLP transport"
  assert_excludes "${patch}" "name: OTEL_METRIC_EXPORT_INTERVAL" \
    "a MOA workload still overrides the code-owned metric export cadence"
done
assert_contains "${ORCHESTRATOR_PATCH_TEXT}" "name: MOA_LINEAGE_SINK" \
  "production does not enable its monitored lineage sink"
assert_contains "${ORCHESTRATOR_PATCH_TEXT}" "value: postgres" \
  "production lineage does not use the durable Postgres sink"
assert_contains "${ORCHESTRATOR_PATCH_TEXT}" "name: MOA_MEMORY_RETRIEVAL_LINEAGE_SAMPLE_RATE" \
  "production does not cap retrieval lineage detail sampling"
assert_contains "${ORCHESTRATOR_PATCH_TEXT}" 'value: "0.10"' \
  "production retrieval lineage sampling is not ten percent"
assert_contains "${RESTATE_PATCH_TEXT}" 'tracing-endpoint = "http://alloy.observability.svc.cluster.local:4319"' \
  "production Restate traces do not use the sampled receiver"
assert_contains "${RESTATE_PATCH_TEXT}" 'tracing-filter = "warn,restate_ingress_http=info,restate_invoker_impl=info"' \
  "production Restate trace filtering is not scoped"
assert_contains "${RESTATE_PATCH_TEXT}" 'log-filter = "warn,restate=info"' \
  "production Restate logging is not scoped"
assert_contains "${RESTATE_PATCH_TEXT}" "experimental-enable-vqueues = true" \
  "production's replacement Restate config drops the base vqueues setting"

echo "Checking that nothing blocks the push path to the collector..."
# MOA pushes telemetry out; nothing scrapes it. That makes egress from moa-system
# to the observability namespace load-bearing, and it is currently permitted only
# because the orchestrator NetworkPolicy declares Ingress alone. Adding an Egress
# policyType without an explicit allow for the collector would stop all telemetry
# silently: the exporter retries into its buffer and every pod stays Ready.
ORCHESTRATOR_NETPOL_TEXT="$(<"${REPO_ROOT}/k8s/base/26-orchestrator-network-policy.yaml")"
assert_excludes "${ORCHESTRATOR_NETPOL_TEXT}" "- Egress" \
  "the orchestrator NetworkPolicy now restricts egress; OTLP push to the collector must be explicitly allowed or telemetry stops with no error anywhere"

echo "Checking alert rules with promtool..."
RULE_FILES=("${ALERTS_DIR}"/*.yaml)
# kustomization.yaml lives in the same directory and is not a rule file.
FILTERED_RULE_FILES=()
for file in "${RULE_FILES[@]}"; do
  [[ "$(basename "${file}")" == "kustomization.yaml" ]] && continue
  FILTERED_RULE_FILES+=("${file}")
done
[[ "${#FILTERED_RULE_FILES[@]}" -gt 0 ]] || die "no alert rule files found under ${ALERTS_DIR}"

for file in "${FILTERED_RULE_FILES[@]}"; do
  extracted="${WORK_DIR}/$(basename "${file}")"
  # promtool reads plain Prometheus rule files; the checked-in resources are
  # PrometheusRule custom resources, whose `spec` IS that file. Extracting it
  # here means the rules promtool checks are byte-for-byte the rules Mimir will
  # evaluate, rather than a second copy that can drift.
  python3 - "${file}" "${extracted}" "${WORK_DIR}/referenced-metrics.txt" <<'PY'
import re
import sys

import yaml

source, target, metrics_out = sys.argv[1], sys.argv[2], sys.argv[3]
documents = [
    document
    for document in yaml.safe_load_all(open(source, encoding="utf-8"))
    if document
]
if len(documents) != 1 or documents[0].get("kind") != "PrometheusRule":
    raise SystemExit(f"{source} must hold exactly one PrometheusRule resource")
resource = documents[0]
labels = resource.get("metadata", {}).get("labels", {})
if labels.get("moa.dev/rule-sync") != "mimir":
    raise SystemExit(
        f"{source} is missing the moa.dev/rule-sync=mimir label, so Alloy's rule "
        "selector will not adopt it and these alerts reach Mimir by no path"
    )
spec = resource.get("spec", {})
groups = spec.get("groups") or []
if not groups:
    raise SystemExit(f"{source} declares no rule groups")
referenced = set()
for group in groups:
    if not group.get("rules"):
        raise SystemExit(f"{source} group {group.get('name')!r} declares no rules")
    for rule in group["rules"]:
        # Metric names are collected from the EXPRESSION only. Scanning the raw
        # file text instead would treat prose as a reference: this file's header
        # names four deliberately deleted metrics to explain where those alerts
        # went, and a text scan reports them as broken links forever.
        referenced.update(re.findall(r"moa_[a-z0-9_]+", rule.get("expr", "")))
with open(metrics_out, "a", encoding="utf-8") as handle:
    for metric in sorted(referenced):
        handle.write(f"{metric}\n")
with open(target, "w", encoding="utf-8") as handle:
    yaml.safe_dump(spec, handle, sort_keys=False)
PY
  promtool check rules --lint=all "${extracted}" >/dev/null \
    || die "promtool rejected the rules extracted from ${file}"
done

echo "Cross-checking alert metric names against the Rust source..."
# The check nothing else performs. A metric renamed in Rust leaves the alert
# expression parsing perfectly and matching no series forever, and neither
# promtool nor kubeconform nor the compiler can see it.
REFERENCED_METRICS="${WORK_DIR}/referenced-metrics.txt"
[[ -s "${REFERENCED_METRICS}" ]] \
  || die "no metric names were extracted from any alert expression"
MISSING_METRICS=()
while read -r metric; do
  if ! grep -rqF "\"${metric}\"" "${REPO_ROOT}/crates"; then
    MISSING_METRICS+=("${metric}")
  fi
done < <(sort -u "${REFERENCED_METRICS}")
if [[ "${#MISSING_METRICS[@]}" -gt 0 ]]; then
  die "alert rules reference metrics that no crate emits: ${MISSING_METRICS[*]}"
fi
echo "  $(sort -u "${REFERENCED_METRICS}" | wc -l | tr -d ' ') referenced metrics all emitted"

echo "Checking the declared alert set..."
DECLARED_ALERTS="$(grep -ohE '^ *- alert: [A-Za-z0-9]+' "${FILTERED_RULE_FILES[@]}" \
  | awk '{print $3}' | sort -u | tr '\n' ' ')"
EXPECTED_SORTED="$(printf '%s\n' "${EXPECTED_ALERTS[@]}" | sort -u | tr '\n' ' ')"
if [[ "${DECLARED_ALERTS}" != "${EXPECTED_SORTED}" ]]; then
  # Print the DIFFERENCE, not both full lists. Two seventeen-name lines differing
  # in one entry is a diff the reader has to compute by eye, and the reader is
  # whoever is looking at a red gate with no local repro.
  printf '%s\n' ${EXPECTED_SORTED} | sort -u >"${WORK_DIR}/expected-alerts"
  printf '%s\n' ${DECLARED_ALERTS} | sort -u >"${WORK_DIR}/declared-alerts"
  die "$(printf 'the shipped alert set changed.\n  no longer declared: %s\n  newly declared:     %s\nUpdate EXPECTED_ALERTS in this script in the same change that adds or removes an alert.' \
    "$(comm -23 "${WORK_DIR}/expected-alerts" "${WORK_DIR}/declared-alerts" | tr '\n' ' ')" \
    "$(comm -13 "${WORK_DIR}/expected-alerts" "${WORK_DIR}/declared-alerts" | tr '\n' ' ')")"
fi

echo "Cross-checking emitted metrics against repository consumers..."
[[ -d "${CANONICAL_DASHBOARD_DIR}" ]] \
  || die "canonical Grafana dashboard directory is missing: ${CANONICAL_DASHBOARD_DIR}"
GRAFANA_SYNC_SCRIPT_TEXT="$(<"${REPO_ROOT}/scripts/observability/sync-grafana-dashboards.sh")"
GRAFANA_SYNC_WORKFLOW_TEXT="$(<"${REPO_ROOT}/.github/workflows/sync-grafana-dashboards.yml")"
assert_contains "${GRAFANA_SYNC_SCRIPT_TEXT}" "dashboards/grafana" \
  "Grafana sync does not publish the canonical dashboard directory"
assert_contains "${GRAFANA_SYNC_WORKFLOW_TEXT}" "sync-grafana-dashboards.sh" \
  "the dashboard sync workflow does not invoke the canonical publisher"
python3 - "${REPO_ROOT}" "${CANONICAL_DASHBOARD_DIR}" <<'PY' || exit 1
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
dashboard_dir = pathlib.Path(sys.argv[2])

# These low-volume degradation and incident signals are intentionally retained
# for ad-hoc backend queries even when no checked-in dashboard or alert consumes
# them. Keeping the exception list here makes every other unconsumed instrument
# fail closed.
keepers = {
    "moa_tool_failure_total",
    "moa_tool_call_duration_seconds",
    "moa_tool_reprovision_total",
    "moa_turn_outcomes_total",
    "moa_memory_operations_total",
    "moa_knowledge_sync_runs_total",
    "moa_retrieval_leg_degraded_total",
    "moa_retrieval_window_abstained_total",
    "moa_retrieval_evidence_floor_dropped_total",
    "moa_retrieval_enrichment_dropped_total",
    "moa_retrieval_lexical_backend_total",
    "moa_provider_coordination_degraded_total",
    "moa_provider_coordination_rejected_total",
    "moa_llm_retry_budget_exhausted_total",
    "moa_lineage_malformed_total",
    "moa_cron_bootstrap_configure_failures_total",
    "moa_turn_admission_decisions_total",
    "moa_session_message_admission_decisions_total",
}

metric_token = re.compile(
    r"(?:moa|perf_gate)_[a-z0-9_]+|gen_ai(?:\.[a-z0-9_]+)+|gen_ai_[a-z0-9_]+"
)
constant = re.compile(
    r"(?:pub\s+)?const\s+([A-Z][A-Z0-9_]*)\s*:\s*&str\s*=\s*"
    r'"((?:moa|perf_gate)_[a-z0-9_]+|gen_ai(?:\.[a-z0-9_]+)+|gen_ai_[a-z0-9_]+)"'
)
emission = re.compile(
    r"(?<!describe_)\b(?:metrics::)?(?:counter|gauge|histogram)!\s*\(\s*"
    r'(?:"((?:moa|perf_gate)_[a-z0-9_]+|gen_ai(?:\.[a-z0-9_]+)+|gen_ai_[a-z0-9_]+)"'
    r"|([A-Z][A-Z0-9_]*))"
)


def canonical(name: str) -> str:
    """Normalize OTLP dotted names and Prometheus histogram series names."""
    name = name.replace(".", "_")
    for suffix in ("_bucket", "_sum", "_count", "_created"):
        if name.endswith(suffix):
            return name[: -len(suffix)]
    return name


source_files = sorted((root / "crates").glob("**/src/**/*.rs"))
source_text = {path: path.read_text(encoding="utf-8") for path in source_files}
constants = {}
for text in source_text.values():
    constants.update(constant.findall(text))

emitted = set()
for path, text in source_text.items():
    for literal, constant_name in emission.findall(text):
        name = literal or constants.get(constant_name)
        if not name:
            raise SystemExit(
                f"{path} emits metric through unresolved constant {constant_name}; "
                "teach validate-observability.sh how to resolve it"
            )
        emitted.add(canonical(name))

consumer_files = []
consumer_files.extend(sorted(dashboard_dir.glob("*.json")))
consumer_files.extend(sorted((root / "ops/prometheus/alerts").glob("*.yaml")))
consumer_files.extend(sorted((root / "crates/moa-loadtest").glob("**/*")))
consumer_files.extend(
    path
    for path in sorted((root / "docs").glob("**/*"))
    if path.is_file() and "engineering-discipline/plans" not in path.as_posix()
)

referenced = set()
for path in consumer_files:
    if not path.is_file():
        continue
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        continue
    referenced.update(canonical(name) for name in metric_token.findall(text))

canonical_keepers = {canonical(name) for name in keepers}
stale_keepers = canonical_keepers - emitted
if stale_keepers:
    raise SystemExit(
        "ad-hoc metric keeper allowlist names instruments that are no longer "
        f"emitted: {', '.join(sorted(stale_keepers))}"
    )

unreferenced = emitted - referenced - canonical_keepers
if unreferenced:
    raise SystemExit(
        "metrics are emitted but have no alert, canonical dashboard, docs, or "
        "load-test consumer and are not deliberate ad-hoc keepers: "
        + ", ".join(sorted(unreferenced))
    )

print(
    f"  OK {len(emitted)} emitted metrics are consumed or explicitly retained; "
    f"canonical dashboards: {dashboard_dir.relative_to(root)}"
)
PY

echo "Observability validation OK"
