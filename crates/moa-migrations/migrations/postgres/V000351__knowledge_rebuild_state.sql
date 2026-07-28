-- Durable state for storage-partition index rebuilds (re-embed and rechunk).
--
-- Upgrading an embedder, changing the chunker, or recovering from index
-- corruption previously had no durable home. An operator ran a bespoke script:
-- no resumable progress, no way to see how far it had gone, no validation
-- before the new vectors became authoritative, and no way back if they were
-- worse. A crash mid-run left a partition half in one embedding space and half
-- in another, which retrieval cannot detect because every row carries the same
-- 1024-dimension shape regardless of which model produced it.
--
-- This migration gives a rebuild the same durability guarantees the rest of the
-- platform has:
--
--   * A rebuild is an *operation* against one storage partition. At most one
--     nonterminal operation exists per partition, enforced by a partial unique
--     index rather than an application check, so two concurrent starts cannot
--     both believe they own the partition.
--   * Every transition is compare-and-swap on `fence_token`. A replayed Restate
--     step, a duplicated retry, or a second worker that inherited a stale view
--     loses the swap and observes it, instead of silently overwriting the
--     winner's progress.
--   * Candidate vectors are written to their OWN table, never into
--     `moa.embeddings`. This is a structural guarantee, not a filter: a
--     production reader that forgets a predicate still cannot see a candidate
--     row, because the row is not in the table it reads.
--   * A generation records its own embedding identity (model, version,
--     dimension) and its own Turbopuffer namespace. Activation is a
--     compare-and-swap on a single pointer row, so the flip is atomic and the
--     prior generation stays intact for rollback until finalization retires it.
--   * Rechunk stages every member it must activate atomically -- chunks, graph
--     deltas, embeddings, ACL snapshots, occurrence identity, and provenance --
--     and cannot activate until all six are staged. A partially staged rechunk
--     is refused rather than applied.
--
-- Estimated cost is exactly that. The embedding provider trait exposes no
-- billed usage, so these columns are named `estimated_` and are derived
-- deterministically from input token counts. Nothing here is a bill.

-- ---------------------------------------------------------------------------
-- Rebuild operations
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS moa.knowledge_rebuild_operation (
    operation_uid            UUID        NOT NULL PRIMARY KEY,
    tenant_id                UUID        NOT NULL,
    storage_partition_id     TEXT        NOT NULL,
    kind                     TEXT        NOT NULL,
    lifecycle                TEXT        NOT NULL,

    -- Owner identity and the compare-and-swap fence. `owner_token` names the
    -- workflow execution that currently owns the operation; `fence_token`
    -- advances on every accepted transition. A caller presents the fence it
    -- believes is current, and a mismatched swap affects zero rows.
    owner_token              UUID        NOT NULL,
    fence_token              BIGINT      NOT NULL DEFAULT 1,

    -- Candidate generation this operation is building, once one exists.
    candidate_generation_uid UUID,

    -- Durable keyset checkpoint. `checkpoint_uid` is the last source uid the
    -- build committed; a resumed run reads strictly greater uids, so a crash
    -- between batches cannot duplicate candidates.
    checkpoint_uid           UUID,
    checkpoint_batch_index   BIGINT      NOT NULL DEFAULT 0,

    -- Exact counts. `vectors_total` is the partition-wide census taken at
    -- planning time; the others accumulate as batches commit.
    vectors_total            BIGINT      NOT NULL DEFAULT 0,
    vectors_rebuilt          BIGINT      NOT NULL DEFAULT 0,
    vectors_failed           BIGINT      NOT NULL DEFAULT 0,

    -- Deterministic estimate derived from input token counts and a configured
    -- per-million-token rate. Never a billed figure.
    estimated_input_tokens   BIGINT      NOT NULL DEFAULT 0,
    estimated_cost_micros    BIGINT      NOT NULL DEFAULT 0,

    -- Provider interaction counters surfaced by status.
    provider_requests        BIGINT      NOT NULL DEFAULT 0,
    provider_throttles       BIGINT      NOT NULL DEFAULT 0,
    provider_retries         BIGINT      NOT NULL DEFAULT 0,

    -- Operator-safe failure surface. The code is a closed vocabulary and the
    -- message is a typed, payload-free summary; provider bodies, prompts, and
    -- document text never land here.
    last_error_code          TEXT,
    last_error_message       TEXT,

    cancel_requested_at      TIMESTAMPTZ,
    validated_at             TIMESTAMPTZ,
    activated_at             TIMESTAMPTZ,
    rolled_back_at           TIMESTAMPTZ,
    finalized_at             TIMESTAMPTZ,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT knowledge_rebuild_operation_kind
        CHECK (kind IN ('reembed', 'rechunk')),
    CONSTRAINT knowledge_rebuild_operation_lifecycle
        CHECK (lifecycle IN (
            'planning',
            'building',
            'validating',
            'awaiting_activation',
            'activated',
            'finalized',
            'rolled_back',
            'cancelled',
            'failed'
        )),
    CONSTRAINT knowledge_rebuild_operation_partition_present
        CHECK (storage_partition_id <> ''),
    CONSTRAINT knowledge_rebuild_operation_counts_non_negative
        CHECK (
            vectors_total >= 0
            AND vectors_rebuilt >= 0
            AND vectors_failed >= 0
            AND estimated_input_tokens >= 0
            AND estimated_cost_micros >= 0
            AND provider_requests >= 0
            AND provider_throttles >= 0
            AND provider_retries >= 0
        ),
    CONSTRAINT knowledge_rebuild_operation_fence_positive
        CHECK (fence_token > 0),
    -- A bounded error surface. Anything longer is a payload leak, not a summary.
    CONSTRAINT knowledge_rebuild_operation_error_message_bounded
        CHECK (last_error_message IS NULL OR length(last_error_message) <= 512)
);

