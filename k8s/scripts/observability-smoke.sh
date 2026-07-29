#!/usr/bin/env bash
# Live, cluster-mutating observability smoke test. Opt-in and fail-closed.
#
# This asserts. The previous version generated traffic and printed a list of
# queries for a human to run, which meant a completely dead telemetry pipeline
# produced a successful run and a tidy summary. Every check below fails the
# script.
#
# It rotates pods, applies a temporary PrometheusRule, and reads production
# telemetry backends, so it is gated on MOA_RUN_LIVE_OBSERVABILITY_SMOKE=1 AND on
# an explicitly named kube context. The context guard is not ceremony: a
# developer's current context is routinely some unrelated cluster, and every
# mutating command here would have been aimed at it.
#
# No secret is ever echoed. Credentials are passed to curl via --config on a
# 0600 temp file, never on a command line where `ps` and shell tracing can read
# them.
set -euo pipefail

if [[ "${MOA_RUN_LIVE_OBSERVABILITY_SMOKE:-0}" != "1" ]]; then
  cat >&2 <<'MSG'
Observability smoke is opt-in and mutates a live cluster.

It rotates every pod in the edge Deployment and orchestrator RestateDeployment,
applies and deletes a temporary PrometheusRule, and starts a real (billed)
model turn.

To run it:
  MOA_RUN_LIVE_OBSERVABILITY_SMOKE=1 \
  SMOKE_KUBE_CONTEXT=<context> \
  SMOKE_MIMIR_QUERY_URL=<https://.../prometheus> \
  SMOKE_MIMIR_RULER_URL=<https://.../prometheus> \
  SMOKE_MIMIR_USER=<user> SMOKE_MIMIR_KEY=<key> \
  SMOKE_DATABASE_URL=<postgres://...> \
  ./k8s/scripts/observability-smoke.sh
MSG
  exit 1
fi

SYSTEM_NAMESPACE="${SYSTEM_NAMESPACE:-moa-system}"
RESTATE_NAMESPACE="${RESTATE_NAMESPACE:-moa-restate}"
OBS_NAMESPACE="${OBS_NAMESPACE:-observability}"
MODEL="${SMOKE_MODEL:-claude-sonnet-4-6}"
PROMPT="${SMOKE_PROMPT:-What is 2+2? Just answer with the number.}"
INGRESS_PORT="${SMOKE_INGRESS_PORT:-18080}"
ROTATION_BUDGET_SECONDS="${SMOKE_ROTATION_BUDGET_SECONDS:-180}"
TELEMETRY_BUDGET_SECONDS="${SMOKE_TELEMETRY_BUDGET_SECONDS:-300}"
RULE_SYNC_BUDGET_SECONDS="${SMOKE_RULE_SYNC_BUDGET_SECONDS:-600}"

die() {
  echo "Observability smoke FAILED: $*" >&2
  exit 1
}

require_env() {
  local name="$1"
  [[ -n "${!name:-}" ]] || die "${name} is required"
}

for required in \
  SMOKE_KUBE_CONTEXT \
  SMOKE_MIMIR_QUERY_URL \
  SMOKE_MIMIR_RULER_URL \
  SMOKE_MIMIR_USER \
  SMOKE_MIMIR_KEY \
  SMOKE_DATABASE_URL; do
  require_env "${required}"
done

for tool in kubectl curl jq psql uuidgen; do
  command -v "${tool}" >/dev/null 2>&1 || die "${tool} is not on PATH"
done

KUBECTL=(kubectl --context "${SMOKE_KUBE_CONTEXT}")
"${KUBECTL[@]}" version >/dev/null 2>&1 \
  || die "cannot reach kube context ${SMOKE_KUBE_CONTEXT}"

WORK_DIR="$(mktemp -d)"
chmod 700 "${WORK_DIR}"
CURL_CONFIG="${WORK_DIR}/curl.conf"
DATABASE_SERVICE_FILE="${WORK_DIR}/pg_service.conf"
DATABASE_PASSWORD_FILE="${WORK_DIR}/pgpass"
(
  umask 077
  printf 'user = "%s:%s"\n' "${SMOKE_MIMIR_USER}" "${SMOKE_MIMIR_KEY}" >"${CURL_CONFIG}"
)
python3 - "${DATABASE_SERVICE_FILE}" "${DATABASE_PASSWORD_FILE}" <<'PY'
import os
import pathlib
import re
import sys
import urllib.parse

