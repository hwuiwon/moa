//! The single crate error type for the persistent KMS provider.

use thiserror::Error;

/// Errors raised while loading the deployment root key or talking to the KEK
/// store.
///
/// These are construction- and backend-level failures. When they surface through
/// the [`moa_crypto::KeyManagementProvider`] trait they are mapped onto
/// [`moa_crypto::Error::Backend`] (see [`From<KmsError> for moa_crypto::Error`]),
/// which deliberately never carries key material — only a human-readable message.
#[derive(Debug, Error)]
pub enum KmsError {
    /// The mounted root-key directory did not contain any usable key files.
    #[error("KMS root-key directory {0} contains no key files")]
    RootKeyDirectoryEmpty(String),

    /// The root-key material was not the expected length.
    #[error("KMS root key must be {expected} bytes, got {actual}")]
    RootKeyLength {
        /// Expected root-key length in bytes.
        expected: usize,
        /// Actual length observed.
        actual: usize,
    },

    /// The root-key material could not be base64-decoded.
    #[error("KMS root key is not valid base64")]
    RootKeyEncoding,

    /// A key filename was empty, non-UTF-8, duplicated, or reserved.
    #[error("invalid KMS root-key generation name: {0}")]
    RootKeyGenerationName(String),

    /// The configured or database-recorded root generation is not mounted.
    #[error("KMS root-key generation {0} is not mounted")]
    RootKeyGenerationMissing(String),

    /// The database-selected active generation differs from this pod's required
    /// generation, so the pod must not become ready.
    #[error("database active KMS generation is {active}, but this pod requires {required}")]
    RequiredGenerationInactive {
        /// Generation selected in shared database state.
        active: String,
        /// Generation required by this pod's configuration.
        required: String,
    },

    /// A retired generation cannot be activated again.
    #[error("KMS root-key generation {0} is retired")]
    RootKeyGenerationRetired(String),

    /// The active generation cannot be retired.
    #[error("active KMS root-key generation {0} cannot be retired")]
    ActiveGenerationRetirement(String),

    /// Live KEKs still depend on the generation being retired.
    #[error("KMS root-key generation {generation} still wraps {references} live KEKs")]
    RootKeyGenerationReferenced {
        /// Generation requested for retirement.
        generation: String,
        /// Number of live KEK references.
        references: i64,
    },

    /// A generation/state compare-and-swap lost a concurrent update.
    #[error("KMS root-key generation state changed concurrently")]
    GenerationConflict,

    /// A KEK-store query failed.
    #[error("KMS key store error: {0}")]
    Database(String),

    /// A stored wrapped KEK was structurally invalid (too short for its nonce
    /// prefix and sealed body).
    #[error("stored wrapped KEK is malformed")]
    MalformedWrappedKek,

    /// Wrapping a KEK under a mounted root key failed.
    #[error("KEK wrap failed")]
    KekWrap,

    /// Unwrapping a KEK under its recorded root generation failed because the
    /// material or stored wrapped value is corrupt.
    #[error("KEK unwrap failed: wrong root key or corrupt key material")]
    KekUnwrap,

    /// A key handle did not have the expected `pg-kek:<tenant>:<subject>:<kek>`
    /// shape.
    #[error("invalid KMS key handle")]
    InvalidHandle,
}

impl From<KmsError> for moa_crypto::Error {
    fn from(error: KmsError) -> Self {
        moa_crypto::Error::Backend(error.to_string())
    }
}
