.PHONY: dev dev-down dev-logs

dev:
	docker compose up -d
	@until docker compose exec -T postgres pg_isready -U moa >/dev/null 2>&1; do \
		echo "waiting for postgres..."; sleep 1; \
	done
	@echo "postgres ready on localhost:5432 (user=moa db=moa)"

dev-down:
	docker compose down

dev-logs:
	docker compose logs -f postgres
