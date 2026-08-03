-- Certified simulator policies consumed by production Behavior Lab trials.

CREATE TABLE IF NOT EXISTS moa.simulator_policy (
    policy_uid UUID NOT NULL,
    revision INTEGER NOT NULL,
    -- NULL is reserved for platform-owned policies. Tenant policy authors cannot
    -- write global rows, and exact global identity wins resolution over a tenant
    -- row with the same UUID/revision.
    storage_partition_id TEXT,
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
    UNIQUE NULLS NOT DISTINCT (policy_uid, revision, storage_partition_id),
    CHECK (scope IS NOT NULL),
    CONSTRAINT simulator_policy_scope CHECK (user_id IS NULL),
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

CREATE INDEX IF NOT EXISTS simulator_policy_storage_partition_idx
    ON moa.simulator_policy (storage_partition_id);

CREATE OR REPLACE FUNCTION moa.simulator_policy_pins_are_immutable() RETURNS trigger AS $$
BEGIN
    IF NEW.policy_uid IS DISTINCT FROM OLD.policy_uid
       OR NEW.revision IS DISTINCT FROM OLD.revision
       OR NEW.storage_partition_id IS DISTINCT FROM OLD.storage_partition_id
       OR NEW.domain IS DISTINCT FROM OLD.domain
       OR NEW.policy_hash IS DISTINCT FROM OLD.policy_hash
       OR NEW.components::TEXT IS DISTINCT FROM OLD.components::TEXT
       OR NEW.valid_from IS DISTINCT FROM OLD.valid_from
       OR NEW.valid_until IS DISTINCT FROM OLD.valid_until
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

CREATE TRIGGER simulator_policy_pins_immutable
    BEFORE UPDATE ON moa.simulator_policy
    FOR EACH ROW EXECUTE FUNCTION moa.simulator_policy_pins_are_immutable();

-- The release gate is a platform control, so its simulator definition must
-- exist on a clean migrated deployment and must not be replaceable by
-- tenant-authored policy bytes. It intentionally starts draft. The migration
-- does not invent a mandate, human authorization, evidence source, or passing
-- study; operators must provision those independent records from real evidence
-- before release submission can resolve this policy.
INSERT INTO moa.simulator_policy (
    policy_uid, revision, storage_partition_id, user_id, domain, policy_hash,
    components, state, valid_from, valid_until
)
VALUES (
    '00000000-0000-4000-8000-0000000d75f1',
    1,
    NULL,
    NULL,
    'artifact-release',
    decode('431821acccbf2fbeae626343d60915638e7372834d19aed7b39b3a5db32b170f', 'hex'),
    '{
      "domain":"artifact-release",
      "model":"gpt-5.4",
      "provider":"openai",
      "decoding":{"temperature_milli":200,"max_output_tokens":512,"seeded":true},
      "system_prompt":"You are the simulated user in a Behavior Lab trial. Follow only the supplied persona, profile, scenario, and data-bundle context. Never call tools or claim to have changed external state. Return one structured simulator-turn response. Set message to the next user turn only for continue; set it to an empty string for terminal decisions.",
      "protocol":{"id":"moa.behavior_lab.simulator_turn","version":1,"schema_hash":"7865a9b6629d915365f3c51d83b56f0a39e8fbcb918889fefa9365e3a9527fae"},
      "context_contract_hash":"4dc4b941df22f89c62f8bfb3c88964ebe1e6be18068d10b4fa140ea634f1a30c",
      "calibration_cohort":{"cohort_id":"platform-release-synthetic-v1","independent_units":100,"content_hash":"5151515151515151515151515151515151515151515151515151515151515151","consent_basis":"authorized_internal_dogfood","deidentification":"synthetic_surrogate"},
      "validity":{"valid_from":"2025-01-01T00:00:00Z","valid_until":"2035-01-01T00:00:00Z"}
    }'::JSONB,
    'draft',
    '2025-01-01T00:00:00Z',
    '2035-01-01T00:00:00Z'
)
ON CONFLICT (policy_uid, revision, storage_partition_id) DO NOTHING;

