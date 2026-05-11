//! Local and optional authentication provider implementations for MOA.

pub mod api_keys;
pub mod local;
pub mod schema;

pub use api_keys::{
    ApiKeyError, CreateApiKeyRequest, CreateApiKeyResponse, Env, IssuedKey, KeyListItem, KeyOwner,
    NewApiKey, ResolvedKey, create, generate, parse_parts, prefix_of, revoke, validate,
};
pub use local::LocalAuthProvider;
