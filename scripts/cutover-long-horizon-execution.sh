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
SESSION_EVENTS_SCHEMA=""
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
  --archive-dir ABSOLUTE_DIRECTORY \
  --session-events-schema NAME \
  [--confirm-destructive-cutover]

Hard-cuts the retired ExecutionRun, ExecutionTask, and ExecutionCompensation
Restate runtime to bounded execution activations. The script always performs
and prints its read-only Postgres and Restate preflight before considering any
mutation. Without --confirm-destructive-cutover it exits after preflight.

The archive directory must already exist, be absolute, and be either empty or
hold this exact cutover's own cutover-manifest.txt from an interrupted earlier
invocation. The database URL, Restate Admin/ingress URLs, old deployment ID,
new immutable deployment URI, and session-events schema are mandatory; no
destructive target is inferred.

--session-events-schema names the Postgres schema, in the same database as
--database-admin-url, that holds the session store's `events` table. Historical
`ExecutionProgress` payloads predate this change set's added required fields
and can no longer decode; a single stale row fails an entire session history
replay, so the cutover deletes them after archiving them as CSV.

That schema is usually `public`: `config.database.schema` is an Option that is
None by default, so the session store resolves `events` through `search_path`.
It is deliberately NOT defaulted here — an unset value must never become a
destructive target. Pass the value your deployment actually uses.

Archived history is out of scope. `session_event_archives` payloads are
compressed blobs that SQL cannot rewrite, and dropping whole archive rows would
destroy unrelated history for the same session. The final step names that
residue explicitly and states the remedy; it does not act on it.

Every stage is idempotent and the read-only preflight runs on every invocation,
so an interrupted cutover is resumed by rerunning the identical command.
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
    --session-events-schema)
      require_value "$1" "${2:-}"
      SESSION_EVENTS_SCHEMA="$2"
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
require_value --session-events-schema "${SESSION_EVENTS_SCHEMA}"

for command in cargo curl find grep head jq pg_dump psql restate seq tee tr wc; do
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
[[ "${SESSION_EVENTS_SCHEMA}" =~ ^[A-Za-z_][A-Za-z0-9_]{0,62}$ ]] \
  || die "--session-events-schema must be a plain unquoted Postgres identifier"

