#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

LIVE=0
RUN_PROVIDERS=0
RUN_LONG_EVAL=0

usage() {
  cat <<'USAGE'
Usage: scripts/run-clean-e2e.sh [--live] [--providers] [--long-eval]

Runs E2E tests against isolated state:
  - temporary Postgres database on the local compose Postgres service
  - temporary restate-server data directory with random ports
  - temporary OpenFGA bootstrap env file

Options:
  --live       Run ignored Restate E2E tests. Requires MOA_RUN_LIVE_E2E=1.
  --providers  Also run live provider/query-rewrite checks. Requires --live and
               the provider tests' own opt-in flags, such as
               MOA_RUN_LIVE_PROVIDER_TESTS=1.
  --long-eval  Also run ignored long-conversation eval smoke. Requires --live.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --live)
      LIVE=1
      ;;
    --providers)
      RUN_PROVIDERS=1
      ;;
    --long-eval)
      RUN_LONG_EVAL=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

truthy() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "required command not found: $1" >&2
    exit 127
  fi
}

run() {
  echo
  echo ">> $*"
  "$@"
}

run_without_provider_keys() {
  echo
  echo ">> env -u ANTHROPIC_API_KEY -u OPENAI_API_KEY -u GOOGLE_API_KEY -u COHERE_API_KEY $*"
  env \
    -u ANTHROPIC_API_KEY \
    -u OPENAI_API_KEY \
    -u GOOGLE_API_KEY \
    -u COHERE_API_KEY \
    "$@"
}

wait_for_http() {
  local url="$1"
  local label="$2"
  local attempts="${3:-60}"
  for _ in $(seq 1 "${attempts}"); do
    if curl -fsS "${url}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "timed out waiting for ${label}: ${url}" >&2
  return 1
}

wait_for_postgres() {
  for _ in $(seq 1 60); do
    if docker compose exec -T postgres pg_isready -U moa_owner -d moa >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "timed out waiting for compose Postgres" >&2
  return 1
}

wait_for_restate_ports() {
  local log_file="$1"
  local ingress_port=""
  local admin_port=""

  for _ in $(seq 1 120); do
    ingress_port="$(
      sed -n 's/.*Ingress HTTP listening.*server.port=\([0-9][0-9]*\).*/\1/p' "${log_file}" | tail -n 1
    )"
    admin_port="$(
      sed -n 's/.*Admin API starting on: http:\/\/[^:]*:\([0-9][0-9]*\)\/.*/\1/p' "${log_file}" | tail -n 1
    )"

    if [[ -n "${ingress_port}" && -n "${admin_port}" ]] \
      && curl -fsS "http://127.0.0.1:${admin_port}/health" >/dev/null 2>&1; then
      RESTATE_INGRESS_URL="http://127.0.0.1:${ingress_port}"
      RESTATE_ADMIN_URL="http://127.0.0.1:${admin_port}"
      export RESTATE_INGRESS_URL RESTATE_ADMIN_URL
      return 0
    fi
    sleep 1
  done

  echo "timed out waiting for restate-server random ports; tail follows:" >&2
  tail -n 80 "${log_file}" >&2 || true
  return 1
}

if [[ "${LIVE}" -eq 1 ]] && ! truthy "${MOA_RUN_LIVE_E2E:-}"; then
  echo "refusing to run live E2E without MOA_RUN_LIVE_E2E=1" >&2
  exit 2
fi
if [[ "${RUN_PROVIDERS}" -eq 1 && "${LIVE}" -ne 1 ]]; then
  echo "--providers requires --live" >&2
  exit 2
fi
if [[ "${RUN_LONG_EVAL}" -eq 1 && "${LIVE}" -ne 1 ]]; then
  echo "--long-eval requires --live" >&2
  exit 2
fi

require_cmd cargo
require_cmd curl
require_cmd docker
require_cmd restate-server
if [[ "${LIVE}" -eq 1 ]]; then
  require_cmd cargo-nextest
fi

cd "${REPO_ROOT}"

