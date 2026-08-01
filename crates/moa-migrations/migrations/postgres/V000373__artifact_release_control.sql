-- Type-owned serving pointers and the artifact release-candidate control plane.
--
-- Before this migration, "published" meant three different things at once: the
-- revision passed reference validation, the revision was the newest one, and the
-- revision was what normal sessions resolved. Any writer that could set
-- `status = 'published'` therefore changed what production served, and there was
-- no behavioral gate anywhere on that path. Skill import wrote published
-- revisions directly, the repository publish helper accepted any revision in any
-- state, and agent rollout moved `agent_installation.current_revision_uid`
-- without consulting artifact publication at all.
--
-- This migration separates the three meanings:
--
--   * Serving is a pointer. `moa.artifact_serving_pointer` names the exact
--     revision a tenant serves for one release-gated artifact (skill or action),
--     carries the revision hash it was activated with, and carries a monotonic
--     `pointer_version` used as the compare-and-set token for every subsequent
--     move. Agents keep their own type-owned pointer
--     (`moa.agent_installation.current_revision_uid`), which gains the same CAS
--     token here.
--
--   * Candidate lifecycle is a state. `moa.artifact_revision.status` becomes the
--     seven explicit non-serving candidate states for release-gated kinds
--     (`draft`, `evaluating`, `ready`, `rejected`, `inconclusive`, `superseded`,
--     `archived`). `published` survives only for kinds whose activation seam is
--     owned elsewhere (connector catalogs and experiment plans); a trigger makes
--     `published` unrepresentable for skills, actions, and agents, and makes the
--     candidate states unrepresentable for the other kinds. `ready` means
--     "evaluated and activatable", never "serving": only a pointer serves.
--
--   * Permission to move a pointer is an attestation.
--     `moa.artifact_activation_attestation` is immutable, expiring, and
--     single-use; `moa.artifact_activation_audit` records the decision that
--     consumed it. The activation repository transaction is the only writer of
--     both, and it moves the pointer in the same transaction as the audit row.
--
-- Deletion: pointer rows cascade from both the artifact and the revision, so the
-- tenant purge sequence needs no new step and a deleted revision can never keep
-- serving. Attestations and audit rows cascade from the artifact for the same
-- reason.

-- ---------------------------------------------------------------------------
-- 1. Candidate states on artifact_revision
-- ---------------------------------------------------------------------------

ALTER TABLE moa.artifact_revision
    DROP CONSTRAINT IF EXISTS artifact_revision_status_check;

-- The baseline created this as an inline unnamed CHECK; find and drop it by
-- definition so the widened constraint is the only status check left.
DO $$
DECLARE
    constraint_name TEXT;
BEGIN
    FOR constraint_name IN
        SELECT conname
        FROM pg_constraint
        WHERE conrelid = 'moa.artifact_revision'::REGCLASS
          AND contype = 'c'
          AND pg_get_constraintdef(oid) LIKE '%status%'
    LOOP
        EXECUTE format(
            'ALTER TABLE moa.artifact_revision DROP CONSTRAINT %I',
            constraint_name
        );
    END LOOP;
END;
$$;

ALTER TABLE moa.artifact_revision
    ADD CONSTRAINT artifact_revision_status_check CHECK (
        status IN (
            'draft',
            'evaluating',
            'ready',
            'rejected',
            'inconclusive',
            'superseded',
            'archived',
            'published'
        )
    );

-- ---------------------------------------------------------------------------
-- 2. Type-owned serving pointer for release-gated artifact kinds
-- ---------------------------------------------------------------------------

-- Identity tuples the release tables reference. Without them a pointer could
-- name artifact A while claiming kind `skill`, or name a revision belonging to a
-- different artifact, and neither mistake would be checkable by the database.
CREATE UNIQUE INDEX IF NOT EXISTS artifact_kind_identity_idx
    ON moa.artifact (artifact_uid, kind);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_revision_artifact_identity_idx
    ON moa.artifact_revision (revision_uid, artifact_uid);