value = os.environ["SMOKE_DATABASE_URL"]
if "\n" in value or "\r" in value:
    raise SystemExit("SMOKE_DATABASE_URL must be a single line")
parsed = urllib.parse.urlsplit(value)
if parsed.scheme not in {"postgres", "postgresql"}:
    raise SystemExit("SMOKE_DATABASE_URL must use the postgres or postgresql scheme")
if not parsed.hostname or not parsed.path.lstrip("/"):
    raise SystemExit("SMOKE_DATABASE_URL must include a host and database")

parameters = {
    "host": parsed.hostname,
    "port": str(parsed.port or 5432),
    "dbname": urllib.parse.unquote(parsed.path.lstrip("/")),
}
password = None
if parsed.username is not None:
    parameters["user"] = urllib.parse.unquote(parsed.username)
if parsed.password is not None:
    password = urllib.parse.unquote(parsed.password)
for key, item in urllib.parse.parse_qsl(parsed.query, keep_blank_values=True):
    if not re.fullmatch(r"[a-z_]+", key):
        raise SystemExit(f"invalid libpq query parameter: {key!r}")
    if key == "password":
        password = item
    else:
        parameters[key] = item


for key, item in parameters.items():
    if "\n" in item or "\r" in item:
        raise SystemExit(f"SMOKE_DATABASE_URL contains a multiline {key} value")


# Keep connection routing in a libpq service file and the credential in a
# pgpass file. psql's argv contains only the non-secret service name.
service_path = pathlib.Path(sys.argv[1])
service_path.write_text(
    "[moa_observability_smoke]\n"
    + "".join(f"{key}={item}\n" for key, item in parameters.items()),
    encoding="utf-8",
)
service_path.chmod(0o600)


def pgpass_escape(item: str) -> str:
    return item.replace("\\", "\\\\").replace(":", "\\:")


password_path = pathlib.Path(sys.argv[2])
password_path.write_text(
    (
        ":".join(
            pgpass_escape(item)
            for item in (
                parameters["host"],
                parameters["port"],
                parameters["dbname"],
                parameters.get("user", "*"),
                password,
            )
        )
        + "\n"
        if password is not None
        else ""
    ),
    encoding="utf-8",
)
password_path.chmod(0o600)
PY
unset SMOKE_DATABASE_URL SMOKE_MIMIR_KEY

PORT_FORWARD_PID=""
ROTATION_WATCH_PIDS=()
CANARY_APPLIED=0
CANARY_RULE_NAME="moa-observability-smoke-canary-$(uuidgen | tr '[:upper:]' '[:lower:]' | cut -c1-8)"
CANARY_ALERT_NAME="MOAObservabilitySmokeCanary"

cleanup() {
  local status=$?
  # Cleanup runs on success AND on failure: a canary alert rule left behind in
  # Mimir is a permanently firing alert nobody owns, and a leaked port-forward
  # holds a local port for the next run to collide with.
  if [[ "${CANARY_APPLIED}" == "1" ]]; then
    "${KUBECTL[@]}" -n "${OBS_NAMESPACE}" delete prometheusrule "${CANARY_RULE_NAME}" \
      --ignore-not-found --wait=false >/dev/null 2>&1 || true
  fi
  if [[ -n "${PORT_FORWARD_PID}" ]] && kill -0 "${PORT_FORWARD_PID}" 2>/dev/null; then
    kill "${PORT_FORWARD_PID}" 2>/dev/null || true
    wait "${PORT_FORWARD_PID}" 2>/dev/null || true
  fi
  for pid in "${ROTATION_WATCH_PIDS[@]}"; do
    if kill -0 "${pid}" 2>/dev/null; then
      kill "${pid}" 2>/dev/null || true
      wait "${pid}" 2>/dev/null || true
    fi
  done
  rm -rf -- "${WORK_DIR}"
  exit "${status}"
}
trap cleanup EXIT

