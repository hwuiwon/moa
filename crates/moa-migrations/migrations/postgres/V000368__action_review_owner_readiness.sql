ALTER TABLE tenant_action_reviews
    ADD COLUMN owner_registered_at TIMESTAMPTZ,
    ADD COLUMN owner_release_delivered_at TIMESTAMPTZ;

ALTER TABLE tenant_action_reviews
    DROP COLUMN requested_event_recorded_at,
    DROP COLUMN decision_event_recorded_at;

CREATE INDEX idx_tenant_action_reviews_owner_release
    ON tenant_action_reviews(created_at, id)
    WHERE status = 'timeout'
      AND owner_registered_at IS NOT NULL
      AND owner_release_delivered_at IS NULL;
