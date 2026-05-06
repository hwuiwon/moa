CREATE TABLE IF NOT EXISTS analytics.compliance_workspaces (
    workspace_id       TEXT PRIMARY KEY,
    enabled            BOOLEAN     NOT NULL DEFAULT TRUE,
    enabled_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    retention_years    INT         NOT NULL DEFAULT 10,
    s3_bucket          TEXT        NOT NULL,
    kms_key_id         TEXT,
    signing_key_label  TEXT        NOT NULL,
    notes              TEXT
);

CREATE TABLE IF NOT EXISTS analytics.compliance_workspace_state (
    workspace_id          TEXT PRIMARY KEY,
    last_integrity_hash   BYTEA,
    last_ts               TIMESTAMPTZ,
    record_count          BIGINT NOT NULL DEFAULT 0,
    last_root_id          UUID
);

CREATE TABLE IF NOT EXISTS analytics.audit_roots (
    root_id            UUID PRIMARY KEY,
    workspace_id       TEXT        NOT NULL,
    window_start       TIMESTAMPTZ NOT NULL,
    window_end         TIMESTAMPTZ NOT NULL,
    record_count       BIGINT      NOT NULL,
    merkle_root        BYTEA       NOT NULL,
    signature          BYTEA       NOT NULL,
    signing_key_label  TEXT        NOT NULL,
    s3_object_uri      TEXT        NOT NULL,
    s3_object_etag     TEXT        NOT NULL,
    object_lock_mode   TEXT        NOT NULL,
    retain_until       TIMESTAMPTZ NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS ix_audit_roots_workspace_window
    ON analytics.audit_roots (workspace_id, window_end DESC);

CREATE SCHEMA IF NOT EXISTS pii_vault;

CREATE TABLE IF NOT EXISTS pii_vault.subject_keys (
    subject_pseudonym BYTEA PRIMARY KEY,
    workspace_id      TEXT        NOT NULL,
    hmac_key_handle   TEXT        NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    erased_at         TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS pii_vault.plaintext_side (
    record_id          UUID PRIMARY KEY,
    subject_pseudonym  BYTEA       NOT NULL,
    workspace_id       TEXT        NOT NULL,
    field_name         TEXT        NOT NULL,
    ciphertext         BYTEA       NOT NULL,
    encryption_context JSONB       NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (subject_pseudonym) REFERENCES pii_vault.subject_keys(subject_pseudonym)
);

CREATE INDEX IF NOT EXISTS ix_plaintext_subject
    ON pii_vault.plaintext_side (subject_pseudonym);

CREATE INDEX IF NOT EXISTS ix_plaintext_workspace
    ON pii_vault.plaintext_side (workspace_id, created_at);