-- At most one nonterminal operation per storage partition. Two concurrent
-- starts cannot both succeed: the loser's INSERT violates this index.
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_rebuild_operation_one_nonterminal
    ON moa.knowledge_rebuild_operation (storage_partition_id)
    WHERE lifecycle NOT IN ('finalized', 'rolled_back', 'cancelled', 'failed');

CREATE INDEX IF NOT EXISTS knowledge_rebuild_operation_tenant_idx
    ON moa.knowledge_rebuild_operation (tenant_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Generations
-- ---------------------------------------------------------------------------

-- One row per embedding generation of one storage partition. The bootstrap
-- generation for an existing partition is created on demand by the first
-- rebuild, so this table does not need to be backfilled.
CREATE TABLE IF NOT EXISTS moa.knowledge_rebuild_generation (
    generation_uid           UUID        NOT NULL PRIMARY KEY,
    tenant_id                UUID        NOT NULL,
    storage_partition_id     TEXT        NOT NULL,
    generation_seq           BIGINT      NOT NULL,
    -- NULL for the bootstrap generation adopted from pre-rebuild state.
    operation_uid            UUID        REFERENCES moa.knowledge_rebuild_operation(operation_uid)
                                             ON DELETE SET NULL,

    -- The generation's own embedding identity. Retrieval compares the query
    -- embedder against this, so a partition cannot serve a query embedded by a
    -- different model than the one that built its vectors.
    embedding_model          TEXT        NOT NULL,
    embedding_model_version  INT         NOT NULL,
    embedding_dimension      INT         NOT NULL,

    -- Generation-specific external namespace. Persisted rather than derived at
    -- call sites so activation is a pointer flip and two generations can never
    -- collide in one namespace.
    turbopuffer_namespace    TEXT        NOT NULL,

    state                    TEXT        NOT NULL,
    -- Set only when every source vector in the partition census has a candidate
    -- row. Activation refuses an incomplete generation.
    complete                 BOOLEAN     NOT NULL DEFAULT FALSE,
    vector_count             BIGINT      NOT NULL DEFAULT 0,
    -- Mean top-K overlap measured against the active generation by bounded
    -- shadow queries. NULL until validation runs.
    validation_overlap       DOUBLE PRECISION,

    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    activated_at             TIMESTAMPTZ,
    retired_at               TIMESTAMPTZ,

    CONSTRAINT knowledge_rebuild_generation_state
        CHECK (state IN ('candidate', 'active', 'retired')),
    CONSTRAINT knowledge_rebuild_generation_seq_positive
        CHECK (generation_seq > 0),
    CONSTRAINT knowledge_rebuild_generation_dimension_positive
        CHECK (embedding_dimension > 0),
    CONSTRAINT knowledge_rebuild_generation_model_present
        CHECK (embedding_model <> ''),
    CONSTRAINT knowledge_rebuild_generation_namespace_present
        CHECK (turbopuffer_namespace <> ''),
    CONSTRAINT knowledge_rebuild_generation_counts_non_negative
        CHECK (vector_count >= 0),
    CONSTRAINT knowledge_rebuild_generation_overlap_range
        CHECK (validation_overlap IS NULL
               OR (validation_overlap >= 0.0 AND validation_overlap <= 1.0)),
    CONSTRAINT knowledge_rebuild_generation_seq_unique
        UNIQUE (storage_partition_id, generation_seq),
    CONSTRAINT knowledge_rebuild_generation_namespace_unique
        UNIQUE (turbopuffer_namespace)
);

-- Exactly one active generation per partition, enforced by the index rather
-- than by whoever happens to be writing.
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_rebuild_generation_one_active
    ON moa.knowledge_rebuild_generation (storage_partition_id)
    WHERE state = 'active';

CREATE INDEX IF NOT EXISTS knowledge_rebuild_generation_operation_idx
    ON moa.knowledge_rebuild_generation (operation_uid)
    WHERE operation_uid IS NOT NULL;

ALTER TABLE moa.knowledge_rebuild_operation
    DROP CONSTRAINT IF EXISTS knowledge_rebuild_operation_candidate_generation_fk;
ALTER TABLE moa.knowledge_rebuild_operation
    ADD CONSTRAINT knowledge_rebuild_operation_candidate_generation_fk
    FOREIGN KEY (candidate_generation_uid)
    REFERENCES moa.knowledge_rebuild_generation(generation_uid)
    ON DELETE SET NULL;

-- ---------------------------------------------------------------------------
-- Active-generation pointer
-- ---------------------------------------------------------------------------

-- The single row production retrieval consults. Kept separate from the
-- generation rows so activation and rollback are one-row compare-and-swaps on
-- `pointer_version`, and so `previous_generation_uid` records exactly what a
-- rollback returns to rather than inferring it from timestamps.
CREATE TABLE IF NOT EXISTS moa.knowledge_active_generation (
    storage_partition_id     TEXT        NOT NULL PRIMARY KEY,
    tenant_id                UUID        NOT NULL,
    generation_uid           UUID        NOT NULL
                                 REFERENCES moa.knowledge_rebuild_generation(generation_uid),
    previous_generation_uid  UUID        REFERENCES moa.knowledge_rebuild_generation(generation_uid),
    pointer_version          BIGINT      NOT NULL DEFAULT 1,
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT knowledge_active_generation_version_positive
        CHECK (pointer_version > 0),
    CONSTRAINT knowledge_active_generation_distinct
        CHECK (previous_generation_uid IS NULL
               OR previous_generation_uid <> generation_uid)
);

-- ---------------------------------------------------------------------------
-- Candidate vectors
-- ---------------------------------------------------------------------------

-- Candidate embeddings live here and only here until activation. No production
-- reader joins this table, so a shadow hit cannot reach retrieval, ranking,
-- hydration, lineage, or citations even if a predicate is forgotten.
--
-- `input_digest` is the SHA-256 of the exact authoritative embedding input that
-- produced the vector. It is what makes "we rebuilt from the real input" a
-- checkable claim rather than an assertion: activation compares digests, and a
-- row whose provenance could not be reconstructed never gets written at all.
CREATE TABLE IF NOT EXISTS moa.knowledge_rebuild_candidate_vector (
    -- Deliberately NOT `ON DELETE CASCADE`. A cascade here would silently cover
    -- for any caller that forgot to remove candidate vectors before removing
    -- their generation -- including tenant purge, whose explicit DELETE would
    -- then be unfalsifiable: neutering it changes nothing observable because
    -- the cascade removes the same rows moments later. Without the cascade the
    -- explicit delete is load-bearing, a forgotten one fails loudly on the
    -- foreign key instead of quietly orphaning tenant embedding material, and
    -- the purge residue assertion can actually prove the step does something.
    generation_uid           UUID        NOT NULL
                                 REFERENCES moa.knowledge_rebuild_generation(generation_uid),
    uid                      UUID        NOT NULL,
    tenant_id                UUID        NOT NULL,
    storage_partition_id     TEXT        NOT NULL,
    user_id                  TEXT,
    label                    TEXT        NOT NULL,
    pii_class                TEXT        NOT NULL DEFAULT 'none',
    embedding                public.halfvec(1024) NOT NULL,
    input_digest             BYTEA       NOT NULL,
    input_token_estimate     INT         NOT NULL DEFAULT 0,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (generation_uid, uid),
    CONSTRAINT knowledge_rebuild_candidate_vector_label
        CHECK (label = ANY(moa.graph_node_labels())),
    CONSTRAINT knowledge_rebuild_candidate_vector_pii_class
        CHECK (pii_class IN ('none', 'pii', 'phi', 'restricted')),
    CONSTRAINT knowledge_rebuild_candidate_vector_digest_shape
        CHECK (octet_length(input_digest) = 32),
    CONSTRAINT knowledge_rebuild_candidate_vector_tokens_non_negative
        CHECK (input_token_estimate >= 0)
);

CREATE INDEX IF NOT EXISTS knowledge_rebuild_candidate_vector_partition_idx
    ON moa.knowledge_rebuild_candidate_vector (storage_partition_id, generation_uid, uid);

-- HNSW over candidate vectors so bounded shadow validation queries do not
-- sequentially scan the candidate set. The index serves validation only; no
-- production query reaches this table.
CREATE INDEX IF NOT EXISTS knowledge_rebuild_candidate_vector_hnsw_idx
    ON moa.knowledge_rebuild_candidate_vector
    USING hnsw (embedding public.halfvec_cosine_ops)
    WITH (m = 16, ef_construction = 64);

-- ---------------------------------------------------------------------------
-- Rechunk staging
-- ---------------------------------------------------------------------------

-- Rechunk stages the complete replacement state for a document version before
-- any of it becomes visible. All six members must be present for a version
-- before that version can activate; `moa.knowledge_rechunk_staged_members`
-- names the required set so the completeness rule has one definition.
CREATE OR REPLACE FUNCTION moa.knowledge_rechunk_staged_members() RETURNS TEXT[]
LANGUAGE SQL IMMUTABLE
AS $$
    SELECT ARRAY[
        'chunk',
        'graph_delta',
        'embedding',
        'acl_snapshot',
        'occurrence_identity',
        'provenance'
    ]::TEXT[];
$$;

CREATE TABLE IF NOT EXISTS moa.knowledge_rechunk_staging (
    staging_uid              UUID        NOT NULL PRIMARY KEY,
    -- Same reasoning as the candidate-vector table: no cascade, so the explicit
    -- deletes that remove staged rechunk state are provable rather than
    -- redundant with FK wiring this table's owner does not control.
    generation_uid           UUID        NOT NULL
                                 REFERENCES moa.knowledge_rebuild_generation(generation_uid),
    tenant_id                UUID        NOT NULL,
    storage_partition_id     TEXT        NOT NULL,
    document_version_uid     UUID        NOT NULL,
    member                   TEXT        NOT NULL,
    -- Member payloads carry only keyed, non-identifying material. The
    -- `acl_snapshot` member stages `SourcePrincipalFingerprint` hex, never a
    -- provider principal: the fingerprint is the only ACL shape allowed to
    -- cross a durable boundary.
    payload                  JSONB       NOT NULL,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT knowledge_rechunk_staging_member
        CHECK (member = ANY(moa.knowledge_rechunk_staged_members())),
    CONSTRAINT knowledge_rechunk_staging_member_unique
        UNIQUE (generation_uid, document_version_uid, member)
);

CREATE INDEX IF NOT EXISTS knowledge_rechunk_staging_generation_idx
    ON moa.knowledge_rechunk_staging (generation_uid, document_version_uid);

-- ---------------------------------------------------------------------------
-- Row-level security
-- ---------------------------------------------------------------------------

ALTER TABLE moa.knowledge_rebuild_operation ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_rebuild_operation FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON moa.knowledge_rebuild_operation;
CREATE POLICY tenant_isolation ON moa.knowledge_rebuild_operation FOR ALL TO moa_app
    USING (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''))
    WITH CHECK (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''));
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.knowledge_rebuild_operation TO moa_app;

