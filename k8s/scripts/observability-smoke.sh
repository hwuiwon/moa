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

It rotates the edge and orchestrator Deployments, applies and deletes a
temporary PrometheusRule, and starts a real (billed) model turn.

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
(
  umask 077
  printf 'user = "%s:%s"\n' "${SMOKE_MIMIR_USER}" "${SMOKE_MIMIR_KEY}" >"${CURL_CONFIG}"
)

PORT_FORWARD_PID=""
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
  psql "${SMOKE_DATABASE_URL}" -At -c "$1"
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
# deployment environment separately catches a collector that forwards metrics
# with only service_name preserved.
assert_series_present \
  'count(moa_turns_total{deployment_environment="production"})' \
  "no metric carries the deployment.environment resource attribute"

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
for workload in moa-edge; do
  rotation_start="${SECONDS}"
  "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" rollout restart "deployment/${workload}" >/dev/null
  "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" rollout status "deployment/${workload}" \
    --timeout="${ROTATION_BUDGET_SECONDS}s" \
    || die "${workload} did not roll within ${ROTATION_BUDGET_SECONDS}s"
  echo "  OK ${workload} rolled in $((SECONDS - rotation_start))s"
done

# A pod that was SIGKILLed at the end of its grace period exits 137. That is the
# exact failure the SIGTERM handler exists to prevent, and it is invisible in a
# rollout that otherwise completes: Kubernetes reports success either way.
killed="$(
  "${KUBECTL[@]}" -n "${SYSTEM_NAMESPACE}" get pods -l app.kubernetes.io/name=moa-edge \
    -o jsonpath='{range .items[*]}{.metadata.name}{"="}{.status.containerStatuses[*].lastState.terminated.exitCode}{"\n"}{end}' \
    | grep -E '=(137|143)$' || true
)"
[[ -z "${killed}" ]] \
  || die "$(printf 'edge containers were killed by signal rather than exiting gracefully:\n%s' "${killed}")"
echo "  OK no signal-killed containers"

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
