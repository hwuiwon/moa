-- Tenant-owned durable sandbox workspaces and strict hand-lease tenancy.
--
-- This is intentionally a breaking migration. A live pre-V58 hand has no
-- workspace identity or portable checkpoint authority, so it must be drained
-- before this schema can become the authorization boundary for durable bytes.

DO $$
BEGIN
    CREATE ROLE moa_workspace_maintenance NOLOGIN INHERIT NOBYPASSRLS;
EXCEPTION
    WHEN duplicate_object THEN NULL;
    WHEN unique_violation THEN NULL;
END $$;

ALTER ROLE moa_workspace_maintenance NOLOGIN INHERIT NOBYPASSRLS;
GRANT moa_app, moa_promoter TO moa_workspace_maintenance;
GRANT USAGE ON SCHEMA moa TO moa_workspace_maintenance;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.hand_leases
        WHERE status IN ('provisioning', 'failed')
    ) THEN
        RAISE EXCEPTION
            'cannot install sandbox workspaces while unresolved provisioning or failed hand leases exist; reconcile or reap them first'
            USING ERRCODE = 'check_violation';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM moa.hand_leases
        WHERE status IN ('active', 'stale', 'reaping')
    ) THEN
        RAISE EXCEPTION
            'cannot install sandbox workspaces while legacy hands remain live; drain and reap active, stale, or reaping leases first'
            USING ERRCODE = 'check_violation';
    END IF;
END;
$$;

CREATE TABLE moa.sandbox_provider_accounts (
    provider_account_id UUID PRIMARY KEY,
    generation BIGINT NOT NULL DEFAULT 1 CHECK (generation > 0),
    provider TEXT NOT NULL CHECK (btrim(provider) <> ''),
    isolation_cell TEXT NOT NULL CHECK (btrim(isolation_cell) <> ''),
    organization_fingerprint TEXT NOT NULL CHECK (btrim(organization_fingerprint) <> ''),
    project_fingerprint TEXT,
    configured_limits JSONB NOT NULL DEFAULT '{}'::JSONB
        CHECK (jsonb_typeof(configured_limits) = 'object'),
    observed_inventory JSONB NOT NULL DEFAULT '{}'::JSONB
        CHECK (jsonb_typeof(observed_inventory) = 'object'),
    admission_headroom JSONB NOT NULL DEFAULT '{}'::JSONB
        CHECK (jsonb_typeof(admission_headroom) = 'object'),
    health TEXT NOT NULL DEFAULT 'unknown'
        CHECK (health IN ('unknown', 'healthy', 'degraded', 'unavailable', 'disabled')),
    observed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sandbox_provider_accounts_cell_key
        UNIQUE (provider, isolation_cell),
    CONSTRAINT sandbox_provider_accounts_generation_key
        UNIQUE (provider_account_id, generation)
);

-- Provider accounts describe deployment isolation cells, not caller-owned
-- objects and contain no credentials. The application role receives read-only
-- access for the closed capacity-admission owner; no request DTO or management
-- handler exposes this table.
REVOKE ALL ON moa.sandbox_provider_accounts FROM moa_app;
GRANT SELECT ON moa.sandbox_provider_accounts TO moa_app;

-- Global maintenance evidence is deliberately outside tenant purge. It records
-- only opaque, provider-verified fingerprints and remains available until an
-- operator resolves the control-plane discrepancy that blocked cleanup.
CREATE TABLE moa.sandbox_provider_inventory_findings (
    provider_account_id UUID NOT NULL,
    provider_account_generation BIGINT NOT NULL CHECK (provider_account_generation > 0),
    resource_fingerprint TEXT NOT NULL CHECK (btrim(resource_fingerprint) <> ''),
    finding_kind TEXT NOT NULL CHECK (
        finding_kind IN ('unknown', 'duplicate', 'wrong_account', 'wrong_owner', 'missing')
    ),
    evidence_digest TEXT NOT NULL CHECK (btrim(evidence_digest) <> ''),
    quarantine_state TEXT NOT NULL DEFAULT 'quarantined' CHECK (
        quarantine_state IN ('quarantined', 'acknowledged', 'resolved')
    ),
    first_seen_at TIMESTAMPTZ NOT NULL,
    last_seen_at TIMESTAMPTZ NOT NULL,
    resolved_at TIMESTAMPTZ,
    resolved_by TEXT,
    resolution_evidence_digest TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (
        provider_account_id, provider_account_generation,
        resource_fingerprint, finding_kind
    ),
    CONSTRAINT sandbox_provider_inventory_findings_account_fk FOREIGN KEY (
        provider_account_id, provider_account_generation
    ) REFERENCES moa.sandbox_provider_accounts (provider_account_id, generation),
    CONSTRAINT sandbox_provider_inventory_findings_seen_order_check CHECK (
        last_seen_at >= first_seen_at
    ),
    CONSTRAINT sandbox_provider_inventory_findings_resolution_check CHECK (
        (
            quarantine_state <> 'resolved'
            AND resolved_at IS NULL
            AND resolved_by IS NULL
            AND resolution_evidence_digest IS NULL
        ) OR (
            quarantine_state = 'resolved'
            AND resolved_at IS NOT NULL
            AND resolved_by IS NOT NULL
            AND btrim(resolved_by) <> ''
            AND resolution_evidence_digest IS NOT NULL
            AND btrim(resolution_evidence_digest) <> ''
        )
    )
);

CREATE INDEX sandbox_provider_inventory_findings_unresolved_idx
    ON moa.sandbox_provider_inventory_findings (
        quarantine_state, provider_account_id, provider_account_generation, last_seen_at
    )
    WHERE quarantine_state <> 'resolved';

REVOKE ALL ON moa.sandbox_provider_inventory_findings FROM moa_app;
GRANT SELECT, INSERT, UPDATE ON moa.sandbox_provider_inventory_findings
    TO moa_workspace_maintenance;

