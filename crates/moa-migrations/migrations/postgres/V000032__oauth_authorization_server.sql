-- First-party OAuth 2.1 Authorization Server storage.
--
-- Postgres is authoritative for every value that must survive a non-sticky
-- Kubernetes hop: bootstrapped clients, consent transactions, authorization
-- codes, grants, CSRF decisions, and token revocation. Only SHA-256 digests of
-- client secrets, CSRF values, codes, and bearer tokens are persisted.

CREATE TABLE oauth_clients (
    client_id             TEXT        PRIMARY KEY,
    client_type           TEXT        NOT NULL CHECK (client_type IN ('public', 'confidential')),
    redirect_uris         TEXT[]      NOT NULL CHECK (cardinality(redirect_uris) > 0),
    scopes                TEXT[]      NOT NULL CHECK (cardinality(scopes) > 0),
    client_secret_hash    TEXT,
    config_hash           TEXT        NOT NULL CHECK (config_hash ~ '^[0-9a-f]{64}$'),
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (
        (client_type = 'public' AND client_secret_hash IS NULL)
        OR (client_type = 'confidential' AND client_secret_hash ~ '^[0-9a-f]{64}$')
    )
);

CREATE TABLE oauth_authorization_transactions (
    id                     UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id              UUID        NOT NULL,
    client_id              TEXT        NOT NULL REFERENCES oauth_clients(client_id),
    subject_id             UUID        NOT NULL,
    subject_type           TEXT        NOT NULL,
    redirect_uri           TEXT        NOT NULL,
    scopes                 TEXT[]      NOT NULL CHECK (cardinality(scopes) > 0),
    resource               TEXT        NOT NULL CHECK (resource <> ''),
    state                  TEXT,
    code_challenge         TEXT        NOT NULL,
    code_challenge_method  TEXT        NOT NULL CHECK (code_challenge_method = 'S256'),
    csrf_hash              TEXT        NOT NULL CHECK (csrf_hash ~ '^[0-9a-f]{64}$'),
    expires_at             TIMESTAMPTZ NOT NULL,
    decision               TEXT        CHECK (decision IN ('approved', 'denied')),
    decided_at             TIMESTAMPTZ,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK ((decision IS NULL) = (decided_at IS NULL))
);

CREATE INDEX idx_oauth_authorization_transactions_tenant_subject
    ON oauth_authorization_transactions (tenant_id, subject_id);

CREATE TABLE oauth_authorization_codes (
    code_hash                TEXT        PRIMARY KEY CHECK (code_hash ~ '^[0-9a-f]{64}$'),
    authorization_request_id UUID        NOT NULL UNIQUE
        REFERENCES oauth_authorization_transactions(id),
    tenant_id                UUID        NOT NULL,
    client_id                TEXT        NOT NULL REFERENCES oauth_clients(client_id),
    subject_id               UUID        NOT NULL,
    subject_type             TEXT        NOT NULL,
    redirect_uri             TEXT        NOT NULL,
    scopes                   TEXT[]      NOT NULL CHECK (cardinality(scopes) > 0),
    resource                 TEXT        NOT NULL CHECK (resource <> ''),
    code_challenge           TEXT        NOT NULL,
    code_challenge_method    TEXT        NOT NULL CHECK (code_challenge_method = 'S256'),
    expires_at               TIMESTAMPTZ NOT NULL,
    consumed_at              TIMESTAMPTZ,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_oauth_authorization_codes_tenant
    ON oauth_authorization_codes (tenant_id);

CREATE TABLE oauth_tokens (
    id                        UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id                 UUID        NOT NULL,
    client_id                 TEXT        NOT NULL REFERENCES oauth_clients(client_id),
    subject_id                UUID        NOT NULL,
    subject_type              TEXT        NOT NULL,
    scopes                    TEXT[]      NOT NULL CHECK (cardinality(scopes) > 0),
    resource                  TEXT        NOT NULL CHECK (resource <> ''),
    access_token_hash         TEXT        NOT NULL UNIQUE CHECK (access_token_hash ~ '^[0-9a-f]{64}$'),
    access_token_expires_at   TIMESTAMPTZ NOT NULL,
    refresh_token_hash        TEXT        NOT NULL UNIQUE CHECK (refresh_token_hash ~ '^[0-9a-f]{64}$'),
    refresh_token_expires_at  TIMESTAMPTZ NOT NULL,
    revoked_at                TIMESTAMPTZ,
    created_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_oauth_tokens_tenant ON oauth_tokens (tenant_id);

ALTER TABLE oauth_clients ENABLE ROW LEVEL SECURITY;
ALTER TABLE oauth_clients FORCE ROW LEVEL SECURITY;
ALTER TABLE oauth_authorization_transactions ENABLE ROW LEVEL SECURITY;
ALTER TABLE oauth_authorization_transactions FORCE ROW LEVEL SECURITY;
ALTER TABLE oauth_authorization_codes ENABLE ROW LEVEL SECURITY;
ALTER TABLE oauth_authorization_codes FORCE ROW LEVEL SECURITY;
ALTER TABLE oauth_tokens ENABLE ROW LEVEL SECURITY;
ALTER TABLE oauth_tokens FORCE ROW LEVEL SECURITY;

CREATE POLICY control_plane_only ON oauth_clients FOR ALL TO moa_app
    USING (lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
        IN ('1', 'true', 't', 'yes', 'on'))
    WITH CHECK (lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
        IN ('1', 'true', 't', 'yes', 'on'));

CREATE POLICY tenant_isolation ON oauth_authorization_transactions FOR ALL TO moa_app
    USING (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
    )
    WITH CHECK (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
    );

CREATE POLICY tenant_isolation ON oauth_authorization_codes FOR ALL TO moa_app
    USING (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
    )
    WITH CHECK (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
    );

CREATE POLICY tenant_isolation ON oauth_tokens FOR ALL TO moa_app
    USING (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
    )
    WITH CHECK (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
    );

GRANT SELECT, INSERT, UPDATE, DELETE ON oauth_clients TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON oauth_authorization_transactions TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON oauth_authorization_codes TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON oauth_tokens TO moa_app;

DO $$
BEGIN
    EXECUTE format('GRANT USAGE ON SCHEMA %I TO moa_app', current_schema());
END $$;
