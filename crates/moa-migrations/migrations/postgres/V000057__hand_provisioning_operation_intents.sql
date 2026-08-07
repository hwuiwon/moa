-- Durable provider-visible operation identities close the crash window between
-- provider creation and recording the resulting hand handle.

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.hand_leases
        WHERE status IN ('provisioning', 'failed')
          AND handle IS NULL
    ) THEN
        RAISE EXCEPTION
            'cannot backfill hand provisioning operation identities while unresolved legacy provisioning or failed leases lack handles'
            USING ERRCODE = 'check_violation';
    END IF;
END;
$$;

ALTER TABLE moa.hand_leases
    ADD COLUMN provisioning_operation_id UUID,
    ADD COLUMN provisioning_deadline_at TIMESTAMPTZ;

-- Legacy rows predate provider-visible operation identities. A random UUID is
-- sufficient for their known handles; new runtime operations use typed UUIDv7.
UPDATE moa.hand_leases
SET provisioning_operation_id = gen_random_uuid(),
    provisioning_deadline_at = now();

UPDATE moa.hand_leases
SET handle = jsonb_set(
    handle,
    '{provisioning_operation_id}',
    to_jsonb(provisioning_operation_id::TEXT),
    true
)
WHERE handle IS NOT NULL;

UPDATE moa.hand_leases
SET reap_not_before = GREATEST(
    COALESCE(reap_not_before, provisioning_deadline_at + interval '30 seconds'),
    provisioning_deadline_at + interval '30 seconds'
)
WHERE status IN ('provisioning', 'failed');

ALTER TABLE moa.hand_leases
    ALTER COLUMN provisioning_operation_id SET NOT NULL,
    ALTER COLUMN provisioning_deadline_at SET NOT NULL,
    ADD CONSTRAINT hand_leases_handle_operation_check CHECK (
        CASE
            WHEN handle IS NULL THEN TRUE
            ELSE jsonb_typeof(handle) = 'object'
                AND jsonb_typeof(handle -> 'provisioning_operation_id') = 'string'
                AND (handle ->> 'provisioning_operation_id')::UUID IS NOT NULL
        END
    ),
    ADD CONSTRAINT hand_leases_active_handle_check CHECK (
        status <> 'active'
        OR (
            handle IS NOT NULL
            AND handle ->> 'provisioning_operation_id' = provisioning_operation_id::TEXT
        )
    ),
    ADD CONSTRAINT hand_leases_destroyed_handle_check CHECK (
        status <> 'destroyed' OR handle IS NULL
    ),
    ADD CONSTRAINT hand_leases_cleanup_schedule_check CHECK (
        status NOT IN ('provisioning', 'failed')
        OR reap_not_before > provisioning_deadline_at
    );

CREATE UNIQUE INDEX idx_hand_leases_provisioning_operation
    ON moa.hand_leases (provisioning_operation_id);

CREATE FUNCTION moa.hand_lease_generation_rotation_guard()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.generation IS DISTINCT FROM OLD.generation THEN
        IF NEW.generation <> OLD.generation + 1 THEN
            RAISE EXCEPTION 'hand lease generation must advance exactly once'
                USING ERRCODE = 'check_violation';
        END IF;
        IF NEW.provisioning_operation_id IS NOT DISTINCT FROM OLD.provisioning_operation_id THEN
            RAISE EXCEPTION 'hand lease generation rotation requires a new provisioning operation id'
                USING ERRCODE = 'check_violation';
        END IF;
        IF NEW.provisioning_deadline_at IS NOT DISTINCT FROM OLD.provisioning_deadline_at THEN
            RAISE EXCEPTION 'hand lease generation rotation requires a new provisioning deadline'
                USING ERRCODE = 'check_violation';
        END IF;
    ELSIF NEW.provisioning_operation_id IS DISTINCT FROM OLD.provisioning_operation_id
       OR NEW.provisioning_deadline_at IS DISTINCT FROM OLD.provisioning_deadline_at THEN
        RAISE EXCEPTION 'hand lease provisioning identity and deadline are immutable within a generation'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER hand_lease_generation_rotation_guard
BEFORE UPDATE ON moa.hand_leases
FOR EACH ROW
EXECUTE FUNCTION moa.hand_lease_generation_rotation_guard();
