-- Provider-neutral workspace, active-compute, and checkpoint capacity.
--
-- This is a hard-break migration. Pre-V60 workspace rows were not guaranteed
-- to own a logical-workspace reservation, so carrying them forward would make
-- the new capacity totals incomplete. Operators must drain/reset that preview
-- state before installing this contract.

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.sandbox_workspaces
        WHERE lifecycle_state <> 'deleted'
    ) OR EXISTS (
        SELECT 1
        FROM moa.sandbox_capacity_reservations
    ) THEN
        RAISE EXCEPTION
            'cannot install provider-neutral sandbox capacity while pre-V60 workspace capacity state exists; destructively reset the preview workspace state'
            USING ERRCODE = 'check_violation';
    END IF;
END;
$$;

ALTER TABLE moa.sandbox_capacity_reservations
    DROP CONSTRAINT sandbox_capacity_reservations_operation_fence_fk,
    DROP CONSTRAINT sandbox_capacity_reservations_operation_kind_key,
    DROP CONSTRAINT sandbox_capacity_reservations_lifetime_volume_shape_check,
    DROP CONSTRAINT sandbox_capacity_reservations_resource_dimension_check,
    ALTER COLUMN operation_id DROP NOT NULL,
    ADD COLUMN expected_delete_generation BIGINT NOT NULL DEFAULT 0
        CHECK (expected_delete_generation >= 0),
    ADD COLUMN hand_provisioning_operation_id UUID,
    ADD COLUMN hand_lease_generation BIGINT
        CHECK (hand_lease_generation IS NULL OR hand_lease_generation > 0),
    ADD CONSTRAINT sandbox_capacity_reservations_resource_dimension_check CHECK (
        resource_dimension IN (
            'workspaces', 'active_hands', 'volumes', 'checkpoints', 'logical_bytes'
        )
    ),
    ADD CONSTRAINT sandbox_capacity_reservations_operation_fence_fk
        FOREIGN KEY (
            operation_id, tenant_id, workspace_id,
            provider_account_id, provider_account_generation,
            expected_writer_epoch, expected_instance_generation
        ) REFERENCES moa.sandbox_workspace_operations (
            operation_id, tenant_id, workspace_id,
            provider_account_id, provider_account_generation,
            expected_writer_epoch, expected_instance_generation
        ) ON DELETE RESTRICT,
    ADD CONSTRAINT sandbox_capacity_reservations_dimension_shape_check CHECK (
        CASE resource_dimension
            WHEN 'workspaces' THEN
                operation_id IS NULL
                AND storage_resource_id IS NULL
                AND hand_provisioning_operation_id IS NULL
                AND hand_lease_generation IS NULL
                AND expected_writer_epoch = 0
                AND expected_instance_generation = 0
                AND expected_delete_generation = 0
                AND quantity = 1
            WHEN 'active_hands' THEN
                operation_id IS NULL
                AND storage_resource_id IS NULL
                AND hand_provisioning_operation_id IS NOT NULL
                AND hand_lease_generation IS NOT NULL
                AND expected_delete_generation = 0
                AND quantity = 1
            WHEN 'volumes' THEN
                operation_id IS NOT NULL
                AND hand_provisioning_operation_id IS NULL
                AND hand_lease_generation IS NULL
                AND expected_delete_generation = 0
                AND quantity = 1
            WHEN 'checkpoints' THEN
                operation_id IS NOT NULL
                AND storage_resource_id IS NULL
                AND hand_provisioning_operation_id IS NULL
                AND hand_lease_generation IS NULL
                AND expected_delete_generation = 0
                AND quantity = 1
            WHEN 'logical_bytes' THEN
                operation_id IS NOT NULL
                AND storage_resource_id IS NULL
                AND hand_provisioning_operation_id IS NULL
                AND hand_lease_generation IS NULL
                AND expected_delete_generation = 0
                AND quantity > 0
            ELSE FALSE
        END
    );

CREATE UNIQUE INDEX sandbox_capacity_one_workspace_lifetime_key
    ON moa.sandbox_capacity_reservations (tenant_id, workspace_id, resource_dimension)
    WHERE resource_dimension = 'workspaces';

