#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${ROOT_DIR}/target/execution-mutants"
ROUTING_CONFIG_PATH="${ROOT_DIR}/.cargo/mutants-execution-routing.toml"
CONTROL_CONFIG_PATH="${ROOT_DIR}/.cargo/mutants-execution-control.toml"
RUNTIME_CONFIG_PATH="${ROOT_DIR}/.cargo/mutants-execution-runtime.toml"
MIN_MUTATION_SCORE="0.90"
RUN_COMPLETED=0
RUN_PHASE="initialization"

record_run_exit() {
  local exit_code="$1"

  if [[ -d "${OUTPUT_DIR}" ]]; then
    if [[ "${RUN_COMPLETED}" -eq 1 && "${exit_code}" -eq 0 ]]; then
      printf 'status=complete\nphase=complete\nexit_code=0\n' >"${OUTPUT_DIR}/run-status.txt"
    else
      printf 'status=failed\nphase=%s\nexit_code=%s\n' "${RUN_PHASE}" "${exit_code}" \
        >"${OUTPUT_DIR}/run-status.txt"
    fi
  fi
}

begin_run_phase() {
  RUN_PHASE="$1"
  printf 'status=started\nphase=%s\nexit_code=pending\n' "${RUN_PHASE}" \
    >"${OUTPUT_DIR}/run-status.txt"
}

record_lane_phase() {
  local lane_dir="$1"
  local phase="$2"
  local status="$3"
  local exit_code="$4"
  local detail="$5"

  printf '%s\t%s\t%s\t%s\n' "${phase}" "${status}" "${exit_code}" "${detail}" \
    >>"${lane_dir}/status.tsv"
}

write_lane_status() {
  local lane_dir="$1"
  local status="$2"
  local phase="$3"
  local exit_code="$4"
  local mutation_exit_code="$5"

  printf 'status=%s\nphase=%s\nexit_code=%s\nmutation_exit_code=%s\n' \
    "${status}" "${phase}" "${exit_code}" "${mutation_exit_code}" \
    >"${lane_dir}/status.txt"
}

fail_lane() {
  local lane_dir="$1"
  local phase="$2"
  local exit_code="$3"
  local mutation_exit_code="$4"
  local detail="$5"
  local message="$6"

  record_lane_phase "${lane_dir}" "${phase}" "failed" "${exit_code}" "${detail}"
  write_lane_status \
    "${lane_dir}" "failed" "${phase}" "${exit_code}" "${mutation_exit_code}"
  echo "${message}" >&2
  exit "${exit_code}"
}

trap 'record_run_exit "$?"' EXIT

rm -rf "${OUTPUT_DIR}" "${OUTPUT_DIR}.old"
mkdir -p "${OUTPUT_DIR}"
printf 'status=started\nphase=%s\nexit_code=pending\n' "${RUN_PHASE}" \
  >"${OUTPUT_DIR}/run-status.txt"

begin_run_phase "dependencies"
if ! cargo mutants --version >/dev/null 2>&1; then
  echo "cargo-mutants is required; install it with: cargo install --locked cargo-mutants" >&2
  exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required to aggregate focused cargo-mutants outcomes" >&2
  exit 1
fi

prepare_postgres() {
  docker compose -f "${ROOT_DIR}/docker-compose.yml" up -d --build postgres
  for _ in $(seq 1 60); do
    if docker compose -f "${ROOT_DIR}/docker-compose.yml" exec -T postgres \
      pg_isready -U moa_owner -d moa >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
  if ! docker compose -f "${ROOT_DIR}/docker-compose.yml" exec -T postgres \
    pg_isready -U moa_owner -d moa >/dev/null 2>&1; then
    echo "PostgreSQL did not become ready for repository generation-fence mutants" >&2
    exit 1
  fi
}

