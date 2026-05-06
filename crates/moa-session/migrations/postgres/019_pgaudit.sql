CREATE EXTENSION IF NOT EXISTS pgaudit;

DO $$
BEGIN
    EXECUTE 'SECURITY LABEL FOR pgaudit ON TABLE moa.node_index IS ''READ, WRITE''';
    EXECUTE 'SECURITY LABEL FOR pgaudit ON TABLE moa.embeddings IS ''READ, WRITE''';
    EXECUTE 'SECURITY LABEL FOR pgaudit ON TABLE moa.graph_changelog IS ''READ, WRITE''';
EXCEPTION
    WHEN others THEN
        RAISE NOTICE
            'pgaudit SECURITY LABELs skipped: %',
            SQLERRM;
END $$;

GRANT USAGE ON SCHEMA moa TO moa_auditor;
GRANT SELECT ON moa.graph_changelog TO moa_auditor;
GRANT SELECT ON moa.node_index TO moa_auditor;
GRANT SELECT ON moa.embeddings TO moa_auditor;

CREATE OR REPLACE VIEW moa.audit_logs AS
SELECT *
FROM moa.graph_changelog
ORDER BY created_at DESC;

GRANT SELECT ON moa.audit_logs TO moa_auditor;
