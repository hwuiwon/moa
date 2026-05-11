.PHONY: dev fga-bootstrap fga-install dev-down dev-wipe dev-logs dev-restate-ui dev-status loadtest-mock loadtest-live

dev:
ifeq ($(MOA_SKIP_FGA),1)
	@echo ">> MOA_SKIP_FGA=1 set; bringing up stack WITHOUT OpenFGA"
	docker compose up -d postgres restate restate-register moa-orchestrator moa-pii-service moa-audit-shipper
else
	@echo ">> bringing up full stack with OpenFGA (default)"
	docker compose up -d
	@$(MAKE) fga-bootstrap
endif

fga-bootstrap:
	@echo ">> waiting for OpenFGA"
	@./scripts/wait-for-fga.sh
	@echo ">> running moa-fga-bootstrap"
	@MOA_OPENFGA_URL=$${MOA_OPENFGA_URL:-http://localhost:8081} \
	 MOA_OPENFGA_PRESHARED_KEY=$${MOA_OPENFGA_PRESHARED_KEY:-localdev-preshared-key-do-not-use-in-prod} \
	 cargo run -q -p moa-fga-bootstrap
	@echo ">> store/model IDs written to .env.fga"

fga-install:
	@echo ">> installing fga CLI"
	go install github.com/openfga/cli/cmd/fga@latest
	@echo ">> done; ensure \$$GOPATH/bin (or \$$HOME/go/bin) is on PATH"

dev-status:
	@docker compose ps
	@echo "---"
	@echo "waiting for orchestrator readiness..."
	@for i in $$(seq 1 30); do \
		if curl -fsS http://localhost:9081/_health/ready >/dev/null 2>&1; then \
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
	@echo "open http://localhost:9070"

loadtest-mock:
	@echo "restarting orchestrator with scripted providers..."
	@MOA_PROVIDERS_OVERRIDE=scripted:/loadtest-scripts/perf-gate.json \
	  docker compose up -d --build --force-recreate moa-orchestrator restate-register
	@$(MAKE) dev-status
	cargo run -p moa-loadtest --release --bin moa-loadtest -- --mode mock --endpoint http://localhost:18080

loadtest-live:
	cargo run -p moa-loadtest --release --bin moa-loadtest -- --mode live --endpoint http://localhost:18080