CREATE TABLE moa.sandbox_tenant_capacity_limits (
    tenant_id UUID PRIMARY KEY,
    configured_limits JSONB NOT NULL DEFAULT '{}'::JSONB
        CHECK (jsonb_typeof(configured_limits) = 'object'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE moa.sandbox_workspaces (
    workspace_id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('worker', 'execution_task')),
    scope_session_id UUID,
    scope_worker_id TEXT,
    scope_run_id UUID,
    scope_task_id UUID,
    provider TEXT NOT NULL CHECK (btrim(provider) <> ''),
    provider_account_id UUID NOT NULL,
    provider_account_generation BIGINT NOT NULL CHECK (provider_account_generation > 0),
    durability_class TEXT NOT NULL CHECK (durability_class = 'portable_filesystem'),
    lifecycle_state TEXT NOT NULL DEFAULT 'creating' CHECK (
        lifecycle_state IN (
            'creating', 'ready', 'active', 'quiescing', 'committing',
            'reconciling', 'restoring', 'failed', 'deleting', 'deleted'
        )
    ),
    writer_epoch BIGINT NOT NULL DEFAULT 0 CHECK (writer_epoch >= 0),
    instance_generation BIGINT NOT NULL DEFAULT 0 CHECK (instance_generation >= 0),
    current_checkpoint_generation BIGINT NOT NULL DEFAULT 0
        CHECK (current_checkpoint_generation >= 0),
    current_checkpoint_id UUID,
    retention_deadline_at TIMESTAMPTZ,
    delete_generation BIGINT NOT NULL DEFAULT 0 CHECK (delete_generation >= 0),
    access_fenced_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sandbox_workspaces_id_tenant_key UNIQUE (workspace_id, tenant_id),
    CONSTRAINT sandbox_workspaces_provider_account_fk FOREIGN KEY (
        provider_account_id, provider_account_generation
    ) REFERENCES moa.sandbox_provider_accounts (provider_account_id, generation),
    CONSTRAINT sandbox_workspaces_scope_check CHECK (
        (
            scope_kind = 'worker'
            AND scope_session_id IS NOT NULL
            AND scope_worker_id IS NOT NULL
            AND btrim(scope_worker_id) <> ''
            AND scope_run_id IS NULL
            AND scope_task_id IS NULL
        ) OR (
            scope_kind = 'execution_task'
            AND scope_session_id IS NULL
            AND scope_worker_id IS NULL
            AND scope_run_id IS NOT NULL
            AND scope_task_id IS NOT NULL
        )
    ),
    CONSTRAINT sandbox_workspaces_delete_fence_check CHECK (
        lifecycle_state NOT IN ('deleting', 'deleted') OR access_fenced_at IS NOT NULL
    )
);

CREATE UNIQUE INDEX sandbox_workspaces_worker_owner_key
    ON moa.sandbox_workspaces (tenant_id, scope_session_id, scope_worker_id)
    WHERE scope_kind = 'worker' AND lifecycle_state <> 'deleted';

CREATE UNIQUE INDEX sandbox_workspaces_execution_task_owner_key
    ON moa.sandbox_workspaces (tenant_id, scope_run_id, scope_task_id)
    WHERE scope_kind = 'execution_task' AND lifecycle_state <> 'deleted';

CREATE INDEX sandbox_workspaces_retention_idx
    ON moa.sandbox_workspaces (lifecycle_state, retention_deadline_at)
    WHERE lifecycle_state <> 'deleted';

CREATE TABLE moa.sandbox_workspace_operations (
    operation_id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    provider_account_id UUID NOT NULL,
    provider_account_generation BIGINT NOT NULL CHECK (provider_account_generation > 0),
    operation_kind TEXT NOT NULL CHECK (
        operation_kind IN (
            'create', 'attach', 'commit', 'checkpoint', 'restore', 'delete'
        )
    ),
    request_hash TEXT NOT NULL CHECK (btrim(request_hash) <> ''),
    expected_writer_epoch BIGINT NOT NULL CHECK (expected_writer_epoch >= 0),
    expected_instance_generation BIGINT NOT NULL CHECK (expected_instance_generation >= 0),
    expected_checkpoint_generation BIGINT NOT NULL CHECK (expected_checkpoint_generation >= 0),
    deadline_at TIMESTAMPTZ NOT NULL,
    reconcile_not_before TIMESTAMPTZ NOT NULL,
    outcome_class TEXT NOT NULL DEFAULT 'not_sent'
        CHECK (outcome_class IN ('not_sent', 'unknown', 'confirmed')),
    confirmed_disposition TEXT CHECK (
        confirmed_disposition IN ('resource_present', 'resource_absent')
    ),
    absence_observation_count INTEGER NOT NULL DEFAULT 0
        CHECK (absence_observation_count >= 0 AND absence_observation_count <= 2),
    absence_first_observed_at TIMESTAMPTZ,
    absence_last_observed_at TIMESTAMPTZ,
    absence_inventory_digest TEXT,
    claim_token UUID,
    claim_expires_at TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    retry_not_before TIMESTAMPTZ,
    provider_error_code TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sandbox_workspace_operations_id_tenant_workspace_key
        UNIQUE (operation_id, tenant_id, workspace_id),
    CONSTRAINT sandbox_workspace_operations_id_tenant_key
        UNIQUE (operation_id, tenant_id),
    CONSTRAINT sandbox_workspace_operations_fence_key
        UNIQUE (
            operation_id, tenant_id, workspace_id,
            provider_account_id, provider_account_generation,
            expected_writer_epoch, expected_instance_generation
        ),
    CONSTRAINT sandbox_workspace_operations_checkpoint_fence_key
        UNIQUE (
            operation_id, tenant_id, workspace_id,
            expected_writer_epoch, expected_instance_generation,
            expected_checkpoint_generation
        ),
    CONSTRAINT sandbox_workspace_operations_workspace_fk
        FOREIGN KEY (workspace_id, tenant_id)
        REFERENCES moa.sandbox_workspaces (workspace_id, tenant_id) ON DELETE RESTRICT,
    CONSTRAINT sandbox_workspace_operations_provider_account_fk
        FOREIGN KEY (provider_account_id, provider_account_generation)
        REFERENCES moa.sandbox_provider_accounts (provider_account_id, generation),
    CONSTRAINT sandbox_workspace_operations_claim_pair_check CHECK (
        (claim_token IS NULL) = (claim_expires_at IS NULL)
    ),
    CONSTRAINT sandbox_workspace_operations_reconcile_after_deadline_check CHECK (
        reconcile_not_before >= deadline_at
    ),
    CONSTRAINT sandbox_workspace_operations_outcome_disposition_pair_check CHECK (
        (outcome_class = 'confirmed') = (confirmed_disposition IS NOT NULL)
    ),
    CONSTRAINT sandbox_workspace_operations_absence_proof_shape_check CHECK (
        (
            absence_observation_count = 0
            AND absence_first_observed_at IS NULL
            AND absence_last_observed_at IS NULL
            AND absence_inventory_digest IS NULL
        ) OR (
            absence_observation_count = 1
            AND absence_first_observed_at IS NOT NULL
            AND absence_last_observed_at = absence_first_observed_at
            AND absence_inventory_digest IS NOT NULL
            AND btrim(absence_inventory_digest) <> ''
        ) OR (
            absence_observation_count = 2
            AND absence_first_observed_at IS NOT NULL
            AND absence_last_observed_at >= absence_first_observed_at + interval '1 second'
            AND absence_inventory_digest IS NOT NULL
            AND btrim(absence_inventory_digest) <> ''
        )
    ),
    CONSTRAINT sandbox_workspace_operations_confirmed_absence_proof_check CHECK (
        confirmed_disposition <> 'resource_absent'
        OR absence_observation_count = 2
        OR (operation_kind <> 'delete' AND absence_observation_count = 0)
    )
);

CREATE INDEX sandbox_workspace_operations_reconcile_idx
    ON moa.sandbox_workspace_operations (
        outcome_class, reconcile_not_before, retry_not_before, created_at
    )
    WHERE outcome_class <> 'confirmed';

CREATE OR REPLACE FUNCTION moa.enforce_workspace_operation_absence_proof()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    has_two_observation_proof BOOLEAN;
BEGIN
    IF NEW.outcome_class <> 'confirmed'
       OR NEW.confirmed_disposition <> 'resource_absent' THEN
        RETURN NEW;
    END IF;

    -- Updates that leave an already-confirmed absence untouched are not a new
    -- absence decision and therefore do not need to replay its transition.
    IF TG_OP = 'UPDATE'
       AND OLD.outcome_class = 'confirmed'
       AND OLD.confirmed_disposition = 'resource_absent' THEN
        RETURN NEW;
    END IF;

    has_two_observation_proof :=
        NEW.absence_observation_count = 2
        AND NEW.absence_first_observed_at IS NOT NULL
        AND NEW.absence_last_observed_at IS NOT NULL
        AND NEW.absence_last_observed_at
            >= NEW.absence_first_observed_at + interval '1 second';

    IF TG_OP = 'INSERT' THEN
        IF NEW.operation_kind <> 'delete' OR NOT has_two_observation_proof THEN
            RAISE EXCEPTION
                'direct confirmed workspace absence requires a delete operation with two separated observations'
                USING ERRCODE = 'check_violation';
        END IF;
        RETURN NEW;
    END IF;

    IF OLD.outcome_class = 'not_sent' AND NEW.operation_kind <> 'delete' THEN
        -- A synchronous non-delete provider operation may authoritatively
        -- report that it created or retained no external resource.
        RETURN NEW;
    END IF;

    IF (OLD.outcome_class = 'unknown' OR NEW.operation_kind = 'delete')
       AND has_two_observation_proof THEN
        RETURN NEW;
    END IF;

    RAISE EXCEPTION
        'ambiguous or delete workspace absence requires two separated observations'
        USING ERRCODE = 'check_violation';
END;
$$;

CREATE TRIGGER sandbox_workspace_operations_absence_proof
BEFORE INSERT OR UPDATE OF outcome_class, confirmed_disposition,
    absence_observation_count, absence_first_observed_at,
    absence_last_observed_at, operation_kind
ON moa.sandbox_workspace_operations
FOR EACH ROW
EXECUTE FUNCTION moa.enforce_workspace_operation_absence_proof();

CREATE TABLE moa.sandbox_workspace_checkpoints (
    checkpoint_id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    generation BIGINT NOT NULL CHECK (generation > 0),
    parent_checkpoint_id UUID,
    parent_generation BIGINT,
    source_writer_epoch BIGINT NOT NULL CHECK (source_writer_epoch >= 0),
    source_instance_generation BIGINT NOT NULL CHECK (source_instance_generation >= 0),
    source_checkpoint_generation BIGINT NOT NULL CHECK (source_checkpoint_generation >= 0),
    content_kind TEXT NOT NULL DEFAULT 'filesystem_v1'
        CHECK (content_kind = 'filesystem_v1'),
    object_reference TEXT CHECK (btrim(object_reference) <> ''),
    manifest_digest TEXT CHECK (btrim(manifest_digest) <> ''),
    logical_bytes BIGINT CHECK (logical_bytes >= 0),
    operation_id UUID NOT NULL,
    lifecycle_state TEXT NOT NULL CHECK (
        lifecycle_state IN ('creating', 'available', 'deleting', 'deleted', 'failed')
    ),
    retention_state TEXT NOT NULL DEFAULT 'retained'
        CHECK (retention_state IN ('retained', 'expired', 'legal_hold', 'deleting', 'deleted')),
    gc_claim_token UUID,
    gc_claim_expires_at TIMESTAMPTZ,
    gc_attempts INTEGER NOT NULL DEFAULT 0 CHECK (gc_attempts >= 0),
    gc_retry_not_before TIMESTAMPTZ,
    deletion_absence_observation_count INTEGER NOT NULL DEFAULT 0
        CHECK (
            deletion_absence_observation_count >= 0
            AND deletion_absence_observation_count <= 2
        ),
    deletion_absence_first_observed_at TIMESTAMPTZ,
    deletion_absence_last_observed_at TIMESTAMPTZ,
    deletion_inventory_digest TEXT,
    deletion_started_at TIMESTAMPTZ,
    deleted_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    verified_at TIMESTAMPTZ,
    CONSTRAINT sandbox_workspace_checkpoints_id_workspace_tenant_key
        UNIQUE (checkpoint_id, workspace_id, tenant_id),
    CONSTRAINT sandbox_workspace_checkpoints_identity_generation_key
        UNIQUE (checkpoint_id, workspace_id, tenant_id, generation),
    CONSTRAINT sandbox_workspace_checkpoints_generation_key
        UNIQUE (tenant_id, workspace_id, generation),
    CONSTRAINT sandbox_workspace_checkpoints_workspace_fk
        FOREIGN KEY (workspace_id, tenant_id)
        REFERENCES moa.sandbox_workspaces (workspace_id, tenant_id) ON DELETE RESTRICT,
    CONSTRAINT sandbox_workspace_checkpoints_parent_fk
        FOREIGN KEY (parent_checkpoint_id, workspace_id, tenant_id, parent_generation)
        REFERENCES moa.sandbox_workspace_checkpoints (
            checkpoint_id, workspace_id, tenant_id, generation
        )
        ON DELETE RESTRICT,
    CONSTRAINT sandbox_workspace_checkpoints_operation_fk
        FOREIGN KEY (
            operation_id, tenant_id, workspace_id,
            source_writer_epoch, source_instance_generation,
            source_checkpoint_generation
        ) REFERENCES moa.sandbox_workspace_operations (
            operation_id, tenant_id, workspace_id,
            expected_writer_epoch, expected_instance_generation,
            expected_checkpoint_generation
        )
        ON DELETE RESTRICT,
    CONSTRAINT sandbox_workspace_checkpoints_parent_generation_check CHECK (
        generation = source_checkpoint_generation + 1
        AND (
            (generation = 1 AND parent_checkpoint_id IS NULL AND parent_generation IS NULL)
            OR (
                generation > 1
                AND parent_checkpoint_id IS NOT NULL
                AND parent_generation = source_checkpoint_generation
            )
        )
    ),
    CONSTRAINT sandbox_workspace_checkpoints_gc_claim_pair_check CHECK (
        (gc_claim_token IS NULL) = (gc_claim_expires_at IS NULL)
    ),
    CONSTRAINT sandbox_workspace_checkpoints_deletion_proof_shape_check CHECK (
        (
            deletion_absence_observation_count = 0
            AND deletion_absence_first_observed_at IS NULL
            AND deletion_absence_last_observed_at IS NULL
            AND deletion_inventory_digest IS NULL
        ) OR (
            deletion_absence_observation_count = 1
            AND deletion_absence_first_observed_at IS NOT NULL
            AND deletion_absence_last_observed_at = deletion_absence_first_observed_at
            AND deletion_inventory_digest IS NOT NULL
            AND btrim(deletion_inventory_digest) <> ''
        ) OR (
            deletion_absence_observation_count = 2
            AND deletion_absence_first_observed_at IS NOT NULL
            AND deletion_absence_last_observed_at
                >= deletion_absence_first_observed_at + interval '1 second'
            AND deletion_inventory_digest IS NOT NULL
            AND btrim(deletion_inventory_digest) <> ''
        )
    ),
    CONSTRAINT sandbox_workspace_checkpoints_payload_state_check CHECK (
        (
            lifecycle_state IN ('creating', 'failed')
            AND object_reference IS NULL
            AND manifest_digest IS NULL
            AND logical_bytes IS NULL
            AND verified_at IS NULL
        ) OR (
            lifecycle_state IN ('available', 'deleting')
            AND object_reference IS NOT NULL
            AND manifest_digest IS NOT NULL
            AND logical_bytes IS NOT NULL
            AND verified_at IS NOT NULL
        ) OR (
            lifecycle_state = 'deleted'
            AND object_reference IS NULL
            AND manifest_digest IS NOT NULL
            AND logical_bytes IS NOT NULL
            AND verified_at IS NOT NULL
            AND retention_state = 'deleted'
            AND deletion_absence_observation_count = 2
            AND deletion_started_at IS NOT NULL
            AND deleted_at IS NOT NULL
        )
    )
);

CREATE INDEX sandbox_workspace_checkpoints_gc_candidates_idx
    ON moa.sandbox_workspace_checkpoints (
        tenant_id, retention_state, gc_retry_not_before, created_at, generation
    )
    WHERE lifecycle_state = 'available' AND retention_state IN ('retained', 'expired');

CREATE OR REPLACE FUNCTION moa.enforce_sandbox_checkpoint_immutability()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF ROW(
        NEW.checkpoint_id, NEW.tenant_id, NEW.workspace_id, NEW.generation,
        NEW.parent_checkpoint_id, NEW.parent_generation,
        NEW.source_writer_epoch, NEW.source_instance_generation,
        NEW.source_checkpoint_generation, NEW.content_kind, NEW.operation_id,
        NEW.created_at
    ) IS DISTINCT FROM ROW(
        OLD.checkpoint_id, OLD.tenant_id, OLD.workspace_id, OLD.generation,
        OLD.parent_checkpoint_id, OLD.parent_generation,
        OLD.source_writer_epoch, OLD.source_instance_generation,
        OLD.source_checkpoint_generation, OLD.content_kind, OLD.operation_id,
        OLD.created_at
    ) THEN
        RAISE EXCEPTION 'sandbox checkpoint identity and fences are immutable'
            USING ERRCODE = 'check_violation';
    END IF;

    IF OLD.lifecycle_state <> 'creating'
       AND NEW.verified_at IS DISTINCT FROM OLD.verified_at
    THEN
        RAISE EXCEPTION 'sandbox checkpoint verification audit is immutable'
            USING ERRCODE = 'check_violation';
    END IF;

    IF ROW(NEW.manifest_digest, NEW.logical_bytes)
       IS DISTINCT FROM ROW(OLD.manifest_digest, OLD.logical_bytes)
       AND NOT (
            OLD.lifecycle_state = 'creating'
            AND NEW.lifecycle_state = 'available'
            AND OLD.manifest_digest IS NULL
            AND OLD.logical_bytes IS NULL
            AND NEW.manifest_digest IS NOT NULL
            AND NEW.logical_bytes IS NOT NULL
       )
    THEN
        RAISE EXCEPTION 'sandbox checkpoint manifest audit is immutable'
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.object_reference IS DISTINCT FROM OLD.object_reference
       AND NOT (
            OLD.lifecycle_state = 'creating'
            AND NEW.lifecycle_state = 'available'
            AND OLD.object_reference IS NULL
            AND NEW.object_reference IS NOT NULL
            AND NEW.manifest_digest IS NOT NULL
            AND NEW.logical_bytes IS NOT NULL
            AND NEW.verified_at IS NOT NULL
       )
       AND NOT (
            OLD.lifecycle_state = 'deleting'
            AND NEW.lifecycle_state = 'deleted'
            AND NEW.object_reference IS NULL
            AND NEW.deletion_absence_observation_count = 2
            AND NEW.deletion_started_at IS NOT NULL
            AND NEW.deleted_at IS NOT NULL
       )
    THEN
        RAISE EXCEPTION 'sandbox checkpoint storage references are immutable without verified deletion'
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.lifecycle_state = 'deleted' AND OLD.lifecycle_state <> 'deleting' THEN
        RAISE EXCEPTION 'sandbox checkpoint deletion must pass through deleting'
            USING ERRCODE = 'check_violation';
    END IF;

    IF OLD.lifecycle_state = 'deleted' AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION 'sandbox checkpoint tombstone is immutable'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER sandbox_workspace_checkpoints_immutable
BEFORE UPDATE ON moa.sandbox_workspace_checkpoints
FOR EACH ROW EXECUTE FUNCTION moa.enforce_sandbox_checkpoint_immutability();

ALTER TABLE moa.sandbox_workspaces
    ADD CONSTRAINT sandbox_workspaces_current_checkpoint_fk
    FOREIGN KEY (current_checkpoint_id, workspace_id, tenant_id)
    REFERENCES moa.sandbox_workspace_checkpoints (checkpoint_id, workspace_id, tenant_id)
    ON DELETE RESTRICT,
    ADD CONSTRAINT sandbox_workspaces_current_checkpoint_pair_check CHECK (
        (current_checkpoint_generation = 0 AND current_checkpoint_id IS NULL)
        OR (current_checkpoint_generation > 0 AND current_checkpoint_id IS NOT NULL)
    );

CREATE TABLE moa.sandbox_workspace_grants (
    grant_id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    subject_type TEXT NOT NULL CHECK (
        subject_type IN ('tenant', 'session', 'contact', 'operator', 'agent', 'api_key')
    ),
    subject_id UUID NOT NULL,
    subject_relation TEXT,
    object_type TEXT NOT NULL DEFAULT 'sandbox_workspace'
        CHECK (object_type = 'sandbox_workspace'),
    object_id UUID NOT NULL,
    relation TEXT NOT NULL CHECK (relation IN ('tenant', 'session', 'owner', 'manage', 'use')),
    desired_state TEXT NOT NULL DEFAULT 'present'
        CHECK (desired_state IN ('present', 'absent')),
    tuple_generation BIGINT NOT NULL DEFAULT 1 CHECK (tuple_generation > 0),
    outbox_state TEXT NOT NULL DEFAULT 'pending'
        CHECK (outbox_state IN ('pending', 'in_flight', 'succeeded', 'dead_letter')),
    workspace_delete_generation BIGINT NOT NULL CHECK (workspace_delete_generation >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sandbox_workspace_grants_workspace_fk
        FOREIGN KEY (workspace_id, tenant_id)
        REFERENCES moa.sandbox_workspaces (workspace_id, tenant_id) ON DELETE RESTRICT,
    CONSTRAINT sandbox_workspace_grants_object_matches_workspace_check
        CHECK (object_id = workspace_id),
    CONSTRAINT sandbox_workspace_grants_subject_relation_check CHECK (subject_relation IS NULL),
    CONSTRAINT sandbox_workspace_grants_relation_matrix_check CHECK (
        (subject_type = 'tenant' AND relation = 'tenant' AND subject_relation IS NULL)
        OR (subject_type = 'session' AND relation = 'session' AND subject_relation IS NULL)
        OR (subject_type = 'contact' AND relation IN ('owner', 'use') AND subject_relation IS NULL)
        OR (subject_type IN ('operator', 'api_key') AND relation IN ('owner', 'manage', 'use') AND subject_relation IS NULL)
        OR (subject_type = 'agent' AND relation = 'use' AND subject_relation IS NULL)
    ),
    CONSTRAINT sandbox_workspace_grants_tuple_key
        UNIQUE NULLS NOT DISTINCT (
            tenant_id, workspace_id, subject_type, subject_id,
            subject_relation, object_type, object_id, relation
        )
);

CREATE TABLE moa.sandbox_storage_resources (
    storage_resource_id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    provider_account_id UUID NOT NULL,
    provider_account_generation BIGINT NOT NULL CHECK (provider_account_generation > 0),
    resource_kind TEXT NOT NULL CHECK (resource_kind = 'volume'),
    security_class TEXT NOT NULL CHECK (btrim(security_class) <> ''),
    deterministic_name TEXT NOT NULL CHECK (btrim(deterministic_name) <> ''),
    provider_reference TEXT CHECK (provider_reference IS NULL OR btrim(provider_reference) <> ''),
    lifecycle_state TEXT NOT NULL CHECK (
        lifecycle_state IN ('creating', 'ready', 'attached', 'deleting', 'deleted', 'unknown', 'failed')
    ),
    generation BIGINT NOT NULL DEFAULT 1 CHECK (generation > 0),
    create_operation_id UUID NOT NULL,
    deletion_operation_id UUID,
    verified_owner_fingerprint TEXT NOT NULL CHECK (btrim(verified_owner_fingerprint) <> ''),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sandbox_storage_resources_id_tenant_key
        UNIQUE (storage_resource_id, tenant_id),
    CONSTRAINT sandbox_storage_resources_provider_account_fk
        FOREIGN KEY (provider_account_id, provider_account_generation)
        REFERENCES moa.sandbox_provider_accounts (provider_account_id, generation),
    CONSTRAINT sandbox_storage_resources_create_operation_fk
        FOREIGN KEY (create_operation_id, tenant_id)
        REFERENCES moa.sandbox_workspace_operations (operation_id, tenant_id)
);

CREATE UNIQUE INDEX sandbox_storage_resources_provider_ref_key
    ON moa.sandbox_storage_resources (provider_account_id, provider_reference)
    WHERE provider_reference IS NOT NULL;

CREATE UNIQUE INDEX sandbox_storage_resources_one_live_tenant_volume_key
    ON moa.sandbox_storage_resources (
        tenant_id, provider_account_id, provider_account_generation, security_class
    )
    WHERE resource_kind = 'volume' AND lifecycle_state <> 'deleted';

CREATE TABLE moa.sandbox_capacity_reservations (
    reservation_id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    provider_account_id UUID NOT NULL,
    provider_account_generation BIGINT NOT NULL CHECK (provider_account_generation > 0),
    workspace_id UUID NOT NULL,
    operation_id UUID NOT NULL,
    storage_resource_id UUID,
    expected_writer_epoch BIGINT NOT NULL CHECK (expected_writer_epoch >= 0),
    expected_instance_generation BIGINT NOT NULL CHECK (expected_instance_generation >= 0),
    resource_dimension TEXT NOT NULL CHECK (
        resource_dimension IN (
            'workspaces', 'volumes', 'checkpoints', 'logical_bytes'
        )
    ),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    reservation_state TEXT NOT NULL DEFAULT 'pending'
        CHECK (reservation_state IN ('pending', 'committed', 'released', 'reconciling')),
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT sandbox_capacity_reservations_provider_account_fk
        FOREIGN KEY (provider_account_id, provider_account_generation)
        REFERENCES moa.sandbox_provider_accounts (provider_account_id, generation),
    CONSTRAINT sandbox_capacity_reservations_workspace_fk
        FOREIGN KEY (workspace_id, tenant_id)
        REFERENCES moa.sandbox_workspaces (workspace_id, tenant_id) ON DELETE RESTRICT,
    CONSTRAINT sandbox_capacity_reservations_storage_resource_fk
        FOREIGN KEY (storage_resource_id, tenant_id)
        REFERENCES moa.sandbox_storage_resources (storage_resource_id, tenant_id)
        ON DELETE RESTRICT,
    CONSTRAINT sandbox_capacity_reservations_operation_fence_fk
        FOREIGN KEY (
            operation_id, tenant_id, workspace_id,
            provider_account_id, provider_account_generation,
            expected_writer_epoch, expected_instance_generation
        ) REFERENCES moa.sandbox_workspace_operations (
            operation_id, tenant_id, workspace_id,
            provider_account_id, provider_account_generation,
            expected_writer_epoch, expected_instance_generation
        ) ON DELETE RESTRICT,
    CONSTRAINT sandbox_capacity_reservations_operation_kind_key
        UNIQUE (tenant_id, operation_id, resource_dimension),
    CONSTRAINT sandbox_capacity_reservations_lifetime_volume_shape_check CHECK (
        storage_resource_id IS NULL
        OR (resource_dimension = 'volumes' AND quantity = 1)
    )
);

ALTER TABLE moa.hand_leases
    ADD COLUMN workspace_id UUID,
    ADD COLUMN workspace_writer_epoch BIGINT,
    ADD COLUMN workspace_instance_generation BIGINT,
    ADD COLUMN restored_checkpoint_id UUID,
    ADD CONSTRAINT hand_leases_session_tenant_fk
        FOREIGN KEY (session_id, tenant_id)
        REFERENCES public.sessions (id, tenant_id) ON DELETE RESTRICT,
    ADD CONSTRAINT hand_leases_workspace_attachment_check CHECK (
        (
            (workspace_id IS NULL
             AND workspace_writer_epoch IS NULL
             AND workspace_instance_generation IS NULL
             AND restored_checkpoint_id IS NULL)
            OR
            (workspace_id IS NOT NULL
             AND workspace_writer_epoch IS NOT NULL
             AND workspace_writer_epoch >= 0
             AND workspace_instance_generation IS NOT NULL
             AND workspace_instance_generation >= 0)
        )
        AND (status NOT IN ('provisioning', 'active') OR workspace_id IS NOT NULL)
    ),
    ADD CONSTRAINT hand_leases_workspace_fk
        FOREIGN KEY (workspace_id, tenant_id)
        REFERENCES moa.sandbox_workspaces (workspace_id, tenant_id) ON DELETE RESTRICT,
    ADD CONSTRAINT hand_leases_restored_checkpoint_fk
        FOREIGN KEY (restored_checkpoint_id, workspace_id, tenant_id)
        REFERENCES moa.sandbox_workspace_checkpoints (checkpoint_id, workspace_id, tenant_id)
        ON DELETE RESTRICT;

CREATE UNIQUE INDEX hand_leases_one_workspace_writer_key
    ON moa.hand_leases (workspace_id)
    WHERE workspace_id IS NOT NULL AND status IN ('provisioning', 'active');

CREATE OR REPLACE FUNCTION moa.reject_tenant_id_change()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id THEN
        RAISE EXCEPTION 'tenant_id is immutable for %.%', TG_TABLE_SCHEMA, TG_TABLE_NAME
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER hand_leases_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.hand_leases
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();

CREATE TRIGGER sandbox_workspaces_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.sandbox_workspaces
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();

CREATE TRIGGER sandbox_tenant_capacity_limits_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.sandbox_tenant_capacity_limits
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();

CREATE TRIGGER sandbox_workspace_operations_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.sandbox_workspace_operations
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();

CREATE TRIGGER sandbox_workspace_checkpoints_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.sandbox_workspace_checkpoints
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();

CREATE TRIGGER sandbox_workspace_grants_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.sandbox_workspace_grants
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();

CREATE TRIGGER sandbox_storage_resources_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.sandbox_storage_resources
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();

CREATE TRIGGER sandbox_capacity_reservations_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.sandbox_capacity_reservations
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();

SELECT moa.apply_tenant_rls('moa.hand_leases');
SELECT moa.apply_tenant_rls('moa.sandbox_workspaces');
SELECT moa.apply_tenant_rls('moa.sandbox_tenant_capacity_limits');
SELECT moa.apply_tenant_rls('moa.sandbox_workspace_operations');
SELECT moa.apply_tenant_rls('moa.sandbox_workspace_checkpoints');
SELECT moa.apply_tenant_rls('moa.sandbox_workspace_grants');
SELECT moa.apply_tenant_rls('moa.sandbox_storage_resources');
SELECT moa.apply_tenant_rls('moa.sandbox_capacity_reservations');

-- Runtime rollout bootstrap is deliberately narrower than granting the
-- application role writes to the global provider-account table. The function
-- inserts one deployment-authored mapping exactly once and fails closed on any
-- identity/generation/fingerprint drift. Mutable capacity policy may change
-- only through a new validated deployment configuration.
CREATE OR REPLACE FUNCTION moa.bootstrap_sandbox_provider_account(
    p_provider_account_id UUID,
    p_generation BIGINT,
    p_provider TEXT,
    p_isolation_cell TEXT,
    p_organization_fingerprint TEXT,
    p_project_fingerprint TEXT,
    p_configured_limits JSONB,
    p_admission_headroom JSONB
) RETURNS VOID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    existing moa.sandbox_provider_accounts%ROWTYPE;
BEGIN
    IF p_generation <= 0
       OR btrim(p_provider) = ''
       OR btrim(p_isolation_cell) = ''
       OR btrim(p_organization_fingerprint) = ''
       OR jsonb_typeof(p_configured_limits) <> 'object'
       OR jsonb_typeof(p_admission_headroom) <> 'object' THEN
        RAISE EXCEPTION 'invalid sandbox provider-account bootstrap mapping'
            USING ERRCODE = 'check_violation';
    END IF;

    SELECT * INTO existing
    FROM moa.sandbox_provider_accounts
    WHERE provider_account_id = p_provider_account_id
    FOR UPDATE;

    IF NOT FOUND THEN
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, project_fingerprint,
            configured_limits, admission_headroom
        ) VALUES (
            p_provider_account_id, p_generation, p_provider, p_isolation_cell,
            p_organization_fingerprint, p_project_fingerprint,
            p_configured_limits, p_admission_headroom
        );
        RETURN;
    END IF;

    IF existing.generation <> p_generation
       OR existing.provider <> p_provider
       OR existing.isolation_cell <> p_isolation_cell
       OR existing.organization_fingerprint <> p_organization_fingerprint
       OR existing.project_fingerprint IS DISTINCT FROM p_project_fingerprint THEN
        RAISE EXCEPTION 'sandbox provider-account bootstrap mapping drifted for %',
            p_provider_account_id
            USING ERRCODE = 'check_violation';
    END IF;

    UPDATE moa.sandbox_provider_accounts
    SET configured_limits = p_configured_limits,
        admission_headroom = p_admission_headroom,
        updated_at = now()
    WHERE provider_account_id = p_provider_account_id;
END;
$$;
ALTER FUNCTION moa.bootstrap_sandbox_provider_account(
    UUID, BIGINT, TEXT, TEXT, TEXT, TEXT, JSONB, JSONB
) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.bootstrap_sandbox_provider_account(
    UUID, BIGINT, TEXT, TEXT, TEXT, TEXT, JSONB, JSONB
) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.bootstrap_sandbox_provider_account(
    UUID, BIGINT, TEXT, TEXT, TEXT, TEXT, JSONB, JSONB
) TO moa_workspace_maintenance;

CREATE OR REPLACE FUNCTION moa.bootstrap_sandbox_tenant_capacity_limit(
    p_tenant_id UUID,
    p_configured_limits JSONB
) RETURNS TEXT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    existing_limits JSONB;
BEGIN
    IF p_tenant_id IS NULL OR jsonb_typeof(p_configured_limits) <> 'object' THEN
        RAISE EXCEPTION 'invalid sandbox tenant capacity bootstrap mapping'
            USING ERRCODE = 'check_violation';

    END IF;

    -- Serialize bootstrap with purge admission. Purge takes the matching
    -- exclusive lock before installing its durable fence, so bootstrap can
    -- neither race past a new fence nor write after one becomes visible.
    PERFORM pg_advisory_xact_lock_shared(
        hashtextextended('moa:destruction:tenant:' || p_tenant_id::TEXT, 0)
    );

    -- An in-progress purge owns this tenant. Maintenance startup must remain
    -- available to finish it, but must not rewrite any tenant-owned row.
    IF EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence AS fence
        JOIN moa.tenant_purge_operations AS purge
          ON purge.tenant_id = fence.tenant_id
         AND purge.operation_id = fence.operation_id
        WHERE fence.tenant_id = p_tenant_id
          AND fence.subject_id IS NULL
          AND fence.operation_kind = 'tenant.purge'
          AND fence.status = 'in_progress'
          AND purge.status = 'in_progress'
    ) THEN
        RETURN 'skipped_fenced';
    END IF;

    -- Purge control rows are permanent. A stale deployment configuration must
    -- never recreate quota state after relational destruction completed.
    IF EXISTS (
        SELECT 1
        FROM moa.tenant_purge_operations AS purge
        WHERE purge.tenant_id = p_tenant_id
          AND purge.status = 'relationally_committed'
    ) THEN
        RAISE EXCEPTION 'sandbox tenant capacity bootstrap refused for completed tenant purge %',
            p_tenant_id
            USING ERRCODE = '55000';
    END IF;

    SELECT configured_limits INTO existing_limits
    FROM moa.sandbox_tenant_capacity_limits
    WHERE tenant_id = p_tenant_id
    FOR UPDATE;

    IF FOUND AND existing_limits = p_configured_limits THEN
        RETURN 'verified';
    END IF;

    INSERT INTO moa.sandbox_tenant_capacity_limits (tenant_id, configured_limits)
    VALUES (p_tenant_id, p_configured_limits)
    ON CONFLICT (tenant_id) DO UPDATE
    SET configured_limits = EXCLUDED.configured_limits,
        updated_at = now();
    RETURN 'applied';