CREATE UNIQUE INDEX sandbox_capacity_one_hand_operation_key
    ON moa.sandbox_capacity_reservations (
        tenant_id, hand_provisioning_operation_id, resource_dimension
    )
    WHERE resource_dimension = 'active_hands';

CREATE UNIQUE INDEX sandbox_capacity_one_workspace_operation_dimension_key
    ON moa.sandbox_capacity_reservations (tenant_id, operation_id, resource_dimension)
    WHERE operation_id IS NOT NULL;

CREATE INDEX sandbox_capacity_reclaimable_expiry_idx
    ON moa.sandbox_capacity_reservations (expires_at, tenant_id, operation_id)
    WHERE reservation_state IN ('pending', 'reconciling')
      AND expires_at IS NOT NULL;

-- Session-terminal cleanup keyset-pages only compute that can still consume
-- provider resources. Keep destroyed history out of both the page and its index.
CREATE INDEX hand_leases_tenant_live_owner_idx
    ON moa.hand_leases (tenant_id, session_id, worker_id, provider)
    WHERE status <> 'destroyed';

CREATE TABLE moa.sandbox_provider_inventory_claims (
    provider_account_id UUID NOT NULL,
    provider_account_generation BIGINT NOT NULL CHECK (provider_account_generation > 0),
    provider TEXT NOT NULL CHECK (btrim(provider) <> ''),
    claim_generation BIGINT NOT NULL DEFAULT 0 CHECK (claim_generation >= 0),
    claim_owner UUID,
    claim_token UUID,
    claimed_at TIMESTAMPTZ,
    claim_expires_at TIMESTAMPTZ,
    scan_cursor TEXT,
    last_succeeded_at TIMESTAMPTZ,
    last_error TEXT CHECK (last_error IS NULL OR btrim(last_error) <> ''),
    last_error_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider_account_id, provider_account_generation),
    CONSTRAINT sandbox_provider_inventory_claims_account_fk
        FOREIGN KEY (provider_account_id, provider_account_generation)
        REFERENCES moa.sandbox_provider_accounts (provider_account_id, generation)
        ON UPDATE CASCADE ON DELETE CASCADE,
    CONSTRAINT sandbox_provider_inventory_claims_owner_shape_check CHECK (
        (claim_owner IS NULL AND claim_token IS NULL
         AND claimed_at IS NULL AND claim_expires_at IS NULL)
        OR
        (claim_owner IS NOT NULL AND claim_token IS NOT NULL
         AND claimed_at IS NOT NULL AND claim_expires_at > claimed_at)
    ),
    CONSTRAINT sandbox_provider_inventory_claims_error_shape_check CHECK (
        (last_error IS NULL) = (last_error_at IS NULL)
    )
);

CREATE INDEX sandbox_provider_inventory_claims_claimable_idx
    ON moa.sandbox_provider_inventory_claims (
        claim_expires_at, last_succeeded_at,
        provider, provider_account_id, provider_account_generation
    );

REVOKE ALL ON moa.sandbox_provider_inventory_claims FROM moa_app;
GRANT SELECT, INSERT, UPDATE ON moa.sandbox_provider_inventory_claims
    TO moa_workspace_maintenance;

