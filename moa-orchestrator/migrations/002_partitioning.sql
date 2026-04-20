-- Optional production-only pg_partman bootstrap.
--
-- The current development schema still uses a non-partitioned `events` table, so
-- this migration intentionally no-ops unless `public.events` has already been
-- converted into a partitioned parent table by an operator-managed migration.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM pg_partitioned_table partitioned
        JOIN pg_class relation ON relation.oid = partitioned.partrelid
        JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname = 'public'
          AND relation.relname = 'events'
    ) THEN
        RAISE NOTICE 'public.events is already partitioned; apply pg_partman bootstrap separately';
    ELSE
        RAISE NOTICE 'Skipping pg_partman bootstrap because public.events is not partitioned yet';
    END IF;
END
$$;
