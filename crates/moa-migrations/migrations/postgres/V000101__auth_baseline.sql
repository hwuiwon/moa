
-- Source: V000101__authz_outbox.sql

-- Transactional outbox for OpenFGA tuple writes.
--
-- A handler that mutates Postgres state queues tuple operations into this
-- table inside the same transaction. The outbox poller claims pending rows
-- and applies them to OpenFGA.
--
-- Each row holds the LATEST DESIRED STATE of one tuple, not the lifetime
-- history of one operation. Tuple identity is
-- `(tuple_user, tuple_relation, tuple_object, model_version)` and is unique;
-- the desired `op` (write|delete) and a monotonically increasing `generation`
-- live on that single row. Enqueue upserts the desired op and bumps the
-- generation, so `write -> delete -> write` converges to the final desired
-- state instead of the first operation permanently owning the identity. The
-- poller applies only the newest generation (compare-and-set on success).

CREATE TABLE authz_outbox (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    op              TEXT        NOT NULL CHECK (op IN ('write', 'delete')),
    tuple_user      TEXT        NOT NULL,
    tuple_relation  TEXT        NOT NULL,
    tuple_object    TEXT        NOT NULL,
    model_version   INTEGER     NOT NULL,
    generation      BIGINT      NOT NULL DEFAULT 1,
    status          TEXT        NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'in_flight', 'succeeded', 'dead_letter')),
    attempts        INTEGER     NOT NULL DEFAULT 0,
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    tenant_id       UUID,
    -- Lease fencing so any pod can safely reclaim an abandoned in-flight row.
    lease_token       UUID,
    lease_expires_at  TIMESTAMPTZ,
    CONSTRAINT authz_outbox_tuple_identity_key
        UNIQUE (tuple_user, tuple_relation, tuple_object, model_version)
);

CREATE INDEX idx_authz_outbox_pending
    ON authz_outbox(next_attempt_at)
    WHERE status = 'pending';

CREATE INDEX idx_authz_outbox_dead_letter
    ON authz_outbox(updated_at DESC)
    WHERE status = 'dead_letter';

CREATE INDEX idx_authz_outbox_tenant
    ON authz_outbox(tenant_id, status)
    WHERE tenant_id IS NOT NULL;

-- Source: V000111__auth_providers_api_keys.sql

-- API keys for the LocalAuthProvider.
--
-- Wire format: moa_<env>_<32-char-base62-random>_<8-char-lowercase-hex-crc32>
-- Example:      moa_dev_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6_3f9a1b2c
--
-- The full key value is never stored; only argon2id(full_key) is stored.
-- Prefix lookup narrows validation to at most one candidate hash.

CREATE TABLE api_keys (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    prefix           TEXT        NOT NULL UNIQUE,
    hash             TEXT        NOT NULL,
    owner_user_id    UUID,
    owner_agent_id   UUID,
    tenant_id        UUID        NOT NULL,
    name             TEXT        NOT NULL,
    description      TEXT,
    env              TEXT        NOT NULL CHECK (env IN ('live','prod','stg','dev')),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at     TIMESTAMPTZ,
    revoked_at       TIMESTAMPTZ,
    revoked_reason   TEXT,
    scopes_synced_at TIMESTAMPTZ,
    CHECK (
        (owner_user_id IS NOT NULL AND owner_agent_id IS NULL)
        OR
        (owner_user_id IS NULL     AND owner_agent_id IS NOT NULL)
    )
);

CREATE INDEX idx_api_keys_owner_user
    ON api_keys(owner_user_id)
    WHERE owner_user_id IS NOT NULL;

CREATE INDEX idx_api_keys_owner_agent
    ON api_keys(owner_agent_id)
    WHERE owner_agent_id IS NOT NULL;

CREATE INDEX idx_api_keys_tenant
    ON api_keys(tenant_id);