-- The pre-study authority for one platform certification. This record is
-- deliberately separate from the submitted study: a study author cannot pick
-- easier bounds, another cohort, a larger budget, or a convenient human-data
-- authorization and then certify those choices with the same artifact.
CREATE TABLE IF NOT EXISTS moa.simulator_certification_mandate (
    mandate_uid UUID PRIMARY KEY,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (
        moa.compute_scope_tier(storage_partition_id, user_id)
    ) STORED,
    policy_uid UUID NOT NULL,
    policy_revision INTEGER NOT NULL,
    policy_hash BYTEA NOT NULL,
    domain TEXT NOT NULL,
    bounds JSONB NOT NULL,
    selection_cohort JSONB NOT NULL,
    certification_cohort JSONB NOT NULL,
    label_protocol JSONB NOT NULL,
    human_data_authorization JSONB NOT NULL,
    study_budget_micro_usd BIGINT NOT NULL,
    required_source_manifest_hash BYTEA NOT NULL,
    study_window_from TIMESTAMPTZ NOT NULL,
    study_window_until TIMESTAMPTZ NOT NULL,
    predeclared_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (policy_uid, policy_revision),
    CHECK (scope = 'global'),
    CONSTRAINT simulator_certification_mandate_global CHECK (
        storage_partition_id IS NULL AND user_id IS NULL
    ),
    CONSTRAINT simulator_certification_mandate_revision_positive CHECK (
        policy_revision >= 1
    ),
    CONSTRAINT simulator_certification_mandate_domain_bounded CHECK (
        length(domain) BETWEEN 1 AND 64
    ),
    CONSTRAINT simulator_certification_mandate_hash_lens CHECK (
        octet_length(policy_hash) = 32
        AND octet_length(required_source_manifest_hash) = 32
    ),
    CONSTRAINT simulator_certification_mandate_budget_positive CHECK (
        study_budget_micro_usd > 0
    ),
    CONSTRAINT simulator_certification_mandate_window_non_empty CHECK (
        study_window_until > study_window_from
    )
);

CREATE OR REPLACE FUNCTION moa.reject_simulator_certification_mutation()
RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION
        '% rows are immutable', TG_TABLE_NAME
        USING ERRCODE = 'P0001';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER simulator_certification_mandate_no_update
    BEFORE UPDATE ON moa.simulator_certification_mandate
    FOR EACH ROW EXECUTE FUNCTION moa.reject_simulator_certification_mutation();

-- Independent platform authority is migration-owned, not study-importer-owned.
-- This first revision is deliberately UNPROVISIONED: its zero certification
-- cohort and source-manifest digests are rejected by the runtime store. A later
-- reviewed migration must introduce a new policy revision and fixed mandate
-- with real cohort/source digests before certification can succeed. Keeping the
-- row here makes the absent operational evidence explicit without fabricating a
-- passing study or letting the promoter choose its own bounds. Provisioning real
-- authority therefore requires a reviewed code-and-migration revision, not an
-- operator update to this row.
INSERT INTO moa.simulator_certification_mandate (
    mandate_uid, storage_partition_id, user_id, policy_uid, policy_revision,
    policy_hash, domain, bounds, selection_cohort, certification_cohort,
    label_protocol, human_data_authorization, study_budget_micro_usd,
    required_source_manifest_hash, study_window_from, study_window_until,
    predeclared_at
)
VALUES (
    '00000000-0000-4000-8000-0000000d75f2',
    NULL,
    NULL,
    '00000000-0000-4000-8000-0000000d75f1',
    1,
    decode('431821acccbf2fbeae626343d60915638e7372834d19aed7b39b3a5db32b170f', 'hex'),
    'artifact-release',
    '{
      "domain":"artifact-release",
      "independent_unit":"human_participant",
      "minimum_support":{
        "selection_units":100,
        "certification_units":120,
        "per_critical_class_units":100,
        "treatment_effect_units_per_arm":150,
        "per_slice_units":40,
        "power_analysis":{
          "analysis_id":"UNPROVISIONED",
          "analysis_hash":"0000000000000000000000000000000000000000000000000000000000000000",
          "detectable_effect_micro":50000,
          "power_permille":800
        }
      },
      "class_confidence_permille":950,
      "critical_classes":[{
        "class":"release_success",
        "min_sensitivity_lower_bound_permille":800,
        "min_specificity_lower_bound_permille":850
      }],
      "effect_equivalence":{
        "margin_micro":50000,
        "method":{"method":"cluster_bootstrap_percentile","resamples":2000,"seed":7},
        "confidence_permille":950
      },
      "max_slice_disagreement_permille":100,
      "recertification_interval_days":90
    }'::JSONB,
    '{
      "cohort_id":"platform-release-synthetic-v1",
      "independent_units":100,
      "content_hash":"5151515151515151515151515151515151515151515151515151515151515151",
      "consent_basis":"authorized_internal_dogfood",
      "deidentification":"synthetic_surrogate"
    }'::JSONB,
    '{
      "cohort_id":"UNPROVISIONED",
      "independent_units":120,
      "content_hash":"0000000000000000000000000000000000000000000000000000000000000000",
      "consent_basis":"explicit_participant_consent",
      "deidentification":"pseudonymized_and_redacted"
    }'::JSONB,
    '{
      "protocol_id":"UNPROVISIONED",
      "version":1,
      "rubric_hash":"0000000000000000000000000000000000000000000000000000000000000000",
      "adjudication":"independent_with_adjudication",
      "annotators":2
    }'::JSONB,
    '{
      "authorization_id":"UNPROVISIONED",
      "approved_by":"UNPROVISIONED",
      "approved_at":"2025-01-01T00:00:00Z",
      "expires_at":"2035-01-01T00:00:00Z"
    }'::JSONB,
    5000000,
    decode(repeat('00', 32), 'hex'),
    '2025-01-01T00:00:00Z',
    '2035-01-01T00:00:00Z',
    '2025-01-01T00:00:00Z'
)
ON CONFLICT (mandate_uid) DO NOTHING;