# Runs one instant PromQL query and prints the raw result vector.
mimir_query() {
  local query="$1" response
  response="$(
    curl -sS --fail --max-time 30 --config "${CURL_CONFIG}" \
      --get "${SMOKE_MIMIR_QUERY_URL}/api/v1/query" \
      --data-urlencode "query=${query}" 2>&1
  )" || die "Mimir query failed for: ${query}"
  jq -e '.status == "success"' <<<"${response}" >/dev/null \
    || die "$(printf 'Mimir returned a non-success status for %s:\n%s' "${query}" "${response}")"
  jq -c '.data.result' <<<"${response}"
}

# Asserts a query returns at least one sample, printing what it did return.
assert_series_present() {
  local query="$1" description="$2" result
  result="$(mimir_query "${query}")"
  [[ "$(jq 'length' <<<"${result}")" -gt 0 ]] \
    || die "$(printf '%s\n  query:    %s\n  returned: %s' "${description}" "${query}" "${result}")"
  echo "  OK ${query}"
}

# Asserts a query returns no samples above zero, printing the offending series.
assert_no_failure_signal() {
  local query="$1" description="$2" result
  result="$(mimir_query "${query}")"
  [[ "$(jq 'length' <<<"${result}")" -eq 0 ]] \
    || die "$(printf '%s\n  query:    %s\n  returned: %s' "${description}" "${query}" "${result}")"
  echo "  OK no ${description}"
}

psql_scalar() {
  PGSERVICEFILE="${DATABASE_SERVICE_FILE}" PGPASSFILE="${DATABASE_PASSWORD_FILE}" \
    psql -w "service=moa_observability_smoke" -At -c "$1"
}

ready_pod_count() {
  local selector="$1"
  "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" get pods -l "${selector}" -o json \
    | jq '[
        .items[]
        | select(.metadata.deletionTimestamp == null)
        | select(any(.status.conditions[]?; .type == "Ready" and .status == "True"))
      ] | length'
}

wait_for_ready_pod_count() {
  local selector="$1" expected="$2" workload="$3"
  local deadline=$((SECONDS + ROTATION_BUDGET_SECONDS)) ready=0
  while ((SECONDS < deadline)); do
    ready="$(ready_pod_count "${selector}")"
    [[ "${ready}" -ge "${expected}" ]] && return 0
    sleep 2
  done
  die "${workload} has ${ready}/${expected} non-terminating Ready pods after rotation"
}