END;
$$;
ALTER FUNCTION moa.bootstrap_sandbox_tenant_capacity_limit(UUID, JSONB) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.bootstrap_sandbox_tenant_capacity_limit(UUID, JSONB) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.bootstrap_sandbox_tenant_capacity_limit(UUID, JSONB)
    TO moa_workspace_maintenance;

CREATE OR REPLACE FUNCTION moa.has_durable_sandbox_workspace_state()
RETURNS BOOLEAN
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    SELECT EXISTS (
        SELECT 1 FROM moa.sandbox_workspaces WHERE lifecycle_state <> 'deleted'
        UNION ALL
        SELECT 1 FROM moa.sandbox_workspace_operations WHERE outcome_class <> 'confirmed'
        UNION ALL
        SELECT 1 FROM moa.sandbox_workspace_checkpoints WHERE lifecycle_state <> 'deleted'
        UNION ALL
        SELECT 1 FROM moa.sandbox_storage_resources WHERE lifecycle_state <> 'deleted'
        UNION ALL
        SELECT 1 FROM moa.sandbox_capacity_reservations WHERE reservation_state <> 'released'
        UNION ALL
        SELECT 1 FROM moa.hand_leases
        WHERE workspace_id IS NOT NULL
          AND status IN ('provisioning', 'active', 'stale', 'reaping', 'failed')
    );
