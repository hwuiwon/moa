DROP INDEX IF EXISTS idx_agent_templates_creator;
DROP INDEX IF EXISTS idx_agent_templates_tenant;
DROP INDEX IF EXISTS idx_agents_operator;
DROP INDEX IF EXISTS idx_agents_tenant;

ALTER TABLE agents
    DROP CONSTRAINT IF EXISTS agents_status_check,
    DROP COLUMN IF EXISTS deactivated_reason,
    DROP COLUMN IF EXISTS deactivated_at,
    DROP COLUMN IF EXISTS status,
    DROP COLUMN IF EXISTS display_name,
    DROP COLUMN IF EXISTS template_id;

DROP TABLE IF EXISTS agent_templates;
