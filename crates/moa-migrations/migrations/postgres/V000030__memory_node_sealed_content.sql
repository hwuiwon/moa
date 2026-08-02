-- One authoritative sealed payload for graph-memory content.
--
-- Restricted/PHI rows keep only fixed placeholders in the generated full-text
-- columns. The real versioned `{name, properties}` document is stored in one
-- moa-crypto envelope ciphertext bound to `(tenant_id, data_subject_id, uid,
-- pii_class)`. `data_subject_id` is explicit for every row: contact-owned data
-- uses the contact UUID and tenant-owned data uses the tenant UUID.
--
-- This fresh-only migration chain installs the final sealed-content invariants
-- immediately.

ALTER TABLE moa.node_index
    ADD COLUMN IF NOT EXISTS content_sealed BYTEA,
    ADD COLUMN IF NOT EXISTS data_subject_id UUID,
    ADD COLUMN IF NOT EXISTS base_confidence DOUBLE PRECISION;

COMMENT ON COLUMN moa.node_index.content_sealed IS
    'One moa-crypto ciphertext containing versioned {name, properties} content for restricted/phi rows; NULL for other classifications.';
COMMENT ON COLUMN moa.node_index.data_subject_id IS
    'Authoritative typed encryption/erasure subject: contact_id for contact-owned rows, tenant_id for tenant-owned rows.';
COMMENT ON COLUMN moa.node_index.base_confidence IS
    'Authoritative confidence-decay anchor; derived ranking metadata, never part of sealed content.';

ALTER TABLE moa.node_index
    ADD CONSTRAINT node_index_data_subject_required
        CHECK (data_subject_id IS NOT NULL),
    ADD CONSTRAINT node_index_data_subject_scope
        CHECK (
            data_subject_id = CASE
                WHEN contact_id IS NOT NULL THEN contact_id
                ELSE tenant_id
            END
        ),
    ADD CONSTRAINT node_index_sealed_content_state
        CHECK (
            (
                pii_class IN ('phi', 'restricted')
                AND data_subject_id IS NOT NULL
                AND name = '[RESTRICTED]'
                AND properties_summary = '{"redacted": true}'::JSONB
                AND content_sealed IS NOT NULL
                AND octet_length(content_sealed) > 0
            )
            OR
            (
                pii_class NOT IN ('phi', 'restricted')
                AND content_sealed IS NULL
            )
        ),
    ADD CONSTRAINT node_index_base_confidence_valid
        CHECK (
            base_confidence IS NULL
            OR (base_confidence >= 0.0 AND base_confidence <= 1.0)
        );

CREATE INDEX IF NOT EXISTS node_index_data_subject_idx
    ON moa.node_index (tenant_id, data_subject_id, uid);

-- Restricted content must never have a semantic projection. The CHECK rejects
-- honest restricted/PHI rows, and the trigger also rejects attempts to attach a
-- falsely reclassified embedding to a sealed node.
ALTER TABLE moa.embeddings
    ADD CONSTRAINT embeddings_unsealed_content_only
        CHECK (pii_class NOT IN ('phi', 'restricted'));

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'moa_embedding_guard') THEN
        CREATE ROLE moa_embedding_guard NOLOGIN NOBYPASSRLS;
    END IF;
    ALTER ROLE moa_embedding_guard NOLOGIN NOBYPASSRLS;
    EXECUTE format('GRANT moa_embedding_guard TO %I', current_user);
END
$$;
GRANT USAGE ON SCHEMA moa TO moa_embedding_guard;
GRANT SELECT ON moa.node_index TO moa_embedding_guard;
CREATE POLICY embedding_guard_read ON moa.node_index
    FOR SELECT TO moa_embedding_guard
    USING (true);

CREATE OR REPLACE FUNCTION moa.reject_sealed_node_embedding()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF NEW.pii_class IN ('phi', 'restricted')
       OR EXISTS (
            SELECT 1
            FROM moa.node_index AS node
            WHERE node.uid = NEW.uid
              AND node.pii_class IN ('phi', 'restricted')
       ) THEN
        RAISE EXCEPTION 'restricted/PHI graph nodes cannot have embeddings'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
ALTER FUNCTION moa.reject_sealed_node_embedding() OWNER TO moa_embedding_guard;
REVOKE ALL ON FUNCTION moa.reject_sealed_node_embedding() FROM PUBLIC;

CREATE TRIGGER embeddings_reject_sealed_node
    BEFORE INSERT OR UPDATE OF uid, storage_partition_id, pii_class ON moa.embeddings
    FOR EACH ROW
    EXECUTE FUNCTION moa.reject_sealed_node_embedding();
