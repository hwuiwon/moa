-- Marks the fresh-install-only contiguous migration epoch.
--
-- The history row written by Refinery is the durable marker. This statement
-- intentionally owns no schema: databases from the retired sparse epoch are
-- rejected by the runner before any migration DDL can execute.

SELECT 1;
