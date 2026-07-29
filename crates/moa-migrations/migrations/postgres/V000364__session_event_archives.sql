-- Terminal-session event archival and retention.
--
-- `events` is append-only and has no lifecycle boundary: a session that ended a
-- year ago still carries every row it ever wrote, on the hottest and most
-- heavily indexed table in the system, in every backup taken since. This table
-- gives that history a normal end state -- moved out of `events` into one
-- verifiable archive row -- without ever making it unrecoverable.
--
-- Shape, and why it is one row per session rather than a time range:
--
--   * `events` is HASH-partitioned on `session_id` across 16 children. Archival
--     is therefore always keyed by session: deleting one session's history
--     prunes to exactly one partition. A retention pass expressed as a
--     timestamp range would instead fan out across all 16 partitions and index
--     trees, which is slower than doing nothing at all. The retention boundary
--     selects WHICH sessions are eligible; the delete is never a range scan.
--
--   * `payload` is the session's entire history, serialized once in sequence
--     order, exactly as the rows were stored -- including claim-check
--     references, which continue to resolve because `session_blobs` is not
--     touched by retention. Hydration reproduces the same `EventRecord` values
--     `get_events` would have returned, so replay of an archived session is
--     indistinguishable from replay of a live one.
--
--   * `content_digest` is the BLAKE3 digest of exactly the bytes in `payload`,
--     not of the source rows. That is the point: it is recomputed from what the
--     database actually stored, so a truncated or rewritten archive is
--     detectable rather than merely unlikely. Nothing is deleted from `events`
--     in a transaction where that digest has not just been re-derived from a
--     read-back of the stored bytes and the decoded history compared, event for
--     event, against the rows about to be deleted.
--
--   * The archive is immutable. `session_event_archives_no_update` refuses
--     every UPDATE, so the copy that replaced live history can never be
--     silently rewritten afterwards. The archive row and the deletion of the
--     rows it replaces are written in one transaction, so there is no state in
--     which an archive exists that does not match the history it stands for.
--
-- `sessions.events_archived_at` is the marker for "this session's history now
-- lives in the archive". It is set in that same transaction. It exists on
-- `sessions` rather than being derived from this table because the append path
-- already holds the `sessions` row under `FOR UPDATE` and reads it there, so an
-- append to an archived session is refused without a single extra round trip.
-- Without that refusal a later append would resurrect rows for an archived
-- session and permanently hide the archive from the read path.
--
-- Deletion is deliberately NOT cascaded from `sessions`. The tenant purge
-- repository carries an explicit delete for this table, and a cascade would
-- make that step unfalsifiable: removing it would change nothing observable
-- because the session delete would remove the same rows moments later. With
-- `ON DELETE RESTRICT`, dropping the purge step fails the tenant's session
-- delete on a foreign-key violation instead of leaving a purged tenant's
-- conversation history sitting in the archive.

ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_id_tenant_key,
    ADD CONSTRAINT sessions_id_tenant_key UNIQUE (id, tenant_id);

CREATE TABLE IF NOT EXISTS session_event_archives (
    session_id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    format_version INTEGER NOT NULL,
    event_count BIGINT NOT NULL,
    first_sequence_num BIGINT NOT NULL,
    last_sequence_num BIGINT NOT NULL,
    payload BYTEA NOT NULL,
    content_digest BYTEA NOT NULL,
    archived_at TIMESTAMPTZ NOT NULL,
    -- An archive of nothing is not an archive: a session with no events has no
    -- history to move, and admitting a zero-row archive would let a verified
    -- purge delete rows it never captured.
    CONSTRAINT session_event_archives_nonempty CHECK (
        event_count > 0 AND octet_length(payload) > 0
    ),
    CONSTRAINT session_event_archives_digest_len CHECK (
        octet_length(content_digest) = 32
    ),
    -- Sequence numbers are dense and zero-based per session, so the covered
    -- span can never be narrower than the number of events it claims to hold.
    CONSTRAINT session_event_archives_sequence_span CHECK (
        first_sequence_num >= 0
        AND last_sequence_num >= first_sequence_num
        AND last_sequence_num - first_sequence_num + 1 >= event_count
    ),
    CONSTRAINT session_event_archives_session_tenant_fkey
        FOREIGN KEY (session_id, tenant_id)
        REFERENCES sessions (id, tenant_id)
        ON DELETE RESTRICT
);

COMMENT ON TABLE session_event_archives IS
    'One verified, immutable copy of a terminal session''s full event history, written in the same transaction that deletes those rows from events.';

CREATE INDEX IF NOT EXISTS idx_session_event_archives_tenant
    ON session_event_archives (tenant_id, archived_at DESC);

CREATE OR REPLACE FUNCTION session_event_archives_immutable_guard() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION
        'session event archive is immutable (session=%, events=%)',
        OLD.session_id, OLD.event_count
        USING ERRCODE = 'P0001';
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION session_event_archives_immutable_guard() IS
    'Refuses UPDATE on session_event_archives so the copy that replaced live session history can never be rewritten.';

DROP TRIGGER IF EXISTS session_event_archives_no_update ON session_event_archives;
CREATE TRIGGER session_event_archives_no_update
    BEFORE UPDATE ON session_event_archives
    FOR EACH ROW EXECUTE FUNCTION session_event_archives_immutable_guard();

SELECT moa.apply_tenant_rls('session_event_archives'::REGCLASS);

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS events_archived_at TIMESTAMPTZ;

COMMENT ON COLUMN sessions.events_archived_at IS
    'Set when this session''s events were moved to session_event_archives; the append path refuses further appends while it is non-NULL.';

-- Serves the retention candidate scan as an index-only range read over exactly
-- the rows that can still be archived. The predicate is the eligibility rule:
-- everything else is either still live or already archived.
CREATE INDEX IF NOT EXISTS idx_sessions_retention_candidates
    ON sessions (tenant_id, COALESCE(completed_at, updated_at))
    WHERE events_archived_at IS NULL
      AND status IN ('completed', 'cancelled', 'failed');