# Every failure-prone stage of this cutover runs after the schema change, so a
# rerun has to be able to resume rather than trip over its own earlier output.
# A non-empty archive directory is accepted only when it holds this exact
# cutover's manifest; anything else is still an unrelated directory and refused.
ARCHIVE_MANIFEST="${ARCHIVE_DIR}/cutover-manifest.txt"
ARCHIVE_STAGE="pending"
if [[ -n "$(find "${ARCHIVE_DIR}" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
  [[ -f "${ARCHIVE_MANIFEST}" ]] \
    || die "--archive-dir must be empty or hold this cutover's cutover-manifest.txt"
  grep -qxF "old_deployment_id=${OLD_DEPLOYMENT_ID}" "${ARCHIVE_MANIFEST}" \
    || die "the existing cutover-manifest.txt names a different --old-deployment-id"
  grep -qxF "new_deployment_uri=${NEW_DEPLOYMENT_URI}" "${ARCHIVE_MANIFEST}" \
    || die "the existing cutover-manifest.txt names a different --new-deployment-uri"
  ARCHIVE_STAGE="complete"
fi

restate_cli() {
  RESTATE_ADMIN_URL="${RESTATE_ADMIN_URL}" \
  RESTATE_INGRESS_URL="${RESTATE_INGRESS_URL}" \
  RESTATE_CLI_CONFIG_HOME="${ARCHIVE_DIR}/restate-cli-config" \
    restate -e local "$@"
}

# The exact central-migration identities this cutover applies. Comparing names
# as well as versions is what lets a resumed invocation treat the current
# migration stage as complete instead of accepting some other chain at the same
# version. The V59 prefix remains resumable if V60 was not recorded.
readonly APPLIED_MIGRATIONS_EXPECTED=$'59|long_horizon_execution\n60|sandbox_active_compute_capacity'
readonly CUTOVER_MIGRATION_MAX=60

applied_migration_identities() {
  psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --tuples-only --no-align \
    --command "SELECT version, name FROM public.refinery_schema_history WHERE version BETWEEN 59 AND ${CUTOVER_MIGRATION_MAX} ORDER BY version;"
}

expected_migration_prefix() {
  local migration_position="$1"
  local prefix_length=$((migration_position - 58))
  [[ "${prefix_length}" -gt 0 ]] || return 0
  head -n "${prefix_length}" <<<"${APPLIED_MIGRATIONS_EXPECTED}"
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
echo "== Read-only preflight: retired service deployments =="
jq '
  [.deployments[]
   | select(any(.services[]?; .name == "ExecutionRun"
                            or .name == "ExecutionTask"
                            or .name == "ExecutionCompensation"))
   | {id, uri, services: [.services[]?.name]}]
' <<<"${DEPLOYMENTS_JSON}"
OLD_SERVICE_DEPLOYMENTS="$(
  jq -r '
    [.deployments[]
     | select(any(.services[]?; .name == "ExecutionRun"
                              or .name == "ExecutionTask"
                              or .name == "ExecutionCompensation"))]
    | length
  ' <<<"${DEPLOYMENTS_JSON}"
)"
if [[ "${OLD_DEPLOYMENT_MATCHES}" == 1 ]]; then
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
  DEPLOYMENT_STAGE="pending"
elif [[ "${OLD_DEPLOYMENT_MATCHES}" == 0 && "${OLD_SERVICE_DEPLOYMENTS}" == 0 ]]; then
  # The only accepted resume shape: an earlier invocation already removed the
  # exact old deployment, and no other deployment exposes a retired execution
  # service. Both halves are required — a missing ID with a retired service
  # still registered elsewhere means something other than this script acted.
  DEPLOYMENT_STAGE="complete"
else
  die "old deployment ID must resolve to exactly one registered deployment, or already be removed with no retired execution service registered anywhere"
fi

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

case "${MIGRATION_POSITION}" in
  58|59)
    [[ "$(applied_migration_identities)" == "$(expected_migration_prefix "${MIGRATION_POSITION}")" ]] \
      || die "the database's applied V59-V${MIGRATION_POSITION} identities do not match this cutover"
    MIGRATION_STAGE="pending"
    ;;
  60)
    [[ "$(applied_migration_identities)" == "${APPLIED_MIGRATIONS_EXPECTED}" ]] \
      || die "the database reports V000060 without the exact V59/V60 identities this cutover applies"
    MIGRATION_STAGE="complete"
    ;;
  *)
    die "the database must be at an exact V000058-V000060 prefix of the V59/V60 hard cut"
    ;;
esac

echo "== Read-only preflight: undecodable historical ExecutionProgress events =="
SESSION_EVENTS_TABLE_PRESENT="$(
  psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --tuples-only --no-align \
    --command "SELECT to_regclass('${SESSION_EVENTS_SCHEMA}.events') IS NOT NULL;"
)"
[[ "${SESSION_EVENTS_TABLE_PRESENT}" == t ]] \
  || die "--session-events-schema must name a schema holding the session store's events table in this database"
STALE_PROGRESS_EVENTS="$(
  psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --tuples-only --no-align \
    --command "SELECT count(*) FROM ${SESSION_EVENTS_SCHEMA}.events WHERE event_type = 'ExecutionProgress';"
)"

printf 'preflight totals: nonterminal_runs=%s old_service_or_deployment_invocations=%s stale_execution_progress_events=%s\n' \
  "${NONTERMINAL_RUNS}" "${OLD_INVOCATIONS}" "${STALE_PROGRESS_EVENTS}"
