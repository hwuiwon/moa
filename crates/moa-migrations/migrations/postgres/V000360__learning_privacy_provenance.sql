-- Normalized learning provenance, proposal-kind state machines, and the
-- privacy decision ledger that makes learning-derived erasure provable.
--
-- Three defects are closed here, and they are one defect wearing three hats.
--
-- 1. PROVENANCE WAS AN UNVALIDATED ARRAY. `learning_candidates
--    .source_experience_ids` and `learning_log.source_refs` were bare `UUID[]`
--    with no foreign key, no tenant equality, and no declared referent type. A
--    privacy erasure could therefore delete a source memory and leave the
--    learning derived from it standing and attributable, because nothing in the
--    database could enumerate the derivation. Worse, nothing could tell whether
--    a given uuid in `source_refs` named a session, a segment, an experience, or
--    a row that no longer existed at all. Enumeration by array membership is not
--    erasure; it is a guess.
--
--    The replacement is a typed one-of: one nullable column per referent kind,
--    each carrying a real composite foreign key that includes the partition (or
--    tenant) column, plus a `source_kind` discriminator constrained so that
--    exactly the matching column is non-null. That is deliberately NOT a
--    polymorphic `(kind, id)` pair -- a `(kind, id)` pair is the array problem
--    with extra steps, since the database still cannot join it or enforce it.
--
--    The partition columns appear IN the foreign keys rather than beside them,
--    so cross-tenant provenance is rejected by the constraint rather than by a
--    query that someone has to remember to write. Where a parent's
--    `storage_partition_id` is nullable (the artifact, experiment, and score
--    tables), a NULL-partition parent is simply unreferenceable: a row that
--    cannot be tenant-attributed must not become the anchor of a tenant-scoped
--    provenance claim, and refusing it is the same "unclassifiable rows fail"
--    rule the backfill below applies to legacy data.
--
-- 2. HALF THE PROPOSAL KINDS HAD NO MATERIALIZER. `learning_candidates` mixed
--    two genuinely reviewable proposals (a skill draft, a skill rollback --
--    both of which have a transactional accept path that changes serving state)
--    with informational memory/policy/eval/prompt suggestions that no code can
--    promote. All of them were written as `Proposed`, so the review surface
--    presented a promise it could not keep: a reviewer could "accept" a policy
--    suggestion and nothing would happen. `proposal_kind` splits the taxonomy
--    from the target domain (`candidate_type` keeps its existing meaning) and
--    the database now enforces which statuses and which transitions each kind
--    admits, rather than trusting every writer to agree.
--
-- 3. ERASURE HAD NO RECORD OF WHAT IT DECIDED. There was no way to distinguish
--    "this row was erased", "this row was retained because a legal hold covers
--    it", "this row was retained because bytes are shared with a subject we may
--    not erase", and "this run was a dry run and would have erased it". A
--    counter cannot answer a regulator. `moa.privacy_erasure_record_decision`
--    records one durable, idempotent disposition per enumerated record per
--    operation attempt, with `applied` separating a plan from an act.
--
-- Ordering note: contribution inserts are refused while a destruction fence is
-- in progress, mirroring `moa.reject_graph_write_during_destruction`. Without
-- that, a turn completing concurrently with an erase could file new derived
-- learning between enumeration and deletion and survive the erase.

-- ---------------------------------------------------------------------------
-- Helper: a cast that answers "is this even a uuid" instead of aborting.
-- ---------------------------------------------------------------------------

-- The legacy payloads hold uuids as free JSON strings, so classification has to
-- read values that may not be uuids at all. A bare `::UUID` cast would abort the
-- whole migration on the first malformed string, which would report "invalid
-- input syntax" instead of the row that is actually wrong.
CREATE OR REPLACE FUNCTION moa.try_uuid(raw TEXT) RETURNS UUID
LANGUAGE plpgsql IMMUTABLE STRICT
SET search_path = pg_catalog
AS $$
BEGIN
    RETURN raw::UUID;
EXCEPTION WHEN invalid_text_representation THEN
    RETURN NULL;
END;
$$;

-- ---------------------------------------------------------------------------
-- Parent uniqueness required for partition/tenant-carrying composite keys.
-- ---------------------------------------------------------------------------

ALTER TABLE learning_candidates
    DROP CONSTRAINT IF EXISTS learning_candidates_id_scope_key,
    ADD CONSTRAINT learning_candidates_id_scope_key
        UNIQUE (id, tenant_id, storage_partition_id);

ALTER TABLE experience_records
    DROP CONSTRAINT IF EXISTS experience_records_id_scope_key,
    ADD CONSTRAINT experience_records_id_scope_key
        UNIQUE (id, tenant_id, storage_partition_id);

ALTER TABLE experience_attributions
    DROP CONSTRAINT IF EXISTS experience_attributions_id_scope_key,
    ADD CONSTRAINT experience_attributions_id_scope_key
        UNIQUE (id, tenant_id, storage_partition_id);

ALTER TABLE task_segments
    DROP CONSTRAINT IF EXISTS task_segments_id_scope_key,
    ADD CONSTRAINT task_segments_id_scope_key
        UNIQUE (id, tenant_id, storage_partition_id);

ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_id_partition_key,
    ADD CONSTRAINT sessions_id_partition_key
        UNIQUE (id, storage_partition_id);

ALTER TABLE learning_log
    DROP CONSTRAINT IF EXISTS learning_log_id_scope_key,
    ADD CONSTRAINT learning_log_id_scope_key
        UNIQUE (id, tenant_id, storage_partition_id);

