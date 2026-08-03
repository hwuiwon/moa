//! DB-backed Auth0 provider tests, consolidated into one harness binary.

#[path = "support.rs"]
mod support;

#[path = "auth_providers_auth0_db/ciba_db.rs"]
mod ciba_db;
#[path = "auth_providers_auth0_db/jwt_validation_db.rs"]
mod jwt_validation_db;
