-- Provider-native selected source state for linked knowledge connections.

ALTER TABLE moa.knowledge_connections
    ADD COLUMN IF NOT EXISTS source_selection JSONB NOT NULL DEFAULT '{}'::JSONB;
