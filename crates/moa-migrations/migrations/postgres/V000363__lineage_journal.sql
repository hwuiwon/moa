-- Durable acceptance queue for lineage rows that have not yet reached the row
-- store.
--
-- This table replaces the pod-local fjall journal. That journal made a promise
-- it could not keep: `record_durable_batch` returned "accepted" once bytes were
-- fsynced to a directory owned by one pod, so a rollout, an eviction, or a node
-- failure destroyed records the caller had already been told were durable. A
-- non-sticky multi-replica Deployment cannot recover them, because no other
-- replica can see that directory. Acceptance now means "committed to Postgres",
-- which every replica can see and any replica can finish.
--
-- The invariant this table carries: a row exists here from the moment the
-- writer's caller is told the batch was accepted until the moment its content
-- has been committed to the row store (or durably dead-lettered) in the SAME
-- transaction that removes it. There is no window in which a record is both
-- acknowledged and absent, and no path that removes a row without either
-- storing it or dead-lettering it.
--
-- Claim protocol. A replica claims the oldest eligible rows with
-- `ORDER BY claimable_at, journal_id ... FOR UPDATE SKIP LOCKED` and stamps an
-- expiring lease. If that replica dies mid-flight the lease simply expires and
-- the rows become claimable again; nothing needs to notice the death.
--
-- `claimable_at` is a GENERATED column rather than a maintained one on purpose.
-- A hand-maintained "next attempt" column can drift from the lease pair, and
-- the direction it drifts in is the dangerous one: a row whose lease was
-- extended but whose eligibility column was not becomes permanently invisible
-- to every claimant, which loses an accepted record silently and forever.
-- Deriving eligibility from the lease pair makes that state unrepresentable.
--
-- `journal_id` is a UUIDv7 minted at acceptance, so ordering by it is ordering
-- by acceptance time. Compliance hash chaining folds rows in claim order, and
-- claim order is therefore acceptance order within a partition.
--
-- This migration is the ONLY definition of the queue. An earlier revision also
-- installed it from `moa_migrations::ensure_lineage_schema`, on the theory that
-- a standalone lineage store might never run the central migration set. No such
-- store exists: every caller of that bootstrap reaches it on a database where
-- these migrations have already run - the orchestrator applies them at
-- `main.rs:175` before building the sink at `:201`, and the test template runs
-- `moa_migrations::run` before its own bootstrap. The second copy was therefore
-- unreachable, and a duplicate that nothing can execute is a duplicate that can
-- only drift.

CREATE SCHEMA IF NOT EXISTS analytics;

CREATE TABLE IF NOT EXISTS analytics.lineage_journal (
    -- UUIDv7 minted at acceptance: identity and FIFO order in one column.
    journal_id           UUID        PRIMARY KEY,
    -- Purge and subject-erasure scope. Tenant-scoped partitions are the tenant
    -- UUID as text, which is what the destruction fence joins against.
    storage_partition_id TEXT        NOT NULL,
    user_id              TEXT,
    -- 'lineage' or 'score': which row shape `payload` decodes to. Kept as a
    -- column so backlog metrics and dead-letter routing never parse JSON.
    event_class          TEXT        NOT NULL,
    -- The serialized pending row, byte-for-byte what the writer will store.
    payload              JSONB       NOT NULL,
    -- The acceptance boundary. Set by the inserting transaction; never updated.
    accepted_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Retry backoff floor. Moved forward when a recoverable write fails.
    available_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Number of claims so far. Bounds retry of a row that fails every time.
    attempts             INTEGER     NOT NULL DEFAULT 0,
    -- Expiring lease pair. Both set or both null; there is no half-leased row.
    lease_owner          UUID,
    lease_expires_at     TIMESTAMPTZ,
    -- Derived eligibility. See the note above: this is generated so it cannot
    -- disagree with the lease pair. GREATEST ignores NULL, so an unleased row
    -- is eligible at `available_at`.
    claimable_at         TIMESTAMPTZ
        GENERATED ALWAYS AS (GREATEST(available_at, lease_expires_at)) STORED,
    CONSTRAINT lineage_journal_lease_pair_check
        CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CONSTRAINT lineage_journal_attempts_check CHECK (attempts >= 0),
    CONSTRAINT lineage_journal_event_class_check
        CHECK (event_class IN ('lineage', 'score')),
    CONSTRAINT lineage_journal_partition_nonempty
        CHECK (storage_partition_id <> '')
);

-- The claim index. Every drain reads through exactly this index; it is stable
-- because `claimable_at` is generated rather than rewritten by claimants.
CREATE INDEX IF NOT EXISTS lineage_journal_claim_idx
    ON analytics.lineage_journal (claimable_at, journal_id);

-- Purge and subject erasure. The partition prefix serves tenant purge; the
-- trailing user column serves subject-scoped erasure without a second index.
CREATE INDEX IF NOT EXISTS lineage_journal_partition_idx
    ON analytics.lineage_journal (storage_partition_id, user_id);

COMMENT ON TABLE analytics.lineage_journal IS
    'Durable lineage acceptance queue. A row is present from acceptance until its content is committed to the row store or dead-lettered in the same transaction that removes it.';

-- Row-level security.
--
-- Only the internal runtime/background plane may touch the queue. A
-- tenant-scoped request connection has no legitimate reason to read pending
-- lineage payloads for any tenant, including its own, and the queue is
-- deliberately cross-tenant so one drain can batch across partitions.
--
-- NOTE for anything that deletes from this table (tenant purge, subject
-- erasure): it must run control-plane scoped. A non-control-plane connection
-- does not merely fail to delete - it sees zero rows, so a residue check run on
-- the same connection passes vacuously. This is the same hazard the tenant
-- purge repository already documents for the credential-vault tables.
ALTER TABLE analytics.lineage_journal ENABLE ROW LEVEL SECURITY;
ALTER TABLE analytics.lineage_journal FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS lineage_journal_runtime_only ON analytics.lineage_journal;
CREATE POLICY lineage_journal_runtime_only ON analytics.lineage_journal
    FOR ALL TO moa_app
    USING (moa.current_control_plane())
    WITH CHECK (moa.current_control_plane());
GRANT SELECT, INSERT, UPDATE, DELETE ON analytics.lineage_journal TO moa_app;
