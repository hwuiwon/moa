//! DB-backed auth-provider tests, consolidated into one harness binary.

#[path = "auth_providers_db/api_keys_lifecycle_db.rs"]
mod api_keys_lifecycle_db;
#[path = "auth_providers_db/builtin_authz_request_db.rs"]
mod builtin_authz_request_db;
