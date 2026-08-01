-- Persist the canonical request and committed outcome used to make terminal
-- learning-review retries deterministic. Existing historical decisions remain
-- readable but cannot be replayed because they predate this evidence.

ALTER TABLE learning_candidate_decision
    ADD COLUMN request_digest BYTEA,
    ADD COLUMN outcome JSONB;

ALTER TABLE learning_candidate_decision
    ADD CONSTRAINT learning_candidate_decision_request_digest_len
        CHECK (request_digest IS NULL OR octet_length(request_digest) = 32),
    ADD CONSTRAINT learning_candidate_decision_replay_evidence_complete
        CHECK ((request_digest IS NULL) = (outcome IS NULL));