$$;
ALTER FUNCTION moa.has_durable_sandbox_workspace_state() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.has_durable_sandbox_workspace_state() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.has_durable_sandbox_workspace_state() TO moa_app;

-- Workspace create/delete composes forced-RLS rows with the existing desired
-- OpenFGA outbox on the same `moa_app` transaction. The outbox tenant-purge
-- guard and tuple-attribution checks remain authoritative.
GRANT SELECT, INSERT, UPDATE ON public.authz_outbox TO moa_app;

CREATE INDEX idx_hand_leases_tenant_owner
    ON moa.hand_leases (tenant_id, session_id, worker_id, provider);

-- Insert the seven tenant-owned workspace tables and the transaction-scoped
-- learning-source claim ledger immediately after hand leases.
-- This preserves the only safe relational order while moving every existing
-- later stage as one collision-free block.
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order + 1000
WHERE stage_order > (
    SELECT stage_order FROM moa.tenant_purge_catalog
    WHERE stage_name = 'moa.hand_leases'
);

UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order - 992
WHERE stage_order >= 1000;

INSERT INTO moa.tenant_purge_catalog (
    stage_order, stage_name, table_schema, table_name, scope_mode, action_mode
)
SELECT hand.stage_order + workspace_stage.stage_offset,
       workspace_stage.stage_name,
       'moa',
       workspace_stage.table_name,
       'tenant_id',
       'delete'
