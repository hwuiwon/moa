-- Canonical effective sandbox profile on every hand lease, plus the tenant
-- policy layer and the durable reaper's claim state.
--
-- Before this migration a lease recorded only a single `expires_at` and no
-- policy identity at all, so a sandbox provisioned under one policy could be
-- resumed under another and nothing owned its hard lifetime. After it, a lease
-- carries the exact six-dimension profile it was provisioned under, the hash
-- covering that profile plus all five contributing revisions, a renewable idle
-- deadline, and an immutable hard deadline the reaper enforces.

-- The tenant policy layer. A tenant with no row contributes the named
-- `tenant-sandbox-unset` identity layer, which restricts nothing; a row here is
-- how a tenant tightens beyond what the deployment already bounds.
CREATE TABLE IF NOT EXISTS moa.tenant_sandbox_policy (
    tenant_id UUID PRIMARY KEY,
    revision TEXT NOT NULL CHECK (length(btrim(revision)) > 0),
    profile JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- `reaping` fences a generation the durable reaper has claimed for destruction.
-- It is deliberately not reachable from provisioning: a claimed generation is
-- never reactivated, only finalized as destroyed or released back to `stale`.
ALTER TABLE moa.hand_leases DROP CONSTRAINT IF EXISTS hand_leases_status_check;
ALTER TABLE moa.hand_leases
    ADD CONSTRAINT hand_leases_status_check
    CHECK (status IN ('provisioning', 'active', 'stale', 'destroyed', 'failed', 'reaping'));

-- The old single deadline was renewed without limit, so it was an idle timeout
-- wearing a lifetime's name. Rename it to what it is and add the hard deadline
-- renewal must never move.
ALTER TABLE moa.hand_leases RENAME COLUMN expires_at TO idle_expires_at;
ALTER TABLE moa.hand_leases ALTER COLUMN idle_expires_at DROP NOT NULL;

ALTER TABLE moa.hand_leases
    ADD COLUMN IF NOT EXISTS hard_expires_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS profile JSONB,
    ADD COLUMN IF NOT EXISTS profile_hash TEXT,
    ADD COLUMN IF NOT EXISTS source_deployment_revision TEXT,
    ADD COLUMN IF NOT EXISTS source_tenant_revision TEXT,
    ADD COLUMN IF NOT EXISTS source_agent_revision TEXT,
    ADD COLUMN IF NOT EXISTS source_route_revision TEXT,
    ADD COLUMN IF NOT EXISTS capability_revision TEXT,
    ADD COLUMN IF NOT EXISTS reap_attempts INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS reap_not_before TIMESTAMPTZ;

-- Every lease written before this migration was provisioned with no stated
-- policy. Inventing one would mean inventing a permissive sandbox, so instead
-- each legacy active/provisioning row becomes stale with an immediately past
-- hard deadline: cleanup work for the reaper, never a reusable sandbox.
UPDATE moa.hand_leases
SET status = 'stale',
    hard_expires_at = now() - INTERVAL '1 second',
    idle_expires_at = now() - INTERVAL '1 second',
    updated_at = now()
WHERE status IN ('active', 'provisioning')
  AND profile_hash IS NULL;

-- An active or provisioning lease must carry its full policy identity. The
-- hard deadline stays nullable because an explicitly `Unbounded` maximum
-- lifetime is a real, stated policy and maps to NULL.
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
            AND capability_revision IS NOT NULL
        )
    );

-- Idle is a deadline inside the hard lifetime, never past it.
ALTER TABLE moa.hand_leases DROP CONSTRAINT IF EXISTS hand_leases_idle_within_hard_check;
ALTER TABLE moa.hand_leases
    ADD CONSTRAINT hand_leases_idle_within_hard_check
    CHECK (
        idle_expires_at IS NULL
        OR hard_expires_at IS NULL
        OR idle_expires_at <= hard_expires_at
    );

-- The old index served lease lookup by the single deadline. The reaper claims
-- on either deadline plus its backoff gate, so replace it.
DROP INDEX IF EXISTS moa.idx_hand_leases_status_expires;
CREATE INDEX IF NOT EXISTS idx_hand_leases_reaper
    ON moa.hand_leases (status, reap_not_before, hard_expires_at, idle_expires_at);
