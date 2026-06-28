#!/usr/bin/env bash
set -euo pipefail

SYSTEM_NAMESPACE="${SYSTEM_NAMESPACE:-moa-system}"
RESTATE_NAMESPACE="${RESTATE_NAMESPACE:-moa-restate}"
EDGE_PORT="${EDGE_PORT:-10000}"
RESTATE_INGRESS_PORT="${RESTATE_INGRESS_PORT:-10010}"
RESTATE_ADMIN_PORT="${RESTATE_ADMIN_PORT:-10011}"
PORT_FORWARD_PIDS=()

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
