DROP TABLE IF EXISTS scim_group_members;
DROP TABLE IF EXISTS scim_groups;

DROP INDEX IF EXISTS idx_users_tenant_active;
DROP INDEX IF EXISTS idx_users_external_id;
DROP INDEX IF EXISTS idx_users_tenant_external_id_unique;
DROP INDEX IF EXISTS idx_users_tenant_email_unique;
