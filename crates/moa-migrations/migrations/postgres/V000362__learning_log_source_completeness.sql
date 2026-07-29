-- Commit-time source completeness for `learning_log`, matching the guarantee
-- V000360 gave `learning_candidates`.
--
-- V000360 normalized both provenance tables, backfilled both, and dropped both
-- legacy array columns — but it installed the deferred completeness trigger on
-- `learning_candidates` only. The asymmetry is invisible in normal operation
-- because `append_learning_log_sources_in_tx` refuses an empty source list, so
-- the production path is closed. That is a property of one Rust function, not of
-- the database: a second writer, a direct SQL insert, or a future path that
-- writes the entry and then fails before writing its sources would leave an
-- unattributable learning entry behind, and nothing would refuse it.
--
-- An unattributable learning entry is precisely the record an erasure cannot
-- reach and an export cannot explain, which is the failure the whole normalized
-- provenance design exists to remove. The guarantee has to hold on both tables
-- or it is not a guarantee about learning provenance — it is a guarantee about
-- candidates that learning entries happen to share.
--
-- DEFERRED, for the reason V000360 states: a statement-level check cannot see
-- an entry's sources at INSERT time, because the producer writes them in the
-- next statement of the same transaction. Refusing at COMMIT is what expresses
-- "this row must end the transaction attributable" — a shape no NOT NULL or
-- CHECK constraint can state.

-- No pre-existing row may violate the new rule. V000360's backfill classified
-- every `source_refs` entry it could and raised on any it could not, so an entry
-- with no source row at all would have to be one that carried an empty array.
-- Failing loudly here is the same call V000360 made for candidates: an
-- unattributable entry is a data defect to surface, never one to migrate past.
DO $$
DECLARE
    sourceless TEXT;
BEGIN
    SELECT string_agg(entry.id::TEXT, ', ')
    INTO sourceless
    FROM learning_log AS entry
    WHERE NOT EXISTS (
        SELECT 1 FROM learning_log_source AS source
        WHERE source.learning_id = entry.id
    );

    IF sourceless IS NOT NULL THEN
        RAISE EXCEPTION
            'V000362: learning-log entries carry no normalized source and cannot be attributed or erased: %',
            sourceless;
    END IF;
END $$;

CREATE OR REPLACE FUNCTION moa.assert_learning_log_sources_complete()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, public, moa
AS $$
DECLARE
    sourceless UUID;
BEGIN
    SELECT entry.id
    INTO sourceless
    FROM learning_log AS entry
    WHERE entry.id = NEW.id
      AND NOT EXISTS (
          SELECT 1 FROM learning_log_source AS source
          WHERE source.learning_id = entry.id
      );

    IF sourceless IS NOT NULL THEN
        RAISE EXCEPTION
            'learning-log entry % committed without any normalized source', sourceless
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

DROP TRIGGER IF EXISTS learning_log_sources_complete ON learning_log;
CREATE CONSTRAINT TRIGGER learning_log_sources_complete
AFTER INSERT ON learning_log
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION moa.assert_learning_log_sources_complete();

-- ---------------------------------------------------------------------------
-- The audited-read role can actually read what subject-access export joins.
-- ---------------------------------------------------------------------------

-- V000360 expanded privacy export to enumerate the learning-derived closure
-- through typed joins, adding three collectors over `learning_candidates`,
-- `learning_log`, and the normalized source/decision tables. Every export
-- collector runs under `SET LOCAL ROLE moa_auditor`, and that role had SELECT on
-- exactly the ten tables the pre-V000360 export touched — none of these.
--
-- So the whole learning-derived branch of a subject-access export failed with
-- `permission denied for table learning_candidates` in any environment where the
-- role is enforced. It could not have been noticed from a test connecting as the
-- owner, because the owner bypasses grants entirely: the queries are correct, the
-- rows are there, and the only thing missing is permission to read them.
--
-- SELECT only, and RLS still applies: all of these tables carry FORCE ROW LEVEL
-- SECURITY, so widening table access does not widen row access. The auditor sees
-- the same rows the subject-scoped queries already restrict it to.
GRANT SELECT ON sessions TO moa_auditor;
GRANT SELECT ON experience_records TO moa_auditor;
GRANT SELECT ON learning_candidates TO moa_auditor;
GRANT SELECT ON learning_candidate_source TO moa_auditor;
GRANT SELECT ON learning_candidate_decision TO moa_auditor;
GRANT SELECT ON learning_log TO moa_auditor;
GRANT SELECT ON learning_log_source TO moa_auditor;
