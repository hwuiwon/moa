-- M27: Workspace vector-backend promotion state lookup.
CREATE INDEX IF NOT EXISTS workspace_state_dual_read_idx
    ON moa.workspace_state (vector_backend_state)
    WHERE vector_backend_state != 'steady';
