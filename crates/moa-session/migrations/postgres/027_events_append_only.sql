REVOKE UPDATE, DELETE, TRUNCATE ON TABLE events FROM moa_app;

CREATE OR REPLACE FUNCTION events_append_only_guard() RETURNS trigger AS $$
BEGIN
  RAISE EXCEPTION 'events table is append-only (op=%, session=%, seq=%)',
    TG_OP, OLD.session_id, OLD.sequence_num
    USING ERRCODE = 'P0001';
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS events_no_update ON events;
CREATE TRIGGER events_no_update
  BEFORE UPDATE ON events
  FOR EACH ROW EXECUTE FUNCTION events_append_only_guard();

DROP TRIGGER IF EXISTS events_no_delete ON events;
CREATE TRIGGER events_no_delete
  BEFORE DELETE ON events
  FOR EACH ROW EXECUTE FUNCTION events_append_only_guard();
