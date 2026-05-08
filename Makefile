.PHONY: dev dev-down dev-wipe dev-logs dev-restate-ui dev-status

dev:
	docker compose up -d --build
	@echo ""
	@echo "moa local stack is starting. ports:"
	@echo "  postgres        localhost:25432  (user=moa_owner db=moa pw=dev)"
	@echo "  restate ingress localhost:18080"
	@echo "  restate admin   localhost:9070   (web UI: http://localhost:9070)"
	@echo "  orchestrator    localhost:9080   (health: http://localhost:9081/_health/live)"
	@echo "  pii service     localhost:8080"
	@echo ""
	@echo "use 'make dev-status' to wait for everything to come up."

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
