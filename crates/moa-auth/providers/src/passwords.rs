//! Password hashing helpers for first-party local users.

use argon2::password_hash::{PasswordHash, SaltString, rand_core::OsRng as SaltOsRng};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use thiserror::Error;

/// Password hashing or verification failure.
#[derive(Debug, Error)]
pub enum PasswordError {
    /// Argon2 returned an unexpected hashing failure.
    #[error("hash error: {0}")]
    Hash(String),
}

/// Hash a plaintext password using Argon2id PHC encoding.
pub fn hash_password(password: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut SaltOsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| PasswordError::Hash(error.to_string()))
}

/// Verify a plaintext password against a stored Argon2 PHC hash.
pub fn verify_password(password: &str, phc: &str) -> Result<bool, PasswordError> {
    let parsed = PasswordHash::new(phc).map_err(|error| PasswordError::Hash(error.to_string()))?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(error) => Err(PasswordError::Hash(error.to_string())),
    }
}
