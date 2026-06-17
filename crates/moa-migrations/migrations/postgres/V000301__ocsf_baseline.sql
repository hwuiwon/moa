
-- Source: V000301__ocsf_security_events.sql

CREATE TABLE IF NOT EXISTS security_events (
    id                  UUID        PRIMARY KEY,
    tenant_id           UUID        NOT NULL,
    class_uid           INTEGER     NOT NULL,
    activity_id         INTEGER     NOT NULL,
    category_uid        INTEGER     NOT NULL,
    severity_id         INTEGER     NOT NULL,
    type_uid            BIGINT      NOT NULL,
    actor_user_uid      TEXT,
    actor_session_uid   TEXT,
    target_resource_uid TEXT,
    event_jcs           BYTEA       NOT NULL,
    signature_hex       TEXT        NOT NULL,
    signing_key_id      UUID        NOT NULL,
    occurred_at         TIMESTAMPTZ NOT NULL,
    inserted_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    shipped_at          TIMESTAMPTZ,
    ship_attempts       INTEGER     NOT NULL DEFAULT 0,
    last_ship_error     TEXT
);

CREATE INDEX IF NOT EXISTS idx_security_events_tenant_time
    ON security_events(tenant_id, occurred_at DESC);

CREATE INDEX IF NOT EXISTS idx_security_events_unshipped
    ON security_events(tenant_id, id)
    WHERE shipped_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_security_events_class
    ON security_events(class_uid, occurred_at DESC);

CREATE OR REPLACE FUNCTION security_events_notify() RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify('security_events', NEW.tenant_id::TEXT);
    RETURN NULL;
END
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS security_events_notify ON security_events;
CREATE TRIGGER security_events_notify
    AFTER INSERT ON security_events
    FOR EACH ROW EXECUTE FUNCTION security_events_notify();

-- Source: V000302__ocsf_tenant_signing_keys.sql

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS tenant_signing_keys (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id      UUID        NOT NULL,
    key_b64        TEXT        NOT NULL,
    active         BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deactivated_at TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_tenant_signing_keys_active
    ON tenant_signing_keys(tenant_id)
    WHERE active = TRUE;

CREATE INDEX IF NOT EXISTS idx_tenant_signing_keys_tenant
    ON tenant_signing_keys(tenant_id, created_at DESC);

-- Source: V000303__ocsf_tenant_audit_destinations.sql

CREATE TABLE IF NOT EXISTS tenant_audit_destinations (
    tenant_id              UUID PRIMARY KEY,
    bucket_name            TEXT NOT NULL,
    region                 TEXT NOT NULL,
    assume_role_arn        TEXT,
    key_prefix             TEXT NOT NULL DEFAULT 'ocsf/',
    object_lock_days       INTEGER NOT NULL DEFAULT 2190,
    encryption_kms_key_arn TEXT
);

-- Audit shipper cleanup index.
CREATE INDEX IF NOT EXISTS idx_security_events_shipped_cleanup
    ON security_events(shipped_at)
    WHERE shipped_at IS NOT NULL;