ALTER TABLE moa.knowledge_rebuild_generation ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_rebuild_generation FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON moa.knowledge_rebuild_generation;
CREATE POLICY tenant_isolation ON moa.knowledge_rebuild_generation FOR ALL TO moa_app
    USING (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''))
    WITH CHECK (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''));
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.knowledge_rebuild_generation TO moa_app;

ALTER TABLE moa.knowledge_active_generation ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_active_generation FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON moa.knowledge_active_generation;
CREATE POLICY tenant_isolation ON moa.knowledge_active_generation FOR ALL TO moa_app
    USING (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''))
    WITH CHECK (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''));
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.knowledge_active_generation TO moa_app;

ALTER TABLE moa.knowledge_rebuild_candidate_vector ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_rebuild_candidate_vector FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON moa.knowledge_rebuild_candidate_vector;
CREATE POLICY tenant_isolation ON moa.knowledge_rebuild_candidate_vector FOR ALL TO moa_app
    USING (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''))
    WITH CHECK (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''));
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.knowledge_rebuild_candidate_vector TO moa_app;

ALTER TABLE moa.knowledge_rechunk_staging ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_rechunk_staging FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON moa.knowledge_rechunk_staging;
CREATE POLICY tenant_isolation ON moa.knowledge_rechunk_staging FOR ALL TO moa_app
    USING (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''))
    WITH CHECK (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''));
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.knowledge_rechunk_staging TO moa_app;

GRANT USAGE ON SCHEMA moa TO moa_app;
