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
