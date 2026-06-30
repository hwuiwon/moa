-- Restore append-only privileges for graph changelog after tenant-boundary RLS grants.

REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM PUBLIC;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_app;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_promoter;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_auditor;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_owner;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_replicator;

GRANT SELECT, INSERT ON moa.graph_changelog TO moa_app;
GRANT SELECT, INSERT ON moa.graph_changelog TO moa_promoter;
GRANT SELECT ON moa.graph_changelog TO moa_auditor;
GRANT SELECT ON moa.graph_changelog TO moa_replicator;
GRANT USAGE, SELECT ON SEQUENCE moa.graph_changelog_change_id_seq TO moa_app, moa_promoter;
