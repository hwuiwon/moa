-- Operation-fenced knowledge link claims and the durable provider-trigger boundary.
--
-- Linking a tenant knowledge connection writes a credential version, persists a
-- connection row, and starts an initial provider sync. Those are three durable
-- effects that a crash or a concurrent link can interleave, which previously
-- allowed a vault version to be orphaned, attached to a connection the upsert
-- did not actually create, or overwritten by a newer link.
--
-- `moa.knowledge_link_claims` makes the whole link one compare-and-swap state
-- machine keyed by `(tenant, operation_id)`:
--
--   reserved -> credential_written -> finalized
--                     |
--                     +-> compensating -> compensated   (terminal)
--
-- The claim records the canonical request hash (so replaying an operation id
-- with different inputs is a typed conflict rather than a silent overwrite), the
-- owning principal, the connection the link expects to own, and the exact
-- previous-active and candidate credential references. Compensation therefore
-- revokes exactly the candidate it wrote and restores exactly the version it
-- superseded, instead of guessing from whatever is active at failure time.
--
-- `moa.knowledge_sync_runs.provider_trigger_completed_at` is the second half:
-- a persisted queued sync run is not evidence that provider dispatch happened. A
-- crash between claiming the run and calling the provider must replay that exact
-- idempotent trigger, and a link cannot finalize until the boundary is durable.

CREATE TABLE IF NOT EXISTS moa.knowledge_link_claims (
    tenant_id                UUID        NOT NULL,
    operation_id             TEXT        NOT NULL,
    request_hash             TEXT        NOT NULL,
    owner_identity_id        UUID,
    connection_uid           UUID        NOT NULL,
    previous_credential_ref  TEXT,
    candidate_credential_ref TEXT,
    state                    TEXT        NOT NULL,
    sync_run_uid             UUID,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, operation_id),
    CONSTRAINT knowledge_link_claims_state_valid
        CHECK (state IN (
            'reserved',
            'credential_written',
            'compensating',
            'compensated',
            'finalized'
        )),
    CONSTRAINT knowledge_link_claims_operation_id_present
        CHECK (operation_id <> ''),
    CONSTRAINT knowledge_link_claims_request_hash_present
        CHECK (request_hash <> ''),
    -- A claim that says a credential exists must name the exact reference, so
    -- finalization can never persist an inferred one. Compensation states may
    -- carry a NULL candidate: a link can fail before it writes anything, and
    -- that failure still passes through the same terminal path.
    CONSTRAINT knowledge_link_claims_candidate_recorded
        CHECK (
            state NOT IN ('credential_written', 'finalized')
            OR candidate_credential_ref IS NOT NULL
        ),
    -- Finalization requires durable evidence that the candidate's initial
    -- provider trigger ran. `AlreadyRunning` alone can never satisfy this.
    CONSTRAINT knowledge_link_claims_finalized_has_sync_run
        CHECK (state <> 'finalized' OR sync_run_uid IS NOT NULL)
);

-- Supports resuming a connection's incomplete link without scanning the tenant.
CREATE INDEX IF NOT EXISTS knowledge_link_claims_connection_idx
    ON moa.knowledge_link_claims (tenant_id, connection_uid);

-- Durable trigger boundary. Set once, by the step that observed a successful
-- provider dispatch; ordinary sync-run status updates never write it, so a
-- later status change cannot erase the evidence.
ALTER TABLE moa.knowledge_sync_runs
    ADD COLUMN IF NOT EXISTS provider_trigger_completed_at TIMESTAMPTZ;

ALTER TABLE moa.knowledge_link_claims ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_link_claims FORCE ROW LEVEL SECURITY;

-- Strict tenant isolation with no control-plane branch, matching the credential
-- tables the claim references: a link claim is always tenant-bound, so a missing
-- or wrong `moa.tenant_id` denies rather than widening to every tenant.
CREATE POLICY tenant_isolation ON moa.knowledge_link_claims FOR ALL TO moa_app
    USING (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''))
    WITH CHECK (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''));

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.knowledge_link_claims TO moa_app;
GRANT USAGE ON SCHEMA moa TO moa_app;
