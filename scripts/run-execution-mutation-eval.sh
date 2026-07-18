#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${ROOT_DIR}/target/execution-mutants"
MUTANTS_OUTPUT_DIR="${OUTPUT_DIR}/mutants.out"
CONFIG_PATH="${ROOT_DIR}/.cargo/mutants-execution.toml"
SELECTED_LIST="$(mktemp)"
trap 'rm -f "${SELECTED_LIST}"' EXIT

if ! cargo mutants --version >/dev/null 2>&1; then
  echo "cargo-mutants is required; install it with: cargo install --locked cargo-mutants" >&2
  exit 1
fi

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

FUNCTION_FILTER='(evaluate_completion|evaluate_coverage|BudgetLedger::try_reserve|BudgetLedger::reconcile_cumulative|validate_amendment_reference_narrowing|validate_plan_references|ExecutionRepository::record_task_outcome|task_outcome_is_exact_replay|classifier_fallback|classifier_fallback_with_response|valid_classifier_output|below_confidence_threshold|ExecutionRouteReason::strategy|durable_upgrade_transition|routing_cost|strategy_cost)'
MUTANT_ARGS=(
  --manifest-path "${ROOT_DIR}/Cargo.toml"
  --config "${CONFIG_PATH}"
  --package moa-execution
  --package moa-brain
  --package moa-core
  --package moa-eval
  --file "crates/moa-execution/src/completion.rs"
  --file "crates/moa-execution/src/budget.rs"
  --file "crates/moa-execution/src/compiler.rs"
  --file "crates/moa-execution/src/repository.rs"
  --file "crates/moa-brain/src/execution_planning/routing.rs"
  --file "crates/moa-core/src/types/execution_planning.rs"
  --file "crates/moa-eval/src/execution/routing.rs"
  --re "${FUNCTION_FILTER}"
)

cd "${ROOT_DIR}"
cargo mutants "${MUTANT_ARGS[@]}" --list >"${SELECTED_LIST}"
if ! grep -q '[^[:space:]]' "${SELECTED_LIST}"; then
  echo "targeted execution mutation selection is empty" >&2
  exit 1
fi

rm -rf "${OUTPUT_DIR}" "${OUTPUT_DIR}.old"
set +e
MOA_DATABASE_URL="${MOA_DATABASE_URL:-postgres://moa_owner:dev@127.0.0.1:10040/moa}" \
  cargo mutants "${MUTANT_ARGS[@]}" --output "${OUTPUT_DIR}"
MUTANTS_STATUS=$?
set -e

if [[ ! -f "${MUTANTS_OUTPUT_DIR}/outcomes.json" ]]; then
  echo "cargo-mutants did not write ${MUTANTS_OUTPUT_DIR}/outcomes.json" >&2
  exit 1
fi
cp "${SELECTED_LIST}" "${OUTPUT_DIR}/selected-mutants.txt"
cp "${MUTANTS_OUTPUT_DIR}/outcomes.json" "${OUTPUT_DIR}/outcomes.json"
cp "${MUTANTS_OUTPUT_DIR}/missed.txt" "${OUTPUT_DIR}/missed.txt"

cargo run -p xtask --locked --features eval-tools -- execution-eval mutation-report \
  --outcomes "${OUTPUT_DIR}/outcomes.json" \
  --output "${OUTPUT_DIR}/report.json" \
  --min-score 0.90

case "${MUTANTS_STATUS}" in
  0|2|3) ;;
  *)
    echo "cargo-mutants failed before a valid score could be established (status ${MUTANTS_STATUS})" >&2
    exit "${MUTANTS_STATUS}"
    ;;
esac
