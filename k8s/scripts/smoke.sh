#!/usr/bin/env bash
set -euo pipefail

SYSTEM_NAMESPACE="${SYSTEM_NAMESPACE:-moa-system}"
RESTATE_NAMESPACE="${RESTATE_NAMESPACE:-moa-restate}"
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

echo "Waiting for Restate cluster readiness..."
kubectl -n "${RESTATE_NAMESPACE}" wait --for=condition=Ready restatecluster/moa-restate --timeout=600s

echo "Waiting for orchestrator pods to become Ready..."
kubectl -n "${SYSTEM_NAMESPACE}" wait --for=condition=Ready pod \
  -l app.kubernetes.io/name=moa-orchestrator \
  --timeout=600s

echo "Port-forwarding Restate ingress and admin API..."
kubectl -n "${RESTATE_NAMESPACE}" port-forward svc/restate 8080:8080 9070:9070 >/tmp/moa-k8s-smoke-port-forward.log 2>&1 &
PORT_FORWARD_PID=$!
sleep 5

echo "Calling Health/ping through Restate ingress..."
curl -sf -X POST http://127.0.0.1:8080/Health/ping | tee /dev/stderr | grep -q "pong"

NOW="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
SESSION_META="$(cat <<EOF
{"id":"$(uuidgen | tr '[:upper:]' '[:lower:]')","workspace_id":"k8s-smoke","user_id":"smoke-user","title":"Kubernetes smoke","status":"created","platform":"desktop","platform_channel":null,"model":"${MODEL}","created_at":"${NOW}","updated_at":"${NOW}","completed_at":null,"parent_session_id":null,"total_input_tokens":0,"total_input_tokens_uncached":0,"total_input_tokens_cache_write":0,"total_input_tokens_cache_read":0,"total_output_tokens":0,"total_cost_cents":0,"event_count":0,"last_checkpoint_seq":null}
EOF
)"

echo "Creating a test session..."
SESSION_ID="$(
  curl -sf -X POST http://127.0.0.1:8080/SessionStore/create_session \
    -H "Content-Type: application/json" \
    -d "${SESSION_META}" | tr -d '"\n'
)"

echo "Initializing Session VO state..."
curl -sf -X POST http://127.0.0.1:8080/SessionStore/init_session_vo \
  -H "Content-Type: application/json" \
  -d "{\"session_id\":\"${SESSION_ID}\",\"meta\":${SESSION_META}}" >/dev/null

echo "Posting a smoke-test message..."
curl -sf -X POST "http://127.0.0.1:8080/Session/${SESSION_ID}/post_message" \
  -H "Content-Type: application/json" \
  -d "{\"text\":\"${PROMPT}\",\"attachments\":[]}" >/dev/null

echo "Polling session status..."
STATUS=""
for _attempt in $(seq 1 30); do
  STATUS="$(
    curl -sf -X POST "http://127.0.0.1:8080/Session/${SESSION_ID}/status" | tr -d '"\n'
  )"
  if [[ -n "${STATUS}" ]] && [[ "${STATUS}" != "created" ]] && [[ "${STATUS}" != "running" ]]; then
    break
  fi
  sleep 2
done

echo "Final session status: ${STATUS}"
test -n "${STATUS}"

echo "Smoke test OK"