CREATE TABLE moa.sandbox_execution_hand_release_receipts (
    receipt_id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    run_uid UUID NOT NULL,
    owner_kind TEXT NOT NULL CHECK (owner_kind IN ('task', 'compensation')),
    task_id UUID,
    compensation_id UUID,
    logical_generation BIGINT NOT NULL CHECK (logical_generation >= 1),
    attempt_generation BIGINT NOT NULL CHECK (attempt_generation >= 1),
    workspace_id UUID,
    writer_epoch BIGINT CHECK (writer_epoch >= 0),
    instance_generation BIGINT CHECK (instance_generation >= 0),
    hand_provisioning_operation_id UUID,
    hand_lease_generation BIGINT CHECK (hand_lease_generation >= 1),
    checkpoint_id UUID,
    checkpoint_generation BIGINT CHECK (checkpoint_generation >= 1),
    checkpoint_manifest_digest TEXT CHECK (btrim(checkpoint_manifest_digest) <> ''),
    checkpoint_logical_bytes BIGINT CHECK (checkpoint_logical_bytes >= 0),
    receipt_state TEXT NOT NULL CHECK (receipt_state IN ('pending', 'released')),
    destroy_outcome TEXT CHECK (destroy_outcome = 'verified_absent'),
    claim_token UUID,
    claim_expires_at TIMESTAMPTZ,
    requested_at TIMESTAMPTZ NOT NULL,
    deadline_at TIMESTAMPTZ NOT NULL,
    released_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sandbox_execution_hand_release_receipts_owner_shape_check CHECK (
        (owner_kind = 'task' AND task_id IS NOT NULL AND compensation_id IS NULL)
        OR
        (owner_kind = 'compensation' AND task_id IS NULL AND compensation_id IS NOT NULL)
    ),
    CONSTRAINT sandbox_execution_hand_release_receipts_task_fk
        FOREIGN KEY (task_id, run_uid, tenant_id)
        REFERENCES moa.execution_task (task_id, run_uid, tenant_id) ON DELETE RESTRICT,
    CONSTRAINT sandbox_execution_hand_release_receipts_compensation_fk
        FOREIGN KEY (compensation_id, run_uid, tenant_id)
        REFERENCES moa.execution_compensation (
            compensation_id, run_uid, tenant_id
        ) ON DELETE RESTRICT,
    CONSTRAINT sandbox_execution_hand_release_receipts_workspace_fk
        FOREIGN KEY (workspace_id, tenant_id)
        REFERENCES moa.sandbox_workspaces (workspace_id, tenant_id) ON DELETE RESTRICT,
    CONSTRAINT sandbox_execution_hand_release_receipts_checkpoint_fk
        FOREIGN KEY (checkpoint_id, workspace_id, tenant_id)
        REFERENCES moa.sandbox_workspace_checkpoints (
            checkpoint_id, workspace_id, tenant_id
        ) ON DELETE RESTRICT,
    CONSTRAINT sandbox_execution_hand_release_receipts_state_shape_check CHECK (
        deadline_at >= requested_at
        AND ((receipt_state = 'pending'
         AND checkpoint_id IS NULL AND checkpoint_generation IS NULL
         AND checkpoint_manifest_digest IS NULL AND checkpoint_logical_bytes IS NULL
         AND destroy_outcome IS NULL AND released_at IS NULL
         AND claim_token IS NOT NULL AND claim_expires_at IS NOT NULL
        )
        OR
        (receipt_state = 'released'
         AND destroy_outcome = 'verified_absent'
         AND claim_token IS NULL AND claim_expires_at IS NULL
         AND released_at IS NOT NULL AND released_at >= requested_at))
        AND ((owner_kind = 'task'
          AND ((workspace_id IS NOT NULL AND writer_epoch IS NOT NULL
            AND instance_generation IS NOT NULL
            AND hand_provisioning_operation_id IS NOT NULL
            AND hand_lease_generation IS NOT NULL
            AND (receipt_state = 'pending'
              OR (checkpoint_id IS NOT NULL AND checkpoint_generation IS NOT NULL
                AND checkpoint_manifest_digest IS NOT NULL
                AND checkpoint_logical_bytes IS NOT NULL)))
          OR (receipt_state = 'released'
            AND workspace_id IS NULL AND writer_epoch IS NULL
            AND instance_generation IS NULL
            AND hand_provisioning_operation_id IS NULL
            AND hand_lease_generation IS NULL
            AND checkpoint_id IS NULL AND checkpoint_generation IS NULL
            AND checkpoint_manifest_digest IS NULL
            AND checkpoint_logical_bytes IS NULL)))
        OR
        (owner_kind = 'compensation'
          AND workspace_id IS NULL AND writer_epoch IS NULL
          AND instance_generation IS NULL
          AND checkpoint_id IS NULL AND checkpoint_generation IS NULL
          AND checkpoint_manifest_digest IS NULL AND checkpoint_logical_bytes IS NULL
          AND ((hand_provisioning_operation_id IS NULL AND hand_lease_generation IS NULL)
            OR (hand_provisioning_operation_id IS NOT NULL
              AND hand_lease_generation IS NOT NULL))))
    )
);

