#!/usr/bin/env bash
set -euo pipefail

SYSTEM_NAMESPACE="${SYSTEM_NAMESPACE:-moa-system}"
RESTATE_NAMESPACE="${RESTATE_NAMESPACE:-moa-restate}"
OBS_NAMESPACE="${OBS_NAMESPACE:-observability}"
MODEL="${SMOKE_MODEL:-claude-sonnet-4-6}"
PROMPT="${SMOKE_PROMPT:-What is 2+2? Just answer with the number.}"
PORT_FORWARD_PID=""

cleanup() {
  if [[ -n "${PORT_FORWARD_PID}" ]] && kill -0 "${PORT_FORWARD_PID}" 2>/dev/null; then
    kill "${PORT_FORWARD_PID}" 2>/dev/null || true
    wait "${PORT_FORWARD_PID}" 2>/dev/null || true
  fi
}

trap cleanup EXIT

echo "Waiting for Alloy rollout..."
kubectl -n "${OBS_NAMESPACE}" rollout status deployment/alloy --timeout=600s

echo "Waiting for orchestrator pods..."
kubectl -n "${SYSTEM_NAMESPACE}" wait --for=condition=Ready pod \
  -l app.kubernetes.io/name=moa-orchestrator \
  --timeout=600s

echo "Port-forwarding Restate ingress..."
kubectl -n "${RESTATE_NAMESPACE}" port-forward svc/restate 8080:8080 >/tmp/moa-otel-smoke-port-forward.log 2>&1 &
PORT_FORWARD_PID=$!
sleep 5

NOW="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
SESSION_META="$(cat <<EOF
{"id":"$(uuidgen | tr '[:upper:]' '[:lower:]')","workspace_id":"observability-smoke","user_id":"smoke-user","title":"Observability smoke","status":"created","platform":"desktop","platform_channel":null,"model":"${MODEL}","created_at":"${NOW}","updated_at":"${NOW}","completed_at":null,"parent_session_id":null,"total_input_tokens":0,"total_input_tokens_uncached":0,"total_input_tokens_cache_write":0,"total_input_tokens_cache_read":0,"total_output_tokens":0,"total_cost_cents":0,"event_count":0,"last_checkpoint_seq":null}
EOF
)"

echo "Creating smoke session..."
SESSION_ID="$(
  curl -sf -X POST http://127.0.0.1:8080/SessionStore/create_session \
    -H "Content-Type: application/json" \
    -d "${SESSION_META}" | tr -d '"\n'
)"

curl -sf -X POST http://127.0.0.1:8080/SessionStore/init_session_vo \
  -H "Content-Type: application/json" \
  -d "{\"session_id\":\"${SESSION_ID}\",\"meta\":${SESSION_META}}" >/dev/null

echo "Posting prompt to generate traces, metrics, and logs..."
curl -sf -X POST "http://127.0.0.1:8080/Session/${SESSION_ID}/post_message" \
  -H "Content-Type: application/json" \
  -d "{\"text\":\"${PROMPT}\",\"attachments\":[]}" >/dev/null

echo "Waiting 45 seconds for telemetry export..."
sleep 45

cat <<EOF
Observability smoke traffic generated.

Tempo:
  Search for span attribute moa.session.id="${SESSION_ID}"

Loki:
  Query {service="moa-orchestrator"} |= "${SESSION_ID}"

Prometheus:
  Check moa_turns_total, moa_turn_latency_seconds, and moa_llm_requests_total

Grafana dashboards:
  - MOA Session Health
  - MOA LLM Gateway
  - MOA Restate Internals
  - MOA Sandbox Fleet
EOF