run_mutation_lane() {
  local lane_name="$1"
  shift
  local lane_dir="${OUTPUT_DIR}/${lane_name}"
  local mutants_output_dir="${lane_dir}/mutants.out"
  local selected_list="${lane_dir}/selected-mutants.txt"
  local selection_status
  local mutants_status
  local -a pipeline_status
  local report_status

  mkdir -p "${lane_dir}"
  printf 'phase\tstatus\texit_code\tdetail\n' >"${lane_dir}/status.tsv"
  record_lane_phase \
    "${lane_dir}" "selection" "started" "pending" "listing targeted mutants"
  write_lane_status "${lane_dir}" "started" "selection" "pending" "pending"

  set +e
  cargo mutants "$@" --list >"${selected_list}" \
    2>"${lane_dir}/selection.log"
  selection_status=$?
  set -e
  if [[ "${selection_status}" -ne 0 ]]; then
    fail_lane \
      "${lane_dir}" "selection" "${selection_status}" "not_started" \
      "cargo-mutants list failed" \
      "cargo-mutants could not list the ${lane_name} execution mutation selection"
  fi
  if ! grep -q '[^[:space:]]' "${selected_list}"; then
    fail_lane \
      "${lane_dir}" "selection" "1" "not_started" "targeted selection was empty" \
      "targeted ${lane_name} execution mutation selection is empty"
  fi
  record_lane_phase \
    "${lane_dir}" "selection" "complete" "0" "selected-mutants.txt persisted"
  record_lane_phase \
    "${lane_dir}" "mutation" "started" "pending" "running selected mutants"
  write_lane_status "${lane_dir}" "started" "mutation" "pending" "pending"

  set +e
  MOA_DATABASE_URL="${MOA_DATABASE_URL:-postgres://moa_owner:dev@127.0.0.1:10040/moa}" \
    cargo mutants "$@" --output "${lane_dir}" 2>&1 | tee "${lane_dir}/mutation.log"
  pipeline_status=("${PIPESTATUS[@]}")
  set -e
  mutants_status="${pipeline_status[0]}"

  if [[ "${pipeline_status[1]}" -ne 0 ]]; then
    fail_lane \
      "${lane_dir}" "mutation" "${pipeline_status[1]}" "${mutants_status}" \
      "mutation log could not be persisted" \
      "cargo-mutants ${lane_name} mutation log could not be persisted"
  fi

  case "${mutants_status}" in
    0|2|3)
      record_lane_phase \
        "${lane_dir}" "mutation" "complete" "${mutants_status}" \
        "cargo-mutants produced a scoreable outcome"
      ;;
    *)
      fail_lane \
        "${lane_dir}" "mutation" "${mutants_status}" "${mutants_status}" \
        "cargo-mutants failed before a valid score" \
        "cargo-mutants ${lane_name} lane failed before a valid score could be established (status ${mutants_status})"
      ;;
  esac

  if [[ ! -f "${mutants_output_dir}/outcomes.json" ]]; then
    fail_lane \
      "${lane_dir}" "artifacts" "1" "${mutants_status}" \
      "mutants.out/outcomes.json was missing" \
      "cargo-mutants did not write ${mutants_output_dir}/outcomes.json"
  fi
  if [[ ! -f "${mutants_output_dir}/missed.txt" ]]; then
    fail_lane \
      "${lane_dir}" "artifacts" "1" "${mutants_status}" \
      "mutants.out/missed.txt was missing" \
      "cargo-mutants did not write ${mutants_output_dir}/missed.txt"
  fi
  cp "${mutants_output_dir}/outcomes.json" "${lane_dir}/outcomes.json"
  cp "${mutants_output_dir}/missed.txt" "${lane_dir}/missed.txt"
  record_lane_phase \
    "${lane_dir}" "artifacts" "complete" "0" \
    "lane outcomes and missed mutants persisted"

  record_lane_phase \
    "${lane_dir}" "report" "started" "pending" "building lane mutation report"
  set +e
  cargo run -p xtask --locked --features eval-tools -- execution-eval mutation-report \
    --outcomes "${lane_dir}/outcomes.json" \
    --output "${lane_dir}/report.json" \
    --min-score 0 2>&1 | tee "${lane_dir}/report.log"
  pipeline_status=("${PIPESTATUS[@]}")
  set -e
  report_status="${pipeline_status[0]}"
  if [[ "${report_status}" -ne 0 || "${pipeline_status[1]}" -ne 0 ]]; then
    if [[ "${report_status}" -eq 0 ]]; then
      report_status="${pipeline_status[1]}"
    fi
    fail_lane \
      "${lane_dir}" "report" "${report_status}" "${mutants_status}" \
      "lane mutation report failed" \
      "${lane_name} lane mutation report failed"
  fi

  record_lane_phase "${lane_dir}" "report" "complete" "0" "lane report persisted"
  write_lane_status "${lane_dir}" "complete" "report" "0" "${mutants_status}"
}

