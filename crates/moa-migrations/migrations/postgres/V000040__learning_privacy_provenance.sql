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
--    provenance claim.
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
-- Cross-domain parent uniqueness required by provenance composite keys.
-- ---------------------------------------------------------------------------

ALTER TABLE contacts
    ADD CONSTRAINT contacts_id_partition_key
        UNIQUE (id, storage_partition_id);

CREATE UNIQUE INDEX artifact_revision_uid_partition_key
    ON moa.artifact_revision (revision_uid, storage_partition_id);

CREATE UNIQUE INDEX artifact_file_uid_partition_key
    ON moa.artifact_file (file_uid, storage_partition_id);

CREATE UNIQUE INDEX experiment_run_uid_partition_key
    ON moa.experiment_run (run_uid, storage_partition_id);

CREATE UNIQUE INDEX experiment_trial_uid_partition_key
    ON moa.experiment_trial (trial_uid, storage_partition_id);

CREATE UNIQUE INDEX score_run_id_partition_key
    ON analytics.score_run (run_id, storage_partition_id);

CREATE INDEX idx_learning_candidates_kind_status
    ON learning_candidates (tenant_id, proposal_kind, status, updated_at DESC);

-- Transitions, not just states. A CHECK constraint sees one row version; only a
-- trigger sees the pair, and the pair is where "an advisory item was quietly
-- promoted" lives. Repository-level compare-and-set remains in place as defense
-- in depth, but the authority is here so a direct SQL writer cannot bypass it.
CREATE FUNCTION moa.enforce_learning_candidate_transition() RETURNS TRIGGER
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

CREATE TRIGGER learning_candidate_transition
BEFORE UPDATE ON learning_candidates
FOR EACH ROW EXECUTE FUNCTION moa.enforce_learning_candidate_transition();

-- ---------------------------------------------------------------------------
-- Session-owned normalized provenance.
-- ---------------------------------------------------------------------------