# Rotate one pod at a time so capacity stays available and every old process can
# be observed through its final Kubernetes DELETED watch event. A successful
# rollout alone cannot distinguish a graceful exit from SIGKILL.
rotate_workload_pods() {
  local selector="$1" container="$2" workload="$3"
  local snapshot desired old_identity_file
  snapshot="$("${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" get pods -l "${selector}" -o json)"
  desired="$(jq '.items | length' <<<"${snapshot}")"
  [[ "${desired}" -gt 0 ]] || die "${workload} selector ${selector} matched no pods"
  old_identity_file="${WORK_DIR}/${workload}-old-uids"
  jq -r '.items[].metadata.uid' <<<"${snapshot}" >"${old_identity_file}"

  while IFS=$'\t' read -r pod_name pod_uid resource_version; do
    local watch_file watch_pid deadline deleted_event exit_code
    watch_file="${WORK_DIR}/${pod_name}-watch.json"
    "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" get pod "${pod_name}" \
      --watch-only \
      --output-watch-events \
      --resource-version="${resource_version}" \
      -o json >"${watch_file}" 2>"${watch_file}.stderr" &
    watch_pid=$!
    ROTATION_WATCH_PIDS+=("${watch_pid}")

    "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" delete pod "${pod_name}" \
      --wait=false >/dev/null \
      || die "could not begin graceful deletion of ${pod_name}"

    deadline=$((SECONDS + ROTATION_BUDGET_SECONDS))
    deleted_event=0
    while ((SECONDS < deadline)); do
      if jq -se --arg uid "${pod_uid}" \
        'any(.[]; .type == "DELETED" and .object.metadata.uid == $uid)' \
        "${watch_file}" >/dev/null 2>&1; then
        deleted_event=1
        break
      fi
      sleep 1
    done
    [[ "${deleted_event}" == "1" ]] \
      || die "${pod_name} produced no terminal DELETED event within ${ROTATION_BUDGET_SECONDS}s"

    kill "${watch_pid}" 2>/dev/null || true
    wait "${watch_pid}" 2>/dev/null || true
    exit_code="$(
      jq -sr --arg uid "${pod_uid}" --arg container "${container}" '
        [
          .[]
          | select(.type == "DELETED" and .object.metadata.uid == $uid)
          | .object.status.containerStatuses[]?
          | select(.name == $container)
          | .state.terminated.exitCode
        ][-1] // empty
      ' "${watch_file}"
    )"
    [[ -n "${exit_code}" ]] \
      || die "${pod_name} was deleted without a terminal state for container ${container}"
    [[ "${exit_code}" == "0" ]] \
      || die "${pod_name}/${container} exited ${exit_code} during graceful rotation"

    wait_for_ready_pod_count "${selector}" "${desired}" "${workload}"
    echo "  OK ${pod_name} (${pod_uid}) exited 0 and was replaced"
  done < <(
    jq -r '
      .items[]
      | [.metadata.name, .metadata.uid, .metadata.resourceVersion]
      | @tsv
    ' <<<"${snapshot}"
  )

  local current_uids stale_uids
  current_uids="$(
    "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" get pods -l "${selector}" \
      -o json | jq -r '.items[].metadata.uid'
  )"
  stale_uids="$(comm -12 <(sort "${old_identity_file}") <(sort <<<"${current_uids}"))"
  [[ -z "${stale_uids}" ]] \
    || die "${workload} still runs pre-rotation pod identities: ${stale_uids}"
}

echo "== Preflight: workloads healthy before anything is rotated"
"${KUBECTL[@]}" -n "${OBS_NAMESPACE}" rollout status deployment/alloy --timeout=600s \
  || die "Alloy is not rolled out"
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" rollout status deployment/moa-edge --timeout=600s \
  || die "moa-edge is not rolled out"
"${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" wait --for=condition=Ready pod \
  -l app.kubernetes.io/name=moa-orchestrator --timeout=600s \
  || die "orchestrator pods are not Ready"

ALLOY_REPLICAS="$("${KUBECTL[@]}" -n "${OBS_NAMESPACE}" get deployment/alloy -o jsonpath='{.spec.replicas}')"
[[ "${ALLOY_REPLICAS}" == "1" ]] \
  || die "Alloy is running ${ALLOY_REPLICAS} replicas; two collectors split the write-ahead log and duplicate rule reconciliation"

echo "== Marked traffic through the real ingress"
"${KUBECTL[@]}" -n "${RESTATE_NAMESPACE}" port-forward svc/restate "${INGRESS_PORT}:8080" \
  >"${WORK_DIR}/port-forward.log" 2>&1 &
PORT_FORWARD_PID=$!
for _attempt in $(seq 1 30); do
  curl -sf --max-time 2 "http://127.0.0.1:${INGRESS_PORT}/" >/dev/null 2>&1 && break
  sleep 1
done

NOW="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
SESSION_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
TENANT_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
SESSION_META="$(cat <<EOF
{"id":"${SESSION_ID}","tenant_id":"${TENANT_ID}","title":"Observability smoke","status":"created","channel":"chat","model":"${MODEL}","created_at":"${NOW}","updated_at":"${NOW}","completed_at":null,"parent_session_id":null,"total_input_tokens":0,"total_input_tokens_uncached":0,"total_input_tokens_cache_write":0,"total_input_tokens_cache_read":0,"total_output_tokens":0,"total_cost_cents":0,"event_count":0,"last_checkpoint_seq":null}
EOF
)"

SESSION_ID="$(
  curl -sf --max-time 30 -X POST "http://127.0.0.1:${INGRESS_PORT}/restate/call/SessionStore/create_session" \
    -H "Content-Type: application/json" -d "${SESSION_META}" | tr -d '"\n'
)" || die "could not create the smoke session"
[[ -n "${SESSION_ID}" ]] || die "session creation returned an empty id"