RUN_ID="${MOA_CLEAN_E2E_RUN_ID:-$(date +%Y%m%d%H%M%S)-$$}"
RUN_SAFE_ID="$(printf '%s' "${RUN_ID}" | tr -c 'A-Za-z0-9_' '_')"
RUN_SHORT_ID="$(printf '%s' "${RUN_SAFE_ID}" | cut -c1-20)"
TMP_PARENT="${MOA_CLEAN_E2E_TMPDIR:-/tmp}"
TMP_ROOT="$(mktemp -d "${TMP_PARENT%/}/me2e.XXXXXX")"
RESTATE_DIR="${TMP_ROOT}/restate"
RESTATE_LOG="${TMP_ROOT}/restate.log"
FGA_ENV="${TMP_ROOT}/fga.env"
ORCH_LOG="${TMP_ROOT}/orchestrator.log"
DB_NAME="moa_e2e_${RUN_SAFE_ID}"
DB_URL="postgres://moa_owner:dev@127.0.0.1:10040/${DB_NAME}"
RESTATE_PID=""
ORCH_PID=""
STARTED_COMPOSE=0
DB_CREATED=0

cleanup() {
  local status=$?

  if [[ -n "${ORCH_PID}" ]] && kill -0 "${ORCH_PID}" 2>/dev/null; then
    kill "${ORCH_PID}" 2>/dev/null || true
    wait "${ORCH_PID}" 2>/dev/null || true
  fi
  if [[ -n "${RESTATE_PID}" ]] && kill -0 "${RESTATE_PID}" 2>/dev/null; then
    kill "${RESTATE_PID}" 2>/dev/null || true
    wait "${RESTATE_PID}" 2>/dev/null || true
  fi
  if [[ "${DB_CREATED}" -eq 1 ]]; then
    docker compose exec -T postgres psql -U moa_owner -d postgres \
      -v ON_ERROR_STOP=1 \
      -c "DROP DATABASE IF EXISTS ${DB_NAME}" >/dev/null 2>&1 || true
  fi
  if [[ -d "${TMP_ROOT}" ]]; then
    rm -rf "${TMP_ROOT}"
  fi
  if [[ "${STARTED_COMPOSE}" -eq 1 ]]; then
    if ! truthy "${MOA_CLEAN_E2E_KEEP_COMPOSE:-}"; then
      docker compose down >/dev/null 2>&1 || true
    fi
  fi

  exit "${status}"
}
trap cleanup EXIT

mkdir -p "${TMP_ROOT}" "${RESTATE_DIR}"

if [[ -z "$(docker compose ps -q 2>/dev/null)" ]]; then
  STARTED_COMPOSE=1
fi

run docker compose up -d --build postgres openfga moa-pii-service
wait_for_postgres
run "${REPO_ROOT}/scripts/wait-for-fga.sh"
wait_for_http "http://127.0.0.1:10050/healthz" "PII sidecar"

run docker compose exec -T postgres psql -U moa_owner -d postgres \
  -v ON_ERROR_STOP=1 \
  -c "CREATE DATABASE ${DB_NAME} OWNER moa_owner"
DB_CREATED=1

run env \
  "MOA_AUTHZ_OPENFGA_URL=${MOA_AUTHZ_OPENFGA_URL:-http://localhost:10030}" \
  "MOA_AUTHZ_OPENFGA_PRESHARED_KEY=${MOA_AUTHZ_OPENFGA_PRESHARED_KEY:-localdev-preshared-key-do-not-use-in-prod}" \
  "MOA_FGA_ENV_OUTPUT=${FGA_ENV}" \
  cargo run -q -p moa-fga-bootstrap

set -a
# shellcheck disable=SC1090
. "${FGA_ENV}"
set +a

echo
echo ">> starting ephemeral restate-server"
restate-server \
  --node-name "e2e-${RUN_SHORT_ID}" \
  --base-dir "${RESTATE_DIR}" \
  --bind-ip 127.0.0.1 \
  --advertised-host 127.0.0.1 \
  --use-random-ports true \
  --log-format compact \
  --log-disable-ansi-codes true \
  >"${RESTATE_LOG}" 2>&1 &
RESTATE_PID=$!
wait_for_restate_ports "${RESTATE_LOG}"

