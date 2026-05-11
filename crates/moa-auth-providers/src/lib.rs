//! Local and optional authentication provider implementations for MOA.

pub mod api_keys;
pub mod builtin_authz;
pub mod bundle;
pub mod local;
pub mod null_vault;
pub mod schema;

#[cfg(feature = "auth0")]
pub use moa_auth_providers_auth0 as auth0;

pub use api_keys::{
    ApiKeyError, CreateApiKeyRequest, CreateApiKeyResponse, Env, IssuedKey, KeyListItem, KeyOwner,
    NewApiKey, ResolvedKey, create, generate, parse_parts, prefix_of, revoke, validate,
};
pub use builtin_authz::BuiltinAsyncAuthzProvider;
pub use bundle::{BuildError, Providers, build_providers, build_providers_with_resolver};
pub use local::LocalAuthProvider;
pub use null_vault::NullTokenVaultProvider;