curl -sf --max-time 30 -X POST "http://127.0.0.1:${INGRESS_PORT}/restate/call/SessionStore/init_session_vo" \
  -H "Content-Type: application/json" \
  -d "{\"session_id\":\"${SESSION_ID}\",\"meta\":${SESSION_META}}" >/dev/null \
  || die "could not initialize the smoke session object"

# The client message id is this script's retry identity: rerunning the smoke test
# with the same session and id replays the original admission instead of buying a
# second turn.
curl -sf --max-time 120 -X POST "http://127.0.0.1:${INGRESS_PORT}/restate/call/Session/${SESSION_ID}/start_turn" \
  -H "Content-Type: application/json" \
  -d "{\"client_message_id\":\"observability-smoke:${SESSION_ID}:0\",\"user_message\":\"${PROMPT}\",\"attachments\":[]}" \
  >/dev/null || die "the smoke turn did not start"
echo "  session ${SESSION_ID}"

echo "== Metrics reached Mimir with an intact resource identity"
deadline=$((SECONDS + TELEMETRY_BUDGET_SECONDS))
observed=""
while ((SECONDS < deadline)); do
  observed="$(mimir_query 'count(moa_turns_total{service_name="moa-orchestrator"})')"
  [[ "$(jq 'length' <<<"${observed}")" -gt 0 ]] && break
  sleep 10
done
[[ "$(jq 'length' <<<"${observed}")" -gt 0 ]] || die "$(cat <<EOF
no moa_turns_total series carrying service_name="moa-orchestrator" appeared in Mimir within ${TELEMETRY_BUDGET_SECONDS}s.
returned: ${observed}
Either OTLP metrics are not reaching Alloy, or the resource attributes are being
dropped on the way through it (otelcol.exporter.prometheus
resource_to_telemetry_conversion), or the metric was renamed by suffixing
(add_metric_suffixes).
EOF
)"
echo "  OK moa_turns_total{service_name=\"moa-orchestrator\"}"

# The resource identity is the join key between traces and metrics. Asserting the
# instance ID catches replicas that collapse into one Prometheus series;
# deployment environment separately catches a collector that forwards metrics
# with only service_name preserved.
assert_series_present \
  'count(moa_turns_total{service_name="moa-orchestrator",service_instance_id!=""})' \
  "no orchestrator metric carries service.instance.id"
assert_series_present \
  'count(moa_turns_total{deployment_environment="production"})' \
  "no metric carries the deployment.environment resource attribute"
assert_series_present \
  'count(up{job="restate"} == 1) == 3' \
  "Alloy does not report exactly three healthy Restate pod scrape targets"

echo "== Canary rule reaches Mimir through the cluster's only synchronizer"
cat >"${WORK_DIR}/canary.yaml" <<EOF
apiVersion: monitoring.coreos.com/v1
kind: PrometheusRule
metadata:
  name: ${CANARY_RULE_NAME}
  namespace: ${OBS_NAMESPACE}
  labels:
    moa.dev/rule-sync: mimir
    moa.dev/temporary: observability-smoke
spec:
  groups:
    - name: ${CANARY_RULE_NAME}
      interval: 60s
      rules:
        - alert: ${CANARY_ALERT_NAME}
          expr: vector(0) > 0
          annotations:
            summary: Temporary observability smoke canary; delete if you find it
EOF
"${KUBECTL[@]}" apply -f "${WORK_DIR}/canary.yaml" >/dev/null || die "could not apply the canary rule"
CANARY_APPLIED=1

deadline=$((SECONDS + RULE_SYNC_BUDGET_SECONDS))
synced=0
while ((SECONDS < deadline)); do
  if curl -sS --fail --max-time 30 --config "${CURL_CONFIG}" \
    "${SMOKE_MIMIR_RULER_URL}/api/v1/rules" 2>/dev/null \
    | grep -qF "${CANARY_ALERT_NAME}"; then
    synced=1
    break
  fi
  sleep 15
