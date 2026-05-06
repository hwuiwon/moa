-- M26: Turbopuffer is an opt-in vector backend.
--
-- `moa.workspace_state.vector_backend` was introduced in 015_graph_changelog.sql
-- with CHECK (vector_backend IN ('pgvector', 'turbopuffer')). This migration is
-- intentionally schema-neutral and documents that M26 uses the existing column.
SELECT 1;
