//! Request-scoped, provenance-aware DLP tokenization for provider egress.
//!
//! ```
//! use moa_dlp::{
//!     TokenDestination, TokenSource, TokenSourceRole, TokenVisibility, detokenize, tokenize,
//! };
//! use moa_memory_pii::{PiiCategory, PiiSpan};
//!
//! let text = "Email jane@acme.com";
//! let spans = [PiiSpan::new(6, 19, PiiCategory::Email, 0.9)];
//! let source = TokenSource::new(
//!     TokenVisibility::Visible,
//!     TokenSourceRole::User,
//!     "messages[0].content",
//! );
//! let (tokenized, vault) = tokenize(text, &spans, source)?;
//! assert_eq!(
//!     detokenize(&tokenized, &vault, TokenDestination::VisibleOutput)?,
//!     text
//! );
//! # Ok::<(), moa_dlp::Error>(())
//! ```

pub mod error;
pub mod vault;

pub use error::{Error, Result};
pub use vault::{
    TOKEN_CLOSE, TOKEN_OPEN, TokenDestination, TokenSource, TokenSourceRole, TokenVault,
    TokenVisibility,
};

use moa_memory_pii::PiiSpan;

/// Tokenizes one string in a fresh request-scoped vault.
pub fn tokenize(
    text: &str,
    spans: &[PiiSpan],
    source: TokenSource,
) -> Result<(String, TokenVault)> {
    let mut vault = TokenVault::new()?;
    let tokenized = vault.tokenize(text, spans, source)?;
    Ok((tokenized, vault))
}

/// Restores known tokens according to their provenance and destination.
pub fn detokenize(text: &str, vault: &TokenVault, destination: TokenDestination) -> Result<String> {
    vault.restore(text, destination)
}

#[cfg(test)]
mod tests {
    use moa_memory_pii::{PiiCategory, PiiSpan};

    use super::*;

    fn source(visibility: TokenVisibility, field: &str) -> TokenSource {
        TokenSource::new(visibility, TokenSourceRole::User, field)
    }

    fn span(text: &str, needle: &str) -> PiiSpan {
        let start = text.find(needle).expect("test needle exists");
        PiiSpan::new(start, start + needle.len(), PiiCategory::Secret, 0.99)
    }

    #[test]
    fn visible_values_round_trip_exactly() {
        // Pins: caller-visible protected values may be reconstructed in visible output.
        let text = "key sk-visible";
        let (tokenized, vault) = tokenize(
            text,
            &[span(text, "sk-visible")],
            source(TokenVisibility::Visible, "messages[0].content"),
        )
        .expect("tokenize visible value");
        assert!(!tokenized.contains("sk-visible"));
        assert_eq!(
            vault
                .restore(&tokenized, TokenDestination::VisibleOutput)
                .expect("restore visible value"),
            text
        );
    }

    #[test]
    fn request_namespaces_make_tokens_unpredictable() {
        // Pins: identical data in different requests must not mint a stable correlation handle.
        let text = "sk-secret";
        let source = source(TokenVisibility::Visible, "messages[0].content");
        let (first, _) = tokenize(text, &[span(text, text)], source.clone()).expect("first vault");
        let (second, _) = tokenize(text, &[span(text, text)], source).expect("second vault");
        assert_ne!(first, second);
        assert!(first.starts_with("⟦MOA_DLP_"));
        assert!(first.ends_with("_SECRET_1⟧"));
    }

