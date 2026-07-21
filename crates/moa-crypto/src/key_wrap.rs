//! Reviewed symmetric key-wrap framing shared by every MOA KMS provider.
//!
//! Wrapped keys are encoded as `nonce || ciphertext-with-tag` and bind caller
//! supplied additional authenticated data. Keeping this framing here prevents
//! providers from carrying subtly different copies of the AEAD implementation.

use zeroize::Zeroizing;

use crate::aead::{self, KEY_LEN, NONCE_LEN};
use crate::error::Error;

/// Length of a supported AES-256 wrapping key.
pub const WRAPPING_KEY_LEN: usize = KEY_LEN;

/// Generate a fresh AES-256 key with the operating-system CSPRNG.
#[must_use]
pub fn generate_key() -> Zeroizing<[u8; WRAPPING_KEY_LEN]> {
    Zeroizing::new(aead::random_key())
}

/// Wrap `plaintext` under `wrapping_key`, binding `aad`.
pub fn wrap_key(
    wrapping_key: &[u8; WRAPPING_KEY_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, Error> {
    let nonce = aead::random_nonce();
    let sealed = aead::seal(wrapping_key, &nonce, plaintext, aad)?;
    let mut framed = Vec::with_capacity(NONCE_LEN + sealed.len());
    framed.extend_from_slice(&nonce);
    framed.extend_from_slice(&sealed);
    Ok(framed)
}

/// Unwrap a `nonce || ciphertext-with-tag` key, requiring the same `aad`.
///
/// The opened bytes are zeroized on drop. Malformed framing is distinguished
/// from authentication failure without revealing any cryptographic detail.
pub fn unwrap_key(
    wrapping_key: &[u8; WRAPPING_KEY_LEN],
    wrapped: &[u8],
    aad: &[u8],
) -> Result<Zeroizing<Vec<u8>>, Error> {
    if wrapped.len() <= NONCE_LEN {
        return Err(Error::MalformedWrappedKey);
    }
    let (nonce, sealed) = wrapped.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| Error::MalformedWrappedKey)?;
    aead::open(wrapping_key, &nonce, sealed, aad).map(Zeroizing::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_key_round_trips_and_binds_aad_offline() {
        // Pins: the shared framing recovers the exact key only with matching AAD.
        let wrapping_key = generate_key();
        let child_key = [7_u8; WRAPPING_KEY_LEN];
        let wrapped = wrap_key(&wrapping_key, &child_key, b"tenant-a").expect("wrap");

        assert_eq!(
            unwrap_key(&wrapping_key, &wrapped, b"tenant-a")
                .expect("unwrap")
                .as_slice(),
            &child_key
        );
        assert!(matches!(
            unwrap_key(&wrapping_key, &wrapped, b"tenant-b"),
            Err(Error::Decryption)
        ));
    }

    #[test]
    fn malformed_wrapped_key_is_rejected_offline() {
        // Pins: a nonce-only frame never reaches the AEAD implementation.
        let wrapping_key = generate_key();
        assert!(matches!(
            unwrap_key(&wrapping_key, &[0_u8; NONCE_LEN], b"aad"),
            Err(Error::MalformedWrappedKey)
        ));
    }
}
