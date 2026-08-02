CREATE TABLE security_events (
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
    retrieval_operation_id TEXT
);

CREATE INDEX idx_security_events_tenant_time
    ON security_events(tenant_id, occurred_at DESC);

-- One signed evidence row per logical retrieval. The caller supplies a
-- replay-stable operation id; uniqueness is database-owned across replicas.
CREATE UNIQUE INDEX security_events_retrieval_operation_uniq
    ON security_events (tenant_id, retrieval_operation_id)
    WHERE retrieval_operation_id IS NOT NULL;

-- Signed evidence is append-only. Tenant purge remains possible because this
-- guard rejects UPDATE only; DELETE is governed by the bounded purge protocol.
CREATE FUNCTION reject_security_event_update() RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog
AS $$
BEGIN
    RAISE EXCEPTION 'security_events rows are immutable'
        USING ERRCODE = '55000';
END
$$;

CREATE TRIGGER security_events_reject_update
    BEFORE UPDATE ON security_events
    FOR EACH ROW EXECUTE FUNCTION reject_security_event_update();

CREATE TABLE tenant_signing_keys (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id      UUID        NOT NULL,
    key_b64        TEXT        NOT NULL,
    active         BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deactivated_at TIMESTAMPTZ
);

CREATE UNIQUE INDEX idx_tenant_signing_keys_active
    ON tenant_signing_keys(tenant_id)
    WHERE active = TRUE;

CREATE INDEX idx_tenant_signing_keys_tenant
    ON tenant_signing_keys(tenant_id, created_at DESC);
