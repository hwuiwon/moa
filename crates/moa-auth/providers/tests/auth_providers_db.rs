//! DB-backed auth-provider tests, consolidated into one harness binary.

#[path = "auth_providers_db/support/mod.rs"]
mod support;

#[cfg(feature = "auth0")]
#[path = "auth0_support/mod.rs"]
mod auth0_support;

#[path = "auth_providers_db/api_keys_lifecycle_db.rs"]
mod api_keys_lifecycle_db;
#[cfg(feature = "auth0")]
#[path = "auth_providers_db/auth0_ciba_db.rs"]
mod auth0_ciba_db;
#[cfg(feature = "auth0")]
#[path = "auth_providers_db/auth0_jwt_validation_db.rs"]
mod auth0_jwt_validation_db;
#[path = "auth_providers_db/builtin_authz_request_db.rs"]
mod builtin_authz_request_db;
#[path = "auth_providers_db/oauth_access_token_auth_db.rs"]
mod oauth_access_token_auth_db;
#[path = "auth_providers_db/oauth_authorization_server_db.rs"]
mod oauth_authorization_server_db;
#[path = "auth_providers_db/tenant_credential_vault_db.rs"]
mod tenant_credential_vault_db;
