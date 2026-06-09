CREATE EXTENSION IF NOT EXISTS pgcrypto;

ALTER TABLE moa.skill
    ADD COLUMN IF NOT EXISTS package_hash BYTEA,
    ADD COLUMN IF NOT EXISTS skill_md_hash BYTEA,
    ADD COLUMN IF NOT EXISTS file_count INT,
    ADD COLUMN IF NOT EXISTS total_size_bytes BIGINT,
    ADD COLUMN IF NOT EXISTS manifest JSONB;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'moa'
          AND table_name = 'skill'
          AND column_name = 'body'
    ) THEN
        UPDATE moa.skill
        SET
            description = COALESCE(description, name),
            tags = COALESCE(tags, ARRAY[]::TEXT[]),
            skill_md_hash = COALESCE(skill_md_hash, body_hash),
            package_hash = COALESCE(package_hash, body_hash),
            file_count = COALESCE(file_count, 1),
            total_size_bytes = COALESCE(total_size_bytes, octet_length(convert_to(body, 'UTF8'))),
            manifest = COALESCE(
                manifest,
                jsonb_build_object(
                    'schema_version', 1,
                    'skill_md_path', 'SKILL.md',
                    'skill_md_estimated_tokens', 1,
                    'allowed_tools', '[]'::jsonb,
                    'use_count', 0,
                    'last_used', NULL,
                    'success_rate', 1.0,
                    'auto_generated', false,
                    'files', jsonb_build_array(jsonb_build_object(
                        'path', 'SKILL.md',
                        'size_bytes', octet_length(convert_to(body, 'UTF8')),
                        'sha256', encode(body_hash, 'hex'),
                        'content_type', 'text/markdown; charset=utf-8',
                        'executable', false
                    ))
                )
            );
    ELSE
        UPDATE moa.skill
        SET
            description = COALESCE(description, name),
            tags = COALESCE(tags, ARRAY[]::TEXT[]);
    END IF;
END $$;

ALTER TABLE moa.skill
    ALTER COLUMN description SET NOT NULL,
    ALTER COLUMN package_hash SET NOT NULL,
    ALTER COLUMN skill_md_hash SET NOT NULL,
    ALTER COLUMN file_count SET NOT NULL,
    ALTER COLUMN total_size_bytes SET NOT NULL,
    ALTER COLUMN manifest SET NOT NULL,
    ALTER COLUMN tags SET DEFAULT ARRAY[]::TEXT[],
    ALTER COLUMN tags SET NOT NULL;

ALTER TABLE moa.skill
    DROP CONSTRAINT IF EXISTS skill_file_count_check,
    ADD CONSTRAINT skill_file_count_check CHECK (file_count > 0);

ALTER TABLE moa.skill
    DROP CONSTRAINT IF EXISTS skill_total_size_bytes_check,
    ADD CONSTRAINT skill_total_size_bytes_check CHECK (total_size_bytes >= 0);

DROP INDEX IF EXISTS skill_active_name_body_hash_uniq;

CREATE UNIQUE INDEX IF NOT EXISTS skill_active_name_package_hash_uniq
    ON moa.skill (
        coalesce(workspace_id, ''),
        coalesce(user_id, ''),
        name,
        package_hash
    )
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS skill_package_hash_idx
    ON moa.skill (package_hash);

CREATE TABLE IF NOT EXISTS moa.skill_file (
    file_uid UUID PRIMARY KEY,
    skill_uid UUID NOT NULL REFERENCES moa.skill(skill_uid) ON DELETE CASCADE,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    path TEXT NOT NULL,
    content BYTEA NOT NULL,
    content_sha256 BYTEA NOT NULL,
    content_type TEXT,
    executable BOOLEAN NOT NULL DEFAULT false,
    file_size_bytes BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (path <> ''),
    CHECK (file_size_bytes >= 0)
);

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'moa'
          AND table_name = 'skill'
          AND column_name = 'body'
    ) THEN
        INSERT INTO moa.skill_file (
            file_uid, skill_uid, workspace_id, user_id, path, content,
            content_sha256, content_type, executable, file_size_bytes
        )
        SELECT
            gen_random_uuid(),
            skill_uid,
            workspace_id,
            user_id,
            'SKILL.md',
            convert_to(body, 'UTF8'),
            body_hash,
            'text/markdown; charset=utf-8',
            false,
            octet_length(convert_to(body, 'UTF8'))
        FROM moa.skill
        WHERE NOT EXISTS (
            SELECT 1
            FROM moa.skill_file f
            WHERE f.skill_uid = moa.skill.skill_uid
              AND f.path = 'SKILL.md'
        );
    END IF;
END $$;

ALTER TABLE moa.skill
    ALTER COLUMN body DROP NOT NULL,
    ALTER COLUMN body_hash DROP NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS skill_file_skill_path_uniq
    ON moa.skill_file (skill_uid, path);

CREATE INDEX IF NOT EXISTS skill_file_scope_idx
    ON moa.skill_file (workspace_id, scope, user_id);

CREATE INDEX IF NOT EXISTS skill_file_skill_idx
    ON moa.skill_file (skill_uid);

SELECT moa.apply_three_tier_rls('moa.skill_file'::REGCLASS);

DROP POLICY IF EXISTS rd_auditor ON moa.skill_file;
CREATE POLICY rd_auditor ON moa.skill_file
    FOR SELECT TO moa_auditor
    USING (true);

GRANT SELECT ON moa.skill_file TO moa_auditor;
