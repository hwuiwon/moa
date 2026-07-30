#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

# Incremental artifacts regrow by tens of gigabytes per gate run — this script
# builds into both the shared target dir and its own fixture target dir, and
# neither cache survives long enough to pay for itself here.
export CARGO_INCREMENTAL=0

LIVE=0
RUN_PROVIDERS=0
RUN_LONG_EVAL=0
RUN_BEHAVIOR_LAB_LIVE=0

usage() {
  cat <<'USAGE'
Usage: scripts/run-clean-e2e.sh [--live] [--providers] [--long-eval]
                                [--behavior-lab-live]

Runs E2E tests against isolated state:
  - temporary Postgres database on the local compose Postgres service
  - temporary restate-server data directory with random ports
  - temporary OpenFGA bootstrap env file

Environment:
  MOA_CLEAN_E2E_TEST_THREADS  Nextest test threads for the fast orchestrator
                              preflight lane. Defaults to 4. DB lanes use the
                              repository nextest profile caps.
  MOA_BEHAVIOR_LAB_BUDGET_USD Approved spend ceiling for --behavior-lab-live.
                              Must be positive with at most six decimal places.

Options:
  --live       Run ignored Restate E2E tests. Requires MOA_RUN_LIVE_E2E=1. This
               includes the deterministic Behavior Lab lanes, which are unbilled.
  --providers  Also run live provider/query-rewrite checks. Requires --live and
               the provider tests' own opt-in flags, such as
               MOA_RUN_LIVE_PROVIDER_TESTS=1.
  --long-eval  Also run ignored long-conversation eval smoke. Requires --live.
  --behavior-lab-live
               Also run the billed Behavior Lab trial-to-score smoke. Requires
               --live, MOA_RUN_LIVE_PROVIDER_TESTS=1, a provider credential, and
               a positive MOA_BEHAVIOR_LAB_BUDGET_USD.
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
    --behavior-lab-live)
      RUN_BEHAVIOR_LAB_LIVE=1
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

elapsed_since() {
  local start="$1"
  local elapsed=$((SECONDS - start))
  format_elapsed_seconds "${elapsed}"
}

format_elapsed_seconds() {
  local elapsed="$1"
  printf '%02d:%02d' $((elapsed / 60)) $((elapsed % 60))
}

markdown_cell() {
  local value="${1//$'\n'/ }"
  value="${value//|/\\|}"
  printf '%s' "${value}"
}

record_timing() {
  local phase="$1"
  local status="$2"
  local elapsed="$3"
  local command="$4"

  TIMING_PHASES+=("${phase}")
  TIMING_STATUSES+=("${status}")
  TIMING_SECONDS+=("${elapsed}")
  TIMING_COMMANDS+=("${command}")
}

begin_timing_phase() {
  CURRENT_PHASE="$1"
  CURRENT_COMMAND="$2"
  CURRENT_PHASE_STARTED_AT="$3"
}

end_timing_phase() {
  local phase="$1"
  local status="$2"
  local start="$3"
  local command="$4"

  record_timing "${phase}" "${status}" "$((SECONDS - start))" "${command}"
  CURRENT_PHASE=""
  CURRENT_COMMAND=""
  CURRENT_PHASE_STARTED_AT=0
}

