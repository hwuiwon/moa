#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"

DATABASE_ADMIN_URL=""
RESTATE_ADMIN_URL=""
RESTATE_INGRESS_URL=""
OLD_DEPLOYMENT_ID=""
NEW_DEPLOYMENT_URI=""
ARCHIVE_DIR=""
CONFIRMED=0

readonly OLD_SERVICES=(ExecutionRun ExecutionTask ExecutionCompensation)
readonly NEW_SERVICES=(
  ExecutionRunController
  ExecutionTaskAttempt
  ExecutionCompensationAttempt
  ExecutionTrigger
  ExecutionDispatcher
  ExecutionDispatchDrain
  ExecutionDispatchReconciler
  ExecutionRetention
  ExecutionSchedule
  DurableTimeout
)
readonly NEW_SERVICE_CONTRACT_JSON='[
  {"name":"ExecutionRunController","ty":"VirtualObject","public":false},
  {"name":"ExecutionTaskAttempt","ty":"Workflow","public":false},
  {"name":"ExecutionCompensationAttempt","ty":"Workflow","public":false},
  {"name":"ExecutionTrigger","ty":"Service","public":false},
  {"name":"ExecutionDispatcher","ty":"Service","public":false},
  {"name":"ExecutionDispatchDrain","ty":"VirtualObject","public":false},
  {"name":"ExecutionDispatchReconciler","ty":"Service","public":false},
  {"name":"ExecutionRetention","ty":"Service","public":false},
  {"name":"ExecutionSchedule","ty":"Service","public":true},
  {"name":"DurableTimeout","ty":"Service","public":false}
]'

# This is the destructive first registration of the bounded execution family: the retired
# deployment contains only OLD_SERVICES. The stateless ExecutionDispatcher router and fleet-keyed
# ExecutionDispatchDrain are therefore registered directly; there is no compatibility deployment.

usage() {
  cat <<'USAGE'
Usage: scripts/cutover-long-horizon-execution.sh \
  --database-admin-url URL \
  --restate-admin-url URL \
  --restate-ingress-url URL \
  --old-deployment-id ID \
  --new-deployment-uri URI \
  --archive-dir ABSOLUTE_EMPTY_DIRECTORY \
  [--confirm-destructive-cutover]

Hard-cuts the retired ExecutionRun, ExecutionTask, and ExecutionCompensation
Restate runtime to bounded execution activations. The script always performs
and prints its read-only Postgres and Restate preflight before considering any
mutation. Without --confirm-destructive-cutover it exits after preflight.

The archive directory must already exist, be absolute, and be empty. The
database URL, Restate Admin/ingress URLs, old deployment ID, and new immutable
deployment URI are mandatory; no destructive target is inferred.
USAGE
}

die() {
  echo "cutover refused: $*" >&2
  exit 2
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

require_value() {
  local option="$1"
  local value="${2:-}"
  [[ -n "${value}" ]] || die "${option} requires a non-empty value"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --database-admin-url)
      require_value "$1" "${2:-}"
      DATABASE_ADMIN_URL="$2"
      shift 2
      ;;
    --restate-admin-url)
      require_value "$1" "${2:-}"
      RESTATE_ADMIN_URL="${2%/}"
      shift 2
      ;;
    --restate-ingress-url)
      require_value "$1" "${2:-}"
      RESTATE_INGRESS_URL="${2%/}"
      shift 2
      ;;
    --old-deployment-id)
      require_value "$1" "${2:-}"
      OLD_DEPLOYMENT_ID="$2"
      shift 2
      ;;
    --new-deployment-uri)
      require_value "$1" "${2:-}"
      NEW_DEPLOYMENT_URI="${2%/}"
      shift 2
      ;;
    --archive-dir)
      require_value "$1" "${2:-}"
      ARCHIVE_DIR="${2%/}"
      shift 2
      ;;
    --confirm-destructive-cutover)
      CONFIRMED=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

