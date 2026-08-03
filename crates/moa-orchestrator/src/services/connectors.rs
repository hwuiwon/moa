//! Authorized orchestration for tenant connector connection management.
//!
//! This module deliberately separates the secret-free management application
//! service from Restate and HTTP adapters. Public handlers and the private
//! credential ingress both supply an authenticated [`moa_core::traits::Identity`]; credential
//! plaintext and host-local staging tokens never enter this API.

use std::time::Duration;

const DESTINATION_ADMISSION_TIMEOUT: Duration = Duration::from_secs(10);
const CREDENTIAL_REVOCATION_HASH_DOMAIN: &str = "moa.connector.connection-credential-revoke.v1";
const CREDENTIAL_READINESS_HASH_DOMAIN: &str = "moa.connector.credential-readiness.v1";
const DEFAULT_CONNECTION_LIST_LIMIT: u16 = 50;

pub mod authz;
pub mod credentials;
pub mod definitions;
pub mod management;
pub(crate) mod restate;
mod wire;