CREATE UNIQUE INDEX sandbox_execution_hand_release_receipts_task_attempt_key
    ON moa.sandbox_execution_hand_release_receipts (
        tenant_id, run_uid, task_id, logical_generation, attempt_generation
    ) WHERE owner_kind = 'task';

CREATE UNIQUE INDEX sandbox_execution_hand_release_receipts_compensation_attempt_key
    ON moa.sandbox_execution_hand_release_receipts (
        tenant_id, run_uid, compensation_id, logical_generation, attempt_generation
    ) WHERE owner_kind = 'compensation';

CREATE INDEX sandbox_execution_hand_release_receipts_workspace_idx
    ON moa.sandbox_execution_hand_release_receipts (
        tenant_id, workspace_id, instance_generation, hand_lease_generation
    ) WHERE workspace_id IS NOT NULL;

CREATE INDEX sandbox_execution_hand_release_receipts_pending_due_idx
    ON moa.sandbox_execution_hand_release_receipts (
        claim_expires_at, deadline_at, tenant_id, receipt_id
    )
    WHERE receipt_state = 'pending';

SELECT moa.apply_tenant_rls('moa.sandbox_execution_hand_release_receipts');
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.sandbox_execution_hand_release_receipts TO moa_app;
GRANT SELECT, INSERT, UPDATE ON moa.sandbox_execution_hand_release_receipts TO moa_workspace_maintenance;

-- Release receipts are part of the terminal execution detail archive. Once the
-- exact finalized archive is bound, late writes would make the archive incomplete;
-- retention deletes remain legal-hold and destruction-fence guarded by V59.
CREATE TRIGGER sandbox_execution_hand_release_receipt_archived_write_guard
BEFORE INSERT OR UPDATE ON moa.sandbox_execution_hand_release_receipts
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_archived_detail_write();

CREATE TRIGGER sandbox_execution_hand_release_receipt_delete_guard
BEFORE DELETE ON moa.sandbox_execution_hand_release_receipts
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_immutable_payload();

CREATE FUNCTION moa.guard_pending_task_hand_release_attempt()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, moa
SET row_security = off
AS $$
BEGIN
    IF (NEW.generation, NEW.attempt_generation)
           IS DISTINCT FROM (OLD.generation, OLD.attempt_generation)
       AND EXISTS (
           SELECT 1
           FROM moa.sandbox_execution_hand_release_receipts AS receipt
           WHERE receipt.tenant_id = OLD.tenant_id
             AND receipt.run_uid = OLD.run_uid
             AND receipt.task_id = OLD.task_id
             AND receipt.owner_kind = 'task'
             AND receipt.logical_generation = OLD.generation
             AND receipt.attempt_generation = OLD.attempt_generation
             AND receipt.receipt_state = 'pending'
       ) THEN
        RAISE EXCEPTION 'execution task attempt cannot advance during sandbox hand release'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

ALTER FUNCTION moa.guard_pending_task_hand_release_attempt() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.guard_pending_task_hand_release_attempt() FROM PUBLIC;

CREATE TRIGGER execution_task_pending_hand_release_guard
BEFORE UPDATE OF generation, attempt_generation ON moa.execution_task
FOR EACH ROW EXECUTE FUNCTION moa.guard_pending_task_hand_release_attempt();