printf 'resumable stage state: archive=%s old_deployment=%s migration=%s\n' \
  "${ARCHIVE_STAGE}" "${DEPLOYMENT_STAGE}" "${MIGRATION_STAGE}"
[[ "${NONTERMINAL_RUNS}" == 0 ]] \
  || die "terminalize or cancel every legacy execution through the old product runtime"
[[ "${OLD_INVOCATIONS}" == 0 ]] \
  || die "retired execution services or the exact old deployment still own nonterminal invocations"

# Stages complete in a fixed order: evidence is archived, then the retired
# deployment is removed, then the schema is cut. A later stage complete over an
# earlier one that is not means something other than this script changed the
# target, so refuse rather than resume onto an unknown state.
[[ "${DEPLOYMENT_STAGE}" != "complete" || "${ARCHIVE_STAGE}" == "complete" ]] \
  || die "the retired deployment is already removed but --archive-dir holds no matching cutover manifest"
[[ "${MIGRATION_STAGE}" != "complete" || "${DEPLOYMENT_STAGE}" == "complete" ]] \
  || die "V59/V60 are already applied while a retired execution deployment is still registered"

echo "Read-only preflight passed. No state has been changed."
[[ "${CONFIRMED}" == 1 ]] \
  || die "rerun with --confirm-destructive-cutover after reviewing the printed evidence"

echo "== Archive terminal execution evidence =="
if [[ "${ARCHIVE_STAGE}" == "complete" ]]; then
  echo "cutover-manifest.txt already records this cutover's evidence; skipping archive"
else
  ARCHIVE_FILE="${ARCHIVE_DIR}/terminal-execution-before-v59.sql"
  printf '%s\n' "${DEPLOYMENTS_JSON}" >"${ARCHIVE_DIR}/restate-deployments-before-cutover.json"
  TERMINAL_INVOCATIONS_SQL="
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
  psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --pset=pager=off \
    --command "\\copy (SELECT * FROM ${SESSION_EVENTS_SCHEMA}.events WHERE event_type = 'ExecutionProgress') TO '${ARCHIVE_DIR}/undecodable-execution-progress-events.csv' WITH (FORMAT csv, HEADER true)"
  # The manifest is the resume marker, so it is written last: a directory that
  # holds it has every preceding artifact in this stage.
  {
    printf 'old_deployment_id=%s\n' "${OLD_DEPLOYMENT_ID}"
    printf 'new_deployment_uri=%s\n' "${NEW_DEPLOYMENT_URI}"
    printf 'nonterminal_runs=%s\n' "${NONTERMINAL_RUNS}"
    printf 'old_deployment_invocations=%s\n' "${OLD_INVOCATIONS}"
    printf 'stale_execution_progress_events=%s\n' "${STALE_PROGRESS_EVENTS}"
    printf 'session_events_schema=%s\n' "${SESSION_EVENTS_SCHEMA}"
    printf 'archive_bytes=%s\n' "$(wc -c <"${ARCHIVE_FILE}" | tr -d ' ')"
  } >"${ARCHIVE_MANIFEST}"
fi

# The retired deployment is removed before the schema is cut. Preflight proves
# current counts are zero, not that new work cannot arrive: while the old
# deployment stays registered, `Execution/start` and the retired
# ExecutionRun/ExecutionTask/ExecutionCompensation handlers remain routable, and
# after V59 they would be routable against a schema they cannot read. The
# journal purge runs first because it addresses those services by name and they
# stop existing the moment the deployment is gone.
echo "== Reset retired execution-service state and remove the exact old deployment =="
if [[ "${DEPLOYMENT_STAGE}" == "complete" ]]; then
  echo "retired deployment is already removed; skipping state reset and removal"
else
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
fi

echo "== Apply repository-owned V59/V60 migration chain =="
if [[ "${MIGRATION_STAGE}" == "complete" ]]; then
  echo "V59/V60 already recorded with their exact identities; skipping migration"
  applied_migration_identities >"${ARCHIVE_DIR}/applied-migrations.txt"
