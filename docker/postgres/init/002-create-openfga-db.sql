-- Create the OpenFGA database if it does not already exist.
-- Postgres init scripts run only on first volume initialization; this is
-- intentionally idempotent for that boundary.
SELECT 'CREATE DATABASE openfga OWNER moa_owner'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'openfga')\gexec