require_value --database-admin-url "${DATABASE_ADMIN_URL}"
require_value --restate-admin-url "${RESTATE_ADMIN_URL}"
require_value --restate-ingress-url "${RESTATE_INGRESS_URL}"
require_value --old-deployment-id "${OLD_DEPLOYMENT_ID}"
require_value --new-deployment-uri "${NEW_DEPLOYMENT_URI}"
require_value --archive-dir "${ARCHIVE_DIR}"

for command in cargo curl find jq pg_dump psql restate seq tee tr wc; do
  require_cmd "${command}"
done

[[ "${DATABASE_ADMIN_URL}" == postgres://* || "${DATABASE_ADMIN_URL}" == postgresql://* ]] \
  || die "--database-admin-url must be an explicit postgres:// or postgresql:// URL"
[[ "${RESTATE_ADMIN_URL}" == http://* || "${RESTATE_ADMIN_URL}" == https://* ]] \
  || die "--restate-admin-url must be an explicit HTTP(S) URL"
[[ "${RESTATE_INGRESS_URL}" == http://* || "${RESTATE_INGRESS_URL}" == https://* ]] \
  || die "--restate-ingress-url must be an explicit HTTP(S) URL"
[[ "${NEW_DEPLOYMENT_URI}" == http://* || "${NEW_DEPLOYMENT_URI}" == https://* ]] \
  || die "--new-deployment-uri must be an explicit immutable HTTP(S) URI"
[[ "${OLD_DEPLOYMENT_ID}" =~ ^[A-Za-z0-9_-]+$ ]] \
  || die "--old-deployment-id contains unsafe characters"
[[ "${ARCHIVE_DIR}" == /* && "${ARCHIVE_DIR}" != / && "${ARCHIVE_DIR}" != *".."* ]] \
  || die "--archive-dir must be an explicit absolute non-root path without '..'"
[[ -d "${ARCHIVE_DIR}" ]] || die "--archive-dir must already exist"
[[ -z "$(find "${ARCHIVE_DIR}" -mindepth 1 -maxdepth 1 -print -quit)" ]] \
  || die "--archive-dir must be empty"

restate_cli() {
  RESTATE_ADMIN_URL="${RESTATE_ADMIN_URL}" \
  RESTATE_INGRESS_URL="${RESTATE_INGRESS_URL}" \
  RESTATE_CLI_CONFIG_HOME="${ARCHIVE_DIR}/restate-cli-config" \
    restate -e local "$@"
}

restate_query() {
  local query="$1"
  curl -fsS \
    -X POST "${RESTATE_ADMIN_URL}/query" \
    -H "accept: application/json" \
    -H "content-type: application/json" \
    --data-binary "$(jq -cn --arg query "${query}" '{query: $query}')"
}

echo "== Read-only preflight: Postgres nonterminal executions =="
readonly NONTERMINAL_SQL="
SELECT run_uid, status, updated_at
FROM moa.execution_run
WHERE status NOT IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled')
ORDER BY updated_at, run_uid;"
psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --pset=pager=off \
  --command "${NONTERMINAL_SQL}"
NONTERMINAL_RUNS="$(
  psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --tuples-only --no-align \
    --command "SELECT count(*) FROM moa.execution_run WHERE status NOT IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled');"
)"

echo "== Read-only preflight: old Restate deployment =="
DEPLOYMENTS_JSON="$(curl -fsS "${RESTATE_ADMIN_URL}/deployments")"
jq --arg deployment_id "${OLD_DEPLOYMENT_ID}" \
  '.deployments[] | select(.id == $deployment_id)' <<<"${DEPLOYMENTS_JSON}"
OLD_DEPLOYMENT_MATCHES="$(
  jq -r --arg deployment_id "${OLD_DEPLOYMENT_ID}" \
    '[.deployments[] | select(.id == $deployment_id)] | length' <<<"${DEPLOYMENTS_JSON}"
)"
[[ "${OLD_DEPLOYMENT_MATCHES}" == 1 ]] \
  || die "old deployment ID must resolve to exactly one registered deployment"
echo "== Read-only preflight: retired service deployments =="
jq '
  [.deployments[]
   | select(any(.services[]?; .name == "ExecutionRun"
                            or .name == "ExecutionTask"
                            or .name == "ExecutionCompensation"))
   | {id, uri, services: [.services[]?.name]}]
' <<<"${DEPLOYMENTS_JSON}"
OLD_SERVICE_DEPLOYMENTS_VALID="$(
  jq -r --arg deployment_id "${OLD_DEPLOYMENT_ID}" '
    [.deployments[]
     | select(any(.services[]?; .name == "ExecutionRun"
                              or .name == "ExecutionTask"
                              or .name == "ExecutionCompensation"))] as $old
    | ($old | length) == 1
      and ($old[0].id == $deployment_id)
      and (["ExecutionRun", "ExecutionTask", "ExecutionCompensation"]
           | all(. as $service | any($old[0].services[]?; .name == $service)))
  ' <<<"${DEPLOYMENTS_JSON}"
)"
[[ "${OLD_SERVICE_DEPLOYMENTS_VALID}" == true ]] \
  || die "the exact old deployment must be the sole deployment containing all retired execution services"

readonly OLD_INVOCATIONS_SQL="
SELECT id, status, target_service_name, target_handler_name,
       pinned_deployment_id, last_attempt_deployment_id
FROM sys_invocation
WHERE (target_service_name IN ('ExecutionRun', 'ExecutionTask', 'ExecutionCompensation')
       OR pinned_deployment_id = '${OLD_DEPLOYMENT_ID}'
       OR last_attempt_deployment_id = '${OLD_DEPLOYMENT_ID}')
  AND status NOT IN ('completed', 'killed')
ORDER BY id;"
OLD_INVOCATIONS_JSON="$(restate_query "${OLD_INVOCATIONS_SQL}")"
jq '.rows' <<<"${OLD_INVOCATIONS_JSON}"
readonly OLD_INVOCATION_COUNT_SQL="
SELECT count(*) AS invocation_count
FROM sys_invocation
WHERE (target_service_name IN ('ExecutionRun', 'ExecutionTask', 'ExecutionCompensation')
       OR pinned_deployment_id = '${OLD_DEPLOYMENT_ID}'
       OR last_attempt_deployment_id = '${OLD_DEPLOYMENT_ID}')
  AND status NOT IN ('completed', 'killed');"
OLD_INVOCATION_COUNT_JSON="$(restate_query "${OLD_INVOCATION_COUNT_SQL}")"
OLD_INVOCATIONS="$(jq -er '.rows[0].invocation_count | tonumber' \
  <<<"${OLD_INVOCATION_COUNT_JSON}")"

echo "== Read-only preflight: central migration position =="
MIGRATION_POSITION="$(
  psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --tuples-only --no-align \
    --command "SELECT COALESCE(max(version), 0) FROM public.refinery_schema_history;"
)"
printf 'latest central migration: V%06d\n' "${MIGRATION_POSITION}"

printf 'preflight totals: nonterminal_runs=%s old_service_or_deployment_invocations=%s\n' \
  "${NONTERMINAL_RUNS}" "${OLD_INVOCATIONS}"
[[ "${NONTERMINAL_RUNS}" == 0 ]] \
  || die "terminalize or cancel every legacy execution through the old product runtime"
[[ "${OLD_INVOCATIONS}" == 0 ]] \
  || die "retired execution services or the exact old deployment still own nonterminal invocations"
[[ "${MIGRATION_POSITION}" == 58 ]] \
  || die "the database must be at exactly V000058 before applying the V59/V60 hard cut"

echo "Read-only preflight passed. No state has been changed."
[[ "${CONFIRMED}" == 1 ]] \
  || die "rerun with --confirm-destructive-cutover after reviewing the printed evidence"

echo "== Archive terminal execution evidence =="
ARCHIVE_FILE="${ARCHIVE_DIR}/terminal-execution-before-v59.sql"
printf '%s\n' "${DEPLOYMENTS_JSON}" >"${ARCHIVE_DIR}/restate-deployments-before-cutover.json"
readonly TERMINAL_INVOCATIONS_SQL="
SELECT id, status, target_service_name, target_handler_name,
       pinned_deployment_id, last_attempt_deployment_id
FROM sys_invocation
WHERE target_service_name IN ('ExecutionRun', 'ExecutionTask', 'ExecutionCompensation')
  AND status IN ('completed', 'killed')
ORDER BY id;"
restate_query "${TERMINAL_INVOCATIONS_SQL}" \
  >"${ARCHIVE_DIR}/terminal-restate-invocations-before-cutover.json"
pg_dump \
  --dbname "${DATABASE_ADMIN_URL}" \
  --data-only \
  --no-owner \
  --no-privileges \
  --table moa.execution_run \
  --table moa.execution_task \
  --table moa.execution_compensation \
  --file "${ARCHIVE_FILE}"
[[ -s "${ARCHIVE_FILE}" ]] || die "terminal execution archive is empty"
{
  printf 'old_deployment_id=%s\n' "${OLD_DEPLOYMENT_ID}"
  printf 'new_deployment_uri=%s\n' "${NEW_DEPLOYMENT_URI}"
  printf 'nonterminal_runs=%s\n' "${NONTERMINAL_RUNS}"
  printf 'old_deployment_invocations=%s\n' "${OLD_INVOCATIONS}"
  printf 'archive_bytes=%s\n' "$(wc -c <"${ARCHIVE_FILE}" | tr -d ' ')"
} >"${ARCHIVE_DIR}/cutover-manifest.txt"

echo "== Apply repository-owned V59/V60 migration chain =="
(
  cd -- "${REPO_ROOT}"
  MOA_DATABASE_URL="${DATABASE_ADMIN_URL}" \
  MOA_DATABASE_ADMIN_URL="${DATABASE_ADMIN_URL}" \
    cargo run -p moa-orchestrator --bin moa-orchestrator-bin --locked -- migrate
)
APPLIED_MIGRATIONS="$(
  psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --tuples-only --no-align \
    --command "SELECT version, name FROM public.refinery_schema_history WHERE version IN (59, 60) ORDER BY version;" \
    | tee "${ARCHIVE_DIR}/applied-migrations.txt"
)"
[[ "${APPLIED_MIGRATIONS}" == $'59|long_horizon_execution\n60|sandbox_active_compute_capacity' ]] \
  || die "the repository runner did not record the exact V59/V60 identities"

echo "== Reset only retired execution-service state and completed journals =="
for service in "${OLD_SERVICES[@]}"; do
  restate_cli --yes state clear "${service}"
  for _attempt in $(seq 1 1000); do
    TERMINAL_SERVICE_COUNT_JSON="$(restate_query "
SELECT count(*) AS invocation_count
FROM sys_invocation
WHERE target_service_name = '${service}'
  AND status IN ('completed', 'killed');")"
    TERMINAL_SERVICE_COUNT="$(jq -er '.rows[0].invocation_count | tonumber' \
      <<<"${TERMINAL_SERVICE_COUNT_JSON}")"
    [[ "${TERMINAL_SERVICE_COUNT}" == 0 ]] && break
    restate_cli --yes invocations purge --limit 500 "${service}" \
      >>"${ARCHIVE_DIR}/restate-invocation-purge.log"
  done
  [[ "${TERMINAL_SERVICE_COUNT}" == 0 ]] \
    || die "retired ${service} invocation history exceeded the bounded purge loop"
done

echo "== Remove exact old deployment and register the new immutable endpoint =="
curl -fsS \
  -X DELETE "${RESTATE_ADMIN_URL}/deployments/${OLD_DEPLOYMENT_ID}?force=true" \
  -o "${ARCHIVE_DIR}/old-deployment-removal.json"
for _attempt in $(seq 1 60); do
  DEPLOYMENTS_JSON="$(curl -fsS "${RESTATE_ADMIN_URL}/deployments")"
  if ! jq -e --arg deployment_id "${OLD_DEPLOYMENT_ID}" \
      '.deployments[] | select(.id == $deployment_id)' \
      <<<"${DEPLOYMENTS_JSON}" >/dev/null; then
    break
  fi
  sleep 2
done
if jq -e --arg deployment_id "${OLD_DEPLOYMENT_ID}" \
    '.deployments[] | select(.id == $deployment_id)' \
    <<<"${DEPLOYMENTS_JSON}" >/dev/null; then
  die "old deployment remained registered after the bounded removal wait"
fi

curl -fsS \
  -X POST "${RESTATE_ADMIN_URL}/deployments" \
  -H "content-type: application/json" \
  --data-binary "$(jq -cn --arg uri "${NEW_DEPLOYMENT_URI}" '{uri: $uri}')" \
  -o "${ARCHIVE_DIR}/new-deployment-registration.json"

echo "== Verify the bounded-activation service inventory =="
for _attempt in $(seq 1 60); do
  DEPLOYMENTS_JSON="$(curl -fsS "${RESTATE_ADMIN_URL}/deployments")"
  SERVICES_JSON="$(curl -fsS "${RESTATE_ADMIN_URL}/services")"
  READY=1
  NEW_DEPLOYMENT_ID="$(
    jq -r --arg uri "${NEW_DEPLOYMENT_URI}" '
      [.deployments[] | select(((.uri // "") | rtrimstr("/")) == $uri)]
      | if length == 1 then .[0].id else "" end
    ' <<<"${DEPLOYMENTS_JSON}"
  )"
  [[ -n "${NEW_DEPLOYMENT_ID}" ]] || READY=0
  for service in "${NEW_SERVICES[@]}"; do
    jq -e --arg service "${service}" --arg uri "${NEW_DEPLOYMENT_URI}" \
      '.deployments[]
       | select((.uri | rtrimstr("/")) == $uri)
       | .services[]?
       | select(.name == $service)' \
      <<<"${DEPLOYMENTS_JSON}" >/dev/null || READY=0
  done
  for service in "${OLD_SERVICES[@]}"; do
    if jq -e --arg service "${service}" \
        '.deployments[].services[]? | select(.name == $service)' \
        <<<"${DEPLOYMENTS_JSON}" >/dev/null; then
      READY=0
    fi
  done
  if ! jq -e \
      --arg deployment_id "${NEW_DEPLOYMENT_ID}" \
      --argjson expected "${NEW_SERVICE_CONTRACT_JSON}" '
        .services as $services
        | $expected
        | all(. as $contract |
            [$services[]
             | select(.deployment_id == $deployment_id
                      and .name == $contract.name
                      and .ty == $contract.ty
                      and .public == $contract.public)]
            | length == 1)
      ' <<<"${SERVICES_JSON}" >/dev/null; then
    READY=0
  fi
  [[ "${READY}" == 1 ]] && break
  sleep 2
done
[[ "${READY}" == 1 ]] \
  || die "new handler type/privacy inventory was incomplete or a retired execution service remained registered"
jq -n \
  --argjson deployments "${DEPLOYMENTS_JSON}" \
  --argjson services "${SERVICES_JSON}" \
  --arg deployment_id "${NEW_DEPLOYMENT_ID}" '
    {
      deployments: [$deployments.deployments[] | {id, uri, services: [.services[]?.name]}],
      verified_new_handlers: [
        $services.services[]
        | select(.deployment_id == $deployment_id)
        | {name, ty, public}
      ]
    }
  ' | tee "${ARCHIVE_DIR}/verified-service-inventory.json"

echo "Long-horizon execution cutover complete. Keep admission gated until the"
echo "maintenance owner is ready and the archived evidence is stored durably."