else
  (
    cd -- "${REPO_ROOT}"
    MOA_DATABASE_URL="${DATABASE_ADMIN_URL}" \
    MOA_DATABASE_ADMIN_URL="${DATABASE_ADMIN_URL}" \
      cargo run -p moa-orchestrator --bin moa-orchestrator-bin --locked -- migrate
  )
  APPLIED_MIGRATIONS="$(applied_migration_identities | tee "${ARCHIVE_DIR}/applied-migrations.txt")"
  [[ "${APPLIED_MIGRATIONS}" == "${APPLIED_MIGRATIONS_EXPECTED}" ]] \
    || die "the repository runner did not record the exact V59/V60 identities"
fi

# `ExecutionProgress` gained ten fields, six of them required, under
# `deny_unknown_fields`. Decoding propagates rather than skips, so one historical
# row fails an entire session history replay, dashboard page, or archive read.
# Every run is already terminal here, so the progress trail has no surviving
# reader; the rows are archived as CSV above and deleted. Delete-where is
# naturally idempotent, so this stage needs no resume marker of its own.
echo "== Retire undecodable historical ExecutionProgress session events =="
psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --pset=pager=off \
  --command "DELETE FROM ${SESSION_EVENTS_SCHEMA}.events WHERE event_type = 'ExecutionProgress';" \
  | tee "${ARCHIVE_DIR}/retired-execution-progress-events.txt"
REMAINING_PROGRESS_EVENTS="$(
  psql -X "${DATABASE_ADMIN_URL}" --set=ON_ERROR_STOP=1 --tuples-only --no-align \
    --command "SELECT count(*) FROM ${SESSION_EVENTS_SCHEMA}.events WHERE event_type = 'ExecutionProgress';"
)"
[[ "${REMAINING_PROGRESS_EVENTS}" == 0 ]] \
  || die "undecodable ExecutionProgress events remain after the retirement delete"
cat >&2 <<WARNING
WARNING: live session events are clean, but archived history is not covered.
  ${SESSION_EVENTS_SCHEMA}.session_event_archives holds compressed payload blobs
  that this delete cannot reach, and any blob containing an ExecutionProgress
  event is now undecodable. decode_event_from_storage propagates rather than
  skips, so reading such an archive fails for that entire session.
  Remedy, deliberately NOT automated here because dropping an archive row
  destroys unrelated history for the same session:
    * local/compose environments: run 'make dev-wipe'.
    * every other environment: identify the affected sessions and drop their
      ${SESSION_EVENTS_SCHEMA}.session_event_archives rows as an explicit,
      separately reviewed operation.
WARNING

echo "== Register the new immutable endpoint =="
DEPLOYMENTS_JSON="$(curl -fsS "${RESTATE_ADMIN_URL}/deployments")"
NEW_DEPLOYMENT_MATCHES="$(
  jq -r --arg uri "${NEW_DEPLOYMENT_URI}" \
    '[.deployments[] | select(((.uri // "") | rtrimstr("/")) == $uri)] | length' \
    <<<"${DEPLOYMENTS_JSON}"
)"
if [[ "${NEW_DEPLOYMENT_MATCHES}" == 0 ]]; then
  curl -fsS \
    -X POST "${RESTATE_ADMIN_URL}/deployments" \
    -H "content-type: application/json" \
    --data-binary "$(jq -cn --arg uri "${NEW_DEPLOYMENT_URI}" '{uri: $uri}')" \
    -o "${ARCHIVE_DIR}/new-deployment-registration.json"
elif [[ "${NEW_DEPLOYMENT_MATCHES}" == 1 ]]; then
  echo "the new immutable endpoint is already registered; skipping registration"
else
  die "the new deployment URI resolves to more than one registered deployment"
fi

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