write_timing_report() {
  local status="$1"
  local status_label="failed"
  if [[ "${TIMING_REPORT_WRITTEN}" -eq 1 ]]; then
    return 0
  fi

  if [[ "${RUN_COMPLETED}" -ne 1 && -n "${CURRENT_PHASE:-}" ]]; then
    record_timing \
      "${CURRENT_PHASE} (interrupted)" \
      "interrupted" \
      "$((SECONDS - CURRENT_PHASE_STARTED_AT))" \
      "${CURRENT_COMMAND}"
    CURRENT_PHASE=""
    CURRENT_COMMAND=""
    CURRENT_PHASE_STARTED_AT=0
  fi

  if [[ "${status}" -eq 0 ]]; then
    status_label="passed"
    if [[ "${RUN_COMPLETED}" -ne 1 ]]; then
      status_label="interrupted"
    fi
  elif [[ "${status}" -eq 130 || "${status}" -eq 143 ]]; then
    status_label="interrupted"
  fi

  mkdir -p "${TIMING_DIR}"
  {
    printf '# Clean E2E Timing Report\n\n'
    printf -- '- run_id: `%s`\n' "${RUN_ID}"
    printf -- '- status: `%s`\n' "${status_label}"
    printf -- '- total_elapsed: `%s`\n' "$(format_elapsed_seconds "$((SECONDS - RUNNER_STARTED_AT))")"
    printf -- '- target_database: `%s`\n\n' "${DB_NAME}"
    printf -- '- preflight_strategy: `nextest fast-pr + db-session + db-memory`\n'
    if [[ -n "${CLEAN_E2E_TEST_THREADS:-}" ]]; then
      printf -- '- preflight_test_threads: `%s`\n' "${CLEAN_E2E_TEST_THREADS}"
    fi
    printf '\n'
    printf '> Durations are wall-clock timings captured by `scripts/run-clean-e2e.sh` around each phase wrapper.\n\n'
    printf '| # | Status | Duration | Seconds | Phase |\n'
    printf '|---:|---|---:|---:|---|\n'
    local i
    for i in "${!TIMING_PHASES[@]}"; do
      printf '| %s | `%s` | `%s` | %s | %s |\n' \
        "$((i + 1))" \
        "${TIMING_STATUSES[$i]}" \
        "$(format_elapsed_seconds "${TIMING_SECONDS[$i]}")" \
        "${TIMING_SECONDS[$i]}" \
        "$(markdown_cell "${TIMING_PHASES[$i]}")"
    done

    printf '\n## Commands\n\n'
    for i in "${!TIMING_COMMANDS[@]}"; do
      printf '%s. `%s`\n' "$((i + 1))" "$(markdown_cell "${TIMING_COMMANDS[$i]}")"
    done
  } >"${TIMING_REPORT}"
  cp "${TIMING_REPORT}" "${TIMING_LATEST_REPORT}" 2>/dev/null || true
  TIMING_REPORT_WRITTEN=1
  echo "clean E2E timing report: ${TIMING_REPORT}"
}

run() {
  echo
  echo ">> $*"
  local start=$SECONDS
  local status=0
  begin_timing_phase "$*" "$*" "${start}"
  set +e
  "$@"
  status=$?
  set -e
  end_timing_phase "$*" "${status}" "${start}" "$*"
  if [[ "${status}" -eq 0 ]]; then
    echo "<< completed in $(elapsed_since "${start}"): $*"
  else
    echo "<< failed after $(elapsed_since "${start}"): $*" >&2
  fi
  return "${status}"
}

run_without_provider_keys() {
  echo
  echo ">> env -u MOA_ANTHROPIC_API_KEY -u MOA_OPENAI_API_KEY -u MOA_GOOGLE_API_KEY -u MOA_COHERE_API_KEY $*"
  local start=$SECONDS
  local status=0
  begin_timing_phase "env -u provider keys $*" "env -u provider keys $*" "${start}"
  set +e
  env \
    -u MOA_ANTHROPIC_API_KEY \
    -u MOA_OPENAI_API_KEY \
    -u MOA_GOOGLE_API_KEY \
    -u MOA_COHERE_API_KEY \
    "$@"
  status=$?
  set -e
  end_timing_phase "env -u provider keys $*" "${status}" "${start}" "env -u provider keys $*"
  if [[ "${status}" -eq 0 ]]; then
    echo "<< completed in $(elapsed_since "${start}"): env -u provider keys $*"
  else
    echo "<< failed after $(elapsed_since "${start}"): env -u provider keys $*" >&2
  fi
  return "${status}"
}