FROM moa.tenant_purge_catalog AS hand
CROSS JOIN (VALUES
    (1, 'moa.sandbox_workspace_grants', 'sandbox_workspace_grants'),
    (2, 'moa.sandbox_capacity_reservations', 'sandbox_capacity_reservations'),
    (3, 'moa.sandbox_workspace_checkpoints', 'sandbox_workspace_checkpoints'),
    (4, 'moa.sandbox_storage_resources', 'sandbox_storage_resources'),
    (5, 'moa.sandbox_workspace_operations', 'sandbox_workspace_operations'),
    (6, 'moa.sandbox_workspaces', 'sandbox_workspaces'),
    (7, 'moa.sandbox_tenant_capacity_limits', 'sandbox_tenant_capacity_limits')
) AS workspace_stage(stage_offset, stage_name, table_name)
WHERE hand.stage_name = 'moa.hand_leases';

INSERT INTO moa.tenant_purge_catalog (
    stage_order, stage_name, table_schema, table_name, scope_mode, action_mode
)
SELECT hand.stage_order + 8,
       'moa.tenant_purge_learning_source_delete_claims',
       'moa',
       'tenant_purge_learning_source_delete_claims',
       'control_residue',
       'retain_control'
FROM moa.tenant_purge_catalog AS hand
WHERE hand.stage_name = 'moa.hand_leases';