-- Post-study promoter import of the one canonical artifact reviewed against the
-- independently supplied source manifest. The store requires both hashes; a
-- caller-supplied aggregate artifact alone is never certification evidence.
CREATE TABLE IF NOT EXISTS moa.simulator_certification_evidence_import (
    mandate_uid UUID PRIMARY KEY
        REFERENCES moa.simulator_certification_mandate(mandate_uid) ON DELETE RESTRICT,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (
        moa.compute_scope_tier(storage_partition_id, user_id)
    ) STORED,
    study_uid UUID NOT NULL UNIQUE,
    study_artifact_hash BYTEA NOT NULL,
    source_manifest_hash BYTEA NOT NULL,
    source_reference TEXT NOT NULL,
    imported_by TEXT NOT NULL,
    imported_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope = 'global'),
    CONSTRAINT simulator_certification_evidence_import_global CHECK (
        storage_partition_id IS NULL AND user_id IS NULL
    ),
    CONSTRAINT simulator_certification_evidence_import_hash_lens CHECK (
        octet_length(study_artifact_hash) = 32
        AND octet_length(source_manifest_hash) = 32
    ),
    CONSTRAINT simulator_certification_evidence_import_source_bounded CHECK (
        length(source_reference) BETWEEN 1 AND 1024
        AND length(imported_by) BETWEEN 1 AND 128
    )
);

CREATE TRIGGER simulator_certification_evidence_import_no_update
    BEFORE UPDATE ON moa.simulator_certification_evidence_import
    FOR EACH ROW EXECUTE FUNCTION moa.reject_simulator_certification_mutation();

CREATE TABLE IF NOT EXISTS moa.simulator_fidelity_study (
    study_uid UUID NOT NULL,
    -- NULL only for operator-recorded evidence certifying a global policy.
    storage_partition_id TEXT,
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
    platform_mandate_uid UUID
        REFERENCES moa.simulator_certification_mandate(mandate_uid) ON DELETE RESTRICT,
    evidence_source_manifest_hash BYTEA,
    observed_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE NULLS NOT DISTINCT (study_uid, storage_partition_id),
    CHECK (scope IS NOT NULL),
    CONSTRAINT simulator_fidelity_study_scope CHECK (user_id IS NULL),
    CONSTRAINT simulator_fidelity_study_verdict_known CHECK (
        verdict IN ('certified', 'failed', 'inconclusive')
    ),
    CONSTRAINT simulator_fidelity_study_hash_lens CHECK (
        octet_length(policy_hash) = 32
        AND octet_length(artifact_hash) = 32
        AND octet_length(selection_cohort_hash) = 32
        AND octet_length(certification_cohort_hash) = 32
        AND (
            evidence_source_manifest_hash IS NULL
            OR octet_length(evidence_source_manifest_hash) = 32
        )
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
    CONSTRAINT simulator_fidelity_study_platform_authority_complete CHECK (
        (
            storage_partition_id IS NULL
            AND platform_mandate_uid IS NOT NULL
            AND evidence_source_manifest_hash IS NOT NULL
        ) OR (
            storage_partition_id IS NOT NULL
            AND platform_mandate_uid IS NULL
            AND evidence_source_manifest_hash IS NULL
        )
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

CREATE TRIGGER simulator_fidelity_study_no_update
    BEFORE UPDATE ON moa.simulator_fidelity_study
    FOR EACH ROW EXECUTE FUNCTION moa.reject_simulator_certification_mutation();

ALTER TABLE moa.experiment_run
    ADD COLUMN simulator_policy JSONB NOT NULL;

SELECT moa.apply_three_tier_rls('moa.simulator_policy'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.simulator_certification_mandate'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.simulator_certification_evidence_import'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.simulator_fidelity_study'::REGCLASS);

-- Platform authority records are append-only even for the promoter. A new
-- policy revision gets a new mandate and evidence import; old authority never
-- changes underneath an audit record.
REVOKE UPDATE, DELETE, TRUNCATE ON moa.simulator_certification_mandate
    FROM moa_app, moa_promoter;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.simulator_certification_evidence_import
    FROM moa_app, moa_promoter;