run_without_external_orchestrator() {
  echo
  echo ">> env -u MOA_RESTATE_INGRESS_URL -u MOA_RESTATE_ADMIN_URL -u MOA_RESTATE_DEPLOYMENT_URI $*"
  local start=$SECONDS
  local status=0
  begin_timing_phase "env -u external orchestrator $*" "env -u external orchestrator $*" "${start}"
  set +e
  env \
    -u MOA_RESTATE_INGRESS_URL \
    -u MOA_RESTATE_ADMIN_URL \
    -u MOA_RESTATE_DEPLOYMENT_URI \
    "$@"
  status=$?
  set -e
  end_timing_phase "env -u external orchestrator $*" "${status}" "${start}" "env -u external orchestrator $*"
  if [[ "${status}" -eq 0 ]]; then
    echo "<< completed in $(elapsed_since "${start}"): env -u external orchestrator $*"
  else
    echo "<< failed after $(elapsed_since "${start}"): env -u external orchestrator $*" >&2
  fi
  return "${status}"
}

run_phase() {
  local label="$1"
  shift
  local command="$*"
  echo
  echo ">> ${label}"
  local start=$SECONDS
  local status=0
  begin_timing_phase "${label}" "${command}" "${start}"
  set +e
  "$@"
  status=$?
  set -e
  end_timing_phase "${label}" "${status}" "${start}" "${command}"
  if [[ "${status}" -eq 0 ]]; then
    echo "<< completed in $(elapsed_since "${start}"): ${label}"
  else
    echo "<< failed after $(elapsed_since "${start}"): ${label}" >&2
  fi
  return "${status}"
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

wait_for_valkey() {
  for _ in $(seq 1 60); do
    if docker compose exec -T valkey valkey-cli ping >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "timed out waiting for compose Valkey" >&2
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

orchestrator_fixture_target_dir() {
  local target_dir="${CARGO_TARGET_DIR:-${REPO_ROOT}/target}"
  case "${target_dir}" in
    /*) ;;
    *) target_dir="${REPO_ROOT}/${target_dir}" ;;
  esac
  printf '%s/orchestrator-fixture-failpoints\n' "${target_dir%/}"
}

orchestrator_binary_path() {
  printf '%s/debug/moa-orchestrator-bin\n' "$(orchestrator_fixture_target_dir)"
}

fga_request() {
  local method="$1"
  local path="$2"
  local data="${3:-}"

  if [[ -n "${data}" ]]; then
    curl -fsS \
      -X "${method}" \
      "${FGA_BOOTSTRAP_URL}${path}" \
      -H "authorization: Bearer ${FGA_BOOTSTRAP_KEY}" \
      -H "content-type: application/json" \
      --data-binary "${data}"
  else
    curl -fsS \
      -X "${method}" \
      "${FGA_BOOTSTRAP_URL}${path}" \
      -H "authorization: Bearer ${FGA_BOOTSTRAP_KEY}"
  fi
}

bootstrap_openfga_model() {
  FGA_BOOTSTRAP_URL="${MOA_AUTHZ_OPENFGA_URL:-http://localhost:10030}"
  FGA_BOOTSTRAP_URL="${FGA_BOOTSTRAP_URL%/}"
  FGA_BOOTSTRAP_KEY="${MOA_AUTHZ_OPENFGA_PRESHARED_KEY:-localdev-preshared-key-do-not-use-in-prod}"
  local store_name="${MOA_AUTHZ_OPENFGA_STORE_NAME:-moa}"
  local env_output="${MOA_FGA_ENV_OUTPUT:-${FGA_ENV}}"
  local continuation=""
  local stores_response=""
  local store_id=""

  while [[ -z "${store_id}" ]]; do
    if [[ -n "${continuation}" ]]; then
      stores_response="$(
        curl -fsS -G \
          "${FGA_BOOTSTRAP_URL}/stores" \
          -H "authorization: Bearer ${FGA_BOOTSTRAP_KEY}" \
          --data-urlencode "continuation_token=${continuation}"
      )"
    else
      stores_response="$(fga_request GET /stores)"
    fi

    store_id="$(
      jq -r --arg name "${store_name}" \
        '.stores[]? | select(.name == $name) | .id' <<<"${stores_response}" | head -n 1
    )"
    if [[ -n "${store_id}" ]]; then
      break
    fi

    continuation="$(jq -r '.continuation_token // ""' <<<"${stores_response}")"
    if [[ -z "${continuation}" ]]; then
      break
    fi
  done

  if [[ -z "${store_id}" ]]; then
    store_id="$(
      jq -n --arg name "${store_name}" '{name: $name}' \
        | fga_request POST /stores @- \
        | jq -r '.id // empty'
    )"
  fi
  if [[ -z "${store_id}" ]]; then
    echo "OpenFGA CreateStore response missing id" >&2
    return 1
  fi

  local model_id
  model_id="$(
    fga_request \
      POST \
      "/stores/${store_id}/authorization-models" \
      @"${REPO_ROOT}/crates/moa-auth/authz-schema/src/schema_v1.json" \
      | jq -r '.authorization_model_id // empty'
  )"
  if [[ -z "${model_id}" ]]; then
    echo "OpenFGA WriteAuthorizationModel response missing authorization_model_id" >&2
    return 1
  fi

  if ! truthy "${MOA_FGA_BOOTSTRAP_SKIP_SMOKE:-false}"; then
    bootstrap_openfga_smoke "${store_id}" "${model_id}"
  fi

  mkdir -p "$(dirname "${env_output}")"
  {
    printf '# generated by run-clean-e2e.sh; safe to re-source\n'
    printf 'MOA_AUTHZ_OPENFGA_URL=%s\n' "${FGA_BOOTSTRAP_URL}"
    printf 'MOA_AUTHZ_OPENFGA_PRESHARED_KEY=%s\n' "${FGA_BOOTSTRAP_KEY}"
    printf 'MOA_AUTHZ_OPENFGA_STORE_ID=%s\n' "${store_id}"
    printf 'MOA_AUTHZ_OPENFGA_MODEL_ID=%s\n' "${model_id}"
  } >"${env_output}"
  cat "${env_output}"
}

bootstrap_openfga_smoke() {
  local store_id="$1"
  local model_id="$2"
  local tenant_id="00000000-0000-0000-0000-00000000ffff"
  local user_id="00000000-0000-0000-0000-00000000fffd"
  local tenant="tenant:${tenant_id}"
  local user="operator:${user_id}"
  local smoke_tuple
  smoke_tuple="$(jq -n --arg user "${user}" --arg object "${tenant}" \
    '{user: $user, relation: "admin", object: $object}')"

  jq -n --argjson tuple "${smoke_tuple}" --arg model "${model_id}" \
    '{authorization_model_id: $model, deletes: {tuple_keys: [$tuple]}}' \
    | fga_request POST "/stores/${store_id}/write" @- >/dev/null 2>&1 || true

  jq -n --argjson tuple "${smoke_tuple}" --arg model "${model_id}" \
    '{authorization_model_id: $model, writes: {tuple_keys: [$tuple]}}' \
    | fga_request POST "/stores/${store_id}/write" @- >/dev/null

  local allowed
  allowed="$(
    jq -n --arg user "${user}" --arg object "${tenant}" --arg model "${model_id}" \
      '{authorization_model_id: $model, tuple_key: {user: $user, relation: "admin", object: $object}}' \
      | fga_request POST "/stores/${store_id}/check" @- \
      | jq -r '.allowed // false'
  )"
  if [[ "${allowed}" != "true" ]]; then
    echo "OpenFGA smoke Check failed: tenant admin expected to administer tenant" >&2
    return 1
  fi

  local list_contains_tenant
  list_contains_tenant="$(
    jq -n --arg user "${user}" --arg model "${model_id}" \
      '{authorization_model_id: $model, type: "tenant", relation: "admin", user: $user}' \
      | fga_request POST "/stores/${store_id}/list-objects" @- \
      | jq -r --arg tenant "${tenant}" '(.objects // []) | index($tenant) != null'
  )"
  if [[ "${list_contains_tenant}" != "true" ]]; then
    echo "OpenFGA smoke ListObjects failed: expected ${tenant}" >&2
    return 1
  fi

  local batch
  batch="$(
    jq -n --arg user "${user}" --arg tenant "${tenant}" --arg model "${model_id}" \
      '{
        authorization_model_id: $model,
        checks: [
          {tuple_key: {user: $user, relation: "admin", object: $tenant}, correlation_id: "c0"},
          {tuple_key: {user: $user, relation: "operator", object: $tenant}, correlation_id: "c1"}
        ]
      }' \
      | fga_request POST "/stores/${store_id}/batch-check" @- \
      | jq -r '[((.result // .results).c0.allowed // false), ((.result // .results).c1.allowed // false)] | @tsv'
  )"
  if [[ "${batch}" != $'true\ttrue' ]]; then
    echo "OpenFGA smoke BatchCheck failed: expected true true, got ${batch}" >&2
    return 1
  fi

  jq -n --argjson tuple "${smoke_tuple}" --arg model "${model_id}" \
    '{authorization_model_id: $model, deletes: {tuple_keys: [$tuple]}}' \
    | fga_request POST "/stores/${store_id}/write" @- >/dev/null
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

# The billed Behavior Lab smoke spends real provider credit. Authorization and a
# positive budget are both required, and they are checked here, before any
# container or database is created, so an unauthorized run cannot get far enough
# to bill anything.
positive_usd_budget() {
  local value="${1:-}"
  [[ "${value}" =~ ^([0-9]+([.][0-9]{1,6})?|[.][0-9]{1,6})$ ]] \
    && [[ "${value}" =~ [1-9] ]]
}

if [[ "${RUN_BEHAVIOR_LAB_LIVE}" -eq 1 ]]; then
  if [[ "${LIVE}" -ne 1 ]]; then
    echo "--behavior-lab-live requires --live" >&2
    exit 2
  fi
  if ! truthy "${MOA_RUN_LIVE_PROVIDER_TESTS:-}"; then
    echo "refusing billed Behavior Lab lane without MOA_RUN_LIVE_PROVIDER_TESTS=1" >&2
    exit 2
  fi
  if ! positive_usd_budget "${MOA_BEHAVIOR_LAB_BUDGET_USD:-}"; then
    echo "refusing billed Behavior Lab lane without a positive MOA_BEHAVIOR_LAB_BUDGET_USD with at most six decimal places" >&2
    exit 2
  fi
  anthropic_key="${MOA_ANTHROPIC_API_KEY:-}"
  openai_key="${MOA_OPENAI_API_KEY:-}"
  google_key="${MOA_GOOGLE_API_KEY:-}"
  if [[ -z "${anthropic_key//[[:space:]]/}" \
    && -z "${openai_key//[[:space:]]/}" \
    && -z "${google_key//[[:space:]]/}" ]]; then
    echo "billed Behavior Lab lane requires MOA_ANTHROPIC_API_KEY, MOA_OPENAI_API_KEY, or MOA_GOOGLE_API_KEY" >&2
    exit 2
  fi
fi

require_cmd cargo
require_cmd curl
require_cmd docker
require_cmd jq
require_cmd restate-server
require_cmd cargo-nextest

cd "${REPO_ROOT}"

RUNNER_STARTED_AT=$SECONDS
RUN_ID="${MOA_CLEAN_E2E_RUN_ID:-$(date +%Y%m%d%H%M%S)-$$}"
RUN_SAFE_ID="$(printf '%s' "${RUN_ID}" | tr -c 'A-Za-z0-9_' '_')"
RUN_SHORT_ID="$(printf '%s' "${RUN_SAFE_ID}" | cut -c1-20)"
ORCH_FEATURES="provider-overrides"
ORCH_E2E_FEATURES="${ORCH_FEATURES},integration"
EXECUTION_EVAL_FEATURES="${ORCH_E2E_FEATURES},execution-planning-failpoints"
TMP_PARENT="${MOA_CLEAN_E2E_TMPDIR:-/tmp}"
TMP_ROOT="$(mktemp -d "${TMP_PARENT%/}/me2e.XXXXXX")"
RESTATE_DIR="${TMP_ROOT}/restate"
RESTATE_LOG="${TMP_ROOT}/restate.log"
FGA_ENV="${TMP_ROOT}/fga.env"
ORCH_LOG="${TMP_ROOT}/orchestrator.log"
TIMING_DIR="${REPO_ROOT}/target/e2e"
TIMING_REPORT="${TIMING_DIR}/clean-e2e-${RUN_SAFE_ID}-timings.md"
TIMING_LATEST_REPORT="${TIMING_DIR}/clean-e2e-latest-timings.md"
DB_NAME="moa_e2e_${RUN_SAFE_ID}"
DB_URL="postgres://moa_owner:dev@127.0.0.1:10040/${DB_NAME}"
RESTATE_PID=""
ORCH_PID=""
STARTED_COMPOSE=0
DB_CREATED=0
TIMING_REPORT_WRITTEN=0
RUN_COMPLETED=0
CURRENT_PHASE=""
CURRENT_COMMAND=""
CURRENT_PHASE_STARTED_AT=0
TIMING_PHASES=()
TIMING_STATUSES=()
TIMING_SECONDS=()
TIMING_COMMANDS=()

cleanup() {
  local status=$?
  if [[ "${status}" -eq 0 && "${RUN_COMPLETED}" -ne 1 ]]; then
    status=130
  fi

  write_timing_report "${status}" || true

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

run docker compose --profile pii up -d --build postgres valkey openfga moa-pii-service
run_phase "wait for compose Postgres" wait_for_postgres
run_phase "wait for compose Valkey" wait_for_valkey
run "${REPO_ROOT}/scripts/wait-for-fga.sh"
run_phase "wait for PII sidecar" wait_for_http "http://127.0.0.1:10050/healthz" "PII sidecar" 180

run docker compose exec -T postgres psql -U moa_owner -d postgres \
  -v ON_ERROR_STOP=1 \
  -c "CREATE DATABASE ${DB_NAME} OWNER moa_owner"
DB_CREATED=1

run_phase "bootstrap OpenFGA model" bootstrap_openfga_model

set -a
# shellcheck disable=SC1090
. "${FGA_ENV}"
set +a
export MOA_FIXTURE_OPENFGA_URL="${MOA_AUTHZ_OPENFGA_URL}"
export MOA_FIXTURE_OPENFGA_PRESHARED_KEY="${MOA_AUTHZ_OPENFGA_PRESHARED_KEY}"

echo
echo ">> starting ephemeral restate-server"
# Fresh data dir every run, so the v1.7 fresh-cluster vqueues limitation
# never applies here.
RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true restate-server \
  --node-name "e2e-${RUN_SHORT_ID}" \
  --base-dir "${RESTATE_DIR}" \
  --bind-ip 127.0.0.1 \
  --advertised-host 127.0.0.1 \
  --use-random-ports true \
  --log-format compact \
  --log-disable-ansi-codes true \
  >"${RESTATE_LOG}" 2>&1 &
RESTATE_PID=$!
run_phase "wait for ephemeral restate-server ports" wait_for_restate_ports "${RESTATE_LOG}"

export MOA_DATABASE_URL="${DB_URL}"
export MOA_RESTATE_INGRESS_URL="${RESTATE_INGRESS_URL}"
export MOA_RESTATE_ADMIN_URL="${RESTATE_ADMIN_URL}"
export MOA_RESTATE_DEPLOYMENT_HOST="127.0.0.1"
export MOA_PII_SERVICE_URL="${MOA_PII_SERVICE_URL:-http://127.0.0.1:10050}"
export MOA_RUNTIME_CACHE_BACKEND="redis"
export MOA_RUNTIME_CACHE_REDIS_URL="redis://127.0.0.1:10051/0"
# Every e2e-spawned orchestrator boots with the in-process ephemeral KMS. Opt in
# so the composition-root fail-closed durability guard permits startup; keys are
# lost on restart, which is acceptable for these hermetic e2e runs. Production
# uses a persistent postgres KMS instead of this flag.
export MOA_KMS_ALLOW_EPHEMERAL=true

CLEAN_E2E_TEST_THREADS="${MOA_CLEAN_E2E_TEST_THREADS:-4}"
run cargo nextest run -p moa-orchestrator --locked --profile fast-pr --test-threads "${CLEAN_E2E_TEST_THREADS}" --no-tests fail
run cargo nextest run -p moa-orchestrator --locked --profile db-session --no-tests fail
run cargo nextest run -p moa-orchestrator --locked --profile db-memory --no-tests fail
run cargo nextest run -p moa-orchestrator --lib --locked --features "${ORCH_FEATURES}" \
  -E 'test(/^runtime::endpoint::tests::skill_learning_workflow_is_always_expected$/)' \
  --no-tests fail
run cargo test -p moa-orchestrator --test skill_learning_review_db --locked --features "${ORCH_FEATURES}" -- --test-threads="${CLEAN_E2E_TEST_THREADS}"
run cargo test -p moa-brain --features eval-harness --test brain_turn_cache_replay_db_memory --locked
run cargo test -p moa-eval --test golden_eval --locked

if [[ "${LIVE}" -eq 1 ]]; then
  # Live local E2Es spawn host-local hands, which only the local security
  # profile permits; production selects the fail-closed cloud profile.
  export MOA_SECURITY_PROFILE=local

  run cargo nextest run -p moa-orchestrator --locked --features "${ORCH_E2E_FEATURES}" --profile restate-service-e2e --run-ignored ignored-only --no-tests fail

  # Deterministic Behavior Lab lane. These binaries resolve the Restate stack
  # from MOA_RESTATE_INGRESS_URL/MOA_RESTATE_ADMIN_URL and spawn their own
  # orchestrator on reserved ports, so they run here — with the ephemeral
  # server's URLs still exported and before the shared orchestrator claims the
  # deployment — rather than in the self-contained fixture arm below. Provider
  # keys are stripped so nothing in this lane can reach a billed model; the
  # billed trial-to-score smoke is excluded by the profile and gated separately.
  run_without_provider_keys cargo nextest run -p moa-orchestrator --locked --features "${ORCH_E2E_FEATURES}" --profile behavior-lab-service-e2e --run-ignored ignored-only --no-tests fail

  if [[ "${RUN_BEHAVIOR_LAB_LIVE}" -eq 1 ]]; then
    # Billed. Authorization and MOA_BEHAVIOR_LAB_BUDGET_USD were verified up
    # front. It runs in this window, alongside the other lanes that register
    # their own Restate deployment, so it never displaces the shared
    # orchestrator registered further below.
    run cargo nextest run -p moa-orchestrator --locked --features "${ORCH_E2E_FEATURES}" --profile behavior-lab-live --run-ignored ignored-only --no-tests fail
  fi

  FIXTURE_ORCHESTRATOR_TARGET_DIR="$(orchestrator_fixture_target_dir)"
  run env CARGO_TARGET_DIR="${FIXTURE_ORCHESTRATOR_TARGET_DIR}" \
    cargo build -p moa-orchestrator --bin moa-orchestrator-bin --features "${EXECUTION_EVAL_FEATURES}" --locked
  MOA_ORCHESTRATOR_BIN="$(orchestrator_binary_path)"
  if [[ ! -f "${MOA_ORCHESTRATOR_BIN}" ]]; then
    echo "expected orchestrator binary was not built: ${MOA_ORCHESTRATOR_BIN}" >&2
    exit 1
  fi
  export MOA_ORCHESTRATOR_BIN

  run_without_external_orchestrator cargo nextest run -p moa-orchestrator --locked --features "${ORCH_E2E_FEATURES}" --profile fixture-service-e2e --run-ignored ignored-only --no-tests fail

  run_without_external_orchestrator cargo nextest run -p moa-orchestrator --locked --features "${EXECUTION_EVAL_FEATURES}" --profile execution-eval-pr --run-ignored ignored-only --no-tests fail

  # Self-contained Behavior Lab lane. `OrchestratorTestFixture::with_execution_fixture`
  # refuses to start when MOA_RESTATE_INGRESS_URL is set ("dedicated execution
  # fixtures cannot use an external orchestrator"), so this arm must unset the
  # Restate URLs and let the fixture bring up its own containers, exactly like
  # fixture-service-e2e above. It reuses the MOA_ORCHESTRATOR_BIN built there.
  run_without_external_orchestrator cargo nextest run -p moa-orchestrator --locked --features "${ORCH_E2E_FEATURES}" --profile behavior-lab-fixture-service-e2e --run-ignored ignored-only --no-tests fail

  ORCH_PORT="${MOA_CLEAN_E2E_ORCH_PORT:-19180}"
  ORCH_HEALTH_PORT="${MOA_CLEAN_E2E_ORCH_HEALTH_PORT:-19181}"
  ORCH_SCIM_PORT="${MOA_CLEAN_E2E_ORCH_SCIM_PORT:-19182}"
  export MOA_RESTATE_DEPLOYMENT_URI="http://127.0.0.1:${ORCH_PORT}"

  echo
  echo ">> starting shared orchestrator for lifecycle smoke tests"
  env -u MOA_COHERE_API_KEY \
    -u MOA_ANTHROPIC_API_KEY \
    -u MOA_OPENAI_API_KEY \
    -u MOA_GOOGLE_API_KEY \
    RUST_LOG="${RUST_LOG:-warn}" \
    MOA_PROVIDERS_OVERRIDE="mock:${RUN_SAFE_ID}" \
    MOA_LOCAL_MEMORY_DIR="${TMP_ROOT}/memory" \
    MOA_LOCAL_SANDBOX_DIR="${TMP_ROOT}/sandbox" \
    MOA_LOCAL_DOCKER_ENABLED=false \
    "${MOA_ORCHESTRATOR_BIN}" \
      --port "${ORCH_PORT}" \
      --health-port "${ORCH_HEALTH_PORT}" \
      --scim-port "${ORCH_SCIM_PORT}" \
      >"${ORCH_LOG}" 2>&1 &
  ORCH_PID=$!
  run_phase "wait for shared orchestrator" wait_for_http "http://127.0.0.1:${ORCH_HEALTH_PORT}/_health/live" "shared orchestrator"

  run curl -fsS \
    -X POST "${RESTATE_ADMIN_URL}/deployments" \
    -H "content-type: application/json" \
    --data "{\"uri\":\"http://127.0.0.1:${ORCH_PORT}\"}"

  run_without_provider_keys cargo nextest run -p moa-orchestrator --locked --features "${ORCH_E2E_FEATURES}" --profile orchestrator-service-e2e --run-ignored ignored-only --no-tests fail

  if [[ "${RUN_PROVIDERS}" -eq 1 ]]; then
    if ! truthy "${MOA_RUN_LIVE_PROVIDER_TESTS:-}"; then
      echo "refusing provider live checks without MOA_RUN_LIVE_PROVIDER_TESTS=1" >&2
      exit 2
    fi
    run cargo nextest run -p moa-orchestrator --locked --features "${ORCH_E2E_FEATURES}" --profile provider-e2e --run-ignored ignored-only --no-tests fail
    run cargo nextest run -p moa-providers --locked --profile provider-e2e --run-ignored ignored-only --no-tests fail
    run cargo nextest run -p moa-brain --locked --profile provider-e2e --run-ignored ignored-only --no-tests fail
  fi

  if [[ "${RUN_LONG_EVAL}" -eq 1 ]]; then
    run_without_external_orchestrator cargo nextest run -p moa-orchestrator --locked --features "${EXECUTION_EVAL_FEATURES}" --profile execution-eval-nightly --run-ignored ignored-only --no-tests fail
    run cargo test -p moa-eval --test long_conversation_smoke_eval --locked -- --ignored --test-threads=1 --nocapture
  fi
fi

echo
echo "clean E2E run completed"
echo "clean E2E elapsed: $(elapsed_since "${RUNNER_STARTED_AT}")"
RUN_COMPLETED=1
write_timing_report 0
