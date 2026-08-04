#!/usr/bin/env bash
set -euo pipefail

# This helper is intentionally pinned. It never reads, changes, or falls back to
# the current kubectl context, so running it cannot query development or GKE.
readonly KUBE_CONTEXT="kind-moa-local"
readonly KUBE_NAMESPACE="moa-system"
readonly LGTM_SERVICE="moa-lgtm"
readonly GRAFANA_URL="http://127.0.0.1:3000"
readonly LOKI_URL="http://127.0.0.1:3100"
readonly TEMPO_URL="http://127.0.0.1:3200"
readonly PROMETHEUS_URL="http://127.0.0.1:9090"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

fail() {
  echo "Phase 0 report failed: $*" >&2
  exit 1
}

for command_name in kubectl curl jq; do
  command -v "${command_name}" >/dev/null || {
    echo "required command is missing: ${command_name}" >&2
    exit 1
  }
done

forward_log="$(mktemp -t moa-phase0-port-forward.XXXXXX)"
forward_pid=""
service_account_id=""

cleanup() {
  if [[ -n "${service_account_id}" ]] && [[ -n "${forward_pid}" ]] \
    && kill -0 "${forward_pid}" 2>/dev/null; then
    curl --silent --user admin:admin --request DELETE \
      "${GRAFANA_URL}/api/serviceaccounts/${service_account_id}" >/dev/null || true
  fi
  if [[ -n "${forward_pid}" ]]; then
    kill "${forward_pid}" 2>/dev/null || true
    wait "${forward_pid}" 2>/dev/null || true
  fi
  rm -f "${forward_log}"
}
trap cleanup EXIT INT TERM

kubectl --context "${KUBE_CONTEXT}" --namespace "${KUBE_NAMESPACE}" \
  port-forward "service/${LGTM_SERVICE}" \
  3000:3000 3100:3100 3200:3200 9090:9090 \
  >"${forward_log}" 2>&1 &
forward_pid=$!

for _ in $(seq 1 30); do
  if curl --silent --fail "${PROMETHEUS_URL}/-/ready" >/dev/null \
    && curl --silent --fail "${GRAFANA_URL}/api/health" >/dev/null \
    && curl --silent --fail "${LOKI_URL}/ready" >/dev/null \
    && curl --silent --fail "${TEMPO_URL}/ready" >/dev/null; then
    break
  fi
  if ! kill -0 "${forward_pid}" 2>/dev/null; then
    cat "${forward_log}" >&2
    exit 1
  fi
  sleep 1
done
curl --silent --show-error --fail "${PROMETHEUS_URL}/-/ready" >/dev/null
curl --silent --show-error --fail "${GRAFANA_URL}/api/health" >/dev/null
curl --silent --show-error --fail "${LOKI_URL}/ready" >/dev/null
curl --silent --show-error --fail "${TEMPO_URL}/ready" >/dev/null

# Exercise the same canonical sync path used for hosted Grafana without asking
# Kustomize to load files outside this overlay's root. The short-lived local
# service account is removed by the EXIT trap and its token is never printed.
service_account_response="$(curl --silent --show-error --fail \
  --user admin:admin \
  --request POST \
  --header 'Content-Type: application/json' \
  --data "{\"name\":\"moa-local-phase0-$$\",\"role\":\"Admin\"}" \
  "${GRAFANA_URL}/api/serviceaccounts")"
service_account_id="$(jq --exit-status --raw-output \
  '.id | select(type == "number")' <<<"${service_account_response}")" \
  || fail "Grafana service-account creation returned no numeric id"
service_account_token="$(curl --silent --show-error --fail \
  --user admin:admin \
  --request POST \
  --header 'Content-Type: application/json' \
  --data "{\"name\":\"moa-local-phase0-$$\"}" \
  "${GRAFANA_URL}/api/serviceaccounts/${service_account_id}/tokens" \
  | jq --exit-status --raw-output '.key | select(type == "string" and length > 0)')" \
  || fail "Grafana service-account token creation returned no token"
env \
  GRAFANA_URL="${GRAFANA_URL}" \
  GRAFANA_SERVICE_ACCOUNT_TOKEN="${service_account_token}" \
  bash "${repo_root}/scripts/observability/sync-grafana-dashboards.sh"
unset service_account_token

prometheus_query() {
  local label="$1"
  local expression="$2"
  local response
  if ! response="$(curl --silent --show-error --fail --get \
    --data-urlencode "query=${expression}" \
    "${PROMETHEUS_URL}/api/v1/query")"; then
    fail "Prometheus query '${label}' request failed: ${expression}"
  fi
  if ! jq --exit-status '.status == "success" and (.data.result | type == "array")' \
    >/dev/null <<<"${response}"; then
    echo "Prometheus query '${label}' failed: ${expression}" >&2
    jq '.' <<<"${response}" >&2 || echo "${response}" >&2
    exit 1
  fi
  printf '%s\n' "${response}"
}

loki_query() {
  local label="$1"
  local expression="$2"
  local response
  if ! response="$(curl --silent --show-error --fail --get \
    --data-urlencode "query=${expression}" \
    "${LOKI_URL}/loki/api/v1/query")"; then
    fail "Loki query '${label}' request failed: ${expression}"
  fi
  if ! jq --exit-status '.status == "success" and (.data.result | type == "array")' \
    >/dev/null <<<"${response}"; then
    echo "Loki query '${label}' failed: ${expression}" >&2
    jq '.' <<<"${response}" >&2 || echo "${response}" >&2
    exit 1
  fi
  printf '%s\n' "${response}"
}

