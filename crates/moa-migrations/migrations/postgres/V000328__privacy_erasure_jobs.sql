-- Durable, resumable privacy-erasure jobs.
--
-- Privacy erasure runs inside one Restate `ctx.run`, but its side effects
-- (approval-token consumption, PII-vault erasure, per-node graph purges, digest
-- and retrieval-lineage deletion) each commit independently. A crash or Restate
-- re-execution after some side effects committed previously stranded the
-- erasure: the approval token was spent, part of the data was erased, and the
-- terminal "approval token replayed" conflict blocked any resume.
--
-- `moa.erasure_jobs` binds each approval-token JTI to exactly one idempotent job
-- keyed by that JTI. A replay of the same request recognizes that it owns the
-- JTI and resumes from the persisted stage; a reuse of the same token for a
-- materially different request (different request fingerprint) is rejected so
-- approval tokens never become generally reusable.
--
-- This is a control-plane bookkeeping table written only by the admin-gated
-- privacy service, so it follows the `moa.audit_jti_used` precedent: explicit
-- role grants, no row-level security.

CREATE TABLE IF NOT EXISTS moa.erasure_jobs (
    jti TEXT PRIMARY KEY,
    tenant_id UUID NOT NULL,
    subject_user_id TEXT NOT NULL,
    -- Deterministic fingerprint of the request parameters bound to this token.
    -- Distinguishes a resume of the same request from a reuse of the token for a
    -- different request.
    request_fingerprint TEXT NOT NULL,
    approver_id TEXT NOT NULL,
    approval_claims JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'running'
        CHECK (status IN ('running', 'completed', 'failed')),
    -- Highest stage the job still needs to run. Stages execute in this order and
    -- a resumed job jumps straight to the persisted stage.
    stage TEXT NOT NULL DEFAULT 'vault'
        CHECK (stage IN ('vault', 'graph', 'digest', 'lineage', 'done')),
    candidate_count BIGINT NOT NULL DEFAULT 0,
    pii_vault_erased BIGINT NOT NULL DEFAULT 0,
    graph_erased BIGINT NOT NULL DEFAULT 0,
    digest_deleted BIGINT NOT NULL DEFAULT 0,
    lineage_deleted BIGINT NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS erasure_jobs_subject_idx
    ON moa.erasure_jobs (tenant_id, subject_user_id, created_at DESC);

GRANT SELECT, INSERT, UPDATE ON moa.erasure_jobs TO moa_app, moa_promoter;
GRANT SELECT ON moa.erasure_jobs TO moa_auditor;
