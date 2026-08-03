-- Move the fixed constrained-HTTP destination out of generic JSON and into a
-- schema-enforced column. Managed Nango/Merge parents intentionally have no
-- HTTP origin because they expose no connector actions.

ALTER TABLE moa.connector_connections
    ADD COLUMN origin TEXT;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.connector_connections
        WHERE artifact_uid IS NOT NULL
          AND (
              NOT (non_secret_config ? 'origin')
              OR jsonb_typeof(non_secret_config -> 'origin') <> 'string'
              OR octet_length(non_secret_config ->> 'origin') NOT BETWEEN 1 AND 2048
              OR non_secret_config ->> 'origin' <> lower(non_secret_config ->> 'origin')
              OR non_secret_config ->> 'origin'
                    !~ '^https?://([a-z0-9](?:[a-z0-9.-]*[a-z0-9])?|\[[0-9a-f:.]+\])(?::[1-9][0-9]{0,4})?$'
              OR (non_secret_config ->> 'origin') ~ '^http://[^/]+:80$'
              OR (non_secret_config ->> 'origin') ~ '^https://[^/]+:443$'
              OR COALESCE(
                    substring(non_secret_config ->> 'origin' FROM ':([0-9]+)$')::INTEGER,
                    1
                 ) > 65535
          )
    ) THEN
        RAISE EXCEPTION 'artifact connector origin is missing or noncanonical'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'connector_connections_origin_canonical';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM moa.connector_connections
        WHERE built_in_key IS NOT NULL
          AND (
              built_in_key NOT IN ('knowledge:nango', 'knowledge:merge')
              OR built_in_version <> 1
              OR non_secret_config ? 'origin'
          )
    ) THEN
        RAISE EXCEPTION 'managed connector parent origin is inconsistent'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'connector_connections_definition_origin_consistent';
    END IF;
END
$$;

UPDATE moa.connector_connections
SET origin = non_secret_config ->> 'origin',
    non_secret_config = non_secret_config - 'origin'
WHERE artifact_uid IS NOT NULL;

ALTER TABLE moa.connector_connections
    ADD CONSTRAINT connector_connections_origin_canonical CHECK (
        origin IS NULL
        OR (
            octet_length(origin) BETWEEN 1 AND 2048
            AND origin = lower(origin)
            AND origin ~ '^https?://([a-z0-9](?:[a-z0-9.-]*[a-z0-9])?|\[[0-9a-f:.]+\])(?::[1-9][0-9]{0,4})?$'
            AND origin !~ '^http://[^/]+:80$'
            AND origin !~ '^https://[^/]+:443$'
            AND COALESCE(substring(origin FROM ':([0-9]+)$')::INTEGER, 1) <= 65535
        )
    ),
    ADD CONSTRAINT connector_connections_definition_origin_consistent CHECK (
        (
            artifact_uid IS NOT NULL
            AND revision_uid IS NOT NULL
            AND origin IS NOT NULL
        )
        OR
        (
            artifact_uid IS NULL
            AND revision_uid IS NULL
            AND built_in_key IN ('knowledge:nango', 'knowledge:merge')
            AND built_in_version = 1
            AND origin IS NULL
        )
    );

COMMENT ON COLUMN moa.connector_connections.origin IS
    'Canonical fixed HTTP(S) origin for artifact-backed constrained-HTTP actions; NULL for closed managed knowledge parents.';