json_get() {
  local label="$1"
  shift
  local response
  if ! response="$(curl --silent --show-error --fail "$@")"; then
    fail "${label} request failed"
  fi
  if ! jq --exit-status '.' >/dev/null <<<"${response}"; then
    echo "${label} returned invalid JSON" >&2
    echo "${response}" >&2
    exit 1
  fi
  printf '%s\n' "${response}"
}

report_prometheus_vector() {
  local measurement="$1"
  local evidence="$2"
  local unit="$3"
  local expression="$4"
  prometheus_query "${measurement}" "${expression}" \
    | jq --exit-status \
      --arg measurement "${measurement}" \
      --arg evidence "${evidence}" \
      --arg unit "${unit}" \
      --arg expression "${expression}" \
      '{measurement: $measurement, evidence: $evidence, unit: $unit,
        expression: $expression,
        status: (if (.data.result | length) == 0 then "no_data" else "measured" end),
        series: [.data.result[] | {
          labels: .metric,
          value: ((.value[1] | tonumber?) // .value[1])
        }]}'
}

report_loki_vector() {
  local measurement="$1"
  local evidence="$2"
  local unit="$3"
  local expression="$4"
  loki_query "${measurement}" "${expression}" \
    | jq --exit-status \
      --arg measurement "${measurement}" \
      --arg evidence "${evidence}" \
      --arg unit "${unit}" \
      --arg expression "${expression}" \
      '{measurement: $measurement, evidence: $evidence, unit: $unit,
        expression: $expression,
        status: (if (.data.result | length) == 0 then "no_data" else "measured" end),
        series: [.data.result[] | {
          labels: .metric,
          value: ((.value[1] | tonumber?) // .value[1])
        }]}'
}

echo "Local Phase 0 observability report"
echo "kubectl context: ${KUBE_CONTEXT}"
echo "window: trailing 1h for stored logs and completed spans"

echo
echo "Active MOA metric series"
report_prometheus_vector \
  "moa_active_series_by_service_and_metric" \
  "exact current Prometheus snapshot" \
  "series" \
  'count by (service_name, __name__) ({__name__=~"moa_.+|gen_ai_.+"})'

echo
echo "Collector acceptance and export failures"
report_prometheus_vector \
  "collector_otlp_metric_points_accepted_total" \
  "collector receiver counter since process start" \
  "metric_points" \
  'sum by (receiver) ({__name__=~"otelcol_receiver_accepted_metric_points(_total)?",receiver=~"otlp.*"})'
report_prometheus_vector \
  "collector_otlp_spans_accepted_total" \
  "collector receiver counter since process start" \
  "spans" \
  'sum by (receiver) ({__name__=~"otelcol_receiver_accepted_spans(_total)?",receiver=~"otlp.*"})'
report_prometheus_vector \
  "collector_otlp_log_records_accepted_total" \
  "collector receiver counter since process start" \
  "log_records" \
  'sum by (receiver) ({__name__=~"otelcol_receiver_accepted_log_records(_total)?",receiver=~"otlp.*"})'
report_prometheus_vector \
  "collector_metric_export_failures_total" \
  "collector exporter counter since process start; no_data means the zero-valued family was not emitted" \
  "metric_points" \
  'sum by (exporter) ({__name__=~"otelcol_exporter_send_failed_metric_points(_total)?"})'
report_prometheus_vector \
  "collector_span_export_failures_total" \
  "collector exporter counter since process start; no_data means the zero-valued family was not emitted" \
  "spans" \
  'sum by (exporter) ({__name__=~"otelcol_exporter_send_failed_spans(_total)?"})'
report_prometheus_vector \
  "collector_log_export_failures_total" \
  "collector exporter counter since process start; no_data means the zero-valued family was not emitted" \
  "log_records" \
  'sum by (exporter) ({__name__=~"otelcol_exporter_send_failed_log_records(_total)?"})'

echo
echo "Direct MOA OTLP logs"
report_loki_vector \
  "moa_log_lines_by_service_1h" \
  "exact stored MOA log-entry count" \
  "log_lines" \
  'sum by (service_name) (count_over_time({service_name=~"moa-.+"}[1h]))'
report_loki_vector \
  "moa_uncompressed_log_body_bytes_by_service_1h" \
  "exact uncompressed stored MOA log-body bytes; excludes index, metadata, compression, and replication overhead" \
  "bytes" \
  'sum by (service_name) (bytes_over_time({service_name=~"moa-.+"}[1h]))'

echo
echo "Tempo completed spans and search inventory"
report_prometheus_vector \
  "tempo_completed_spans_by_service_1h" \
  "Tempo span-metrics counter grouped by service, reset-adjusted and boundary-extrapolated by Prometheus" \
  "spans" \
  'sum by (service) (increase({__name__=~"traces_span.*_calls_total"}[1h]))'
json_get "Tempo search inventory" --get --data-urlencode 'limit=20' \
  "${TEMPO_URL}/api/search" \
  | jq '{
      returned_traces: ((.traces // []) | length),
      traces_by_root_service: ((.traces // [])
        | sort_by(.rootServiceName)
        | group_by(.rootServiceName)
        | map({service: (.[0].rootServiceName // "unknown_service"), traces: length})),
      search_metrics: (.metrics // {})
    }'

echo
echo "Provisioned dashboard inventory"
json_get "Grafana dashboard inventory" --user admin:admin --get \
  --data-urlencode 'type=dash-db' "${GRAFANA_URL}/api/search" \
  | jq '{count: length, dashboards: map({uid, title, folderTitle})}'
