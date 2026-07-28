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

ALLOY_PVC_TEXT="$(<"${ALLOY_PVC}")"
assert_contains "${ALLOY_PVC_TEXT}" "ReadWriteOnce" "Alloy PVC is not ReadWriteOnce"
assert_contains "${ALLOY_PVC_TEXT}" "storage: 20Gi" "Alloy PVC is not the expected 20Gi buffer"

ALLOY_RBAC_TEXT="$(<"${ALLOY_RBAC}")"
assert_contains "${ALLOY_RBAC_TEXT}" "monitoring.coreos.com" \
  "Alloy has no RBAC for PrometheusRule resources, so the rule synchronizer silently syncs nothing"
assert_contains "${ALLOY_RBAC_TEXT}" "prometheusrules" \
  "Alloy has no RBAC for PrometheusRule resources, so the rule synchronizer silently syncs nothing"

echo "Checking that the exporters and the collector agree on transport..."
# The link nothing else checks. Production names an OTLP endpoint and protocol in
# a kustomize patch; the collector declares its receivers in a different language
# in a different directory. Move either one and telemetry stops with no error
# anywhere: the exporter retries into its buffer, every pod stays Ready, and the
# only symptom is a dashboard that goes quiet.
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
    protocol = re.search(
        r"name:\s*MOA_OBSERVABILITY_OTLP_PROTOCOL\s*\n\s*value:\s*(\S+)", text
    )
    if not endpoint or not protocol:
        raise SystemExit(
            f"{path} does not set both MOA_OBSERVABILITY_OTLP_ENDPOINT and "
            "_PROTOCOL; a half-configured exporter falls back to the SDK default "
            "collector, which is localhost"
        )
    port = re.search(r":(\d+)/?$", endpoint.group(1))
    if not port:
        raise SystemExit(f"{path} OTLP endpoint {endpoint.group(1)!r} names no port")
    configured[workload] = (protocol.group(1), port.group(1))

if len(set(configured.values())) != 1:
    raise SystemExit(
        "production workloads disagree about the OTLP transport: "
        f"{configured}. One of them is pointed at a receiver that is not there."
    )
protocol, port = next(iter(configured.values()))

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
print(f"  OK production exports {protocol} to :{port} and the collector listens there")
PY

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

echo "Observability validation OK"