export MOA_TEST_POSTGRES_URL="${DB_URL}"
export MOA_DATABASE_URL="${DB_URL}"
export MOA_RESTATE_INGRESS_URL="${RESTATE_INGRESS_URL}"
export MOA_RESTATE_ADMIN_URL="${RESTATE_ADMIN_URL}"
export MOA_RESTATE_DEPLOYMENT_HOST="127.0.0.1"
export MOA_PII_SERVICE_URL="${MOA_PII_SERVICE_URL:-http://127.0.0.1:10050}"

run cargo test -p moa-orchestrator --tests --locked -- --test-threads=1
run cargo test -p moa-orchestrator --locked --features provider-overrides,skill-learning skill_learning -- --test-threads=1
run cargo test -p moa-brain --features eval-harness --test brain_turn_cache_replay_db_memory --locked
run cargo test -p moa-eval --test golden_eval --locked

if [[ "${LIVE}" -eq 1 ]]; then
  run cargo nextest run -p moa-orchestrator --locked --features provider-overrides,integration,skill-learning --profile restate-service-e2e --run-ignored ignored-only

  run cargo build -p moa-orchestrator --bin moa-orchestrator-bin --features provider-overrides,skill-learning --locked

  ORCH_PORT="${MOA_CLEAN_E2E_ORCH_PORT:-19180}"
  ORCH_HEALTH_PORT="${MOA_CLEAN_E2E_ORCH_HEALTH_PORT:-19181}"
  ORCH_SCIM_PORT="${MOA_CLEAN_E2E_ORCH_SCIM_PORT:-19182}"
  export MOA_RESTATE_DEPLOYMENT_URI="http://127.0.0.1:${ORCH_PORT}"

  echo
  echo ">> starting shared orchestrator for lifecycle smoke tests"
  env -u COHERE_API_KEY \
    -u ANTHROPIC_API_KEY \
    -u OPENAI_API_KEY \
    -u GOOGLE_API_KEY \
    RUST_LOG="${RUST_LOG:-warn}" \
    MOA_PROVIDERS_OVERRIDE="mock:${RUN_SAFE_ID}" \
    MOA_LOCAL_MEMORY_DIR="${TMP_ROOT}/memory" \
    MOA_LOCAL_SANDBOX_DIR="${TMP_ROOT}/sandbox" \
    MOA_LOCAL_DOCKER_ENABLED=false \
    target/debug/moa-orchestrator-bin \
      --port "${ORCH_PORT}" \
      --health-port "${ORCH_HEALTH_PORT}" \
      --scim-port "${ORCH_SCIM_PORT}" \
      >"${ORCH_LOG}" 2>&1 &
  ORCH_PID=$!
  wait_for_http "http://127.0.0.1:${ORCH_HEALTH_PORT}/_health/live" "shared orchestrator"

  run curl -fsS \
    -X POST "${RESTATE_ADMIN_URL}/deployments" \
    -H "content-type: application/json" \
    --data "{\"uri\":\"http://127.0.0.1:${ORCH_PORT}\"}"

  run_without_provider_keys cargo nextest run -p moa-orchestrator --locked --features provider-overrides,integration,skill-learning --profile orchestrator-service-e2e --run-ignored ignored-only

  if [[ "${RUN_PROVIDERS}" -eq 1 ]]; then
    if ! truthy "${MOA_RUN_LIVE_PROVIDER_TESTS:-}"; then
      echo "refusing provider live checks without MOA_RUN_LIVE_PROVIDER_TESTS=1" >&2
      exit 2
    fi
    run cargo nextest run -p moa-orchestrator --locked --features provider-overrides,integration,skill-learning --profile provider-e2e --run-ignored ignored-only
    run cargo nextest run -p moa-providers --locked --profile provider-e2e --run-ignored ignored-only
    run cargo nextest run -p moa-brain --locked --profile provider-e2e --run-ignored ignored-only
  fi

  if [[ "${RUN_LONG_EVAL}" -eq 1 ]]; then
    run cargo test -p moa-eval --test long_conversation_smoke_eval --locked -- --ignored --test-threads=1 --nocapture
  fi
fi

echo
echo "clean E2E run completed"