ALTER TABLE contacts
    DROP CONSTRAINT IF EXISTS contacts_id_partition_key,
    ADD CONSTRAINT contacts_id_partition_key
        UNIQUE (id, storage_partition_id);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_revision_uid_partition_key
    ON moa.artifact_revision (revision_uid, storage_partition_id);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_file_uid_partition_key
    ON moa.artifact_file (file_uid, storage_partition_id);

CREATE UNIQUE INDEX IF NOT EXISTS experiment_run_uid_partition_key
    ON moa.experiment_run (run_uid, storage_partition_id);

CREATE UNIQUE INDEX IF NOT EXISTS experiment_trial_uid_partition_key
    ON moa.experiment_trial (trial_uid, storage_partition_id);

CREATE UNIQUE INDEX IF NOT EXISTS score_run_id_partition_key
    ON analytics.score_run (run_id, storage_partition_id);

-- The `events` primary key is `(id, session_id)` because the table is hash
-- partitioned by `session_id`, so an event reference must carry both halves.
CREATE UNIQUE INDEX IF NOT EXISTS events_id_session_partition_key
    ON events (id, session_id, storage_partition_id);

-- ---------------------------------------------------------------------------
-- Proposal kind: the review contract, split from the target domain.
-- ---------------------------------------------------------------------------

ALTER TABLE learning_candidates
    ADD COLUMN IF NOT EXISTS proposal_kind TEXT;

-- Classification runs before the constraints so a row that cannot be classified
-- fails loudly here rather than passing under a permissive default.
--
-- A skill candidate is a ROLLBACK proposal when it names both the promotion it
-- reverses and the revision it archives; it is a DRAFT when its recorded draft
-- revision actually resolves to a real artifact revision in the same partition.
-- A skill suggestion whose draft revision does not resolve never had a
-- materializer, so it is authoring work, not a reviewable proposal -- which is
-- exactly the false review contract this migration exists to remove.
UPDATE learning_candidates AS candidate
SET proposal_kind = CASE
        WHEN candidate.candidate_type = 'skill'
             AND candidate.payload ? 'promotion_candidate_id'
             AND candidate.payload ? 'promoted_revision_uid'
            THEN 'skill_rollback'
        WHEN candidate.candidate_type = 'skill'
             AND EXISTS (
                 SELECT 1
                 FROM moa.artifact_revision AS revision
                 WHERE revision.revision_uid
                       = moa.try_uuid(candidate.payload ->> 'draft_artifact_revision_uid')
                   AND revision.storage_partition_id = candidate.storage_partition_id
             )
            THEN 'skill_draft'
        WHEN candidate.candidate_type = 'skill' THEN 'skill_authoring'
        WHEN candidate.candidate_type = 'memory' THEN 'memory_advisory'
        WHEN candidate.candidate_type = 'policy' THEN 'policy_authoring'
        WHEN candidate.candidate_type = 'prompt' THEN 'prompt_authoring'
        WHEN candidate.candidate_type = 'eval' THEN 'eval_authoring'
        ELSE NULL
    END
WHERE candidate.proposal_kind IS NULL;

DO $$
DECLARE
    unclassified TEXT;
BEGIN
    SELECT string_agg(id::TEXT, ', ')
    INTO unclassified
    FROM learning_candidates
    WHERE proposal_kind IS NULL;

    IF unclassified IS NOT NULL THEN
        RAISE EXCEPTION
            'V000360: learning candidates carry an unclassifiable candidate_type and cannot be assigned a proposal_kind: %',
            unclassified;
    END IF;
END $$;

-- A rollback proposal that does not actually name a real promotion is not a
-- rollback; accepting it would archive a revision on the strength of a payload
-- string. Validate the relationship before the constraints freeze it.
DO $$
DECLARE
    dangling TEXT;
BEGIN
    SELECT string_agg(candidate.id::TEXT, ', ')
    INTO dangling
    FROM learning_candidates AS candidate
    WHERE candidate.proposal_kind = 'skill_rollback'
      AND NOT EXISTS (
          SELECT 1
          FROM learning_candidates AS promotion
          WHERE promotion.id = moa.try_uuid(candidate.payload ->> 'promotion_candidate_id')
            AND promotion.tenant_id = candidate.tenant_id
      );

    IF dangling IS NOT NULL THEN
        RAISE EXCEPTION
            'V000360: rollback proposals reference a promotion candidate that does not exist in the same tenant: %',
            dangling;
    END IF;
END $$;

-- Informational kinds move onto their own terminal-only lifecycle. A promoted
-- or rolled-back informational row is a contradiction -- nothing could have
-- promoted it -- so it is a hard failure rather than a silent remap.
DO $$
DECLARE
    impossible TEXT;
BEGIN
    SELECT string_agg(id::TEXT, ', ')
    INTO impossible
    FROM learning_candidates
    WHERE proposal_kind IN (
              'memory_advisory',
              'skill_authoring',
              'policy_authoring',
              'prompt_authoring',
              'eval_authoring'
          )
      AND status IN ('promoted', 'rolled_back');

    IF impossible IS NOT NULL THEN
        RAISE EXCEPTION
            'V000360: informational learning candidates carry a promoted/rolled_back status no materializer could have produced: %',
            impossible;
    END IF;
END $$;

UPDATE learning_candidates
SET status = CASE
        WHEN status = 'rejected' THEN 'dismissed'
        WHEN proposal_kind = 'memory_advisory' THEN 'advisory'
        ELSE 'needs_authoring'
    END,
    status_reason = COALESCE(status_reason, 'reclassified by V000360: informational proposal kind')