CREATE TABLE learning_candidate_source (
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
    privacy_anchor_id     UUID GENERATED ALWAYS AS (
        CASE
            WHEN source_kind = 'event' THEN event_session_id
            ELSE COALESCE(
                experience_id,
                attribution_id,
                session_id,
                segment_id,
                contact_id,
                promotion_candidate_id,
                artifact_revision_uid,
                experiment_run_uid,
                experiment_trial_uid,
                score_run_id
            )
        END
    ) STORED NOT NULL,
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
CREATE UNIQUE INDEX learning_candidate_source_unique
    ON learning_candidate_source (
        candidate_id,
        source_kind,
        COALESCE(experience_id, attribution_id, session_id, event_id, segment_id,
                 contact_id, promotion_candidate_id, artifact_revision_uid,
                 experiment_run_uid, experiment_trial_uid, score_run_id)
    );

-- Every reverse traversal uses one typed discriminator plus one UUID equality.
-- Event provenance deliberately anchors on its session because subject closure
-- resolves events through their owning session rather than scanning event ids.
CREATE INDEX learning_candidate_source_privacy_anchor_idx
    ON learning_candidate_source (tenant_id, source_kind, privacy_anchor_id);
CREATE INDEX learning_candidate_source_scope_idx
    ON learning_candidate_source (storage_partition_id, scope, user_id);

CREATE TABLE learning_log_source (
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
    privacy_anchor_id    UUID GENERATED ALWAYS AS (
        COALESCE(
            candidate_id,
            experience_id,
            session_id,
            segment_id,
            artifact_revision_uid
        )
    ) STORED NOT NULL,
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

CREATE UNIQUE INDEX learning_log_source_unique
    ON learning_log_source (
        learning_id,
        source_kind,
        COALESCE(candidate_id, experience_id, session_id, segment_id, artifact_revision_uid)
    );
CREATE INDEX learning_log_source_privacy_anchor_idx
    ON learning_log_source (tenant_id, source_kind, privacy_anchor_id);
CREATE INDEX learning_log_source_scope_idx
    ON learning_log_source (storage_partition_id, scope, user_id);

-- Historical disposition of one review decision. Export reads this to answer
-- "what happened to the proposal derived from my data", which a mutable status
-- column cannot answer after the fact.
CREATE TABLE learning_candidate_decision (
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
    request_digest       BYTEA,
    outcome              JSONB,
    decided_at           TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT learning_candidate_decision_owner_fk
        FOREIGN KEY (candidate_id, tenant_id, storage_partition_id)
        REFERENCES learning_candidates (id, tenant_id, storage_partition_id),
    CONSTRAINT learning_candidate_decision_kind_valid
        CHECK (decision IN (
            'accepted_skill', 'accepted_rollback', 'rejected', 'dismissed'
        )),
    CONSTRAINT learning_candidate_decision_request_digest_len
        CHECK (request_digest IS NULL OR octet_length(request_digest) = 32),
    CONSTRAINT learning_candidate_decision_replay_evidence_complete
        CHECK ((request_digest IS NULL) = (outcome IS NULL))
);

-- Exactly one durable audit per decision per candidate: a replayed dismiss
-- writes nothing new instead of appending a second identical audit.
CREATE UNIQUE INDEX learning_candidate_decision_unique
    ON learning_candidate_decision (candidate_id, decision);
CREATE INDEX learning_candidate_decision_scope_idx
    ON learning_candidate_decision (storage_partition_id, scope, user_id);

SELECT moa.apply_three_tier_rls('learning_candidate_source'::REGCLASS);
SELECT moa.apply_three_tier_rls('learning_log_source'::REGCLASS);
SELECT moa.apply_three_tier_rls('learning_candidate_decision'::REGCLASS);

-- ---------------------------------------------------------------------------
-- Artifact-owned contributions: which derived bytes came from whose data.
-- ---------------------------------------------------------------------------

CREATE TABLE moa.artifact_revision_contribution (
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

CREATE UNIQUE INDEX artifact_revision_contribution_unique
    ON moa.artifact_revision_contribution (
        revision_uid, candidate_id, contribution_kind, COALESCE(file_uid, revision_uid)
    );
CREATE INDEX artifact_revision_contribution_candidate_idx
    ON moa.artifact_revision_contribution (tenant_id, candidate_id);
CREATE INDEX artifact_revision_contribution_revision_idx
    ON moa.artifact_revision_contribution (revision_uid);

-- Regression-suite bytes used to live inside `learning_candidates.payload` as
-- JSON strings, which put attributable generated text in a column nothing could
-- enumerate, join, or erase. They now belong to the artifact owner, which is
-- also the only component that can assemble review input from them.
CREATE TABLE moa.artifact_suite_contribution (
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

CREATE UNIQUE INDEX artifact_suite_contribution_unique
    ON moa.artifact_suite_contribution (candidate_id, suite_kind, suite_name);
CREATE INDEX artifact_suite_contribution_session_idx
    ON moa.artifact_suite_contribution (tenant_id, source_session_id)
    WHERE source_session_id IS NOT NULL;
CREATE INDEX artifact_suite_contribution_experience_idx
    ON moa.artifact_suite_contribution (tenant_id, source_experience_id)
    WHERE source_experience_id IS NOT NULL;

SELECT moa.apply_three_tier_rls('moa.artifact_revision_contribution'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_suite_contribution'::REGCLASS);

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
CREATE FUNCTION moa.reject_learning_contribution_during_destruction()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog, public, moa
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence AS fence
        WHERE fence.tenant_id = NEW.tenant_id::UUID
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

CREATE TABLE moa.privacy_erasure_record_decision (
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

CREATE UNIQUE INDEX privacy_erasure_record_decision_unique
    ON moa.privacy_erasure_record_decision
       (tenant_id, subject_user_id, attempt_id, record_kind, record_id);
CREATE INDEX privacy_erasure_record_decision_subject_idx
    ON moa.privacy_erasure_record_decision
       (tenant_id, subject_user_id, decided_at);

SELECT moa.apply_tenant_rls('moa.privacy_erasure_record_decision'::REGCLASS);

-- Commit-time completeness lets producers insert an owner and its sources in
-- separate statements, while still making attribution a database invariant.
-- Source mutation triggers also protect the reverse direction: moving or
-- deleting the final source fails unless the owner disappears in the same
-- transaction.
CREATE FUNCTION moa.assert_learning_candidate_sources_complete()
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

CREATE FUNCTION moa.assert_learning_candidate_source_owner_complete()
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
    WHERE candidate.id = OLD.candidate_id
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

CREATE CONSTRAINT TRIGGER learning_candidate_source_owner_complete
AFTER UPDATE OR DELETE ON learning_candidate_source
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION moa.assert_learning_candidate_source_owner_complete();

CREATE FUNCTION moa.assert_learning_log_sources_complete()
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

CREATE CONSTRAINT TRIGGER learning_log_sources_complete
AFTER INSERT ON learning_log
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION moa.assert_learning_log_sources_complete();

CREATE FUNCTION moa.assert_learning_log_source_owner_complete()
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
    WHERE entry.id = OLD.learning_id
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

CREATE CONSTRAINT TRIGGER learning_log_source_owner_complete
AFTER UPDATE OR DELETE ON learning_log_source
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION moa.assert_learning_log_source_owner_complete();
