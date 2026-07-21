//! PKCE (Proof Key for Code Exchange, RFC 7636) verification.
//!
//! Only the `S256` challenge method is supported. OAuth 2.1 forbids the `plain`
//! method for confidential exchanges, so a `plain` (or any unknown) method is
//! rejected rather than downgraded.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Minimum length of a PKCE `code_verifier` / `code_challenge` (RFC 7636 §4.1).
const MIN_VERIFIER_LEN: usize = 43;
/// Maximum length of a PKCE `code_verifier` / `code_challenge` (RFC 7636 §4.1).
const MAX_VERIFIER_LEN: usize = 128;

/// Supported PKCE code-challenge method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeChallengeMethod {
    /// `code_challenge = BASE64URL(SHA256(code_verifier))`.
    S256,
}

impl CodeChallengeMethod {
    /// Parse the `code_challenge_method` request parameter.
    ///
    /// Returns `None` for `plain` and any unknown value, which callers must
    /// treat as a hard rejection (fail closed).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "S256" => Some(Self::S256),
            _ => None,
        }
    }

    /// The canonical string form persisted alongside the authorization code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::S256 => "S256",
        }
    }
}

/// Return whether a presented `code_challenge` is structurally valid.
///
/// The challenge must fall within the RFC 7636 length bounds. This is a cheap
/// pre-check at `/authorize`; the true binding is enforced at `/token` by
/// [`verify_code_challenge`].
#[must_use]
pub fn is_valid_code_challenge(challenge: &str) -> bool {
    (MIN_VERIFIER_LEN..=MAX_VERIFIER_LEN).contains(&challenge.len())
        && challenge.bytes().all(is_unreserved)
}

/// Verify a PKCE `code_verifier` against the stored `code_challenge`.
///
/// Returns `true` only when the base64url-unpadded SHA-256 of the verifier
/// constant-time-equals the challenge. Fails closed on an out-of-range verifier
/// length. The comparison is constant-time to avoid leaking how many leading
/// characters matched.
#[must_use]
pub fn verify_code_challenge(challenge: &str, method: CodeChallengeMethod, verifier: &str) -> bool {
    match method {
        CodeChallengeMethod::S256 => {
            if !(MIN_VERIFIER_LEN..=MAX_VERIFIER_LEN).contains(&verifier.len()) {
                return false;
            }
            let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            computed.as_bytes().ct_eq(challenge.as_bytes()).into()
        }
    }
}

/// Whether `byte` is in the PKCE unreserved character set (RFC 7636 §4.1):
/// `ALPHA / DIGIT / "-" / "." / "_" / "~"`.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compute the canonical S256 challenge for a verifier, matching a client.
    fn challenge_for(verifier: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }

    #[test]
    fn s256_correct_verifier_passes() {
        // Pins: the exact verifier that produced the challenge verifies.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = challenge_for(verifier);
        assert!(verify_code_challenge(
            &challenge,
            CodeChallengeMethod::S256,
            verifier
        ));
    }

    #[test]
    fn s256_wrong_verifier_fails() {
        // Pins: a different verifier (even same length) is rejected.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = challenge_for(verifier);
        let wrong = "zzjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(wrong.len(), verifier.len());
        assert!(!verify_code_challenge(
            &challenge,
            CodeChallengeMethod::S256,
            wrong
        ));
    }

    #[test]
    fn short_verifier_is_rejected() {
        // Pins: an out-of-range verifier length fails closed before hashing.
        let challenge = challenge_for("short");
        assert!(!verify_code_challenge(
            &challenge,
            CodeChallengeMethod::S256,
            "short"
        ));
    }

    #[test]
    fn plain_method_is_unsupported() {
        // Pins: OAuth 2.1 forbids `plain`; parsing it returns None (hard reject).
        assert_eq!(CodeChallengeMethod::parse("plain"), None);
        assert_eq!(CodeChallengeMethod::parse("s256"), None);
        assert_eq!(
            CodeChallengeMethod::parse("S256"),
            Some(CodeChallengeMethod::S256)
        );
    }

    #[test]
    fn challenge_shape_validation() {
        // Pins: `/authorize` accepts a well-formed challenge and rejects junk.
        let good = challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert!(is_valid_code_challenge(&good));
        assert!(!is_valid_code_challenge("too-short"));
        assert!(!is_valid_code_challenge(&"a".repeat(200)));
        assert!(!is_valid_code_challenge(&format!("{good}/has+bad=chars")));
    }
}
