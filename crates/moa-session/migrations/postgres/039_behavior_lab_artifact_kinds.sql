ALTER TABLE moa.artifact
    DROP CONSTRAINT IF EXISTS artifact_kind_check;

DELETE FROM moa.artifact
WHERE kind IN (
    'simulation_persona',
    'simulation_profile',
    'simulation_data_bundle',
    'simulation_scenario'
);

ALTER TABLE moa.artifact
    ADD CONSTRAINT artifact_kind_check CHECK (
        kind IN (
            'skill',
            'connector',
            'workflow',
            'action',
            'experiment_plan'
        )
    );
