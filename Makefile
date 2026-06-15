.PHONY: dev fga-bootstrap dev-down dev-wipe dev-logs dev-restate-ui dev-status e2e-clean e2e-clean-live loadtest-mock loadtest-live

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

fga-bootstrap:
	@echo ">> waiting for OpenFGA"
	@./scripts/wait-for-fga.sh
	@echo ">> running moa-fga-bootstrap"
	@MOA_AUTHZ_OPENFGA_URL=$${MOA_AUTHZ_OPENFGA_URL:-http://localhost:10030} \
	 MOA_AUTHZ_OPENFGA_PRESHARED_KEY=$${MOA_AUTHZ_OPENFGA_PRESHARED_KEY:-localdev-preshared-key-do-not-use-in-prod} \
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
