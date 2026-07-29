-- Tenant-owned MCP dispatch never had a production create/bind lifecycle.
-- Remove its unreachable schema and material kind rather than preserving a
-- second credential ownership model beside deployment-owned connectors.
DROP TABLE IF EXISTS tenant_mcp_connection_bindings;

DELETE FROM tenant_credential_operations
WHERE kind = 'mcp_bearer';

DELETE FROM tenant_credential_versions
WHERE kind = 'mcp_bearer';

ALTER TABLE tenant_credential_versions
    DROP CONSTRAINT IF EXISTS tenant_credential_versions_kind_valid;

ALTER TABLE tenant_credential_versions
    ADD CONSTRAINT tenant_credential_versions_kind_valid
    CHECK (kind IN ('provider_api_key', 'oauth'));
