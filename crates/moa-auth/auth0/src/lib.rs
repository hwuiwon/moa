//! Auth0 and generic OIDC authentication provider implementations for MOA.
//!
//! This crate is compiled only when the parent `moa-auth-providers` crate's
//! `auth0` feature is enabled, or when testing this provider crate directly.

pub mod auth0_provider;
pub mod ciba;
pub mod group_sync;
pub mod jwks_cache;
pub mod oidc_provider;
pub mod vault;

pub use auth0_provider::{Auth0AuthProvider, resolve_or_provision_static};
pub use ciba::Auth0AsyncAuthzProvider;
pub use group_sync::{Auth0GroupReader, GroupSyncError, IdpGroupReader, OidcGroupSync};
pub use oidc_provider::OidcAuthProvider;
pub use vault::Auth0TokenVaultProvider;
