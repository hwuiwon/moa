.PHONY: dev fga-bootstrap dev-down dev-wipe dev-logs dev-restate-ui dev-status test-fast test-affected test-ci test-db-session test-db-memory test-authz-pentest test-service-e2e test-provider-e2e build-timings e2e-clean e2e-clean-live loadtest-mock loadtest-live graphify

# Install the repo-pinned graphify CLI (version from
# .agents/skills/graphify/.graphify_version) via uv, so every contributor runs
# the same version the skill and .claude hooks expect.
graphify:
	@./scripts/setup-graphify.sh

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
	@echo "restarting orchestrator with scripted providers..."
	@MOA_PROVIDERS_OVERRIDE=scripted:/loadtest-scripts/perf-gate.json \
	  docker compose up -d --build --force-recreate moa-orchestrator restate-register
	@$(MAKE) dev-status
	cargo run -p moa-loadtest --release --bin moa-loadtest -- --mode mock --endpoint http://localhost:10010

loadtest-live:
	cargo run -p moa-loadtest --release --bin moa-loadtest -- --mode live --endpoint http://localhost:10010

# Generates a local-dev RSA keypair for contact-token signing and prints the
# env exports the compose stack needs for edge-mode load tests.
loadtest-edge-keys:
	@mkdir -p target/loadtest-keys
	@[ -f target/loadtest-keys/contact-tokens.pem ] || \
	  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out target/loadtest-keys/contact-tokens.pem 2>/dev/null
	@openssl rsa -in target/loadtest-keys/contact-tokens.pem -pubout -out target/loadtest-keys/contact-tokens.pub.pem 2>/dev/null
	@echo "export MOA_AUTH_CONTACT_TOKENS_PRIVATE_KEY_PEM=\"$$(cat target/loadtest-keys/contact-tokens.pem)\""
	@echo "export MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM=\"$$(cat target/loadtest-keys/contact-tokens.pub.pem)\""