WHERE proposal_kind IN (
          'memory_advisory',
          'skill_authoring',
          'policy_authoring',
          'prompt_authoring',
          'eval_authoring'
      )
  AND status IN ('proposed', 'evaluating', 'rejected');

ALTER TABLE learning_candidates
    ALTER COLUMN proposal_kind SET NOT NULL;

ALTER TABLE learning_candidates
    DROP CONSTRAINT IF EXISTS learning_candidates_proposal_kind_valid,
    ADD CONSTRAINT learning_candidates_proposal_kind_valid
        CHECK (proposal_kind IN (
            'skill_draft',
            'skill_rollback',
            'memory_advisory',
            'skill_authoring',
            'policy_authoring',
            'prompt_authoring',
            'eval_authoring'
        ));

-- The (kind, status) product is enumerated rather than described, so a status
-- that no accept path can produce for a kind cannot be written at all.
ALTER TABLE learning_candidates
    DROP CONSTRAINT IF EXISTS learning_candidates_kind_status_valid,
    ADD CONSTRAINT learning_candidates_kind_status_valid
        CHECK (
            (proposal_kind = 'skill_draft'
                AND status IN ('proposed', 'evaluating', 'promoted', 'rejected', 'rolled_back'))
            OR (proposal_kind = 'skill_rollback'
                AND status IN ('proposed', 'evaluating', 'promoted', 'rejected'))
            OR (proposal_kind = 'memory_advisory'
                AND status IN ('advisory', 'dismissed'))
            OR (proposal_kind IN (
                    'skill_authoring',
                    'policy_authoring',
                    'prompt_authoring',
                    'eval_authoring'
                )
                AND status IN ('needs_authoring', 'dismissed'))
        );

CREATE INDEX IF NOT EXISTS idx_learning_candidates_kind_status
    ON learning_candidates (tenant_id, proposal_kind, status, updated_at DESC);

-- Transitions, not just states. A CHECK constraint sees one row version; only a
-- trigger sees the pair, and the pair is where "an advisory item was quietly
-- promoted" lives. Repository-level compare-and-set remains in place as defense
-- in depth, but the authority is here so a direct SQL writer cannot bypass it.
CREATE OR REPLACE FUNCTION moa.enforce_learning_candidate_transition() RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, public, moa
AS $$
BEGIN
    IF NEW.proposal_kind IS DISTINCT FROM OLD.proposal_kind THEN
        RAISE EXCEPTION
            'learning candidate % may not change proposal_kind (% -> %)',
            OLD.id, OLD.proposal_kind, NEW.proposal_kind
            USING ERRCODE = '23514';
    END IF;

    IF NEW.status = OLD.status THEN
        RETURN NEW;
    END IF;

    IF NOT (
        (OLD.proposal_kind IN ('skill_draft', 'skill_rollback') AND (
            (OLD.status = 'proposed' AND NEW.status = 'evaluating')
            OR (OLD.status = 'evaluating' AND NEW.status IN ('promoted', 'rejected'))
            -- Owner-only claim release after a transient execution failure, so a
            -- crashed accept never strands a proposal in `evaluating`.
            OR (OLD.status = 'evaluating' AND NEW.status = 'proposed')
        ))
        OR (OLD.proposal_kind = 'skill_draft'
            AND OLD.status = 'promoted' AND NEW.status = 'rolled_back')
        OR (OLD.proposal_kind = 'memory_advisory'
            AND OLD.status = 'advisory' AND NEW.status = 'dismissed')
        OR (OLD.proposal_kind IN (
                'skill_authoring',
                'policy_authoring',
                'prompt_authoring',
                'eval_authoring'
            )
            AND OLD.status = 'needs_authoring' AND NEW.status = 'dismissed')
    ) THEN
        RAISE EXCEPTION
            'learning candidate % of kind % may not transition % -> %',
            OLD.id, OLD.proposal_kind, OLD.status, NEW.status
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS learning_candidate_transition ON learning_candidates;
CREATE TRIGGER learning_candidate_transition
BEFORE UPDATE ON learning_candidates
FOR EACH ROW EXECUTE FUNCTION moa.enforce_learning_candidate_transition();