CREATE FUNCTION moa.guard_pending_compensation_hand_release_attempt()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, moa
SET row_security = off
AS $$
BEGIN
    IF (NEW.generation, NEW.attempt_generation)
           IS DISTINCT FROM (OLD.generation, OLD.attempt_generation)
       AND EXISTS (
           SELECT 1
           FROM moa.sandbox_execution_hand_release_receipts AS receipt
           WHERE receipt.tenant_id = OLD.tenant_id
             AND receipt.run_uid = OLD.run_uid
             AND receipt.compensation_id = OLD.compensation_id
             AND receipt.owner_kind = 'compensation'
             AND receipt.logical_generation = OLD.generation
             AND receipt.attempt_generation = OLD.attempt_generation
             AND receipt.receipt_state = 'pending'
       ) THEN
        RAISE EXCEPTION 'execution compensation cannot advance during sandbox hand release'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

ALTER FUNCTION moa.guard_pending_compensation_hand_release_attempt() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.guard_pending_compensation_hand_release_attempt() FROM PUBLIC;

CREATE TRIGGER execution_compensation_pending_hand_release_guard
BEFORE UPDATE OF generation, attempt_generation ON moa.execution_compensation
FOR EACH ROW EXECUTE FUNCTION moa.guard_pending_compensation_hand_release_attempt();

CREATE FUNCTION moa.guard_pending_hand_release_generation()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, moa
SET row_security = off
AS $$
BEGIN
    IF (NEW.generation, NEW.provisioning_operation_id)
           IS DISTINCT FROM (OLD.generation, OLD.provisioning_operation_id)
       AND EXISTS (
           SELECT 1
           FROM moa.sandbox_execution_hand_release_receipts AS receipt
           WHERE receipt.tenant_id = OLD.tenant_id
             AND receipt.hand_provisioning_operation_id = OLD.provisioning_operation_id
             AND receipt.hand_lease_generation = OLD.generation
             AND receipt.receipt_state = 'pending'
       ) THEN
        RAISE EXCEPTION 'hand lease generation cannot rotate during execution hand release'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

ALTER FUNCTION moa.guard_pending_hand_release_generation() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.guard_pending_hand_release_generation() FROM PUBLIC;

CREATE TRIGGER hand_lease_pending_release_generation_guard
BEFORE UPDATE OF generation, provisioning_operation_id ON moa.hand_leases
FOR EACH ROW EXECUTE FUNCTION moa.guard_pending_hand_release_generation();

-- Workspace creation runs under tenant RLS but provider-account admission must
-- include other tenants. This narrowly-scoped definer function returns no
-- cross-tenant data and validates the inserted workspace before charging it.
CREATE FUNCTION moa.reserve_sandbox_workspace_capacity(
    p_tenant_id UUID,
    p_workspace_id UUID,
    p_provider_account_id UUID,
    p_provider_account_generation BIGINT,
    p_expected_delete_generation BIGINT
) RETURNS UUID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    reservation_id UUID := gen_random_uuid();
    tenant_limit BIGINT;
    provider_limit BIGINT;
    tenant_used BIGINT;
    provider_used BIGINT;
    tenant_label JSONB;
    provider_label JSONB;
