ALTER TABLE action_policy_rules
    DROP CONSTRAINT IF EXISTS action_policy_rules_scope_check;

ALTER TABLE action_policy_rules
    ADD CONSTRAINT action_policy_rules_scope_check
        CHECK (scope IN ('tenant', 'contact'));

ALTER TABLE action_policy_rules
    DROP CONSTRAINT IF EXISTS action_policy_rules_global_partition_check;

ALTER TABLE action_policy_rules
    DROP CONSTRAINT IF EXISTS action_policy_rules_tenant_storage_partition_check;

ALTER TABLE action_policy_rules
    ADD CONSTRAINT action_policy_rules_storage_partition_check
        CHECK (storage_partition_id <> 'global');
