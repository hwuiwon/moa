-- Persist the origin policy layer in a hand lease's policy identity.
--
-- `SandboxPolicySources` gained an `origin` revision so that eval-owned
-- experiment trials and model-generated code bind `EgressPolicy::DenyAll` as part
-- of the resolved profile rather than as a check some later dispatch path has to
-- remember to run. Every revision in that struct is hash-significant, so adding
-- this one moved every policy identity hash exactly once.
--
-- A lease provisioned before the origin layer existed therefore no longer
-- matches the policy it was provisioned under. That is the intended consequence,
-- not a migration hazard: the same treatment V000359 applied when it introduced
-- the policy identity in the first place.

ALTER TABLE moa.hand_leases
    ADD COLUMN IF NOT EXISTS source_origin_revision TEXT;

-- Every lease written before this migration stated no origin. Inventing one
-- would mean inventing a provenance — and the permissive default is exactly the
-- wrong guess for a trial sandbox. Each legacy active/provisioning row becomes
-- stale with an immediately past hard deadline: cleanup work for the reaper,
-- never a reusable sandbox.
UPDATE moa.hand_leases
SET status = 'stale',
    hard_expires_at = now() - INTERVAL '1 second',
    idle_expires_at = now() - INTERVAL '1 second',
    updated_at = now()
WHERE status IN ('active', 'provisioning')
  AND source_origin_revision IS NULL;

-- An active or provisioning lease must carry its full policy identity, which now
-- includes the origin layer.
ALTER TABLE moa.hand_leases DROP CONSTRAINT IF EXISTS hand_leases_policy_identity_check;
ALTER TABLE moa.hand_leases
    ADD CONSTRAINT hand_leases_policy_identity_check
    CHECK (
        status NOT IN ('active', 'provisioning')
        OR (
            profile IS NOT NULL
            AND profile_hash IS NOT NULL
            AND source_deployment_revision IS NOT NULL
            AND source_tenant_revision IS NOT NULL
            AND source_agent_revision IS NOT NULL
            AND source_route_revision IS NOT NULL
            AND source_origin_revision IS NOT NULL
            AND capability_revision IS NOT NULL
        )
    );