COMMENT ON TABLE moa.tenant_purge_catalog IS
    'Closed 142-table tenant-offboarding residue surface. Sandbox provider accounts and inventory findings are global maintenance authority; the two nullable-scope simulator certification authority tables are also intentionally global and absent.';

-- V40's deferred learning-source completeness triggers protect the reverse
-- ownership direction: deleting an owner's final source is invalid unless the
-- owner disappears in the same transaction. Tenant purge intentionally drains
-- source rows before several different referenced parents, so it cannot pair
-- every source and parent in one stage (candidate sources can form valid cycles,
-- and log sources can reference candidates deleted before log owners). Admit
-- only source DELETE events authorized at the exact owner-run statement boundary.
-- The owner-only bounded batch function records a same-transaction, same-row
-- capability immediately before its source DELETE. The always-queued deferred
-- DELETE trigger consumes only that exact claim; without one it executes the
-- ordinary completeness assertion. Runtime roles cannot mint or read claims,
-- and a caller-set GUC alone cannot create one. UPDATE remains fully protected.
CREATE TABLE moa.tenant_purge_learning_source_delete_claims (
    transaction_id XID8 NOT NULL,
    source_table TEXT NOT NULL CHECK (
        source_table IN ('learning_candidate_source', 'learning_log_source')
    ),
    source_id UUID NOT NULL,
    owner_id UUID NOT NULL,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    operation_id TEXT NOT NULL CHECK (btrim(operation_id) <> ''),
    PRIMARY KEY (
        transaction_id,
        source_table,
        source_id,
        owner_id,
        tenant_id,
        storage_partition_id,
        operation_id
    )
);
ALTER TABLE moa.tenant_purge_learning_source_delete_claims OWNER TO moa_owner;
REVOKE ALL ON moa.tenant_purge_learning_source_delete_claims FROM PUBLIC;
REVOKE ALL ON moa.tenant_purge_learning_source_delete_claims FROM moa_app, moa_promoter;

CREATE FUNCTION moa.consume_tenant_purge_learning_source_delete_claim(
    p_source_table TEXT,
    p_source_id UUID,
    p_owner_id UUID,
    p_tenant_id TEXT,
    p_storage_partition_id TEXT
) RETURNS BOOLEAN
LANGUAGE plpgsql
VOLATILE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    parsed_tenant UUID;
    affected BIGINT;
BEGIN
    BEGIN
        parsed_tenant := p_tenant_id::UUID;
    EXCEPTION WHEN invalid_text_representation THEN
        RETURN FALSE;
    END;
    DELETE FROM moa.tenant_purge_learning_source_delete_claims AS claim
    WHERE claim.transaction_id = pg_current_xact_id()
      AND claim.source_table = p_source_table
      AND claim.source_id = p_source_id
      AND claim.owner_id = p_owner_id
      AND claim.tenant_id = parsed_tenant
      AND claim.storage_partition_id = p_storage_partition_id
      AND claim.operation_id = NULLIF(
            current_setting('moa.tenant_purge_operation_id', true),
            ''
          );
    GET DIAGNOSTICS affected = ROW_COUNT;
    RETURN affected = 1;
END;
$$;
ALTER FUNCTION moa.consume_tenant_purge_learning_source_delete_claim(TEXT, UUID, UUID, TEXT, TEXT)
    OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.consume_tenant_purge_learning_source_delete_claim(TEXT, UUID, UUID, TEXT, TEXT)
    FROM PUBLIC;

CREATE FUNCTION moa.run_tenant_purge_learning_source_batch(
    p_tenant_id UUID,
    p_operation_id TEXT,
    p_source_table TEXT,
    p_limit INTEGER
) RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    affected BIGINT;
    claim_count BIGINT;
