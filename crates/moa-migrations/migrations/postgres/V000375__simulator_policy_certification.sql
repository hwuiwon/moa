-- Certified simulator policies consumed by production Behavior Lab trials.

CREATE TABLE IF NOT EXISTS moa.simulator_policy (
    policy_uid UUID NOT NULL,
    revision INTEGER NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (
        moa.compute_scope_tier(storage_partition_id, user_id)
    ) STORED,
    domain TEXT NOT NULL,
    policy_hash BYTEA NOT NULL,
    components JSONB NOT NULL,
    state TEXT NOT NULL,
    valid_from TIMESTAMPTZ NOT NULL,
    valid_until TIMESTAMPTZ NOT NULL,
    certification_study_uid UUID,
    certification_artifact_hash BYTEA,
    certified_policy_hash BYTEA,
    certified_from TIMESTAMPTZ,
    certified_until TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (policy_uid, revision, storage_partition_id),
    CHECK (scope IS NOT NULL),
    CONSTRAINT simulator_policy_is_tenant_scoped CHECK (user_id IS NULL),
    CONSTRAINT simulator_policy_revision_positive CHECK (revision >= 1),
    CONSTRAINT simulator_policy_domain_bounded CHECK (length(domain) BETWEEN 1 AND 64),
    CONSTRAINT simulator_policy_hash_len CHECK (octet_length(policy_hash) = 32),
    CONSTRAINT simulator_policy_state_known CHECK (
        state IN ('draft', 'certified', 'rejected', 'revoked')
    ),
    CONSTRAINT simulator_policy_validity_non_empty CHECK (valid_until > valid_from),
    CONSTRAINT simulator_policy_certification_hash_lens CHECK (
        (certification_artifact_hash IS NULL OR octet_length(certification_artifact_hash) = 32)
        AND (certified_policy_hash IS NULL OR octet_length(certified_policy_hash) = 32)
    ),
    CONSTRAINT simulator_policy_certification_complete CHECK (
        (certification_study_uid IS NULL) = (certification_artifact_hash IS NULL)
        AND (certification_study_uid IS NULL) = (certified_policy_hash IS NULL)
        AND (certification_study_uid IS NULL) = (certified_from IS NULL)
        AND (certification_study_uid IS NULL) = (certified_until IS NULL)
    ),
    CONSTRAINT simulator_policy_certification_window_non_empty CHECK (
        certified_until IS NULL OR certified_until > certified_from
    ),
    CONSTRAINT simulator_policy_certified_requires_window CHECK (
        state <> 'certified' OR certification_study_uid IS NOT NULL
    )
);

CREATE INDEX IF NOT EXISTS simulator_policy_domain_state_idx
    ON moa.simulator_policy (storage_partition_id, domain, state, certified_until);

CREATE OR REPLACE FUNCTION moa.simulator_policy_pins_are_immutable() RETURNS trigger AS $$
BEGIN
    IF NEW.policy_uid <> OLD.policy_uid
       OR NEW.revision <> OLD.revision
       OR NEW.storage_partition_id <> OLD.storage_partition_id
       OR NEW.domain <> OLD.domain
       OR NEW.policy_hash <> OLD.policy_hash
       OR NEW.components::TEXT <> OLD.components::TEXT
       OR NEW.valid_from <> OLD.valid_from
       OR NEW.valid_until <> OLD.valid_until
    THEN
        RAISE EXCEPTION
            'simulator policy pins are immutable (policy=%, revision=%)',
            OLD.policy_uid, OLD.revision
            USING ERRCODE = 'P0001';
    END IF;
    NEW.updated_at := now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS simulator_policy_pins_immutable ON moa.simulator_policy;
CREATE TRIGGER simulator_policy_pins_immutable
    BEFORE UPDATE ON moa.simulator_policy
    FOR EACH ROW EXECUTE FUNCTION moa.simulator_policy_pins_are_immutable();

CREATE TABLE IF NOT EXISTS moa.simulator_fidelity_study (
    study_uid UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (
        moa.compute_scope_tier(storage_partition_id, user_id)
    ) STORED,
    policy_uid UUID NOT NULL,
    policy_revision INTEGER NOT NULL,
    policy_hash BYTEA NOT NULL,
    domain TEXT NOT NULL,
    verdict TEXT NOT NULL,
    artifact_json TEXT NOT NULL,
    artifact_hash BYTEA NOT NULL,
    outcome JSONB NOT NULL,
    selection_cohort_id TEXT NOT NULL,
    selection_cohort_hash BYTEA NOT NULL,
    selection_cohort_units INTEGER NOT NULL,
    certification_cohort_id TEXT NOT NULL,
    certification_cohort_hash BYTEA NOT NULL,
    certification_cohort_units INTEGER NOT NULL,
    budget_micro_usd BIGINT NOT NULL,
    spent_micro_usd BIGINT NOT NULL,
    human_data_authorization_id TEXT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (study_uid, storage_partition_id),
    CHECK (scope IS NOT NULL),
    CONSTRAINT simulator_fidelity_study_is_tenant_scoped CHECK (user_id IS NULL),
    CONSTRAINT simulator_fidelity_study_verdict_known CHECK (
        verdict IN ('certified', 'failed', 'inconclusive')
    ),
    CONSTRAINT simulator_fidelity_study_hash_lens CHECK (
        octet_length(policy_hash) = 32
        AND octet_length(artifact_hash) = 32
        AND octet_length(selection_cohort_hash) = 32
        AND octet_length(certification_cohort_hash) = 32
    ),
    CONSTRAINT simulator_fidelity_study_artifact_bounded CHECK (
        length(artifact_json) BETWEEN 2 AND 1048576
    ),
    CONSTRAINT simulator_fidelity_study_units_positive CHECK (
        selection_cohort_units > 0 AND certification_cohort_units > 0
    ),
    CONSTRAINT simulator_fidelity_study_spend_non_negative CHECK (
        budget_micro_usd >= 0 AND spent_micro_usd >= 0
    ),
    CONSTRAINT simulator_fidelity_study_authorization_bounded CHECK (
        length(human_data_authorization_id) BETWEEN 1 AND 128
    ),
    CONSTRAINT simulator_fidelity_study_cohorts_distinct CHECK (
        selection_cohort_id <> certification_cohort_id
        AND selection_cohort_hash <> certification_cohort_hash
    ),
    CONSTRAINT simulator_fidelity_study_policy_fkey
        FOREIGN KEY (policy_uid, policy_revision, storage_partition_id)
        REFERENCES moa.simulator_policy (policy_uid, revision, storage_partition_id)
        ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS simulator_fidelity_study_policy_idx
    ON moa.simulator_fidelity_study (
        storage_partition_id, policy_uid, policy_revision, observed_at
    );

CREATE OR REPLACE FUNCTION moa.simulator_fidelity_study_immutable_guard() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION
        'fidelity study records are immutable (study=%, policy=%)',
        OLD.study_uid, OLD.policy_uid
        USING ERRCODE = 'P0001';
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS simulator_fidelity_study_no_update ON moa.simulator_fidelity_study;
CREATE TRIGGER simulator_fidelity_study_no_update
    BEFORE UPDATE ON moa.simulator_fidelity_study
    FOR EACH ROW EXECUTE FUNCTION moa.simulator_fidelity_study_immutable_guard();

ALTER TABLE moa.experiment_run
    ADD COLUMN IF NOT EXISTS simulator_policy JSONB;

SELECT moa.apply_three_tier_rls('moa.simulator_policy'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.simulator_fidelity_study'::REGCLASS);
