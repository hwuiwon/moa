.PHONY: dev fga-bootstrap dev-down dev-wipe dev-logs dev-restate-ui dev-status test-fast test-affected test-ci test-db-session test-db-memory test-authz-pentest test-service-e2e test-provider-e2e build-timings e2e-clean e2e-clean-live loadtest-mock loadtest-live loadtest-capacity loadtest-capacity-edge loadtest-capacity-direct-append loadtest-capacity-brackets chaos-smoke chaos-matrix codegraph

codegraph:
	@./scripts/codegraph init

dev:
ifeq ($(MOA_SKIP_FGA),1)
	@echo ">> MOA_SKIP_FGA=1 set; bringing up stack WITHOUT OpenFGA"
	docker compose up -d --build postgres restate moa-orchestrator restate-register moa-edge
else
	@echo ">> bringing up full stack with OpenFGA (default)"
	docker compose up -d --build postgres restate openfga
	@$(MAKE) fga-bootstrap
	@set -a; . ./.env.fga; set +a; docker compose up -d --build moa-orchestrator restate-register moa-edge
endif

# CARGO_TARGET_DIR=target/tools keeps this `cargo run -p` build out of the
# main target dir: a single-package build unifies features differently than a
# workspace build, and sharing the dir makes the next `cargo test`/nextest run
# recompile the flip-flopped crates.
fga-bootstrap:
	@echo ">> waiting for OpenFGA"
	@./scripts/wait-for-fga.sh
	@echo ">> running moa-fga-bootstrap"
	@MOA_AUTHZ_OPENFGA_URL=$${MOA_AUTHZ_OPENFGA_URL:-http://localhost:10030} \
	 MOA_AUTHZ_OPENFGA_PRESHARED_KEY=$${MOA_AUTHZ_OPENFGA_PRESHARED_KEY:-localdev-preshared-key-do-not-use-in-prod} \
	 CARGO_TARGET_DIR=target/tools \
	 cargo run -q -p moa-fga-bootstrap
	@echo ">> store/model IDs written to .env.fga"

dev-status:
	@docker compose ps
	@echo "---"
	@echo "waiting for orchestrator readiness..."
	@for i in $$(seq 1 30); do \
		if curl -fsS http://localhost:10021/_health/ready >/dev/null 2>&1; then \
			echo "orchestrator ready"; exit 0; \
		fi; \
		sleep 2; \
	done; \
	echo "orchestrator readiness timed out"; exit 1

dev-down:
	docker compose down

dev-wipe:
	docker compose down -v

dev-logs:
	docker compose logs -f moa-orchestrator restate

dev-restate-ui:
	@echo "open http://localhost:10011"

# Doc tests are intentionally not part of test-fast: the workspace currently
# has zero runnable doc examples and `cargo test --doc` still costs ~90s of
# rustdoc builds. test-ci keeps the doc-test pass as the safety net.
test-fast:
	@command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest is required; install with: cargo install cargo-nextest --locked"; exit 127; }
	cargo nextest run --locked --profile fast-pr

# Runs only tests for crates affected by the current change set (vs. the
# merge base with main). Fastest inner-loop target; use test-fast before a PR.
test-affected:
	@command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest is required; install with: cargo install cargo-nextest --locked"; exit 127; }
	./scripts/test-affected.sh

test-ci:
	@command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest is required; install with: cargo install cargo-nextest --locked"; exit 127; }
	cargo nextest run --locked --profile ci
	cargo test --locked --doc

test-db-session:
	@command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest is required; install with: cargo install cargo-nextest --locked"; exit 127; }
	cargo nextest run --locked --profile db-session

test-db-memory:
	@command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest is required; install with: cargo install cargo-nextest --locked"; exit 127; }
	cargo nextest run --locked --profile db-memory

test-authz-pentest:
	@command -v cargo-nextest >/dev/null 2>&1 || { echo "cargo-nextest is required; install with: cargo install cargo-nextest --locked"; exit 127; }
	cargo nextest run --locked --profile authz-pentest

test-service-e2e: e2e-clean-live

test-provider-e2e:
	@: $${MOA_RUN_LIVE_E2E:?set MOA_RUN_LIVE_E2E=1 to run live/billed E2E checks}
	@: $${MOA_RUN_LIVE_PROVIDER_TESTS:?set MOA_RUN_LIVE_PROVIDER_TESTS=1 to run provider E2E checks}
	./scripts/run-clean-e2e.sh --live --providers

build-timings:
	cargo test --workspace --no-run --locked --timings

e2e-clean:
	./scripts/run-clean-e2e.sh

e2e-clean-live:
	@: $${MOA_RUN_LIVE_E2E:?set MOA_RUN_LIVE_E2E=1 to run live/billed E2E checks}
	./scripts/run-clean-e2e.sh --live

loadtest-mock:
	@echo "starting loadtest dependencies with OpenFGA and safe RustFS host ports..."
	@MOA_RUSTFS_PORT=$${MOA_RUSTFS_PORT:-10090} \
	  MOA_RUSTFS_CONSOLE_PORT=$${MOA_RUSTFS_CONSOLE_PORT:-10091} \
	  docker compose up -d --build postgres restate openfga valkey rustfs rustfs-init
	@docker compose run --rm restate-rules-bootstrap >/dev/null
	@$(MAKE) fga-bootstrap
	@echo "restarting orchestrator with scripted providers..."
	@set -a; . ./.env.fga; set +a; \
	  MOA_RUSTFS_PORT=$${MOA_RUSTFS_PORT:-10090} \
	  MOA_RUSTFS_CONSOLE_PORT=$${MOA_RUSTFS_CONSOLE_PORT:-10091} \
	  MOA_DATABASE_MAX_CONNECTIONS=$${MOA_DATABASE_MAX_CONNECTIONS:-5} \
	  MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS=$${MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS:-1} \
	  MOA_DATABASE_CONNECT_TIMEOUT_SECONDS=$${MOA_DATABASE_CONNECT_TIMEOUT_SECONDS:-3} \
	  MOA_PROVIDERS_OVERRIDE=scripted:/loadtest-scripts/perf-gate.json \
	  docker compose up -d --build --force-recreate moa-orchestrator restate-register
	@$(MAKE) dev-status
	@mkdir -p target/perf-gate
	@set -a; . ./.env.fga; set +a; \
	  cargo run -p moa-loadtest --release --bin perf_gate -- \
	  --profile mock-short --endpoint http://localhost:10010 \
	  --duration $${MOA_LOADTEST_MOCK_DURATION:-30s} \
	  --vus $${MOA_LOADTEST_MOCK_VUS:-2} \
	  --qps $${MOA_LOADTEST_MOCK_QPS:-2} \
	  --max-p95-ms $${MOA_LOADTEST_MOCK_MAX_P95_MS:-5000} \
	  --max-error-rate $${MOA_LOADTEST_MOCK_MAX_ERROR_RATE:-0.01} \
	  --metrics-endpoint http://localhost:10023/metrics \
	  --prom-out target/perf-gate/mock-short.prom

loadtest-live:
	cargo run -p moa-loadtest --release --bin moa-loadtest -- --mode live --endpoint http://localhost:10010

# T2 direct-ingress capacity run: realistic scripted workload, ramp to the
# knee, report plus Restate state snapshots written under target/perf-gate/.
loadtest-capacity:
	@echo "starting capacity dependencies with the production Restate rule..."
	@MOA_RUSTFS_PORT=$${MOA_RUSTFS_PORT:-10090} \
	  MOA_RUSTFS_CONSOLE_PORT=$${MOA_RUSTFS_CONSOLE_PORT:-10091} \
	  docker compose up -d --build postgres restate openfga valkey rustfs rustfs-init
	@docker compose run --rm restate-rules-bootstrap >/dev/null
	@$(MAKE) fga-bootstrap
	@echo "restarting orchestrator with realistic scripted providers..."
	@set -a; . ./.env.fga; set +a; \
	  MOA_RUSTFS_PORT=$${MOA_RUSTFS_PORT:-10090} \
	  MOA_RUSTFS_CONSOLE_PORT=$${MOA_RUSTFS_CONSOLE_PORT:-10091} \
	  MOA_DATABASE_MAX_CONNECTIONS=$${MOA_DATABASE_MAX_CONNECTIONS:-20} \
	  MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS=$${MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS:-1} \
	  MOA_DATABASE_CONNECT_TIMEOUT_SECONDS=$${MOA_DATABASE_CONNECT_TIMEOUT_SECONDS:-3} \
	  MOA_SESSION_DIRECT_TURN_EVENT_APPEND=$${MOA_SESSION_DIRECT_TURN_EVENT_APPEND:-false} \
	  MOA_PROVIDERS_OVERRIDE=scripted:/loadtest-scripts/realistic.json \
	  docker compose up -d --build --force-recreate moa-orchestrator restate-register moa-edge
	@$(MAKE) dev-status
	@mkdir -p target/perf-gate
	@set -a; . ./.env.fga; set +a; \
	  report_path=$${MOA_LOADTEST_REPORT:-target/perf-gate/capacity-direct.json}; \
	  report_tmp=$$report_path.tmp; \
	  state_prefix=$${report_path%.json}; \
	  rules_before=$$(docker compose run --rm --no-deps restate-rules-bootstrap sql --json 'SELECT * FROM sys_rules'); \
	  limits_before=$$(docker compose run --rm --no-deps restate-rules-bootstrap sql --json 'SELECT * FROM sys_user_limits'); \
	  jq -n --argjson rules "$$rules_before" --argjson limits "$$limits_before" \
	    '{sys_rules: $$rules, sys_user_limits: $$limits}' > $$state_prefix-restate-before.json; \
	  source_revision=$$(git rev-parse --verify HEAD 2>/dev/null || echo unknown); \
	  source_state=clean; git diff --quiet --ignore-submodules HEAD -- || source_state=dirty; \
	  compose_project=$${COMPOSE_PROJECT_NAME:-moa}; \
	  shape=$${MOA_LOADTEST_SHAPE:-ramp}; \
	  rate_end_arg=; \
	  if [ "$$shape" != steady ]; then rate_end_arg="--rate-end $${MOA_LOADTEST_RATE_END:-200}"; fi; \
	  status=0; \
	MOA_LOADTEST_SOURCE_REVISION=$$source_revision \
	MOA_LOADTEST_SOURCE_STATE=$$source_state \
	MOA_LOADTEST_FOREGROUND_DB_CONNECTIONS=$${MOA_DATABASE_MAX_CONNECTIONS:-20} \
	MOA_LOADTEST_BACKGROUND_DB_CONNECTIONS=$${MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS:-1} \
	MOA_LOADTEST_COMPOSE_PROJECT=$$compose_project \
	MOA_LOADTEST_STATE_IDENTITY=$${compose_project}_moa-restate-data \
	MOA_LOADTEST_RESTATE_RULE_PROFILE="$$(printf '%s' "$$rules_before" | jq -c .)" \
	MOA_SESSION_DIRECT_TURN_EVENT_APPEND=$${MOA_SESSION_DIRECT_TURN_EVENT_APPEND:-false} \
	cargo run -p moa-loadtest --release --bin moa-loadtest -- \
	  --mode mock --endpoint http://localhost:10010 \
	  --shape $$shape --rate $${MOA_LOADTEST_RATE:-5} $$rate_end_arg \
	  --duration $${MOA_LOADTEST_DURATION:-10m} \
	  --profile mixed --think-time-ms 2000 --sessions 800 --tenants 8 \
	  $${MOA_LOADTEST_EDGE_ARGS:-} \
	  --metrics-endpoint http://localhost:10023/metrics \
	  --output json > $$report_tmp || status=$$?; \
	  rules_after=$$(docker compose run --rm --no-deps restate-rules-bootstrap sql --json 'SELECT * FROM sys_rules') || \
	    { echo "warning: post-run Restate rules snapshot failed" >&2; rules_after='[]'; }; \
	  limits_after=$$(docker compose run --rm --no-deps restate-rules-bootstrap sql --json 'SELECT * FROM sys_user_limits') || \
	    { echo "warning: post-run Restate limits snapshot failed" >&2; limits_after='[]'; }; \
	  jq -n --argjson rules "$$rules_after" --argjson limits "$$limits_after" \
	    '{sys_rules: $$rules, sys_user_limits: $$limits}' > $$state_prefix-restate-after.json; \
	  if jq -e . $$report_tmp >/dev/null 2>&1; then \
	    mv $$report_tmp $$report_path; \
	    if [ $$status -ne 0 ]; then \
	      echo "capacity ramp reached expected overload; preserving valid report"; \
	    fi; \
	  else \
	    rm -f $$report_tmp; \
	    if [ $$status -eq 0 ]; then status=1; fi; \
	    exit $$status; \
	  fi
	@echo "capacity report: $${MOA_LOADTEST_REPORT:-target/perf-gate/capacity-direct.json}"

# Production edge/auth/tenant-scope/SSE lane. It writes a distinct report so
# direct-ingress machinery results cannot be mistaken for edge certification.
loadtest-capacity-edge:
	@MOA_LOADTEST_EDGE_ARGS="--edge-endpoint http://localhost:10000" \
	  MOA_LOADTEST_REPORT=target/perf-gate/capacity-edge.json \
	  $(MAKE) loadtest-capacity

# Direct named-action event append variant for a controlled capacity comparison.
loadtest-capacity-direct-append:
	@MOA_SESSION_DIRECT_TURN_EVENT_APPEND=true \
	  MOA_LOADTEST_REPORT=target/perf-gate/capacity-direct-append.json \
	  $(MAKE) loadtest-capacity

# Fresh-state randomized comparison campaign. Each profile gets a unique
# Compose project (and therefore a fresh Restate volume), while `down` preserves
# that volume for post-run inspection. This target is intentionally long-running.
loadtest-capacity-brackets:
	@mkdir -p target/perf-gate/brackets
	@run_id=$$(date -u +%Y%m%d%H%M%S); \
	  profiles='5:ramp:200 10:ramp:200 20:ramp:200 20:steady:50 20:steady:55 20:steady:60 20:steady:65'; \
	  order=$$(printf '%s\n' $$profiles | awk 'BEGIN{srand()} {print rand(), $$0}' | sort -n | cut -d' ' -f2-); \
	  printf '%s\n' "$$order" > target/perf-gate/brackets/$$run_id-order.txt; \
	  for profile in $$order; do \
	    pool=$${profile%%:*}; remainder=$${profile#*:}; shape=$${remainder%%:*}; rate=$${remainder##*:}; \
	    project=moa-capacity-$${run_id}-$${pool}-$${shape}-$${rate}; \
	    report=target/perf-gate/brackets/$${run_id}-pool$${pool}-$${shape}$${rate}-direct.json; \
	    COMPOSE_PROJECT_NAME=$$project \
	      MOA_DATABASE_MAX_CONNECTIONS=$$pool \
	      MOA_LOADTEST_SHAPE=$$shape \
	      MOA_LOADTEST_RATE=$$([ "$$shape" = steady ] && printf '%s' "$$rate" || printf '%s' 5) \
	      MOA_LOADTEST_RATE_END=$$rate \
	      MOA_LOADTEST_REPORT=$$report \
	      $(MAKE) loadtest-capacity; \
	    status=$$?; \
	    COMPOSE_PROJECT_NAME=$$project docker compose down; \
	    if [ $$status -ne 0 ]; then exit $$status; fi; \
	  done

# Long steady soak at ~70% of measured capacity; watch the window series for
# drift (leaks, compaction pressure, partition growth).
loadtest-soak:
	@mkdir -p target/perf-gate
	cargo run -p moa-loadtest --release --bin moa-loadtest -- \
	  --mode mock --endpoint http://localhost:10010 \
	  --shape soak --rate $${SOAK_RATE:-50} --duration $${SOAK_DURATION:-2h} \
	  --profile mixed --think-time-ms 2000 --sessions 800 --tenants 8 \
	  --metrics-endpoint http://localhost:10023/metrics \
	  --output json | tee target/perf-gate/soak.json >/dev/null
	@echo "soak report: target/perf-gate/soak.json"

# One fast chaos experiment (provider 429 storm) against the compose stack.
chaos-smoke:
	@: $${MOA_AUTHZ_OPENFGA_STORE_ID:?run make fga-bootstrap and export the OpenFGA env first}
	MOA_RUSTFS_PORT=$${MOA_RUSTFS_PORT:-10090} \
	  MOA_RUSTFS_CONSOLE_PORT=$${MOA_RUSTFS_CONSOLE_PORT:-10091} \
	  MOA_RUN_CHAOS_TESTS=1 cargo nextest run -p moa-loadtest --test chaos_docker \
	  --run-ignored all --no-capture --test-threads 1 \
	  -E 'test(chaos_provider_429_storm_degrades_then_recovers_docker)'

# The full chaos experiment matrix. Experiments recreate the orchestrator and
# stop/kill stack services; run only against a disposable dev stack.
chaos-matrix:
	@: $${MOA_AUTHZ_OPENFGA_STORE_ID:?run make fga-bootstrap and export the OpenFGA env first}
	MOA_RUSTFS_PORT=$${MOA_RUSTFS_PORT:-10090} \
	  MOA_RUSTFS_CONSOLE_PORT=$${MOA_RUSTFS_CONSOLE_PORT:-10091} \
	  MOA_RUN_CHAOS_TESTS=1 cargo nextest run -p moa-loadtest --test chaos_docker \
	  --run-ignored all --no-capture --test-threads 1 --no-fail-fast

# Generates a local-dev RSA keypair for contact-token signing and prints the
# env exports the compose stack needs for edge-mode load tests.
loadtest-edge-keys:
	@mkdir -p target/loadtest-keys
	@[ -f target/loadtest-keys/contact-tokens.pem ] || \
	  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out target/loadtest-keys/contact-tokens.pem 2>/dev/null
	@openssl rsa -in target/loadtest-keys/contact-tokens.pem -pubout -out target/loadtest-keys/contact-tokens.pub.pem 2>/dev/null
	@echo "export MOA_AUTH_CONTACT_TOKENS_PRIVATE_KEY_PEM=\"$$(cat target/loadtest-keys/contact-tokens.pem)\""
	@echo "export MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM=\"$$(cat target/loadtest-keys/contact-tokens.pub.pem)\""
