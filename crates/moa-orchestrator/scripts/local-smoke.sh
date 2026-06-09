#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd)"
RESTATE_DATA_DIR="${RESTATE_DATA_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/moa-restate-dev.XXXXXX")}"
RESTATE_PID=""
ORCH_PID=""

cleanup() {
  if [[ -n "${ORCH_PID}" ]] && kill -0 "${ORCH_PID}" 2>/dev/null; then
    kill "${ORCH_PID}" 2>/dev/null || true
    wait "${ORCH_PID}" 2>/dev/null || true
  fi
  if [[ -n "${RESTATE_PID}" ]] && kill -0 "${RESTATE_PID}" 2>/dev/null; then
    kill "${RESTATE_PID}" 2>/dev/null || true
    wait "${RESTATE_PID}" 2>/dev/null || true
  fi
  if [[ -d "${RESTATE_DATA_DIR}" ]]; then
    rm -rf "${RESTATE_DATA_DIR}"
  fi
}

trap cleanup EXIT

cd "${REPO_ROOT}"

: "${MOA_DATABASE_URL:?set MOA_DATABASE_URL before running local-smoke.sh}"
: "${MOA_RESTATE_ADMIN_URL:?set MOA_RESTATE_ADMIN_URL before running local-smoke.sh}"
: "${MOA_RESTATE_INGRESS_URL:?set MOA_RESTATE_INGRESS_URL before running local-smoke.sh}"

echo "Starting restate-server in background..."
restate-server --node-name local --base-dir "${RESTATE_DATA_DIR}" &
RESTATE_PID=$!
sleep 2

echo "Starting moa-orchestrator..."
RUST_LOG="${RUST_LOG:-info}" \
cargo run -p moa-orchestrator -- --port 10020 --health-port 10021 &
ORCH_PID=$!
sleep 3

echo "Registering deployment..."
curl --fail --silent --show-error \
  -X POST "${MOA_RESTATE_ADMIN_URL}/deployments" \
  -H "content-type: application/json" \
  --data '{"uri":"http://localhost:10020"}'

echo "Calling Health/ping..."
curl --fail --silent --show-error -X POST http://localhost:10010/Health/ping
echo

echo "Calling Health/version..."
curl --fail --silent --show-error -X POST http://localhost:10010/Health/version
echo
