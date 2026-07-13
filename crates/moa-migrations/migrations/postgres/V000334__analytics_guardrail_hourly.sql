-- Hourly guardrail-outcome rollup for the security-posture dashboard.
--
-- Prompt-injection and guardrail panels would otherwise JSONB-scan the hottest
-- `events` table on every dashboard cycle. This materialized view pre-aggregates
-- GuardrailCheck events into hourly buckets keyed by tenant, storage partition,
-- direction, pass/fail, and enforcement so the dashboard reads a small columnar
-- rollup instead. Input-direction failed checks (`direction = 'input' AND NOT
-- passed`) are the prompt-injection signal.
--
-- Refreshed CONCURRENTLY by the analytics materialized-view cron alongside the
-- other analytics fact views (see run_analytics_mv_refreshes); the unique index
-- below is what makes the concurrent refresh legal.

CREATE SCHEMA IF NOT EXISTS analytics;

DROP MATERIALIZED VIEW IF EXISTS analytics.guardrail_hourly;

CREATE MATERIALIZED VIEW analytics.guardrail_hourly AS
SELECT
    date_trunc('hour', e.timestamp) AS bucket,
    e.tenant_id,
    e.storage_partition_id,
    COALESCE(e.payload -> 'data' ->> 'direction', 'unknown') AS direction,
    COALESCE((e.payload -> 'data' ->> 'passed')::BOOLEAN, FALSE) AS passed,
    COALESCE((e.payload -> 'data' ->> 'enforced')::BOOLEAN, FALSE) AS enforced,
    COUNT(*) AS checks
FROM events e
WHERE e.event_type = 'GuardrailCheck'
GROUP BY 1, 2, 3, 4, 5, 6;

-- Required for REFRESH MATERIALIZED VIEW CONCURRENTLY; also the natural key.
CREATE UNIQUE INDEX analytics_guardrail_hourly_uidx
    ON analytics.guardrail_hourly(
        tenant_id, storage_partition_id, bucket, direction, passed, enforced
    );

-- Serves the dashboard's tenant + time-ordered scans.
CREATE INDEX analytics_guardrail_hourly_tenant_bucket_idx
    ON analytics.guardrail_hourly(tenant_id, bucket DESC);
