-- Dual control (four-eyes) with segregation of duties for privileged, irreversible
-- operations.
--
-- A privileged operation that opts into dual control (a per-deployment/per-tenant
-- policy — OFF by default) cannot execute until a SECOND, DISTINCT tenant admin
-- has approved the specific request the first admin raised. This is a SOX/finance
-- control: the operator who requests an irreversible action can never be the same
-- operator who authorizes it.
--
-- Lifecycle of one row:
--   * request(...)  by operator A  -> status = 'pending', requested_by = A.
--   * approve(id)   by operator B  -> status = 'approved', approved_by = B,
--                                     REJECTED when B = requested_by (segregation
--                                     of duties). A is now bound to A, B to B.
--   * consume(...)  at execute time -> status = 'consumed', consumed_ref set to the
--                                     idempotency key of the consuming execution.
--
-- An approval is "valid" for consumption only when status = 'approved',
-- approved_by IS NOT NULL, and approved_by <> requested_by (a distinct approver).
-- The first consumer of privacy ERASURE is the initial guarded operation; legal-
-- hold-release and data-export are the intended next consumers.
--
-- operation_type identifies the guarded operation class (e.g. 'privacy.erase').
-- operation_ref is a versioned BLAKE3 digest of the operation's length-framed
-- parameters (tenant, operation type, and the caller's canonical reference), so
-- an approval binds to one specific request and cannot be redeemed for a different
-- one. Sensitive tenant/subject/reason reference material is never stored raw here.
--
-- This is a control-plane compliance table written by the admin-gated privacy
-- surface and read by the guarded operation's execute path. RLS mirrors
-- moa.legal_hold / token_vault_connections: tenant-scoped transactions are pinned
-- to their own tenant for both read and write, with a control-plane escape hatch.

CREATE TABLE moa.dual_control_request (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id      UUID        NOT NULL,
    operation_type TEXT        NOT NULL,
    operation_ref  TEXT        NOT NULL,
    status         TEXT        NOT NULL DEFAULT 'pending',
    requested_by   TEXT        NOT NULL,
    requested_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    approved_by    TEXT,
    approved_at    TIMESTAMPTZ,
    consumed_at    TIMESTAMPTZ,
    consumed_ref   TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT dual_control_status_check
        CHECK (status IN ('pending', 'approved', 'consumed')),
    CONSTRAINT dual_control_operation_ref_digest_check
        CHECK (operation_ref ~ '^v1:blake3:[0-9a-f]{64}$'),
    -- Segregation of duties enforced in the database as defense-in-depth: an
    -- approver can never be the requester. The service layer rejects a self-
    -- approval first with a precise error; this constraint is the backstop that
    -- makes the invariant impossible to violate even if that check regresses.
    CONSTRAINT dual_control_sod_check
        CHECK (approved_by IS NULL OR approved_by <> requested_by)
);

COMMENT ON TABLE moa.dual_control_request IS
    'Four-eyes dual-control requests for privileged, irreversible operations. A request is approved by a DISTINCT tenant admin (segregation of duties) before the guarded operation may consume the approval and execute.';
COMMENT ON COLUMN moa.dual_control_request.operation_type IS
    'Guarded operation class, e.g. privacy.erase.';
COMMENT ON COLUMN moa.dual_control_request.operation_ref IS
    'Versioned, domain-separated BLAKE3 digest of length-framed operation parameters. Never stores raw sensitive reference material.';
COMMENT ON COLUMN moa.dual_control_request.consumed_ref IS
    'Idempotency key of the execution that consumed this approval, so a durable re-execution of the same operation is not treated as a second consumption.';

-- Consumption lookup keyed by (tenant, operation_type, operation_ref). The
-- partial predicate includes approved rows and consumed rows needed for an
-- idempotent same-consumer replay.
CREATE INDEX idx_dual_control_consumption_lookup
    ON moa.dual_control_request (tenant_id, operation_type, operation_ref)
    WHERE status IN ('approved', 'consumed');

ALTER TABLE moa.dual_control_request ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.dual_control_request FORCE ROW LEVEL SECURITY;

-- Tenant isolation with a control-plane escape hatch, mirroring moa.legal_hold /
-- token_vault_connections: control-plane transactions may read across tenants;
-- tenant-scoped transactions are pinned to their own tenant for read and write.
CREATE POLICY tenant_isolation ON moa.dual_control_request FOR ALL TO moa_app
    USING (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), '')
    )
    WITH CHECK (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), '')
    );

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.dual_control_request TO moa_app;
GRANT USAGE ON SCHEMA moa TO moa_app;