-- ---------------------------------------------------------------------------
-- Session-owned normalized provenance.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS learning_candidate_source (
    id                    UUID        PRIMARY KEY,
    candidate_id          UUID        NOT NULL,
    tenant_id             TEXT        NOT NULL,
    storage_partition_id  TEXT        NOT NULL,
    user_id               TEXT,
    scope                 TEXT GENERATED ALWAYS AS
                              (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    source_kind           TEXT        NOT NULL,
    experience_id         UUID,
    attribution_id        UUID,
    session_id            UUID,
    event_id              UUID,
    event_session_id      UUID,
    segment_id            UUID,
    contact_id            UUID,
    promotion_candidate_id UUID,
    artifact_revision_uid UUID,
    experiment_run_uid    UUID,
    experiment_trial_uid  UUID,
    score_run_id          UUID,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT learning_candidate_source_owner_fk
        FOREIGN KEY (candidate_id, tenant_id, storage_partition_id)
        REFERENCES learning_candidates (id, tenant_id, storage_partition_id),

    -- One column per referent kind, each with a real composite key. The
    -- discriminator selects which column must be present; every other typed
    -- column must be absent, so a row can never claim two referents or none.
    CONSTRAINT learning_candidate_source_kind_valid
        CHECK (source_kind IN (
            'experience', 'attribution', 'session', 'event', 'task_segment',
            'contact', 'promotion_candidate', 'artifact_revision',
            'experiment_run', 'experiment_trial', 'score_run'
        )),
    CONSTRAINT learning_candidate_source_exactly_one
        CHECK (
            (CASE WHEN experience_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN attribution_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN session_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN event_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN segment_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN contact_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN promotion_candidate_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN artifact_revision_uid IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN experiment_run_uid IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN experiment_trial_uid IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN score_run_id IS NOT NULL THEN 1 ELSE 0 END)
            = 1
        ),
    CONSTRAINT learning_candidate_source_kind_matches_column
        CHECK (
            (source_kind = 'experience' AND experience_id IS NOT NULL)
            OR (source_kind = 'attribution' AND attribution_id IS NOT NULL)
            OR (source_kind = 'session' AND session_id IS NOT NULL)
            OR (source_kind = 'event' AND event_id IS NOT NULL AND event_session_id IS NOT NULL)
            OR (source_kind = 'task_segment' AND segment_id IS NOT NULL)
            OR (source_kind = 'contact' AND contact_id IS NOT NULL)
            OR (source_kind = 'promotion_candidate' AND promotion_candidate_id IS NOT NULL)
            OR (source_kind = 'artifact_revision' AND artifact_revision_uid IS NOT NULL)
            OR (source_kind = 'experiment_run' AND experiment_run_uid IS NOT NULL)
            OR (source_kind = 'experiment_trial' AND experiment_trial_uid IS NOT NULL)
            OR (source_kind = 'score_run' AND score_run_id IS NOT NULL)
        ),
    CONSTRAINT learning_candidate_source_event_pair
        CHECK ((event_id IS NULL) = (event_session_id IS NULL)),

    CONSTRAINT learning_candidate_source_experience_fk
        FOREIGN KEY (experience_id, tenant_id, storage_partition_id)
        REFERENCES experience_records (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_candidate_source_attribution_fk
        FOREIGN KEY (attribution_id, tenant_id, storage_partition_id)
        REFERENCES experience_attributions (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_candidate_source_session_fk
        FOREIGN KEY (session_id, storage_partition_id)
        REFERENCES sessions (id, storage_partition_id),
    CONSTRAINT learning_candidate_source_event_fk
        FOREIGN KEY (event_id, event_session_id, storage_partition_id)
        REFERENCES events (id, session_id, storage_partition_id),
    CONSTRAINT learning_candidate_source_segment_fk
        FOREIGN KEY (segment_id, tenant_id, storage_partition_id)
        REFERENCES task_segments (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_candidate_source_contact_fk
        FOREIGN KEY (contact_id, storage_partition_id)
        REFERENCES contacts (id, storage_partition_id),
    CONSTRAINT learning_candidate_source_promotion_fk
        FOREIGN KEY (promotion_candidate_id, tenant_id, storage_partition_id)
        REFERENCES learning_candidates (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_candidate_source_revision_fk
        FOREIGN KEY (artifact_revision_uid, storage_partition_id)
        REFERENCES moa.artifact_revision (revision_uid, storage_partition_id),
    CONSTRAINT learning_candidate_source_experiment_run_fk
        FOREIGN KEY (experiment_run_uid, storage_partition_id)
        REFERENCES moa.experiment_run (run_uid, storage_partition_id),
    CONSTRAINT learning_candidate_source_experiment_trial_fk
        FOREIGN KEY (experiment_trial_uid, storage_partition_id)
        REFERENCES moa.experiment_trial (trial_uid, storage_partition_id),
    CONSTRAINT learning_candidate_source_score_run_fk
        FOREIGN KEY (score_run_id, storage_partition_id)
        REFERENCES analytics.score_run (run_id, storage_partition_id)
);

-- Dedupe: filing the same source twice for one candidate is a no-op rather than
-- a duplicate that inflates every closure count downstream.
CREATE UNIQUE INDEX IF NOT EXISTS learning_candidate_source_unique
    ON learning_candidate_source (
        candidate_id,
        source_kind,
        COALESCE(experience_id, attribution_id, session_id, event_id, segment_id,
                 contact_id, promotion_candidate_id, artifact_revision_uid,
                 experiment_run_uid, experiment_trial_uid, score_run_id)
    );

-- Reverse traversal: privacy erasure walks source -> candidate, which is the
-- direction the closure is actually enumerated in.
CREATE INDEX IF NOT EXISTS learning_candidate_source_experience_idx
    ON learning_candidate_source (tenant_id, experience_id)
    WHERE experience_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS learning_candidate_source_session_idx
    ON learning_candidate_source (tenant_id, session_id)
    WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS learning_candidate_source_contact_idx
    ON learning_candidate_source (tenant_id, contact_id)
    WHERE contact_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS learning_candidate_source_scope_idx
    ON learning_candidate_source (storage_partition_id, scope, user_id);

CREATE TABLE IF NOT EXISTS learning_log_source (
    id                   UUID        PRIMARY KEY,
    learning_id          UUID        NOT NULL,
    tenant_id            TEXT        NOT NULL,
    storage_partition_id TEXT        NOT NULL,
    user_id              TEXT,
    scope                TEXT GENERATED ALWAYS AS
                             (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    source_kind          TEXT        NOT NULL,
    candidate_id         UUID,
    experience_id        UUID,
    session_id           UUID,
    segment_id           UUID,
    artifact_revision_uid UUID,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT learning_log_source_owner_fk
        FOREIGN KEY (learning_id, tenant_id, storage_partition_id)
        REFERENCES learning_log (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_log_source_kind_valid
        CHECK (source_kind IN (
            'candidate', 'experience', 'session', 'task_segment', 'artifact_revision'
        )),
    CONSTRAINT learning_log_source_exactly_one
        CHECK (
            (CASE WHEN candidate_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN experience_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN session_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN segment_id IS NOT NULL THEN 1 ELSE 0 END)
          + (CASE WHEN artifact_revision_uid IS NOT NULL THEN 1 ELSE 0 END)
            = 1
        ),
    CONSTRAINT learning_log_source_kind_matches_column
        CHECK (
            (source_kind = 'candidate' AND candidate_id IS NOT NULL)
            OR (source_kind = 'experience' AND experience_id IS NOT NULL)
            OR (source_kind = 'session' AND session_id IS NOT NULL)
            OR (source_kind = 'task_segment' AND segment_id IS NOT NULL)
            OR (source_kind = 'artifact_revision' AND artifact_revision_uid IS NOT NULL)
        ),
    CONSTRAINT learning_log_source_candidate_fk
        FOREIGN KEY (candidate_id, tenant_id, storage_partition_id)
        REFERENCES learning_candidates (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_log_source_experience_fk
        FOREIGN KEY (experience_id, tenant_id, storage_partition_id)
        REFERENCES experience_records (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_log_source_session_fk
        FOREIGN KEY (session_id, storage_partition_id)
        REFERENCES sessions (id, storage_partition_id),
    CONSTRAINT learning_log_source_segment_fk
        FOREIGN KEY (segment_id, tenant_id, storage_partition_id)
        REFERENCES task_segments (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_log_source_revision_fk
        FOREIGN KEY (artifact_revision_uid, storage_partition_id)
        REFERENCES moa.artifact_revision (revision_uid, storage_partition_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS learning_log_source_unique
    ON learning_log_source (
        learning_id,
        source_kind,
        COALESCE(candidate_id, experience_id, session_id, segment_id, artifact_revision_uid)
    );
CREATE INDEX IF NOT EXISTS learning_log_source_candidate_idx
    ON learning_log_source (tenant_id, candidate_id)
    WHERE candidate_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS learning_log_source_experience_idx
    ON learning_log_source (tenant_id, experience_id)
    WHERE experience_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS learning_log_source_session_idx
    ON learning_log_source (tenant_id, session_id)
    WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS learning_log_source_scope_idx
    ON learning_log_source (storage_partition_id, scope, user_id);

-- Historical disposition of one review decision. Export reads this to answer
-- "what happened to the proposal derived from my data", which a mutable status
-- column cannot answer after the fact.
CREATE TABLE IF NOT EXISTS learning_candidate_decision (
    id                   UUID        PRIMARY KEY,
    candidate_id         UUID        NOT NULL,
    tenant_id            TEXT        NOT NULL,
    storage_partition_id TEXT        NOT NULL,
    user_id              TEXT,
    scope                TEXT GENERATED ALWAYS AS
                             (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    decision             TEXT        NOT NULL,
    from_status          TEXT        NOT NULL,
    to_status            TEXT        NOT NULL,
    reviewer_subject     TEXT,
    reason               TEXT,
    decided_at           TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT learning_candidate_decision_owner_fk
        FOREIGN KEY (candidate_id, tenant_id, storage_partition_id)
        REFERENCES learning_candidates (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_candidate_decision_kind_valid
        CHECK (decision IN (
            'accepted_skill', 'accepted_rollback', 'rejected', 'dismissed'
        ))
);

-- Exactly one durable audit per decision per candidate: a replayed dismiss
-- writes nothing new instead of appending a second identical audit.
CREATE UNIQUE INDEX IF NOT EXISTS learning_candidate_decision_unique
    ON learning_candidate_decision (candidate_id, decision);
CREATE INDEX IF NOT EXISTS learning_candidate_decision_scope_idx
    ON learning_candidate_decision (storage_partition_id, scope, user_id);

SELECT moa.apply_three_tier_rls('learning_candidate_source'::REGCLASS);
SELECT moa.apply_three_tier_rls('learning_log_source'::REGCLASS);
SELECT moa.apply_three_tier_rls('learning_candidate_decision'::REGCLASS);

ALTER TABLE learning_candidate_source FORCE ROW LEVEL SECURITY;
ALTER TABLE learning_log_source FORCE ROW LEVEL SECURITY;
ALTER TABLE learning_candidate_decision FORCE ROW LEVEL SECURITY;

-- ---------------------------------------------------------------------------
-- Artifact-owned contributions: which derived bytes came from whose data.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS moa.artifact_revision_contribution (
    contribution_uid     UUID        PRIMARY KEY,
    storage_partition_id TEXT        NOT NULL,
    user_id              TEXT,
    scope                TEXT GENERATED ALWAYS AS
                             (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    revision_uid         UUID        NOT NULL,
    -- NULL means the contribution covers the revision's own definition and
    -- source text rather than one addressable file.
    file_uid             UUID,
    candidate_id         UUID        NOT NULL,
    tenant_id            TEXT        NOT NULL,
    contribution_kind    TEXT        NOT NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT artifact_revision_contribution_revision_fk
        FOREIGN KEY (revision_uid, storage_partition_id)
        REFERENCES moa.artifact_revision (revision_uid, storage_partition_id),
    CONSTRAINT artifact_revision_contribution_file_fk
        FOREIGN KEY (file_uid, storage_partition_id)
        REFERENCES moa.artifact_file (file_uid, storage_partition_id),
    CONSTRAINT artifact_revision_contribution_candidate_fk
        FOREIGN KEY (candidate_id, tenant_id, storage_partition_id)
        REFERENCES learning_candidates (id, tenant_id, storage_partition_id),
    -- `generated_definition` marks LLM output that fused every source it saw.
    -- It is declared NON-SUBTRACTABLE: erasing one contributor's evidence
    -- cannot carve that contributor back out of a generated paragraph, so the
    -- whole serving revision is invalidated instead of partially rewritten.
    CONSTRAINT artifact_revision_contribution_kind_valid
        CHECK (contribution_kind IN ('generated_definition', 'generated_file')),
    CONSTRAINT artifact_revision_contribution_file_kind
        CHECK ((contribution_kind = 'generated_file') = (file_uid IS NOT NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_revision_contribution_unique
    ON moa.artifact_revision_contribution (
        revision_uid, candidate_id, contribution_kind, COALESCE(file_uid, revision_uid)
    );
CREATE INDEX IF NOT EXISTS artifact_revision_contribution_candidate_idx
    ON moa.artifact_revision_contribution (tenant_id, candidate_id);
CREATE INDEX IF NOT EXISTS artifact_revision_contribution_revision_idx
    ON moa.artifact_revision_contribution (revision_uid);

-- Regression-suite bytes used to live inside `learning_candidates.payload` as
-- JSON strings, which put attributable generated text in a column nothing could
-- enumerate, join, or erase. They now belong to the artifact owner, which is
-- also the only component that can assemble review input from them.
CREATE TABLE IF NOT EXISTS moa.artifact_suite_contribution (
    contribution_uid     UUID        PRIMARY KEY,
    storage_partition_id TEXT        NOT NULL,
    user_id              TEXT,
    scope                TEXT GENERATED ALWAYS AS
                             (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    tenant_id            TEXT        NOT NULL,
    candidate_id         UUID        NOT NULL,
    -- NULL until the draft revision this suite guards actually exists.
    revision_uid         UUID,
    suite_kind           TEXT        NOT NULL,
    suite_name           TEXT        NOT NULL,
    suite_source         TEXT        NOT NULL,
    source_session_id    UUID,
    source_experience_id UUID,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT artifact_suite_contribution_candidate_fk
        FOREIGN KEY (candidate_id, tenant_id, storage_partition_id)
        REFERENCES learning_candidates (id, tenant_id, storage_partition_id),
    CONSTRAINT artifact_suite_contribution_revision_fk
        FOREIGN KEY (revision_uid, storage_partition_id)
        REFERENCES moa.artifact_revision (revision_uid, storage_partition_id),
    CONSTRAINT artifact_suite_contribution_session_fk
        FOREIGN KEY (source_session_id, storage_partition_id)
        REFERENCES sessions (id, storage_partition_id),
    CONSTRAINT artifact_suite_contribution_experience_fk
        FOREIGN KEY (source_experience_id, tenant_id, storage_partition_id)
        REFERENCES experience_records (id, tenant_id, storage_partition_id),
    CONSTRAINT artifact_suite_contribution_kind_valid
        CHECK (suite_kind IN ('generated', 'accumulated')),
    CONSTRAINT artifact_suite_contribution_source_present
        CHECK (source_session_id IS NOT NULL OR source_experience_id IS NOT NULL)
);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_suite_contribution_unique
    ON moa.artifact_suite_contribution (candidate_id, suite_kind, suite_name);
CREATE INDEX IF NOT EXISTS artifact_suite_contribution_session_idx
    ON moa.artifact_suite_contribution (tenant_id, source_session_id)
    WHERE source_session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS artifact_suite_contribution_experience_idx
    ON moa.artifact_suite_contribution (tenant_id, source_experience_id)
    WHERE source_experience_id IS NOT NULL;

SELECT moa.apply_three_tier_rls('moa.artifact_revision_contribution'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_suite_contribution'::REGCLASS);

ALTER TABLE moa.artifact_revision_contribution FORCE ROW LEVEL SECURITY;
ALTER TABLE moa.artifact_suite_contribution FORCE ROW LEVEL SECURITY;

-- ---------------------------------------------------------------------------
-- Fence: no new derived learning lands while an erase is enumerating.
-- ---------------------------------------------------------------------------

-- Mirrors `moa.reject_graph_write_during_destruction`, with one deliberate
-- difference: only an IN-PROGRESS fence blocks. A committed fence is a
-- permanent record that an erase happened, and permanently refusing every
-- future contribution for the tenant would make one subject's erasure stop all
-- learning for everyone in that tenant forever.
--
-- The check is tenant-wide rather than subject-scoped because a contribution
-- row does not name a data subject directly -- its subject is reachable only by
-- walking to the source rows the erase is concurrently deleting. Failing closed
-- for the whole tenant during the seconds an erase runs is the cheap side of
-- that trade; the expensive side would be a contribution that slips in between
-- enumeration and deletion and survives.
CREATE OR REPLACE FUNCTION moa.reject_learning_contribution_during_destruction()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, public, moa
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence AS fence
        WHERE fence.tenant_id::TEXT = NEW.tenant_id
          AND fence.status = 'in_progress'
    ) THEN
        RAISE EXCEPTION
            'learning contribution refused: destruction is fenced for this tenant'
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER learning_candidate_source_destruction_fence
BEFORE INSERT ON learning_candidate_source
FOR EACH ROW EXECUTE FUNCTION moa.reject_learning_contribution_during_destruction();

CREATE TRIGGER learning_log_source_destruction_fence
BEFORE INSERT ON learning_log_source
FOR EACH ROW EXECUTE FUNCTION moa.reject_learning_contribution_during_destruction();

CREATE TRIGGER artifact_revision_contribution_destruction_fence
BEFORE INSERT ON moa.artifact_revision_contribution
FOR EACH ROW EXECUTE FUNCTION moa.reject_learning_contribution_during_destruction();

CREATE TRIGGER artifact_suite_contribution_destruction_fence
BEFORE INSERT ON moa.artifact_suite_contribution
FOR EACH ROW EXECUTE FUNCTION moa.reject_learning_contribution_during_destruction();

-- ---------------------------------------------------------------------------
-- PII-owned erasure decision ledger and the two new erase stages.
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS moa.privacy_erasure_record_decision (
    decision_uid  UUID        PRIMARY KEY,
    tenant_id     UUID        NOT NULL,
    -- Direct subject ownership is what makes a subject export scoped without
    -- reconstructing a derivation after the source rows have been erased.
    subject_user_id TEXT      NOT NULL,
    -- Approval JTI plus outcome mode (`dry_run`, `legal_hold`, or `applied`).
    -- A replay reuses it; a later applied use of an unconsumed JTI does not
    -- conflict with the earlier unapplied decision.
    attempt_id    TEXT        NOT NULL,
    record_kind   TEXT        NOT NULL,
    record_id     TEXT        NOT NULL,
    disposition   TEXT        NOT NULL,
    -- FALSE for a dry run: a planned disposition is a plan, never evidence of
    -- deletion. Nothing may report a dry run as an erase.
    applied       BOOLEAN     NOT NULL,
    reason        TEXT,
    decided_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT privacy_erasure_record_decision_kind_valid
        CHECK (record_kind IN (
            'learning_candidate',
            'learning_log',
            'artifact_revision',
            'artifact_suite_contribution',
            'experience_record',
            'experience_attribution'
        )),
    CONSTRAINT privacy_erasure_record_decision_disposition_valid
        CHECK (disposition IN (
            'erased',
            'invalidated_revision',
            'retained_legal_hold'
        )),
    -- A legal hold must mutate nothing, so its decision can never be `applied`.
    -- This is the constraint that makes "a hold causes zero protected-data
    -- mutation" checkable rather than merely intended.
    CONSTRAINT privacy_erasure_record_decision_hold_never_applied
        CHECK (disposition <> 'retained_legal_hold' OR applied = FALSE)
);

CREATE UNIQUE INDEX IF NOT EXISTS privacy_erasure_record_decision_unique
    ON moa.privacy_erasure_record_decision
       (tenant_id, subject_user_id, attempt_id, record_kind, record_id);
CREATE INDEX IF NOT EXISTS privacy_erasure_record_decision_subject_idx
    ON moa.privacy_erasure_record_decision
       (tenant_id, subject_user_id, decided_at);

SELECT moa.apply_tenant_rls('moa.privacy_erasure_record_decision'::REGCLASS);
ALTER TABLE moa.privacy_erasure_record_decision FORCE ROW LEVEL SECURITY;

-- The reverse-derived stages run BEFORE the vault/graph stages, because
-- learning derived from a memory has to be resolved while the memory it points
-- at still exists -- otherwise the closure walk finds nothing to walk.
ALTER TABLE moa.erasure_jobs
    ADD COLUMN IF NOT EXISTS learning_erased BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS artifact_erased BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS decisions_recorded BIGINT NOT NULL DEFAULT 0;

ALTER TABLE moa.erasure_jobs
    DROP CONSTRAINT IF EXISTS erasure_jobs_stage_check,
    ADD CONSTRAINT erasure_jobs_stage_check
        CHECK (stage IN ('learning', 'artifacts', 'vault', 'graph', 'digest', 'lineage', 'done'));

ALTER TABLE moa.erasure_jobs
    ALTER COLUMN stage SET DEFAULT 'learning';

-- ---------------------------------------------------------------------------
-- Deterministic backfill of the legacy provenance arrays, then their removal.
-- ---------------------------------------------------------------------------

INSERT INTO learning_candidate_source
    (id, candidate_id, tenant_id, storage_partition_id, user_id, source_kind, experience_id)
SELECT
    gen_random_uuid(),
    candidate.id,
    candidate.tenant_id,
    candidate.storage_partition_id,
    candidate.user_id,
    'experience',
    source.experience_id
FROM learning_candidates AS candidate
CROSS JOIN LATERAL unnest(candidate.source_experience_ids) AS source(experience_id)
JOIN experience_records AS experience
  ON experience.id = source.experience_id
 AND experience.tenant_id = candidate.tenant_id
 AND experience.storage_partition_id = candidate.storage_partition_id
ON CONFLICT DO NOTHING;

DO $$
DECLARE
    dangling TEXT;
BEGIN
    SELECT string_agg(DISTINCT candidate.id::TEXT, ', ')
    INTO dangling
    FROM learning_candidates AS candidate
    CROSS JOIN LATERAL unnest(candidate.source_experience_ids) AS source(experience_id)
    WHERE NOT EXISTS (
        SELECT 1
        FROM experience_records AS experience
        WHERE experience.id = source.experience_id
          AND experience.tenant_id = candidate.tenant_id
          AND experience.storage_partition_id = candidate.storage_partition_id
    );

    IF dangling IS NOT NULL THEN
        RAISE EXCEPTION
            'V000360: learning candidates reference source experiences that do not exist in the same tenant partition: %',
            dangling;
    END IF;
END $$;

-- `learning_log.source_refs` never declared what it pointed at, so the backfill
-- probes each referent table in a fixed order. A uuid that matches none of them
-- is exactly the unclassifiable case the plan requires to fail rather than be
-- dropped -- dropping it would silently sever a derivation chain.
INSERT INTO learning_log_source
    (id, learning_id, tenant_id, storage_partition_id, user_id,
     source_kind, candidate_id, experience_id, session_id, segment_id)
SELECT
    gen_random_uuid(),
    entry.id,
    entry.tenant_id,
    entry.storage_partition_id,
    entry.user_id,
    classified.source_kind,
    CASE WHEN classified.source_kind = 'candidate' THEN source.ref END,
    CASE WHEN classified.source_kind = 'experience' THEN source.ref END,
    CASE WHEN classified.source_kind = 'session' THEN source.ref END,
    CASE WHEN classified.source_kind = 'task_segment' THEN source.ref END
FROM learning_log AS entry
CROSS JOIN LATERAL unnest(entry.source_refs) AS source(ref)
CROSS JOIN LATERAL (
    SELECT CASE
        WHEN EXISTS (
            SELECT 1 FROM learning_candidates AS candidate
            WHERE candidate.id = source.ref
              AND candidate.tenant_id = entry.tenant_id
              AND candidate.storage_partition_id = entry.storage_partition_id
        ) THEN 'candidate'
        WHEN EXISTS (
            SELECT 1 FROM experience_records AS experience
            WHERE experience.id = source.ref
              AND experience.tenant_id = entry.tenant_id
              AND experience.storage_partition_id = entry.storage_partition_id
        ) THEN 'experience'
        WHEN EXISTS (
            SELECT 1 FROM task_segments AS segment
            WHERE segment.id = source.ref
              AND segment.tenant_id = entry.tenant_id
              AND segment.storage_partition_id = entry.storage_partition_id
        ) THEN 'task_segment'
        WHEN EXISTS (
            SELECT 1 FROM sessions AS session
            WHERE session.id = source.ref
              AND session.storage_partition_id = entry.storage_partition_id
        ) THEN 'session'
        ELSE NULL
    END AS source_kind
) AS classified
WHERE classified.source_kind IS NOT NULL
ON CONFLICT DO NOTHING;

DO $$
DECLARE
    unclassifiable TEXT;
BEGIN
    SELECT string_agg(DISTINCT entry.id::TEXT, ', ')
    INTO unclassifiable
    FROM learning_log AS entry
    CROSS JOIN LATERAL unnest(entry.source_refs) AS source(ref)
    WHERE NOT EXISTS (
        SELECT 1 FROM learning_log_source AS filed
        WHERE filed.learning_id = entry.id
          AND COALESCE(filed.candidate_id, filed.experience_id,
                       filed.session_id, filed.segment_id) = source.ref
    );

    IF unclassifiable IS NOT NULL THEN
        RAISE EXCEPTION
            'V000360: learning-log entries carry source_refs that resolve to no candidate, experience, segment, or session in the same tenant partition: %',
            unclassifiable;
    END IF;
END $$;

-- A rollback proposal's link to the promotion it reverses moves out of the
-- payload, so accepting a rollback stops depending on a JSON string being
-- present and well-formed.
INSERT INTO learning_candidate_source
    (id, candidate_id, tenant_id, storage_partition_id, user_id,
     source_kind, promotion_candidate_id)
SELECT
    gen_random_uuid(),
    candidate.id,
    candidate.tenant_id,
    candidate.storage_partition_id,
    candidate.user_id,
    'promotion_candidate',
    promotion.id
FROM learning_candidates AS candidate
JOIN learning_candidates AS promotion
  ON promotion.id = moa.try_uuid(candidate.payload ->> 'promotion_candidate_id')
 AND promotion.tenant_id = candidate.tenant_id
 AND promotion.storage_partition_id = candidate.storage_partition_id
WHERE candidate.proposal_kind = 'skill_rollback'
ON CONFLICT DO NOTHING;

-- Every candidate must now stand on at least one normalized source. A candidate
-- with none is unattributable: erasure could never reach it and export could
-- never explain it.
DO $$
DECLARE
    sourceless TEXT;
BEGIN
    SELECT string_agg(candidate.id::TEXT, ', ')
    INTO sourceless
    FROM learning_candidates AS candidate
    WHERE NOT EXISTS (
        SELECT 1 FROM learning_candidate_source AS source
        WHERE source.candidate_id = candidate.id
    );

    IF sourceless IS NOT NULL THEN
        RAISE EXCEPTION
            'V000360: learning candidates carry no normalized source and cannot be attributed or erased: %',
            sourceless;
    END IF;
END $$;

ALTER TABLE learning_candidates DROP COLUMN IF EXISTS source_experience_ids;
ALTER TABLE learning_log DROP COLUMN IF EXISTS source_refs;

-- Commit-time completeness. A statement-level constraint trigger cannot see a
-- candidate's sources at INSERT time (the producer writes them next), but it
-- can refuse to let the transaction commit without them. That is what stops the
-- insert-then-forget shape a plain NOT NULL cannot express.
CREATE OR REPLACE FUNCTION moa.assert_learning_candidate_sources_complete()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, public, moa
AS $$
DECLARE
    sourceless UUID;
BEGIN
    SELECT candidate.id
    INTO sourceless
    FROM learning_candidates AS candidate
    WHERE candidate.id = NEW.id
      AND NOT EXISTS (
          SELECT 1 FROM learning_candidate_source AS source
          WHERE source.candidate_id = candidate.id
      );

    IF sourceless IS NOT NULL THEN
        RAISE EXCEPTION
            'learning candidate % committed without any normalized source', sourceless
            USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER learning_candidate_sources_complete
AFTER INSERT ON learning_candidates
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION moa.assert_learning_candidate_sources_complete();
