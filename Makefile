.PHONY: dev fga-bootstrap dev-down dev-wipe dev-logs dev-restate-ui dev-status test-fast test-affected test-ci test-db-session test-db-memory test-authz-pentest test-service-e2e test-provider-e2e build-timings e2e-clean e2e-clean-live loadtest-mock loadtest-live chaos-smoke chaos-matrix codegraph

codegraph:
	@./scripts/codegraph init

dev:
ifeq ($(MOA_SKIP_FGA),1)
	@echo ">> MOA_SKIP_FGA=1 set; bringing up stack WITHOUT OpenFGA"
	docker compose up -d --build postgres restate moa-orchestrator restate-register moa-edge moa-pii-service moa-audit-shipper
else
	@echo ">> bringing up full stack with OpenFGA (default)"
	docker compose up -d --build postgres restate openfga moa-pii-service moa-audit-shipper
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
	  docker compose up -d --build postgres restate openfga valkey rustfs rustfs-init moa-pii-service moa-audit-shipper
	@$(MAKE) fga-bootstrap
	@echo "restarting orchestrator with scripted providers..."
	@set -a; . ./.env.fga; set +a; \
	  MOA_RUSTFS_PORT=$${MOA_RUSTFS_PORT:-10090} \
	  MOA_RUSTFS_CONSOLE_PORT=$${MOA_RUSTFS_CONSOLE_PORT:-10091} \
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

# T2 capacity run: realistic scripted workload, ramp to the knee, windowed
# report written to target/perf-gate/capacity.json.
loadtest-capacity:
	@echo "restarting orchestrator with realistic scripted providers..."
	@MOA_PROVIDERS_OVERRIDE=scripted:/loadtest-scripts/realistic.json \
	  docker compose up -d --build --force-recreate moa-orchestrator restate-register
	@$(MAKE) dev-status
	@mkdir -p target/perf-gate
	cargo run -p moa-loadtest --release --bin moa-loadtest -- \
	  --mode mock --endpoint http://localhost:10010 \
	  --shape ramp --rate 5 --rate-end 200 --duration 10m \
	  --profile mixed --think-time-ms 2000 --sessions 800 --tenants 8 \
	  --metrics-endpoint http://localhost:10023/metrics \
	  --output json | tee target/perf-gate/capacity.json >/dev/null
	@echo "capacity report: target/perf-gate/capacity.json"

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