BEGIN
    IF p_source_table NOT IN ('learning_candidate_source', 'learning_log_source')
       OR p_limit < 1
       OR p_operation_id IS DISTINCT FROM NULLIF(
            current_setting('moa.tenant_purge_operation_id', true),
            ''
          )
       OR moa.tenant_purge_bypass_valid(p_tenant_id) IS NOT TRUE
    THEN
        RAISE EXCEPTION 'learning-source purge batch requires the exact active owner purge'
            USING ERRCODE = '55000';
    END IF;

    IF p_source_table = 'learning_candidate_source' THEN
        WITH batch AS MATERIALIZED (
            SELECT target.id, target.candidate_id AS owner_id,
                   target.tenant_id::UUID AS tenant_id,
                   target.storage_partition_id
            FROM public.learning_candidate_source AS target
            WHERE target.storage_partition_id = p_tenant_id::TEXT
              AND target.tenant_id = p_tenant_id::TEXT
            LIMIT p_limit
            FOR UPDATE
        )
        INSERT INTO moa.tenant_purge_learning_source_delete_claims (
            transaction_id, source_table, source_id, owner_id, tenant_id,
            storage_partition_id, operation_id
        )
        SELECT pg_current_xact_id(), p_source_table, batch.id, batch.owner_id,
               batch.tenant_id, batch.storage_partition_id, p_operation_id
        FROM batch;

        DELETE FROM public.learning_candidate_source AS target
        USING moa.tenant_purge_learning_source_delete_claims AS claim
        WHERE claim.transaction_id = pg_current_xact_id()
          AND claim.source_table = p_source_table
          AND claim.tenant_id = p_tenant_id
          AND claim.storage_partition_id = p_tenant_id::TEXT
          AND claim.operation_id = p_operation_id
          AND target.id = claim.source_id
          AND target.candidate_id = claim.owner_id
          AND target.tenant_id = claim.tenant_id::TEXT
          AND target.storage_partition_id = claim.storage_partition_id;
    ELSE
        WITH batch AS MATERIALIZED (
            SELECT target.id, target.learning_id AS owner_id,
                   target.tenant_id::UUID AS tenant_id,
                   target.storage_partition_id
            FROM public.learning_log_source AS target
            WHERE target.storage_partition_id = p_tenant_id::TEXT
              AND target.tenant_id = p_tenant_id::TEXT
            LIMIT p_limit
            FOR UPDATE
        )
        INSERT INTO moa.tenant_purge_learning_source_delete_claims (
            transaction_id, source_table, source_id, owner_id, tenant_id,
            storage_partition_id, operation_id
        )
        SELECT pg_current_xact_id(), p_source_table, batch.id, batch.owner_id,
               batch.tenant_id, batch.storage_partition_id, p_operation_id
        FROM batch;

        DELETE FROM public.learning_log_source AS target
        USING moa.tenant_purge_learning_source_delete_claims AS claim
        WHERE claim.transaction_id = pg_current_xact_id()
          AND claim.source_table = p_source_table
          AND claim.tenant_id = p_tenant_id
          AND claim.storage_partition_id = p_tenant_id::TEXT
          AND claim.operation_id = p_operation_id
          AND target.id = claim.source_id
          AND target.learning_id = claim.owner_id
          AND target.tenant_id = claim.tenant_id::TEXT
          AND target.storage_partition_id = claim.storage_partition_id;
    END IF;
    GET DIAGNOSTICS affected = ROW_COUNT;

    SELECT count(*) INTO claim_count
        FROM moa.tenant_purge_learning_source_delete_claims AS claim
        WHERE claim.transaction_id = pg_current_xact_id()
          AND claim.source_table = p_source_table
          AND claim.tenant_id = p_tenant_id
          AND claim.storage_partition_id = p_tenant_id::TEXT
          AND claim.operation_id = p_operation_id;
    IF claim_count <> affected THEN
        RAISE EXCEPTION 'learning-source purge claim/delete count mismatch (% claims, % deletes)',
            claim_count, affected
            USING ERRCODE = '55000';
    END IF;
    RETURN affected;
END;
$$;
ALTER FUNCTION moa.run_tenant_purge_learning_source_batch(UUID, TEXT, TEXT, INTEGER)
    OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.run_tenant_purge_learning_source_batch(UUID, TEXT, TEXT, INTEGER)
    FROM PUBLIC, moa_app, moa_promoter;

CREATE OR REPLACE FUNCTION moa.assert_learning_candidate_source_owner_complete()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, public, moa
AS $$
DECLARE
    sourceless UUID;
BEGIN
    SELECT candidate.id
    INTO sourceless
    FROM learning_candidates AS candidate
    WHERE candidate.id = OLD.candidate_id
      AND NOT EXISTS (
          SELECT 1 FROM learning_candidate_source AS source
          WHERE source.candidate_id = candidate.id
      );

    IF sourceless IS NOT NULL THEN
        RAISE EXCEPTION
            'learning candidate % committed without any normalized source', sourceless
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
ALTER FUNCTION moa.assert_learning_candidate_source_owner_complete() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.assert_learning_candidate_source_owner_complete() FROM PUBLIC;

CREATE OR REPLACE FUNCTION moa.assert_learning_log_source_owner_complete()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, public, moa
AS $$
DECLARE
    sourceless UUID;
BEGIN
    SELECT entry.id
    INTO sourceless
    FROM learning_log AS entry
    WHERE entry.id = OLD.learning_id
      AND NOT EXISTS (
          SELECT 1 FROM learning_log_source AS source
          WHERE source.learning_id = entry.id
      );

    IF sourceless IS NOT NULL THEN
        RAISE EXCEPTION
            'learning-log entry % committed without any normalized source', sourceless
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
ALTER FUNCTION moa.assert_learning_log_source_owner_complete() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.assert_learning_log_source_owner_complete() FROM PUBLIC;

CREATE FUNCTION moa.assert_learning_candidate_source_delete_complete()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public, moa
AS $$
DECLARE
    sourceless UUID;
BEGIN
    IF moa.consume_tenant_purge_learning_source_delete_claim(
        'learning_candidate_source',
        OLD.id,
        OLD.candidate_id,
        OLD.tenant_id,
        OLD.storage_partition_id
    ) IS TRUE THEN
        RETURN NULL;
    END IF;

    SELECT candidate.id
    INTO sourceless
    FROM learning_candidates AS candidate
    WHERE candidate.id = OLD.candidate_id
      AND NOT EXISTS (
          SELECT 1 FROM learning_candidate_source AS source
          WHERE source.candidate_id = candidate.id
      );
    IF sourceless IS NOT NULL THEN
        RAISE EXCEPTION
            'learning candidate % committed without any normalized source', sourceless
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
ALTER FUNCTION moa.assert_learning_candidate_source_delete_complete() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.assert_learning_candidate_source_delete_complete() FROM PUBLIC;

CREATE FUNCTION moa.assert_learning_log_source_delete_complete()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public, moa
AS $$
DECLARE
    sourceless UUID;
BEGIN
    IF moa.consume_tenant_purge_learning_source_delete_claim(
        'learning_log_source',
        OLD.id,
        OLD.learning_id,
        OLD.tenant_id,
        OLD.storage_partition_id
    ) IS TRUE THEN
        RETURN NULL;
    END IF;

    SELECT entry.id
    INTO sourceless
    FROM learning_log AS entry
    WHERE entry.id = OLD.learning_id
      AND NOT EXISTS (
          SELECT 1 FROM learning_log_source AS source
          WHERE source.learning_id = entry.id
      );
    IF sourceless IS NOT NULL THEN
        RAISE EXCEPTION
            'learning-log entry % committed without any normalized source', sourceless
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;
ALTER FUNCTION moa.assert_learning_log_source_delete_complete() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.assert_learning_log_source_delete_complete() FROM PUBLIC;

DROP TRIGGER learning_candidate_source_owner_complete
    ON public.learning_candidate_source;
CREATE CONSTRAINT TRIGGER learning_candidate_source_owner_complete
AFTER UPDATE ON public.learning_candidate_source
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION moa.assert_learning_candidate_source_owner_complete();
CREATE CONSTRAINT TRIGGER learning_candidate_source_owner_complete_delete
AFTER DELETE ON public.learning_candidate_source
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION moa.assert_learning_candidate_source_delete_complete();

DROP TRIGGER learning_log_source_owner_complete
    ON public.learning_log_source;
CREATE CONSTRAINT TRIGGER learning_log_source_owner_complete
AFTER UPDATE ON public.learning_log_source
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION moa.assert_learning_log_source_owner_complete();
CREATE CONSTRAINT TRIGGER learning_log_source_owner_complete_delete
AFTER DELETE ON public.learning_log_source
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW
EXECUTE FUNCTION moa.assert_learning_log_source_delete_complete();

ALTER TABLE moa.tenant_purge_operations
    ADD COLUMN sandbox_external_absence_confirmed_at TIMESTAMPTZ,
    ADD COLUMN sandbox_external_absence_digest TEXT
        CHECK (
            sandbox_external_absence_digest IS NULL
            OR btrim(sandbox_external_absence_digest) <> ''
        ),
    ADD CONSTRAINT tenant_purge_sandbox_external_proof_pair_check CHECK (
        (sandbox_external_absence_confirmed_at IS NULL)
        = (sandbox_external_absence_digest IS NULL)
    );

-- The application cannot set a generic bypass and mutate fenced rows. These
-- narrowly bounded SECURITY DEFINER functions validate the exact active purge
-- fence before the owner-local operation GUC becomes meaningful to the write
-- guard. PUBLIC never receives execution.
CREATE FUNCTION moa.fence_sandbox_workspaces_for_tenant_purge(
    p_tenant_id UUID,
    p_operation_id TEXT
) RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    affected BIGINT;
BEGIN
    IF p_tenant_id IS NULL OR p_operation_id IS NULL OR btrim(p_operation_id) = ''
       OR NOT EXISTS (
            SELECT 1
            FROM moa.destruction_operation_fence AS fence
            JOIN moa.tenant_purge_operations AS purge
              ON purge.tenant_id = fence.tenant_id
             AND purge.operation_id = fence.operation_id
            WHERE fence.tenant_id = p_tenant_id
              AND fence.subject_id IS NULL
              AND fence.operation_id = p_operation_id
              AND fence.operation_kind = 'tenant.purge'
              AND fence.status = 'in_progress'
              AND purge.status = 'in_progress'
       )
    THEN
        RAISE EXCEPTION 'sandbox workspace purge fence requires the exact active tenant purge'
            USING ERRCODE = '55000';
    END IF;
    PERFORM set_config('moa.tenant_purge_operation_id', p_operation_id, true);
    UPDATE moa.sandbox_workspaces
    SET lifecycle_state = CASE
            WHEN lifecycle_state = 'deleted' THEN 'deleted'
            ELSE 'deleting'
        END,
        access_fenced_at = COALESCE(access_fenced_at, now()),
        delete_generation = CASE
            WHEN access_fenced_at IS NULL THEN delete_generation + 1
            ELSE delete_generation
        END,
        updated_at = now()
    WHERE tenant_id = p_tenant_id
      AND lifecycle_state <> 'deleted';
    GET DIAGNOSTICS affected = ROW_COUNT;
    RETURN affected;