    #[test]
    fn span_validation_is_atomic() {
        // Pins: invalid classifier offsets never partly mutate a vault or consume a counter.
        let mut vault = TokenVault::new().expect("vault");
        let source = source(TokenVisibility::Visible, "messages[0].content");
        let invalid = [
            PiiSpan::new(1, 1, PiiCategory::Secret, 0.9),
            PiiSpan::new(3, 2, PiiCategory::Secret, 0.9),
            PiiSpan::new(0, 99, PiiCategory::Secret, 0.9),
        ];
        for span in invalid {
            assert!(vault.tokenize("abc", &[span], source.clone()).is_err());
            assert!(vault.is_empty());
        }
        assert!(matches!(
            vault.tokenize(
                "é",
                &[PiiSpan::new(0, 2, PiiCategory::Secret, 0.9)],
                source.clone()
            ),
            Err(Error::NonUtf8Boundary { .. })
        ));
        assert!(matches!(
            vault.tokenize(
                "abcd",
                &[
                    PiiSpan::new(0, 3, PiiCategory::Secret, 0.9),
                    PiiSpan::new(2, 4, PiiCategory::Secret, 0.9)
                ],
                source.clone()
            ),
            Err(Error::OverlappingSpans { .. })
        ));
        let tokenized = vault
            .tokenize("abc", &[span("abc", "abc")], source)
            .expect("valid tokenization after failures");
        assert!(tokenized.ends_with("_SECRET_1⟧"));
    }

    #[test]
    fn reserved_token_syntax_is_rejected() {
        // Pins: a caller cannot smuggle a vault-looking token into cleartext input.
        let mut vault = TokenVault::new().expect("vault");
        assert!(matches!(
            vault.tokenize(
                "literal ⟦token⟧",
                &[],
                source(TokenVisibility::Visible, "messages[0].content")
            ),
            Err(Error::LiteralTokenSyntax)
        ));
    }

    #[test]
    fn identical_hidden_and_visible_values_have_distinct_destinations() {
        // Pins: plaintext equality cannot erase provenance or permit a hidden echo.
        let mut vault = TokenVault::new().expect("vault");
        let text = "sk-shared";
        let visible = vault
            .tokenize(
                text,
                &[span(text, text)],
                source(TokenVisibility::Visible, "messages[0].content"),
            )
            .expect("visible token");
        let hidden_source = TokenSource::new(
            TokenVisibility::Hidden,
            TokenSourceRole::System,
            "messages[1].content",
        );
        let hidden = vault
            .tokenize(text, &[span(text, text)], hidden_source)
            .expect("hidden token");
        assert_ne!(visible, hidden);
        assert_eq!(
            vault
                .restore(
                    &format!("{visible} {hidden}"),
                    TokenDestination::VisibleOutput
                )
                .expect("visible destination"),
            "sk-shared [REDACTED]"
        );
        assert!(matches!(
            vault.restore(&hidden, TokenDestination::ToolArgument),
            Err(Error::DestinationDenied {
                role: TokenSourceRole::System,
                destination: TokenDestination::ToolArgument,
                ..
            })
        ));
    }

    #[test]
    fn token_identity_includes_exact_source() {
        // Pins: repeated values share a token only when their provenance is identical.
        let mut vault = TokenVault::new().expect("vault");
        let text = "sk-repeat";
        let source = source(TokenVisibility::Visible, "messages[0].content");
        let first = vault
            .tokenize(text, &[span(text, text)], source.clone())
            .expect("first");
        let second = vault
            .tokenize(text, &[span(text, text)], source)
            .expect("second");
        assert_eq!(first, second);
        assert_eq!(vault.len(), 1);
    }

    #[test]
    fn debug_and_classification_view_do_not_expose_originals() {
        // Pins: diagnostics and residual scans cannot reveal or reclassify vault contents.
        let text = "sk-topsecret";
        let (tokenized, vault) = tokenize(
            text,
            &[span(text, text)],
            source(TokenVisibility::Visible, "messages[0].content"),
        )
        .expect("tokenize");
        assert!(!format!("{vault:?}").contains(text));
        assert_eq!(vault.classification_view(&tokenized), "[DLP_TOKEN]");
    }

    #[test]
    fn empty_spans_are_an_identity_transform() {
        // Pins: confidently safe fields pass through without adding vault state.
        let (text, vault) = tokenize(
            "safe",
            &[],
            source(TokenVisibility::Visible, "messages[0].content"),
        )
        .expect("safe text");
        assert_eq!(text, "safe");
        assert!(vault.is_empty());
    }
}