ROUTING_FUNCTION_FILTER='(classifier_fallback|classifier_fallback_with_response|valid_classifier_output|below_confidence_threshold|execution_route_rationale_is_valid|durable_upgrade_transition|routing_cost|strategy_cost)'
ROUTING_MUTANT_ARGS=(
  --manifest-path "${ROOT_DIR}/Cargo.toml"
  --config "${ROUTING_CONFIG_PATH}"
  --package moa-brain
  --package moa-core
  --package moa-eval
  --file "crates/moa-brain/src/execution_planning/routing.rs"
  --file "crates/moa-core/src/types/execution_planning.rs"
  --file "crates/moa-eval/src/execution/routing.rs"
  --re "${ROUTING_FUNCTION_FILTER}"
)

CONTROL_FUNCTION_FILTER='(configure_durable_upgrade_tool_schema|durable_upgrade_signal_from_control_call)'
CONTROL_MUTANT_ARGS=(
  --manifest-path "${ROOT_DIR}/Cargo.toml"
  --config "${CONTROL_CONFIG_PATH}"
  --package moa-orchestrator
  --file "crates/moa-orchestrator/src/workflows/turn_execution/tools.rs"
  --re "${CONTROL_FUNCTION_FILTER}"
)

RUNTIME_FUNCTION_FILTER='(evaluate_completion|evaluate_coverage|BudgetLedger::try_reserve|BudgetLedger::reconcile_cumulative|validate_amendment_reference_narrowing|validate_plan_references|ExecutionRepository::record_task_outcome|task_outcome_is_exact_replay)'
RUNTIME_MUTANT_ARGS=(
  --manifest-path "${ROOT_DIR}/Cargo.toml"
  --config "${RUNTIME_CONFIG_PATH}"
  --package moa-execution
  --file "crates/moa-execution/src/completion.rs"
  --file "crates/moa-execution/src/budget.rs"
  --file "crates/moa-execution/src/compiler.rs"
  --file "crates/moa-execution/src/repository.rs"
  --re "${RUNTIME_FUNCTION_FILTER}"
)

cd "${ROOT_DIR}"

begin_run_phase "routing"
run_mutation_lane "routing" "${ROUTING_MUTANT_ARGS[@]}"
begin_run_phase "control"
run_mutation_lane "control" "${CONTROL_MUTANT_ARGS[@]}"
begin_run_phase "postgres"
prepare_postgres
begin_run_phase "runtime"
run_mutation_lane "runtime" "${RUNTIME_MUTANT_ARGS[@]}"

begin_run_phase "aggregation"
jq -s '{outcomes: [.[].outcomes[]]}' \
  "${OUTPUT_DIR}/routing/outcomes.json" \
  "${OUTPUT_DIR}/control/outcomes.json" \
  "${OUTPUT_DIR}/runtime/outcomes.json" >"${OUTPUT_DIR}/outcomes.json" \
  2>"${OUTPUT_DIR}/aggregation.log"
LC_ALL=C sort -u \
  "${OUTPUT_DIR}/routing/selected-mutants.txt" \
  "${OUTPUT_DIR}/control/selected-mutants.txt" \
  "${OUTPUT_DIR}/runtime/selected-mutants.txt" >"${OUTPUT_DIR}/selected-mutants.txt" \
  2>>"${OUTPUT_DIR}/aggregation.log"
LC_ALL=C sort -u \
  "${OUTPUT_DIR}/routing/missed.txt" \
  "${OUTPUT_DIR}/control/missed.txt" \
  "${OUTPUT_DIR}/runtime/missed.txt" >"${OUTPUT_DIR}/missed.txt" \
  2>>"${OUTPUT_DIR}/aggregation.log"

begin_run_phase "aggregate_report"
cargo run -p xtask --locked --features eval-tools -- execution-eval mutation-report \
  --outcomes "${OUTPUT_DIR}/outcomes.json" \
  --output "${OUTPUT_DIR}/report.json" \
  --min-score "${MIN_MUTATION_SCORE}" 2>&1 | tee "${OUTPUT_DIR}/aggregate-report.log"
begin_run_phase "complete"
RUN_COMPLETED=1
