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
