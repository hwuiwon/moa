CREATE TABLE IF NOT EXISTS tenant_audit_destinations (
    tenant_id              UUID PRIMARY KEY,
    bucket_name            TEXT NOT NULL,
    region                 TEXT NOT NULL,
    assume_role_arn        TEXT,
    key_prefix             TEXT NOT NULL DEFAULT 'ocsf/',
    object_lock_days       INTEGER NOT NULL DEFAULT 2190,
    encryption_kms_key_arn TEXT
);
