//! Local and optional authentication provider implementations for MOA.

pub mod api_keys;
#[cfg(feature = "auth0")]
pub mod auth0;
pub mod builtin_authz;
pub mod bundle;
pub mod contact_tokens;
pub mod disabled;
pub mod local;
pub mod oauth_access_token;
pub mod oauth_as;
pub mod passwords;
pub mod postgres_credential_vault;
pub mod user_sessions;

pub use api_keys::{
    ApiKeyError, CreateApiKeyRequest, CreateApiKeyResponse, Env, IssuedKey, KeyListItem, KeyOwner,
    NewApiKey, ResolvedKey, create, generate, parse_parts, prefix_of, revoke, validate,
};
pub use builtin_authz::BuiltinAsyncAuthzProvider;
pub use bundle::{
    BuildError, build_async_authz_provider, build_auth_provider, build_contact_token_issuer,
};
pub use contact_tokens::{ContactTokenError, ContactTokenIssuer, ContactTokenVerifier};
pub use disabled::DisabledAuthProvider;
pub use local::LocalAuthProvider;
pub use oauth_access_token::{OAuthAccessTokenProvider, looks_like_oauth_access_token};
pub use oauth_as::{
    AuthorizationRequest, AuthorizationSubject, CodeChallengeMethod, CodeExchangeRequest,
    IntrospectionResponse, IssuedAuthorizationCode, OAuthClient, OAuthClientRegistry, OAuthError,
    OAuthServer, OAuthStore, ResolvedAccessToken, TokenGrant,
};
pub use passwords::{PasswordError, hash_password, verify_password};
pub use postgres_credential_vault::PostgresCredentialVault;
pub use user_sessions::{
    IssuedUserSessionToken, NewUserSessionToken, ResolvedUserSessionToken, UserSessionTokenError,
    looks_like_user_session_token,
};