BEGIN
    IF p_tenant_id IS NULL
       OR p_workspace_id IS NULL
       OR p_provider_account_id IS NULL
       OR p_provider_account_generation <= 0
       OR p_expected_delete_generation < 0
       OR (
            current_setting('moa.control_plane', true) IS DISTINCT FROM 'true'
            AND current_setting('moa.tenant_id', true) IS DISTINCT FROM p_tenant_id::TEXT
       ) THEN
        RAISE EXCEPTION 'invalid or cross-tenant workspace capacity request'
            USING ERRCODE = 'check_violation';
    END IF;

    PERFORM pg_advisory_xact_lock(
        hashtextextended('sandbox-capacity:tenant:' || p_tenant_id::TEXT, 0)
    );
    PERFORM pg_advisory_xact_lock(
        hashtextextended('sandbox-capacity:provider:' || p_provider_account_id::TEXT, 0)
    );

    IF NOT EXISTS (
        SELECT 1
        FROM moa.sandbox_workspaces AS workspace
        WHERE workspace.tenant_id = p_tenant_id
          AND workspace.workspace_id = p_workspace_id
          AND workspace.provider_account_id = p_provider_account_id
          AND workspace.provider_account_generation = p_provider_account_generation
          AND workspace.lifecycle_state = 'creating'
          AND workspace.writer_epoch = 0
          AND workspace.instance_generation = 0
          AND workspace.delete_generation = p_expected_delete_generation
          AND workspace.access_fenced_at IS NULL
    ) THEN
        RAISE EXCEPTION 'workspace capacity request lost its exact creation fence'
            USING ERRCODE = 'check_violation';
    END IF;

    SELECT limits.configured_limits -> 'workspaces'
    INTO tenant_label
    FROM moa.sandbox_tenant_capacity_limits AS limits
    WHERE limits.tenant_id = p_tenant_id
    FOR UPDATE;

    SELECT account.configured_limits -> 'workspaces'
    INTO provider_label
    FROM moa.sandbox_provider_accounts AS account
    WHERE account.provider_account_id = p_provider_account_id
      AND account.generation = p_provider_account_generation;

    IF NOT FOUND THEN
        RAISE EXCEPTION 'provider-account generation not found'
            USING ERRCODE = 'foreign_key_violation';
    END IF;

    IF tenant_label IS NOT NULL THEN
        IF jsonb_typeof(tenant_label) <> 'number'
           OR tenant_label::TEXT !~ '^[0-9]+$' THEN
            RAISE EXCEPTION 'tenant workspaces capacity limit must be a nonnegative integer'
                USING ERRCODE = 'check_violation';
        END IF;
        tenant_limit := tenant_label::TEXT::BIGINT;
    END IF;

    IF provider_label IS NOT NULL THEN
        IF jsonb_typeof(provider_label) <> 'number'
           OR provider_label::TEXT !~ '^[0-9]+$' THEN
            RAISE EXCEPTION 'provider account workspaces capacity limit must be a nonnegative integer'
                USING ERRCODE = 'check_violation';
        END IF;
        provider_limit := provider_label::TEXT::BIGINT;
    END IF;

    SELECT count(*) INTO tenant_used
    FROM moa.sandbox_capacity_reservations AS reservation
    WHERE reservation.tenant_id = p_tenant_id
      AND reservation.resource_dimension = 'workspaces'
      AND reservation.reservation_state IN ('pending', 'committed', 'reconciling');

    SELECT count(*) INTO provider_used
    FROM moa.sandbox_capacity_reservations AS reservation
    WHERE reservation.provider_account_id = p_provider_account_id
      AND reservation.resource_dimension = 'workspaces'
      AND reservation.reservation_state IN ('pending', 'committed', 'reconciling');

    IF tenant_limit IS NOT NULL AND tenant_used + 1 > tenant_limit THEN
        RAISE EXCEPTION 'tenant workspaces capacity exceeded: % + 1 > %',
            tenant_used, tenant_limit
            USING ERRCODE = 'check_violation';
    END IF;
    IF provider_limit IS NOT NULL AND provider_used + 1 > provider_limit THEN
        RAISE EXCEPTION 'provider account workspaces capacity exceeded: % + 1 > %',
            provider_used, provider_limit
            USING ERRCODE = 'check_violation';
    END IF;

    INSERT INTO moa.sandbox_capacity_reservations (
        reservation_id, tenant_id, provider_account_id,
        provider_account_generation, workspace_id, operation_id,
        expected_writer_epoch, expected_instance_generation,
        expected_delete_generation, resource_dimension, quantity,
        reservation_state
    ) VALUES (
        reservation_id, p_tenant_id, p_provider_account_id,
        p_provider_account_generation, p_workspace_id, NULL,
        0, 0, p_expected_delete_generation, 'workspaces', 1,
        'committed'
    );
    RETURN reservation_id;
END;
$$;

ALTER FUNCTION moa.reserve_sandbox_workspace_capacity(UUID, UUID, UUID, BIGINT, BIGINT)
    OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.reserve_sandbox_workspace_capacity(UUID, UUID, UUID, BIGINT, BIGINT)
    FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.reserve_sandbox_workspace_capacity(UUID, UUID, UUID, BIGINT, BIGINT)
    TO moa_app, moa_workspace_maintenance;