END;
$$;
ALTER FUNCTION moa.fence_sandbox_workspaces_for_tenant_purge(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.fence_sandbox_workspaces_for_tenant_purge(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.fence_sandbox_workspaces_for_tenant_purge(UUID, TEXT)
    TO moa_workspace_maintenance;

CREATE FUNCTION moa.confirm_sandbox_external_absence_for_tenant_purge(
    p_tenant_id UUID,
    p_operation_id TEXT,
    p_evidence_digest TEXT
) RETURNS VOID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF p_tenant_id IS NULL OR p_operation_id IS NULL OR btrim(p_operation_id) = ''
       OR p_evidence_digest IS NULL OR btrim(p_evidence_digest) = ''
       OR NOT EXISTS (
            SELECT 1
            FROM moa.destruction_operation_fence AS fence
            JOIN moa.tenant_purge_operations AS purge
              ON purge.tenant_id = fence.tenant_id
             AND purge.operation_id = fence.operation_id
            WHERE fence.tenant_id = p_tenant_id
              AND fence.subject_id IS NULL
              AND fence.operation_id = p_operation_id
              AND fence.operation_kind = 'tenant.purge'
              AND fence.status = 'in_progress'
              AND purge.status = 'in_progress'
       )
    THEN
        RAISE EXCEPTION 'sandbox absence proof requires the exact active tenant purge'
            USING ERRCODE = '55000';
    END IF;
    UPDATE moa.tenant_purge_operations
    SET sandbox_external_absence_confirmed_at = now(),
        sandbox_external_absence_digest = p_evidence_digest,
        updated_at = now()
    WHERE tenant_id = p_tenant_id
      AND operation_id = p_operation_id
      AND status = 'in_progress';
    IF NOT FOUND THEN
        RAISE EXCEPTION 'sandbox absence proof lost its active purge fence'
            USING ERRCODE = '55000';
    END IF;
END;
$$;
ALTER FUNCTION moa.confirm_sandbox_external_absence_for_tenant_purge(UUID, TEXT, TEXT)
    OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.confirm_sandbox_external_absence_for_tenant_purge(UUID, TEXT, TEXT)
    FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.confirm_sandbox_external_absence_for_tenant_purge(UUID, TEXT, TEXT)
    TO moa_workspace_maintenance;

CREATE FUNCTION moa.require_sandbox_external_absence_for_tenant_purge(
    p_tenant_id UUID,
    p_operation_id TEXT
) RETURNS VOID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM moa.tenant_purge_operations
        WHERE tenant_id = p_tenant_id
          AND operation_id = p_operation_id
          AND status IN ('in_progress', 'relationally_committed')
          AND sandbox_external_absence_confirmed_at IS NOT NULL
          AND sandbox_external_absence_digest IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'tenant relational purge requires durable sandbox external absence proof'
            USING ERRCODE = '55000';
    END IF;
END;
$$;
ALTER FUNCTION moa.require_sandbox_external_absence_for_tenant_purge(UUID, TEXT)
    OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.require_sandbox_external_absence_for_tenant_purge(UUID, TEXT)
    FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.require_sandbox_external_absence_for_tenant_purge(UUID, TEXT)
    TO moa_app, moa_promoter, moa_workspace_maintenance;

DO $sandbox_workspace_purge_fences$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'sandbox_tenant_capacity_limits',
        'sandbox_workspaces',
        'sandbox_workspace_operations',
        'sandbox_workspace_checkpoints',
        'sandbox_workspace_grants',
        'sandbox_storage_resources',
        'sandbox_capacity_reservations'
    ]
    LOOP
        EXECUTE format(
            'CREATE TRIGGER moa_tenant_purge_fence_insert '
            'AFTER INSERT ON moa.%I '
            'REFERENCING NEW TABLE AS tenant_purge_new_rows '
            'FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement(''tenant_id'')',
            table_name
        );
        EXECUTE format(
            'CREATE TRIGGER moa_tenant_purge_fence_update '
            'AFTER UPDATE ON moa.%I '
            'REFERENCING OLD TABLE AS tenant_purge_old_rows '
            'NEW TABLE AS tenant_purge_new_rows '
            'FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement(''tenant_id'')',
            table_name
        );
    END LOOP;
END
$sandbox_workspace_purge_fences$;

-- Teach the bounded owner-only purge about the workspace head/checkpoint cycle.
-- The checkpoint stage first clears the fenced current-head pointer, then drains
-- descendants before parents. External absence is proved by the maintenance
-- coordinator before relational stages are allowed to begin.
DO $sandbox_workspace_purge_function$
DECLARE
    predecessor TEXT;
    replacement TEXT;
    old_branch CONSTANT TEXT :=
        '        IF catalog_row.action_mode = ''clear_then_delete'' THEN';
    new_branch CONSTANT TEXT :=
        '        IF catalog_row.table_schema = ''public'' '
        'AND catalog_row.table_name IN (''learning_candidate_source'', ''learning_log_source'') THEN
'
        '            affected := moa.run_tenant_purge_learning_source_batch(
'
        '                p_tenant_id,
'
        '                p_operation_id,
'
        '                catalog_row.table_name,
'
        '                p_limit
'
        '            );
'
        '        ELSIF catalog_row.table_schema = ''moa'' '
        'AND catalog_row.table_name = ''sandbox_workspace_checkpoints'' THEN
'
        '            WITH batch AS (
'
        '                SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid
'
        '                FROM moa.sandbox_workspaces AS target
'
        '                WHERE target.tenant_id = p_tenant_id
'
        '                  AND target.current_checkpoint_id IS NOT NULL
'
        '                LIMIT p_limit
'
        '                FOR UPDATE
'
        '            )
'
        '            UPDATE moa.sandbox_workspaces AS target
'
        '            SET current_checkpoint_id = NULL,
'
        '                current_checkpoint_generation = 0,
'
        '                updated_at = now()
'
        '            FROM batch
'
        '            WHERE target.tableoid = batch.row_tableoid
'
        '              AND target.ctid = batch.row_ctid;
'
        '            GET DIAGNOSTICS affected = ROW_COUNT;
'
        '
'
        '            IF affected = 0 THEN
'
        '                WITH batch AS (
'
        '                    SELECT target.tableoid AS row_tableoid, target.ctid AS row_ctid
'
        '                    FROM moa.sandbox_workspace_checkpoints AS target
'
        '                    WHERE target.tenant_id = p_tenant_id
'
        '                    ORDER BY target.generation DESC
'
        '                    LIMIT p_limit
'
        '                    FOR UPDATE
'
        '                )
'
        '                DELETE FROM moa.sandbox_workspace_checkpoints AS target
'
        '                USING batch
'
        '                WHERE target.tableoid = batch.row_tableoid
'
        '                  AND target.ctid = batch.row_ctid;
'
        '                GET DIAGNOSTICS affected = ROW_COUNT;
'
        '            END IF;
'
        '        ELSIF catalog_row.action_mode = ''clear_then_delete'' THEN';
BEGIN
    SELECT pg_get_functiondef('moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE)
    INTO predecessor;
    IF predecessor NOT LIKE '%catalog_count <> 134%'
       OR predecessor NOT LIKE '%exactly 134 tables%'
       OR position(old_branch IN predecessor) = 0
    THEN
        RAISE EXCEPTION 'unexpected V55 tenant purge function definition'
            USING ERRCODE = '55000';
    END IF;
    replacement := replace(predecessor, old_branch, new_branch);
    replacement := replace(replacement, 'catalog_count <> 134', 'catalog_count <> 142');
    replacement := replace(replacement, 'exactly 134 tables', 'exactly 142 tables');
    EXECUTE replacement;
END
$sandbox_workspace_purge_function$;

ALTER FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT)
    TO moa_app, moa_promoter;