done
[[ "${synced}" == "1" ]] \
  || die "the canary rule never reached Mimir within ${RULE_SYNC_BUDGET_SECONDS}s; the PrometheusRule synchronizer is not running, lacks RBAC, or its rule selector does not match moa.dev/rule-sync=mimir"
echo "  OK canary rule synchronized"

"${KUBECTL[@]}" -n "${OBS_NAMESPACE}" delete prometheusrule "${CANARY_RULE_NAME}" >/dev/null
CANARY_APPLIED=0
deadline=$((SECONDS + RULE_SYNC_BUDGET_SECONDS))
removed=0
while ((SECONDS < deadline)); do
  if ! curl -sS --fail --max-time 30 --config "${CURL_CONFIG}" \
    "${SMOKE_MIMIR_RULER_URL}/api/v1/rules" 2>/dev/null \
    | grep -qF "${CANARY_ALERT_NAME}"; then
    removed=1
    break
  fi
  sleep 15
done
[[ "${removed}" == "1" ]] \
  || die "the canary rule was deleted from the cluster but is still in Mimir; the synchronizer adds rules and never removes them, so every deleted alert lives forever"
echo "  OK canary rule removed"

echo "== Lineage committed to Postgres and drained from the journal"
deadline=$((SECONDS + TELEMETRY_BUDGET_SECONDS))
final_rows=0
while ((SECONDS < deadline)); do
  final_rows="$(psql_scalar "SELECT count(*) FROM analytics.turn_lineage WHERE session_id = '${SESSION_ID}'")"
  [[ "${final_rows}" -gt 0 ]] && break
  sleep 10
done
[[ "${final_rows}" -gt 0 ]] \
  || die "no lineage rows were written for session ${SESSION_ID} within ${TELEMETRY_BUDGET_SECONDS}s"
echo "  OK ${final_rows} final lineage rows"

deadline=$((SECONDS + TELEMETRY_BUDGET_SECONDS))
pending="unknown"
while ((SECONDS < deadline)); do
  pending="$(psql_scalar "SELECT count(*) FROM analytics.lineage_journal WHERE session_id = '${SESSION_ID}'")"
  [[ "${pending}" == "0" ]] && break
  sleep 10
done
[[ "${pending}" == "0" ]] \
  || die "the lineage journal still holds ${pending} rows for session ${SESSION_ID}; acceptance is committing but delivery is not draining"
echo "  OK journal drained"

echo "== Graceful rotation of both workloads"
rotation_start="${SECONDS}"
rotate_workload_pods "app.kubernetes.io/name=moa-edge" edge moa-edge
rotate_workload_pods \
  "app.kubernetes.io/name=moa-orchestrator" \
  orchestrator \
  moa-orchestrator
echo "  OK both workloads rotated in $((SECONDS - rotation_start))s"

echo "== Audit and lineage survived the rotation"
audit_rows="$(psql_scalar "SELECT count(*) FROM moa.security_events WHERE time > now() - interval '1 hour'")"
[[ "${audit_rows}" -gt 0 ]] \
  || die "no security audit events were persisted in the last hour; the audit writer is dropping or never draining"
echo "  OK ${audit_rows} audit events persisted"

surviving="$(psql_scalar "SELECT count(*) FROM analytics.turn_lineage WHERE session_id = '${SESSION_ID}'")"
[[ "${surviving}" -ge "${final_rows}" ]] \
  || die "lineage rows for ${SESSION_ID} went from ${final_rows} to ${surviving} across the rotation"
echo "  OK lineage intact across rotation"

echo "== No new failure signals"
assert_no_failure_signal \
  'increase(moa_lineage_dropped_total[15m]) > 0' \
  "lineage drops"
assert_no_failure_signal \
  'increase(moa_lineage_drain_timeout_total[15m]) > 0' \
  "lineage drain timeouts"
assert_no_failure_signal \
  'moa_lineage_writer_state{state="failed"} > 0' \
  "failed lineage writers"
assert_no_failure_signal \
  'increase(moa_ocsf_audit_events_dropped_total[15m]) > 0' \
  "dropped audit events"

echo
echo "Observability smoke OK (session ${SESSION_ID})"
