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

echo "Starting restate-server in background..."
restate-server --node-name local --base-dir "${RESTATE_DATA_DIR}" &
RESTATE_PID=$!
sleep 2

echo "Starting moa-orchestrator..."
POSTGRES_URL="${POSTGRES_URL:-postgres://unused}" \
RUST_LOG="${RUST_LOG:-info}" \
cargo run -p moa-orchestrator -- --port 9080 &
ORCH_PID=$!
sleep 3

echo "Registering deployment..."
restate deployments register http://localhost:9080 --yes

echo "Calling Health/ping..."
curl --fail --silent --show-error -X POST http://localhost:8080/Health/ping
echo

echo "Calling Health/version..."
curl --fail --silent --show-error -X POST http://localhost:8080/Health/version
echo