CREATE TABLE api_key_revocations (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    api_key_id    UUID        NOT NULL REFERENCES api_keys(id),
    revoked_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    reason        TEXT        NOT NULL,
    actor_user_id UUID
);

CREATE INDEX idx_api_key_revocations_key
    ON api_key_revocations(api_key_id);

-- Source: V000112__auth_providers_builtin_approvals.sql

-- Pending approval requests for the BuiltinAsyncAuthzProvider.
--
-- A handler writes a row here and waits on the Restate awakeable id. The
-- approval decision handler updates this row and resolves the awakeable.

CREATE TABLE builtin_pending_approvals (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id          UUID        NOT NULL,
    deciding_user_id    UUID        NOT NULL,
    tenant_id           UUID        NOT NULL,
    awakeable_id        TEXT        NOT NULL UNIQUE,
    action_summary      TEXT        NOT NULL,
    action_details      JSONB       NOT NULL,
    status              TEXT        NOT NULL DEFAULT 'pending'
                                        CHECK (status IN ('pending', 'approved', 'denied', 'timeout')),
    deny_reason         TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at          TIMESTAMPTZ NOT NULL,
    decided_at          TIMESTAMPTZ,
    decided_by_user_id  UUID,
    resolved_at         TIMESTAMPTZ
);

CREATE INDEX idx_builtin_approvals_pending
    ON builtin_pending_approvals(deciding_user_id, created_at DESC)
    WHERE status = 'pending';

CREATE INDEX idx_builtin_approvals_session
    ON builtin_pending_approvals(session_id);

CREATE INDEX idx_builtin_approvals_expires
    ON builtin_pending_approvals(expires_at)
    WHERE status = 'pending';

-- Source: V000121__auth0_user_map.sql

-- Maps an external identity provider's subject (Auth0 `sub`, OIDC `sub`)
-- to MOA's internal users.id UUID. The users table is owned by the
-- orchestrator SCIM migration.

CREATE TABLE IF NOT EXISTS auth0_user_map (
    sub        TEXT NOT NULL,
    tenant_id  UUID NOT NULL,
    user_id    UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (sub, tenant_id)
);

CREATE INDEX IF NOT EXISTS idx_auth0_user_map_user ON auth0_user_map(user_id);

-- Source: V000122__auth0_linked_connections.sql

-- Per-user record of Auth0 connected accounts available through Token Vault.
--
-- MOA never stores the third-party access or refresh tokens. Auth0 stores
-- those in Token Vault; this table only tracks which connection names a MOA
-- user has linked and the scopes most recently observed for that connection.

CREATE TABLE IF NOT EXISTS linked_connections (
    user_id          UUID        NOT NULL,
    connection_name  TEXT        NOT NULL,
    linked_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    scopes_granted   TEXT[]      NOT NULL DEFAULT '{}',
    external_sub     TEXT,
    PRIMARY KEY (user_id, connection_name)
);

CREATE INDEX IF NOT EXISTS idx_linked_connections_connection
    ON linked_connections(connection_name);

-- Queue reclaim and terminal cleanup indexes.
CREATE INDEX IF NOT EXISTS idx_authz_outbox_in_flight_reclaim
    ON authz_outbox(next_attempt_at, updated_at)
    WHERE status = 'in_flight';

CREATE INDEX IF NOT EXISTS idx_authz_outbox_terminal_cleanup
    ON authz_outbox(updated_at)
    WHERE status IN ('succeeded', 'dead_letter');

CREATE INDEX IF NOT EXISTS idx_builtin_approvals_terminal_cleanup
    ON builtin_pending_approvals(decided_at, expires_at)
    WHERE status IN ('approved', 'denied', 'timeout');

CREATE INDEX IF NOT EXISTS idx_builtin_approvals_unresolved_terminal
    ON builtin_pending_approvals(decided_at, expires_at)
    WHERE status IN ('approved', 'denied', 'timeout') AND resolved_at IS NULL;