CREATE TABLE IF NOT EXISTS moa.artifact_serving_pointer (
    artifact_uid UUID PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    -- Always NULL. The column exists so three-tier RLS applies unchanged, and
    -- the check below states the release system's scope rule in the schema:
    -- there is no contact-scoped serving pointer.
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    kind TEXT NOT NULL,
    revision_uid UUID NOT NULL,
    revision_version INT NOT NULL,
    revision_hash BYTEA NOT NULL,
    -- Compare-and-set token. Every activation reads it, requires the caller's
    -- expectation to match, and increments it in the same statement.
    pointer_version BIGINT NOT NULL DEFAULT 1,
    activation_target TEXT NOT NULL,
    -- Every pointer names the attestation consumed to create or move it.
    attestation_uid UUID NOT NULL,
    activated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT artifact_serving_pointer_tenant_scope CHECK (user_id IS NULL),
    CONSTRAINT artifact_serving_pointer_kind CHECK (kind IN ('skill', 'action')),
    CONSTRAINT artifact_serving_pointer_target CHECK (
        activation_target IN ('skill_visibility', 'action_visibility')
    ),
    CONSTRAINT artifact_serving_pointer_hash_len CHECK (octet_length(revision_hash) = 32),
    CONSTRAINT artifact_serving_pointer_version_positive CHECK (
        revision_version > 0 AND pointer_version > 0
    ),
    -- The pointer's kind must be the artifact's kind, and the revision it names
    -- must belong to that same artifact.
    CONSTRAINT artifact_serving_pointer_artifact_fkey
        FOREIGN KEY (artifact_uid, kind)
        REFERENCES moa.artifact (artifact_uid, kind)
        ON DELETE CASCADE,
    CONSTRAINT artifact_serving_pointer_revision_fkey
        FOREIGN KEY (revision_uid, artifact_uid)
        REFERENCES moa.artifact_revision (revision_uid, artifact_uid)
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS artifact_serving_pointer_scope_idx
    ON moa.artifact_serving_pointer (storage_partition_id, kind);

CREATE INDEX IF NOT EXISTS artifact_serving_pointer_revision_idx
    ON moa.artifact_serving_pointer (revision_uid);

SELECT moa.apply_three_tier_rls('moa.artifact_serving_pointer'::REGCLASS);

COMMENT ON TABLE moa.artifact_serving_pointer IS
    'Exact revision a tenant serves for one release-gated artifact. The only serving authority for skill and action resolution; moved only by the activation repository transaction.';

-- Normalize rows authored before release control without preserving their old
-- serving semantics. Contact-scoped release artifacts have no valid release
-- subject, and formerly published revisions must pass the new gate before they
-- can serve again. Existing agent installations retain their own exact pointer,
-- but the revision itself becomes historical rather than implicitly published.
UPDATE moa.artifact_revision r
SET status = 'archived',
    updated_at = now()
FROM moa.artifact a
WHERE a.artifact_uid = r.artifact_uid
  AND a.user_id IS NOT NULL
  AND a.kind IN ('skill', 'action', 'agent')
  AND r.status <> 'archived';

UPDATE moa.artifact a
SET valid_to = now(),
    updated_at = now()
WHERE a.valid_to IS NULL
  AND a.user_id IS NOT NULL
  AND a.kind IN ('skill', 'action', 'agent');

UPDATE moa.artifact_revision r
SET status = 'superseded',
    updated_at = now()
FROM moa.artifact a
WHERE a.artifact_uid = r.artifact_uid
  AND a.kind IN ('skill', 'action', 'agent')
  AND r.status = 'published';

-- ---------------------------------------------------------------------------
-- 3. Kind-scoped status legality
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION moa.artifact_revision_state_guard() RETURNS trigger AS $$
DECLARE
    artifact_kind TEXT;
BEGIN
    SELECT kind INTO artifact_kind
    FROM moa.artifact
    WHERE artifact_uid = NEW.artifact_uid;

    IF artifact_kind IS NULL THEN
        RAISE EXCEPTION
            'artifact revision % names a missing artifact %',
            NEW.revision_uid, NEW.artifact_uid
            USING ERRCODE = 'P0001';
    END IF;

    IF artifact_kind IN ('skill', 'action', 'agent') THEN
        IF NEW.status = 'published' THEN
            RAISE EXCEPTION
                'release-gated artifact kind % cannot use the published status (revision %); serving is a pointer',
                artifact_kind, NEW.revision_uid
                USING ERRCODE = 'P0001';
        END IF;
    ELSIF NEW.status NOT IN ('draft', 'published', 'archived') THEN
        RAISE EXCEPTION
            'artifact kind % cannot use release candidate status % (revision %)',
            artifact_kind, NEW.status, NEW.revision_uid
            USING ERRCODE = 'P0001';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION moa.artifact_revision_state_guard() IS
    'Makes `published` unrepresentable for skill/action/agent revisions and the release candidate states unrepresentable for other kinds, so no writer can reintroduce the overloaded published state.';

DROP TRIGGER IF EXISTS artifact_revision_state_guard ON moa.artifact_revision;
CREATE TRIGGER artifact_revision_state_guard
    BEFORE INSERT OR UPDATE OF status ON moa.artifact_revision
    FOR EACH ROW EXECUTE FUNCTION moa.artifact_revision_state_guard();

-- ---------------------------------------------------------------------------
-- 4. Server-side release policies
-- ---------------------------------------------------------------------------

-- Canonical digest of every field that can change a release decision. The
-- JSONB text form gives stable key ordering while arrays preserve the policy's
-- declared assertion and metric order. Identity fields are included so the
-- digest cannot be retained while a row is relabelled as another revision or
-- target class.
CREATE OR REPLACE FUNCTION moa.artifact_release_policy_content_hash(
    p_name TEXT,
    p_revision INT,
    p_target_class TEXT,
    p_blocking_assertions JSONB,
    p_primary_gate_family JSONB,
    p_attestation_ttl_secs BIGINT,
    p_resource_policy_hash BYTEA
) RETURNS BYTEA
LANGUAGE sql
IMMUTABLE
PARALLEL SAFE
STRICT
AS $$
    SELECT digest(
        jsonb_build_object(
            'schema', 'moa.artifact_release_policy/v1',
            'name', p_name,
            'revision', p_revision,
            'target_class', p_target_class,
            'blocking_assertions', p_blocking_assertions,
            'primary_gate_family', p_primary_gate_family,
            'attestation_ttl_secs', p_attestation_ttl_secs,
            'resource_policy_hash', encode(p_resource_policy_hash, 'hex')
        )::TEXT,
        'sha256'
    );
$$;

COMMENT ON FUNCTION moa.artifact_release_policy_content_hash(
    TEXT, INT, TEXT, JSONB, JSONB, BIGINT, BYTEA
) IS
    'Canonical SHA-256 digest of the complete artifact-release policy authority, including identity, blockers, metric declarations, attestation TTL, and resource policy.';

CREATE TABLE IF NOT EXISTS moa.artifact_release_policy (
    policy_uid UUID PRIMARY KEY,
    -- NULL storage partition makes the row `global` scope: readable from every
    -- tenant context, writable only by moa_promoter. That is the schema-level
    -- reason a candidate submitter cannot weaken the platform default.
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    name TEXT NOT NULL,
    revision INT NOT NULL,
    target_class TEXT NOT NULL CHECK (
        target_class IN ('skill_visibility', 'action_visibility', 'agent_deployment')
    ),
    -- Mandatory deterministic platform score assertions plus the primary gate family. Both
    -- are checked as nonempty here and re-validated in the release types.
    blocking_assertions JSONB NOT NULL,
    primary_gate_family JSONB NOT NULL,
    attestation_ttl_secs BIGINT NOT NULL,
    resource_policy_hash BYTEA NOT NULL,
    policy_hash BYTEA NOT NULL,
    valid_from TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_to TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT artifact_release_policy_tenant_scope CHECK (user_id IS NULL),
    CONSTRAINT artifact_release_policy_nonempty CHECK (
        jsonb_typeof(blocking_assertions) = 'array'
        AND jsonb_array_length(blocking_assertions) > 0
        AND jsonb_typeof(primary_gate_family) = 'array'
        AND jsonb_array_length(primary_gate_family) > 0
    ),
    CONSTRAINT artifact_release_policy_ttl CHECK (
        attestation_ttl_secs BETWEEN 60 AND 604800
    ),
    CONSTRAINT artifact_release_policy_hash_len CHECK (
        octet_length(policy_hash) = 32 AND octet_length(resource_policy_hash) = 32
    ),
    CONSTRAINT artifact_release_policy_revision CHECK (revision > 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_release_policy_active_uniq
    ON moa.artifact_release_policy (
        coalesce(storage_partition_id, ''),
        target_class
    )
    WHERE valid_to IS NULL;

SELECT moa.apply_three_tier_rls('moa.artifact_release_policy'::REGCLASS);

COMMENT ON TABLE moa.artifact_release_policy IS
    'Server-resolved release gate. Platform defaults are global rows no tenant role can write; a tenant override is written under the tenant admin relation, never chosen by a candidate submitter.';

CREATE OR REPLACE FUNCTION moa.artifact_release_policy_guard() RETURNS trigger AS $$
DECLARE
    expected_hash BYTEA;
BEGIN
    expected_hash := moa.artifact_release_policy_content_hash(
        NEW.name,
        NEW.revision,
        NEW.target_class,
        NEW.blocking_assertions,
        NEW.primary_gate_family,
        NEW.attestation_ttl_secs,
        NEW.resource_policy_hash
    );
    IF NEW.policy_hash <> expected_hash THEN
        RAISE EXCEPTION
            'artifact release policy % revision % hash does not match its authority',
            NEW.name, NEW.revision
            USING ERRCODE = '22023';
    END IF;

    IF TG_OP = 'UPDATE'
        AND (
            NEW.policy_uid IS DISTINCT FROM OLD.policy_uid
            OR NEW.storage_partition_id IS DISTINCT FROM OLD.storage_partition_id
            OR NEW.user_id IS DISTINCT FROM OLD.user_id
            OR NEW.name IS DISTINCT FROM OLD.name
            OR NEW.revision IS DISTINCT FROM OLD.revision
            OR NEW.target_class IS DISTINCT FROM OLD.target_class
            OR NEW.blocking_assertions IS DISTINCT FROM OLD.blocking_assertions
            OR NEW.primary_gate_family IS DISTINCT FROM OLD.primary_gate_family
            OR NEW.attestation_ttl_secs IS DISTINCT FROM OLD.attestation_ttl_secs
            OR NEW.resource_policy_hash IS DISTINCT FROM OLD.resource_policy_hash
            OR NEW.policy_hash IS DISTINCT FROM OLD.policy_hash
            OR NEW.valid_from IS DISTINCT FROM OLD.valid_from
            OR NEW.created_at IS DISTINCT FROM OLD.created_at
            OR (
                OLD.valid_to IS NOT NULL
                AND NEW.valid_to IS DISTINCT FROM OLD.valid_to
            )
        )
    THEN
        RAISE EXCEPTION
            'artifact release policy % revision % is immutable; insert a new revision',
            OLD.name, OLD.revision
            USING ERRCODE = 'P0001';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION moa.artifact_release_policy_guard() IS
    'Validates the canonical policy digest on insert and permits only the first valid_to lifecycle closure on update; policy rotation is insert-new-revision.';

DROP TRIGGER IF EXISTS artifact_release_policy_immutable
    ON moa.artifact_release_policy;
CREATE TRIGGER artifact_release_policy_immutable
    BEFORE INSERT OR UPDATE ON moa.artifact_release_policy
    FOR EACH ROW EXECUTE FUNCTION moa.artifact_release_policy_guard();

-- Platform default policies. Without a resolvable policy the release surface
-- rejects a candidate before evaluation, so these rows are what make the gate
-- usable at all. The six independent-unit requirement is a diagnostic evidence
-- floor over scenario/persona/profile clusters; repetitions do not satisfy it.
-- Comparative results do not authorize activation until the exact production
-- design has a passing operating-characteristic assessment.
WITH policy_identity (policy_uid, name, revision, target_class) AS (
    VALUES
        (
            '00000000-0000-4000-8000-0000000d7301'::UUID,
            'platform-default-skill-visibility'::TEXT,
            2::INT,
            'skill_visibility'::TEXT
        ),
        (
            '00000000-0000-4000-8000-0000000d7302',
            'platform-default-action-visibility',
            2,
            'action_visibility'
        ),
        (
            '00000000-0000-4000-8000-0000000d7303',
            'platform-default-agent-deployment',
            2,
            'agent_deployment'
        )
),
policy_body (
    blocking_assertions,
    primary_gate_family,
    attestation_ttl_secs,
    resource_policy_hash
) AS (
    VALUES (
        '[{"id":"scenario_outcome","version":"v1","determinism":"deterministic"},{"id":"target_completed","version":"v1","determinism":"deterministic"},{"id":"result_produced","version":"v1","determinism":"deterministic"},{"id":"privacy_safe_output","version":"v1","determinism":"deterministic"}]'::JSONB,
        '[{"metric":"result_produced","direction":"higher_is_better","estimand":"paired difference in result-production probability","target_population":"approved artifact-release scenarios","independent_unit":"scenario_persona_profile","cluster_key":"scenario_persona_profile","paired_key":"scenario_persona_profile_repetition","confidence_method":"cluster_matched_risk_difference_bootstrap","unit":"proportion","margin_bp":500,"alpha_bp":250,"acceptable_alternative_bp":0,"unacceptable_alternative_bp":-1000,"resamples":2000,"min_independent_units":6,"holm_regression_alpha_bp":250}]'::JSONB,
        86400::BIGINT,
        digest('moa.release.resource_policy.v1', 'sha256')
    )
)
INSERT INTO moa.artifact_release_policy (
    policy_uid, storage_partition_id, user_id, name, revision, target_class,
    blocking_assertions, primary_gate_family, attestation_ttl_secs,
    resource_policy_hash, policy_hash
)
SELECT
    identity.policy_uid,
    NULL,
    NULL,
    identity.name,
    identity.revision,
    identity.target_class,
    body.blocking_assertions,
    body.primary_gate_family,
    body.attestation_ttl_secs,
    body.resource_policy_hash,
    moa.artifact_release_policy_content_hash(
        identity.name,
        identity.revision,
        identity.target_class,
        body.blocking_assertions,
        body.primary_gate_family,
        body.attestation_ttl_secs,
        body.resource_policy_hash
    )
FROM policy_identity AS identity
CROSS JOIN policy_body AS body
ON CONFLICT (policy_uid) DO NOTHING;

-- ---------------------------------------------------------------------------
-- 5. Release candidates
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS moa.artifact_release_candidate (
    revision_uid UUID PRIMARY KEY,
    artifact_uid UUID NOT NULL
        REFERENCES moa.artifact(artifact_uid) ON DELETE CASCADE,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    activation_target TEXT NOT NULL CHECK (
        activation_target IN ('skill_visibility', 'action_visibility', 'agent_deployment')
    ),
    -- Installation the candidate would deploy into, for agent subjects only.
    target_installation_uid UUID
        REFERENCES moa.agent_installation(installation_uid) ON DELETE CASCADE,
    -- The exact `EvaluationSubjectV1` and its canonical digest. Activation
    -- recomputes the digest from this row plus live pointer state and refuses to
    -- proceed when they differ.
    subject JSONB NOT NULL,
    subject_digest BYTEA NOT NULL,
    candidate_revision_hash BYTEA NOT NULL,
    policy_uid UUID NOT NULL REFERENCES moa.artifact_release_policy(policy_uid),
    policy_revision INT NOT NULL,
    policy_hash BYTEA NOT NULL,
    -- Coalescing slot. At most one `active` and one `pending` candidate per
    -- artifact: rapid submissions collapse to one running attempt plus the
    -- newest waiting subject.
    slot TEXT NOT NULL CHECK (slot IN ('active', 'pending', 'released')),
    generation BIGINT NOT NULL,
    attempt_count INT NOT NULL DEFAULT 0,
    last_run_uid UUID,
    last_decision TEXT,
    submitted_by TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT artifact_release_candidate_tenant_scope CHECK (user_id IS NULL),
    CONSTRAINT artifact_release_candidate_agent_target CHECK (
        (activation_target = 'agent_deployment') = (target_installation_uid IS NOT NULL)
    ),
    CONSTRAINT artifact_release_candidate_hash_len CHECK (
        octet_length(subject_digest) = 32
        AND octet_length(candidate_revision_hash) = 32
        AND octet_length(policy_hash) = 32
    ),
    CONSTRAINT artifact_release_candidate_generation CHECK (generation > 0),
    CONSTRAINT artifact_release_candidate_attempts CHECK (attempt_count >= 0),
    -- A candidate cannot claim a revision that belongs to a different artifact.
    CONSTRAINT artifact_release_candidate_revision_fkey
        FOREIGN KEY (revision_uid, artifact_uid)
        REFERENCES moa.artifact_revision (revision_uid, artifact_uid)
        ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_release_candidate_active_slot_uniq
    ON moa.artifact_release_candidate (artifact_uid)
    WHERE slot = 'active';

CREATE UNIQUE INDEX IF NOT EXISTS artifact_release_candidate_pending_slot_uniq
    ON moa.artifact_release_candidate (artifact_uid)
    WHERE slot = 'pending';

CREATE INDEX IF NOT EXISTS artifact_release_candidate_scope_idx
    ON moa.artifact_release_candidate (storage_partition_id, activation_target, updated_at DESC);

SELECT moa.apply_three_tier_rls('moa.artifact_release_candidate'::REGCLASS);

COMMENT ON TABLE moa.artifact_release_candidate IS
    'Release attempt bookkeeping for one immutable candidate revision: its exact evaluation subject, the server-resolved policy, and its coalescing slot. Candidate lifecycle state lives in moa.artifact_revision.status.';

-- ---------------------------------------------------------------------------
-- 6. Activation attestations
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS moa.artifact_activation_attestation (
    attestation_uid UUID PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    artifact_uid UUID NOT NULL
        REFERENCES moa.artifact(artifact_uid) ON DELETE CASCADE,
    candidate_revision_uid UUID NOT NULL,
    activation_target TEXT NOT NULL CHECK (
        activation_target IN ('skill_visibility', 'action_visibility', 'agent_deployment')
    ),
    target_installation_uid UUID
        REFERENCES moa.agent_installation(installation_uid) ON DELETE CASCADE,
    subject_digest BYTEA NOT NULL,
    -- Only a passing deterministic verdict is persistable. Regression and
    -- inconclusive outcomes move the candidate state instead; they never mint a
    -- permission to serve.
    verdict TEXT NOT NULL CHECK (verdict = 'pass'),
    run_uid UUID NOT NULL,
    trial_uids UUID[] NOT NULL,
    evidence_ids UUID[] NOT NULL,
    decision JSONB NOT NULL,
    policy_uid UUID NOT NULL REFERENCES moa.artifact_release_policy(policy_uid),
    policy_revision INT NOT NULL,
    policy_hash BYTEA NOT NULL,
    decided_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    consumed_by_audit_uid UUID,
    CONSTRAINT artifact_activation_attestation_tenant_scope CHECK (user_id IS NULL),
    CONSTRAINT artifact_activation_attestation_agent_target CHECK (
        (activation_target = 'agent_deployment') = (target_installation_uid IS NOT NULL)
    ),
    CONSTRAINT artifact_activation_attestation_hash_len CHECK (
        octet_length(subject_digest) = 32 AND octet_length(policy_hash) = 32
    ),
    CONSTRAINT artifact_activation_attestation_evidence_nonempty CHECK (
        array_length(trial_uids, 1) > 0 AND array_length(evidence_ids, 1) > 0
    ),
    CONSTRAINT artifact_activation_attestation_expiry CHECK (expires_at > created_at),
    CONSTRAINT artifact_activation_attestation_consumption CHECK (
        (consumed_at IS NULL) = (consumed_by_audit_uid IS NULL)
    ),
    -- The attested revision must belong to the attested artifact.
    CONSTRAINT artifact_activation_attestation_revision_fkey
        FOREIGN KEY (candidate_revision_uid, artifact_uid)
        REFERENCES moa.artifact_revision (revision_uid, artifact_uid)
        ON DELETE CASCADE
);

-- Single use is a uniqueness property, not a code convention: one attestation
-- can appear in at most one audit row.
CREATE UNIQUE INDEX IF NOT EXISTS artifact_activation_attestation_consumer_uniq
    ON moa.artifact_activation_attestation (consumed_by_audit_uid)
    WHERE consumed_by_audit_uid IS NOT NULL;

CREATE INDEX IF NOT EXISTS artifact_activation_attestation_candidate_idx
    ON moa.artifact_activation_attestation (candidate_revision_uid, created_at DESC);

CREATE INDEX IF NOT EXISTS artifact_activation_attestation_open_idx
    ON moa.artifact_activation_attestation (storage_partition_id, expires_at)
    WHERE consumed_at IS NULL;

SELECT moa.apply_three_tier_rls('moa.artifact_activation_attestation'::REGCLASS);

CREATE OR REPLACE FUNCTION moa.artifact_activation_attestation_guard() RETURNS trigger AS $$
BEGIN
    IF NEW.attestation_uid <> OLD.attestation_uid
        OR NEW.storage_partition_id <> OLD.storage_partition_id
        OR NEW.artifact_uid <> OLD.artifact_uid
        OR NEW.candidate_revision_uid <> OLD.candidate_revision_uid
        OR NEW.activation_target <> OLD.activation_target
        OR NEW.target_installation_uid IS DISTINCT FROM OLD.target_installation_uid
        OR NEW.subject_digest <> OLD.subject_digest
        OR NEW.verdict <> OLD.verdict
        OR NEW.run_uid <> OLD.run_uid
        OR NEW.trial_uids <> OLD.trial_uids
        OR NEW.evidence_ids <> OLD.evidence_ids
        OR NEW.decision <> OLD.decision
        OR NEW.policy_uid <> OLD.policy_uid
        OR NEW.policy_revision <> OLD.policy_revision
        OR NEW.policy_hash <> OLD.policy_hash
        OR NEW.decided_by <> OLD.decided_by
        OR NEW.created_at <> OLD.created_at
        OR NEW.expires_at <> OLD.expires_at
    THEN
        RAISE EXCEPTION
            'activation attestation % is immutable except for consumption',
            OLD.attestation_uid
            USING ERRCODE = 'P0001';
    END IF;

    IF OLD.consumed_at IS NOT NULL THEN
        RAISE EXCEPTION
            'activation attestation % was already consumed at %',
            OLD.attestation_uid, OLD.consumed_at
            USING ERRCODE = 'P0001';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION moa.artifact_activation_attestation_guard() IS
    'Refuses every attestation UPDATE except the first consumption, so a spent permission to serve cannot be reset or rewritten.';

DROP TRIGGER IF EXISTS artifact_activation_attestation_immutable
    ON moa.artifact_activation_attestation;
CREATE TRIGGER artifact_activation_attestation_immutable
    BEFORE UPDATE ON moa.artifact_activation_attestation
    FOR EACH ROW EXECUTE FUNCTION moa.artifact_activation_attestation_guard();

COMMENT ON TABLE moa.artifact_activation_attestation IS
    'Immutable, expiring, single-use permission to move one type-owned serving pointer to one exact candidate revision under one exact evaluation subject.';

-- ---------------------------------------------------------------------------
-- 7. Activation audit
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS moa.artifact_activation_audit (
    audit_uid UUID PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    artifact_uid UUID NOT NULL
        REFERENCES moa.artifact(artifact_uid) ON DELETE CASCADE,
    activation_target TEXT NOT NULL,
    target_installation_uid UUID
        REFERENCES moa.agent_installation(installation_uid) ON DELETE CASCADE,
    -- An activation consumes an attestation. A rollback moves the pointer back to
    -- a revision that already served under an earlier attestation, so it carries
    -- none of its own -- and the check below makes the two cases distinguishable
    -- instead of letting a NULL attestation pass as an activation.
    decision_kind TEXT NOT NULL DEFAULT 'activation'
        CHECK (decision_kind IN ('activation', 'rollback')),
    attestation_uid UUID
        REFERENCES moa.artifact_activation_attestation(attestation_uid) ON DELETE RESTRICT,
    subject_digest BYTEA,
    previous_revision_uid UUID,
    previous_pointer_version BIGINT NOT NULL,
    activated_revision_uid UUID
        REFERENCES moa.artifact_revision(revision_uid) ON DELETE RESTRICT,
    activated_pointer_version BIGINT NOT NULL,
    decided_by TEXT NOT NULL,
    reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT artifact_activation_audit_tenant_scope CHECK (user_id IS NULL),
    CONSTRAINT artifact_activation_audit_activation_shape CHECK (
        (decision_kind = 'activation') = (attestation_uid IS NOT NULL)
        AND (decision_kind = 'activation') = (subject_digest IS NOT NULL)
        AND (decision_kind <> 'activation' OR activated_revision_uid IS NOT NULL)
    ),
    CONSTRAINT artifact_activation_audit_hash_len CHECK (
        subject_digest IS NULL OR octet_length(subject_digest) = 32
    ),
    CONSTRAINT artifact_activation_audit_pointer_moves CHECK (
        activated_pointer_version = previous_pointer_version + 1
    )
);

CREATE INDEX IF NOT EXISTS artifact_activation_audit_artifact_idx
    ON moa.artifact_activation_audit (artifact_uid, created_at DESC);

CREATE INDEX IF NOT EXISTS artifact_activation_audit_scope_idx
    ON moa.artifact_activation_audit (storage_partition_id, created_at DESC);

SELECT moa.apply_three_tier_rls('moa.artifact_activation_audit'::REGCLASS);

CREATE OR REPLACE FUNCTION moa.artifact_activation_audit_guard() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'DELETE'
        AND current_user = 'moa_artifact_releaser'
        AND OLD.storage_partition_id = current_setting(
            'moa.artifact_release_purge_partition',
            true
        )
    THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION
        'activation audit % is append-only',
        OLD.audit_uid
        USING ERRCODE = 'P0001';
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION moa.artifact_activation_audit_guard() IS
    'Refuses UPDATE and DELETE on moa.artifact_activation_audit so a serving change cannot be un-recorded.';

DROP TRIGGER IF EXISTS artifact_activation_audit_append_only
    ON moa.artifact_activation_audit;
CREATE TRIGGER artifact_activation_audit_append_only
    BEFORE UPDATE OR DELETE ON moa.artifact_activation_audit
    FOR EACH ROW EXECUTE FUNCTION moa.artifact_activation_audit_guard();

COMMENT ON TABLE moa.artifact_activation_audit IS
    'One row per serving pointer move, written in the same transaction as the move and the attestation consumption.';

-- The application can read serving state, but it cannot write the pointer or
-- append an activation audit directly. Both mutations cross these exact-scope
-- SECURITY DEFINER transitions, which validate the attestation/candidate link
-- again at the database boundary and keep pointer CAS plus audit append atomic.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'moa_artifact_activator') THEN
        CREATE ROLE moa_artifact_activator NOLOGIN NOINHERIT NOBYPASSRLS;
    ELSE
        ALTER ROLE moa_artifact_activator NOLOGIN NOINHERIT NOBYPASSRLS;
    END IF;
END;
$$;

GRANT USAGE ON SCHEMA moa TO moa_artifact_activator;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.artifact_serving_pointer
    TO moa_artifact_activator;
GRANT SELECT, INSERT ON moa.artifact_activation_audit TO moa_artifact_activator;
GRANT SELECT ON moa.artifact_release_candidate, moa.artifact_activation_attestation
    TO moa_artifact_activator;
GRANT SELECT ON moa.artifact, moa.artifact_revision TO moa_artifact_activator;

DROP POLICY IF EXISTS artifact_activator_scope ON moa.artifact_serving_pointer;
CREATE POLICY artifact_activator_scope ON moa.artifact_serving_pointer
    FOR ALL TO moa_artifact_activator
    USING (
        storage_partition_id = current_setting('moa.storage_partition_id', true)
    )
    WITH CHECK (
        storage_partition_id = current_setting('moa.storage_partition_id', true)
    );

DROP POLICY IF EXISTS artifact_activator_read ON moa.artifact_release_candidate;
CREATE POLICY artifact_activator_read ON moa.artifact_release_candidate
    FOR SELECT TO moa_artifact_activator
    USING (
        storage_partition_id = current_setting('moa.storage_partition_id', true)
    );

DROP POLICY IF EXISTS artifact_activator_read ON moa.artifact;
CREATE POLICY artifact_activator_read ON moa.artifact
    FOR SELECT TO moa_artifact_activator
    USING (
        storage_partition_id = current_setting('moa.storage_partition_id', true)
        AND user_id IS NULL
    );

DROP POLICY IF EXISTS artifact_activator_read ON moa.artifact_revision;
CREATE POLICY artifact_activator_read ON moa.artifact_revision
    FOR SELECT TO moa_artifact_activator
    USING (
        artifact_uid IN (
            SELECT artifact_uid
            FROM moa.artifact
            WHERE storage_partition_id = current_setting(
                'moa.storage_partition_id', true
            )
              AND user_id IS NULL
        )
    );

DROP POLICY IF EXISTS artifact_activator_read ON moa.artifact_activation_attestation;
CREATE POLICY artifact_activator_read ON moa.artifact_activation_attestation
    FOR SELECT TO moa_artifact_activator
    USING (
        storage_partition_id = current_setting('moa.storage_partition_id', true)
    );

DROP POLICY IF EXISTS artifact_activator_scope ON moa.artifact_activation_audit;
CREATE POLICY artifact_activator_scope ON moa.artifact_activation_audit
    FOR ALL TO moa_artifact_activator
    USING (
        storage_partition_id = current_setting('moa.storage_partition_id', true)
    )
    WITH CHECK (
        storage_partition_id = current_setting('moa.storage_partition_id', true)
    );

CREATE OR REPLACE FUNCTION moa.lock_artifact_serving_pointer(
    p_storage_partition_id TEXT,
    p_artifact_uid UUID
) RETURNS VOID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    scoped_partition TEXT;
BEGIN
    scoped_partition := current_setting('moa.storage_partition_id', true);
    IF p_storage_partition_id IS NULL
        OR btrim(p_storage_partition_id) = ''
        OR scoped_partition IS NULL
        OR scoped_partition <> p_storage_partition_id
    THEN
        RAISE EXCEPTION 'artifact serving lock scope does not match requested partition'
            USING ERRCODE = '42501';
    END IF;
    -- The advisory lock also serializes the first activation, when no pointer row
    -- exists yet and therefore there is no row PostgreSQL could lock.
    PERFORM pg_advisory_xact_lock(
        hashtextextended(p_artifact_uid::TEXT, 7046029254386353131)
    );
    PERFORM 1
    FROM moa.artifact_serving_pointer
    WHERE artifact_uid = p_artifact_uid
      AND storage_partition_id = p_storage_partition_id
    FOR UPDATE;
END;
$$;

ALTER FUNCTION moa.lock_artifact_serving_pointer(TEXT, UUID)
    OWNER TO moa_artifact_activator;
REVOKE ALL ON FUNCTION moa.lock_artifact_serving_pointer(TEXT, UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.lock_artifact_serving_pointer(TEXT, UUID) TO moa_app;

CREATE OR REPLACE FUNCTION moa.apply_artifact_activation_transition(
    p_audit_uid UUID,
    p_storage_partition_id TEXT,
    p_artifact_uid UUID,
    p_kind TEXT,
    p_activation_target TEXT,
    p_target_installation_uid UUID,
    p_attestation_uid UUID,
    p_subject_digest BYTEA,
    p_previous_revision_uid UUID,
    p_previous_pointer_version BIGINT,
    p_activated_revision_uid UUID,
    p_activated_revision_version INT,
    p_activated_revision_hash BYTEA,
    p_activated_pointer_version BIGINT,
    p_decided_by TEXT,
    p_reason TEXT,
    p_now TIMESTAMPTZ
) RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    affected BIGINT := 1;
    scoped_partition TEXT;
BEGIN
    scoped_partition := current_setting('moa.storage_partition_id', true);
    IF p_storage_partition_id IS NULL
        OR btrim(p_storage_partition_id) = ''
        OR scoped_partition IS NULL
        OR scoped_partition <> p_storage_partition_id
    THEN
        RAISE EXCEPTION 'artifact activation scope does not match requested partition'
            USING ERRCODE = '42501';
    END IF;
    IF p_activated_pointer_version <> p_previous_pointer_version + 1 THEN
        RAISE EXCEPTION 'artifact activation pointer version is not monotonic'
            USING ERRCODE = '22023';
    END IF;
    IF NOT EXISTS (
        SELECT 1
        FROM moa.artifact_release_candidate candidate
        JOIN moa.artifact_revision revision
          ON revision.revision_uid = candidate.revision_uid
         AND revision.artifact_uid = candidate.artifact_uid
        JOIN moa.artifact artifact
          ON artifact.artifact_uid = candidate.artifact_uid
        JOIN moa.artifact_activation_attestation attestation
          ON attestation.candidate_revision_uid = candidate.revision_uid
         AND attestation.artifact_uid = candidate.artifact_uid
         AND attestation.storage_partition_id = candidate.storage_partition_id
         AND attestation.activation_target = candidate.activation_target
         AND attestation.target_installation_uid IS NOT DISTINCT FROM
             candidate.target_installation_uid
         AND attestation.subject_digest = candidate.subject_digest
        WHERE candidate.revision_uid = p_activated_revision_uid
          AND candidate.artifact_uid = p_artifact_uid
          AND candidate.storage_partition_id = p_storage_partition_id
          AND candidate.activation_target = p_activation_target
          AND candidate.target_installation_uid IS NOT DISTINCT FROM
              p_target_installation_uid
          AND candidate.subject_digest = p_subject_digest
          AND revision.status = 'ready'
          AND revision.version = p_activated_revision_version
          AND revision.canonical_hash = p_activated_revision_hash
          AND artifact.kind = p_kind
          AND attestation.attestation_uid = p_attestation_uid
          AND attestation.consumed_at IS NULL
          AND attestation.expires_at > p_now
    ) THEN
        RAISE EXCEPTION 'artifact activation attestation does not match the ready candidate'
            USING ERRCODE = '42501';
    END IF;

    IF p_activation_target IN ('skill_visibility', 'action_visibility') THEN
        IF (p_activation_target = 'skill_visibility') <> (p_kind = 'skill')
            OR (p_activation_target = 'action_visibility') <> (p_kind = 'action')
            OR p_target_installation_uid IS NOT NULL
        THEN
            RAISE EXCEPTION 'artifact activation target and kind disagree'
                USING ERRCODE = '22023';
        END IF;
        IF p_previous_revision_uid IS NULL THEN
            IF p_previous_pointer_version <> 0 THEN
                RAISE EXCEPTION 'an absent serving pointer must start at version zero'
                    USING ERRCODE = '22023';
            END IF;
            INSERT INTO moa.artifact_serving_pointer (
                artifact_uid, storage_partition_id, user_id, kind, revision_uid,
                revision_version, revision_hash, pointer_version,
                activation_target, attestation_uid, activated_at, updated_at
            )
            VALUES (
                p_artifact_uid, p_storage_partition_id, NULL, p_kind,
                p_activated_revision_uid, p_activated_revision_version,
                p_activated_revision_hash, p_activated_pointer_version,
                p_activation_target, p_attestation_uid, p_now, p_now
            )
            ON CONFLICT (artifact_uid) DO NOTHING;
            GET DIAGNOSTICS affected = ROW_COUNT;
        ELSE
            UPDATE moa.artifact_serving_pointer
            SET revision_uid = p_activated_revision_uid,
                revision_version = p_activated_revision_version,
                revision_hash = p_activated_revision_hash,
                pointer_version = p_activated_pointer_version,
                attestation_uid = p_attestation_uid,
                activated_at = p_now,
                updated_at = p_now
            WHERE artifact_uid = p_artifact_uid
              AND storage_partition_id = p_storage_partition_id
              AND revision_uid = p_previous_revision_uid
              AND pointer_version = p_previous_pointer_version;
            GET DIAGNOSTICS affected = ROW_COUNT;
        END IF;
    ELSIF p_activation_target = 'agent_deployment' THEN
        IF p_kind <> 'agent' OR p_target_installation_uid IS NULL THEN
            RAISE EXCEPTION 'agent activation target and kind disagree'
                USING ERRCODE = '22023';
        END IF;
    ELSE
        RAISE EXCEPTION 'unsupported artifact activation target %', p_activation_target
            USING ERRCODE = '22023';
    END IF;

    IF affected <> 1 THEN
        RETURN affected;
    END IF;

    INSERT INTO moa.artifact_activation_audit (
        audit_uid, storage_partition_id, user_id, artifact_uid,
        activation_target, target_installation_uid, attestation_uid,
        subject_digest, previous_revision_uid, previous_pointer_version,
        activated_revision_uid, activated_pointer_version, decided_by, reason
    )
    VALUES (
        p_audit_uid, p_storage_partition_id, NULL, p_artifact_uid,
        p_activation_target, p_target_installation_uid, p_attestation_uid,
        p_subject_digest, p_previous_revision_uid, p_previous_pointer_version,
        p_activated_revision_uid, p_activated_pointer_version, p_decided_by,
        p_reason
    );
    RETURN affected;
END;
$$;

ALTER FUNCTION moa.apply_artifact_activation_transition(
    UUID, TEXT, UUID, TEXT, TEXT, UUID, UUID, BYTEA, UUID, BIGINT, UUID,
    INT, BYTEA, BIGINT, TEXT, TEXT, TIMESTAMPTZ
) OWNER TO moa_artifact_activator;
REVOKE ALL ON FUNCTION moa.apply_artifact_activation_transition(
    UUID, TEXT, UUID, TEXT, TEXT, UUID, UUID, BYTEA, UUID, BIGINT, UUID,
    INT, BYTEA, BIGINT, TEXT, TEXT, TIMESTAMPTZ
) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.apply_artifact_activation_transition(
    UUID, TEXT, UUID, TEXT, TEXT, UUID, UUID, BYTEA, UUID, BIGINT, UUID,
    INT, BYTEA, BIGINT, TEXT, TEXT, TIMESTAMPTZ
) TO moa_app;

CREATE OR REPLACE FUNCTION moa.apply_artifact_rollback_transition(
    p_audit_uid UUID,
    p_storage_partition_id TEXT,
    p_artifact_uid UUID,
    p_activation_target TEXT,
    p_expected_activation_audit_uid UUID,
    p_previous_revision_uid UUID,
    p_previous_pointer_version BIGINT,
    p_activated_pointer_version BIGINT,
    p_decided_by TEXT,
    p_reason TEXT
) RETURNS BIGINT
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    affected BIGINT;
    scoped_partition TEXT;
BEGIN
    scoped_partition := current_setting('moa.storage_partition_id', true);
    IF p_storage_partition_id IS NULL
        OR btrim(p_storage_partition_id) = ''
        OR scoped_partition IS NULL
        OR scoped_partition <> p_storage_partition_id
    THEN
        RAISE EXCEPTION 'artifact rollback scope does not match requested partition'
            USING ERRCODE = '42501';
    END IF;
    IF p_activated_pointer_version <> p_previous_pointer_version + 1 THEN
        RAISE EXCEPTION 'artifact rollback pointer version is not monotonic'
            USING ERRCODE = '22023';
    END IF;
    IF NOT EXISTS (
        SELECT 1
        FROM moa.artifact_activation_audit
        WHERE audit_uid = p_expected_activation_audit_uid
          AND storage_partition_id = p_storage_partition_id
          AND artifact_uid = p_artifact_uid
          AND decision_kind = 'activation'
          AND activated_revision_uid = p_previous_revision_uid
          AND activated_pointer_version = p_previous_pointer_version
    ) THEN
        RETURN 0;
    END IF;

    DELETE FROM moa.artifact_serving_pointer
    WHERE artifact_uid = p_artifact_uid
      AND storage_partition_id = p_storage_partition_id
      AND revision_uid = p_previous_revision_uid
      AND pointer_version = p_previous_pointer_version;
    GET DIAGNOSTICS affected = ROW_COUNT;
    IF affected <> 1 THEN
        RETURN affected;
    END IF;

    INSERT INTO moa.artifact_activation_audit (
        audit_uid, storage_partition_id, user_id, artifact_uid,
        activation_target, target_installation_uid, decision_kind,
        attestation_uid, subject_digest, previous_revision_uid,
        previous_pointer_version, activated_revision_uid,
        activated_pointer_version, decided_by, reason
    )
    VALUES (
        p_audit_uid, p_storage_partition_id, NULL, p_artifact_uid,
        p_activation_target, NULL, 'rollback', NULL, NULL,
        p_previous_revision_uid, p_previous_pointer_version, NULL,
        p_activated_pointer_version, p_decided_by, p_reason
    );
    RETURN affected;
END;
$$;

ALTER FUNCTION moa.apply_artifact_rollback_transition(
    UUID, TEXT, UUID, TEXT, UUID, UUID, BIGINT, BIGINT, TEXT, TEXT
) OWNER TO moa_artifact_activator;
REVOKE ALL ON FUNCTION moa.apply_artifact_rollback_transition(
    UUID, TEXT, UUID, TEXT, UUID, UUID, BIGINT, BIGINT, TEXT, TEXT
) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.apply_artifact_rollback_transition(
    UUID, TEXT, UUID, TEXT, UUID, UUID, BIGINT, BIGINT, TEXT, TEXT
) TO moa_app;

REVOKE INSERT, UPDATE, DELETE ON moa.artifact_serving_pointer
    FROM moa_app, moa_promoter;
REVOKE INSERT ON moa.artifact_activation_audit FROM moa_app, moa_promoter;

COMMENT ON FUNCTION moa.apply_artifact_activation_transition(
    UUID, TEXT, UUID, TEXT, TEXT, UUID, UUID, BYTEA, UUID, BIGINT, UUID,
    INT, BYTEA, BIGINT, TEXT, TEXT, TIMESTAMPTZ
) IS
    'Validates one unconsumed attestation, applies a skill/action pointer CAS when applicable, and appends the matching activation audit in one exact-tenant transition.';
COMMENT ON FUNCTION moa.lock_artifact_serving_pointer(TEXT, UUID) IS
    'Takes the transaction-scoped artifact lock and existing pointer row lock required before reading an activation or rollback baseline; it also serializes an absent first pointer.';
COMMENT ON FUNCTION moa.apply_artifact_rollback_transition(
    UUID, TEXT, UUID, TEXT, UUID, UUID, BIGINT, BIGINT, TEXT, TEXT
) IS
    'Removes one exact serving pointer epoch and appends its rollback audit in one exact-tenant transition.';

-- ---------------------------------------------------------------------------
-- 8. Agent installation CAS token
-- ---------------------------------------------------------------------------

-- The agent serving pointer is the installation, so it needs the same
-- compare-and-set token and the same link to the attestation that permitted the
-- move. Existing installations start at 0 and their first gated deploy moves
-- them to 1.
ALTER TABLE moa.agent_installation
    ADD COLUMN IF NOT EXISTS serving_pointer_version BIGINT NOT NULL DEFAULT 0;

ALTER TABLE moa.agent_installation
    ADD COLUMN IF NOT EXISTS activation_attestation_uid UUID;

ALTER TABLE moa.agent_installation
    DROP CONSTRAINT IF EXISTS agent_installation_attestation_fkey;
ALTER TABLE moa.agent_installation
    ADD CONSTRAINT agent_installation_attestation_fkey
        FOREIGN KEY (activation_attestation_uid)
        REFERENCES moa.artifact_activation_attestation(attestation_uid)
        ON DELETE SET NULL;

ALTER TABLE moa.agent_installation
    DROP CONSTRAINT IF EXISTS agent_installation_pointer_version_nonnegative;
ALTER TABLE moa.agent_installation
    ADD CONSTRAINT agent_installation_pointer_version_nonnegative
        CHECK (serving_pointer_version >= 0);
