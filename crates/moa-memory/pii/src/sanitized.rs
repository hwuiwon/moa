//! Opaque, fallible sanitization primitive for automatic-learning boundaries.
//!
//! Everything MOA learns from without a human in the loop — skill distillation,
//! skill improvement, sibling re-synthesis, regression-suite generation, and the
//! embeddings that route them — must be built from irreversibly redacted content.
//! This module is the single place where "raw text" becomes something those paths
//! are allowed to hold.
//!
//! [`SanitizedText`] has private fields, no raw-string constructor, no
//! `From<String>`, and no `Deserialize`, so it cannot be forged from attacker- or
//! caller-controlled bytes and cannot be reconstituted from a wire payload. The
//! only way to obtain one is [`sanitize_with`], which runs the full rejection
//! list before it returns.
//!
//! This is deliberately *not* the reversible request-scoped tokenization in the
//! `moa-providers` provider-governance layer. A DLP token is a placeholder that a
//! later restoration step can turn back into the original value; sanitization
//! here is one-way and the original bytes are never recoverable from the result.
//! Because the two mechanisms must never be confused, text that already carries
//! the reserved DLP delimiters is rejected outright rather than sanitized: a
//! reversible token reaching a learning corpus would smuggle a restorable secret
//! into a durable artifact.

use moa_core::types::security::SensitivityClass;

use crate::{PiiCategory, PiiClassifier, PiiResult, PiiSpan, redact_text, redaction_replacement};

/// Opening delimiter reserved for reversible request-scoped DLP tokens.
///
/// Owned here rather than in `moa-providers` because provider governance already
/// depends on this crate, and the sanitizer must be able to refuse text that
/// carries a reversible token without the dependency edge running backwards.
pub const RESERVED_DLP_TOKEN_OPEN: char = '⟦';

/// Closing delimiter reserved for reversible request-scoped DLP tokens.
pub const RESERVED_DLP_TOKEN_CLOSE: char = '⟧';

/// Every PII category, used to recognize this crate's own redaction placeholders.
const ALL_CATEGORIES: [PiiCategory; 11] = [
    PiiCategory::Person,
    PiiCategory::Email,
    PiiCategory::Phone,
    PiiCategory::Address,
    PiiCategory::Ssn,
    PiiCategory::MedicalRecord,
    PiiCategory::FinancialAccount,
    PiiCategory::GovernmentId,
    PiiCategory::Url,
    PiiCategory::Date,
    PiiCategory::Secret,
];

/// Stable reason one sanitization attempt was refused.
///
/// Every variant is fieldless on purpose. These reasons are surfaced in durable
/// workflow errors and log lines, so they must never be able to carry the input
/// text, a span excerpt, or a classifier's own error string — any of which would
/// re-leak exactly what the sanitizer refused to release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum SanitizationRejection {
    /// The input already carried the reserved reversible DLP token delimiters.
    #[error("sanitization_rejected: reserved_dlp_token")]
    ReservedDlpToken,
    /// The classifier failed. The source error is intentionally discarded.
    #[error("sanitization_rejected: classifier_error")]
    ClassifierError,
    /// The classifier abstained or returned its fail-closed fallback.
    #[error("sanitization_rejected: classifier_abstained")]
    ClassifierAbstained,
    /// The content classified as restricted, which never proceeds to learning.
    #[error("sanitization_rejected: restricted_class")]
    RestrictedClass,
    /// A detected span carried the secret/credential category.
    #[error("sanitization_rejected: secret_category")]
    SecretCategory,
    /// A span was empty or inverted, so the region to redact is undefined.
    #[error("sanitization_rejected: malformed_span")]
    MalformedSpan,
    /// A span extended past the end of the classified text.
    #[error("sanitization_rejected: span_out_of_range")]
    SpanOutOfRange,
    /// A span boundary fell inside a multi-byte UTF-8 character.
    #[error("sanitization_rejected: span_not_char_boundary")]
    SpanNotCharBoundary,
    /// Two spans overlapped, so one region would be only partially redacted.
    #[error("sanitization_rejected: overlapping_spans")]
    OverlappingSpans,
    /// Re-classifying the redacted text still found sensitive content.
    #[error("sanitization_rejected: residual_sensitivity")]
    ResidualSensitivity,
}

impl SanitizationRejection {
    /// Returns the stable machine-readable reason code.
    ///
    /// Callers persist and log this code; it is part of the observable contract
    /// and must stay stable across detector and policy changes.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ReservedDlpToken => "reserved_dlp_token",
            Self::ClassifierError => "classifier_error",
            Self::ClassifierAbstained => "classifier_abstained",
            Self::RestrictedClass => "restricted_class",
            Self::SecretCategory => "secret_category",
            Self::MalformedSpan => "malformed_span",
            Self::SpanOutOfRange => "span_out_of_range",
            Self::SpanNotCharBoundary => "span_not_char_boundary",
            Self::OverlappingSpans => "overlapping_spans",
            Self::ResidualSensitivity => "residual_sensitivity",
        }
    }
}

/// Irreversibly redacted text plus the classification that produced it.
///
/// Constructible only by [`sanitize_with`]. There is deliberately no
/// `From<String>`, no `new`, and no `Deserialize`: a type that could be built
/// from a bare string would let a caller declare raw transcript content
/// "sanitized" by assertion, which is the exact failure this type exists to make
/// unrepresentable.
#[derive(Clone, PartialEq, Eq)]
pub struct SanitizedText {
    redacted: String,
    class: SensitivityClass,
    categories: Vec<PiiCategory>,
    detector_version: String,
}

impl SanitizedText {
    /// Returns the redacted content.
    #[must_use]
    pub fn redacted(&self) -> &str {
        &self.redacted
    }

    /// Consumes the value and returns the redacted content.
    #[must_use]
    pub fn into_redacted(self) -> String {
        self.redacted
    }

    /// Returns the sensitivity class the classifier assigned to the original text.
    ///
    /// This is the *original* classification, retained as provenance: a reviewer
    /// needs to know that a clean-looking line was redacted down from PII.
    #[must_use]
    pub const fn class(&self) -> SensitivityClass {
        self.class
    }

    /// Returns the sorted, deduplicated categories that were redacted.
    #[must_use]
    pub fn categories(&self) -> &[PiiCategory] {
        &self.categories
    }

    /// Returns the classifier model and serving version that produced the result.
    #[must_use]
    pub fn detector_version(&self) -> &str {
        &self.detector_version
    }
}

impl std::fmt::Debug for SanitizedText {
    /// Renders provenance only.
    ///
    /// The redacted content is safe by construction, but a `Debug` that prints it
    /// would make every incidental log line a copy of the learning corpus. The
    /// fields here are the ones a reader actually needs when debugging a
    /// rejection or a provenance mismatch.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SanitizedText")
            .field("class", &self.class)
            .field("categories", &self.categories)
            .field("detector_version", &self.detector_version)
            .field("redacted_len", &self.redacted.len())
            .finish()
    }
}

/// Sanitizes one input string, or refuses with a stable reason code.
///
/// The full gate, in order:
///
/// 1. Text carrying the reserved reversible DLP delimiters is refused before any
///    classification, because a restorable token must never enter a learning
///    corpus.
/// 2. A classifier error or abstention is a refusal, never a pass-through: an
///    unavailable detector must not become an implicit "no PII found".
/// 3. Restricted content and any secret-category span are refused outright. PII
///    and PHI proceed, but only through irreversible redaction.
/// 4. Spans are validated before they are applied, so a malformed, out-of-range,
///    non-char-boundary, or overlapping span refuses instead of silently
///    redacting the wrong region — or nothing at all.
/// 5. The redacted text is re-classified, and anything still sensitive refuses.
///    This is what catches a detector that found one of two occurrences.
pub async fn sanitize_with(
    classifier: &dyn PiiClassifier,
    text: &str,
) -> Result<SanitizedText, SanitizationRejection> {
    if contains_reserved_dlp_delimiter(text) {
        return Err(SanitizationRejection::ReservedDlpToken);
    }
    let result = classifier
        .classify(text)
        .await
        .map_err(|_| SanitizationRejection::ClassifierError)?;
    let sanitized = sanitize_classified(text, &result)?;
    let residual = classifier
        .classify(sanitized.redacted())
        .await
        .map_err(|_| SanitizationRejection::ClassifierError)?;
    ensure_no_residual_sensitivity(sanitized.redacted(), &residual)?;
    Ok(sanitized)
}

/// Returns whether text carries either reserved reversible DLP token delimiter.
#[must_use]
pub fn contains_reserved_dlp_delimiter(text: &str) -> bool {
    text.contains(RESERVED_DLP_TOKEN_OPEN) || text.contains(RESERVED_DLP_TOKEN_CLOSE)
}

/// Applies the class, category, and span gates to one classifier result.
///
/// Split from [`sanitize_with`] so the deterministic policy can be exercised
/// against hand-built results without an async classifier.
fn sanitize_classified(
    text: &str,
    result: &PiiResult,
) -> Result<SanitizedText, SanitizationRejection> {
    if result.abstained {
        return Err(SanitizationRejection::ClassifierAbstained);
    }
    if result.class == SensitivityClass::Restricted {
        return Err(SanitizationRejection::RestrictedClass);
    }
    if result
        .spans
        .iter()
        .any(|span| span.category == PiiCategory::Secret)
    {
        return Err(SanitizationRejection::SecretCategory);
    }
    validate_spans(text, &result.spans)?;

    let mut categories = result
        .spans
        .iter()
        .map(|span| span.category)
        .collect::<Vec<_>>();
    categories.sort_unstable_by_key(|category| category.field_name());
    categories.dedup();

    Ok(SanitizedText {
        redacted: redact_text(text, &result.spans),
        class: result.class,
        categories,
        detector_version: result.model_version.clone(),
    })
}

/// Rejects spans that cannot be applied exactly as detected.
///
/// `redact_text` silently drops a span it cannot apply, which would leave the
/// original bytes in place while the result still claimed to be sanitized. Every
/// such span is refused here instead, before any redaction runs.
fn validate_spans(text: &str, spans: &[PiiSpan]) -> Result<(), SanitizationRejection> {
    let mut ordered = spans.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|span| (span.start, span.end));

    let mut previous_end = 0usize;
    for span in ordered {
        if span.start >= span.end {
            return Err(SanitizationRejection::MalformedSpan);
        }
        if span.end > text.len() {
            return Err(SanitizationRejection::SpanOutOfRange);
        }
        if !text.is_char_boundary(span.start) || !text.is_char_boundary(span.end) {
            return Err(SanitizationRejection::SpanNotCharBoundary);
        }
        if span.start < previous_end {
            return Err(SanitizationRejection::OverlappingSpans);
        }
        previous_end = span.end;
    }
    Ok(())
}

/// Refuses redacted text that still classifies as sensitive.
///
/// This crate's own bracketed placeholders are tolerated: several heuristics key
/// off a neighbouring token (an `MRN` label marks the token that follows it), so
/// re-classifying `MRN [MEDICAL_RECORD_REDACTED]` re-flags the placeholder that
/// just replaced the identifier. A span covering exactly a placeholder is
/// therefore evidence the redaction worked, not evidence it leaked.
fn ensure_no_residual_sensitivity(
    redacted: &str,
    residual: &PiiResult,
) -> Result<(), SanitizationRejection> {
    if residual.abstained {
        return Err(SanitizationRejection::ClassifierAbstained);
    }
    let unresolved = residual
        .spans
        .iter()
        .any(|span| !covers_redaction_placeholder(redacted, span));
    if unresolved {
        return Err(SanitizationRejection::ResidualSensitivity);
    }
    // A class without spans is a detector that reports sensitivity it cannot
    // locate; there is nothing left to redact, so the text cannot be released.
    if residual.spans.is_empty() && residual.class != SensitivityClass::None {
        return Err(SanitizationRejection::ResidualSensitivity);
    }
    Ok(())
}

/// Returns whether a residual span covers exactly one redaction placeholder.
fn covers_redaction_placeholder(redacted: &str, span: &PiiSpan) -> bool {
    if span.start >= span.end
        || span.end > redacted.len()
        || !redacted.is_char_boundary(span.start)
        || !redacted.is_char_boundary(span.end)
    {
        return false;
    }
    let covered = &redacted[span.start..span.end];
    ALL_CATEGORIES
        .iter()
        .any(|category| covered == redaction_replacement(*category))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(class: SensitivityClass, spans: Vec<PiiSpan>) -> PiiResult {
        PiiResult {
            class,
            spans,
            model_version: "test-detector:v1".to_string(),
            abstained: false,
        }
    }

    #[test]
    fn restricted_and_secret_inputs_are_refused_before_redaction() {
        // Pins: restricted class and any secret-category span both refuse. Neither
        // may be "handled" by redaction — a credential that was redacted is still
        // a credential that reached the learning boundary.
        let restricted = result(SensitivityClass::Restricted, Vec::new());
        assert_eq!(
            sanitize_classified("token sk-abc", &restricted).unwrap_err(),
            SanitizationRejection::RestrictedClass
        );

        let secret_span = result(
            SensitivityClass::Pii,
            vec![PiiSpan::new(6, 12, PiiCategory::Secret, 0.9)],
        );
        assert_eq!(
            sanitize_classified("token sk-abc", &secret_span).unwrap_err(),
            SanitizationRejection::SecretCategory
        );
    }

    #[test]
    fn abstention_refuses_instead_of_passing_text_through() {
        // Pins: an abstaining detector is a refusal. The fail-closed result carries
        // no spans, so a pass-through would release the original bytes unredacted.
        let abstained = PiiResult::fail_closed("test-detector:v1");
        assert_eq!(
            sanitize_classified("alice@example.com", &abstained).unwrap_err(),
            SanitizationRejection::ClassifierAbstained
        );
    }

    #[test]
    fn each_invalid_span_shape_refuses_with_its_own_reason() {
        // Pins: the four span defects are distinguished, because they mean different
        // detector bugs. Every one refuses rather than being silently dropped by
        // `redact_text`, which would leave the raw bytes in the "sanitized" output.
        let text = "alice@example.com and bob@example.com";
        assert_eq!(
            sanitize_classified(
                text,
                &result(
                    SensitivityClass::Pii,
                    vec![PiiSpan::new(5, 5, PiiCategory::Email, 0.9)]
                )
            )
            .unwrap_err(),
            SanitizationRejection::MalformedSpan
        );
        assert_eq!(
            sanitize_classified(
                text,
                &result(
                    SensitivityClass::Pii,
                    vec![PiiSpan::new(0, text.len() + 5, PiiCategory::Email, 0.9)]
                )
            )
            .unwrap_err(),
            SanitizationRejection::SpanOutOfRange
        );
        assert_eq!(
            sanitize_classified(
                text,
                &result(
                    SensitivityClass::Pii,
                    vec![
                        PiiSpan::new(0, 17, PiiCategory::Email, 0.9),
                        PiiSpan::new(10, 20, PiiCategory::Person, 0.9),
                    ]
                )
            )
            .unwrap_err(),
            SanitizationRejection::OverlappingSpans
        );

        let multibyte = "héllo alice@example.com";
        assert_eq!(
            sanitize_classified(
                multibyte,
                &result(
                    SensitivityClass::Pii,
                    vec![PiiSpan::new(2, 6, PiiCategory::Person, 0.9)]
                )
            )
            .unwrap_err(),
            SanitizationRejection::SpanNotCharBoundary
        );
    }

    #[tokio::test]
    async fn reserved_dlp_delimiters_refuse_before_classification() {
        // Pins: a reversible DLP token is refused on sight. Sanitization is one-way;
        // a restorable placeholder inside a durable learning artifact would let the
        // original secret be reconstructed later.
        struct NeverCalled;
        #[async_trait::async_trait]
        impl PiiClassifier for NeverCalled {
            async fn classify(&self, _text: &str) -> crate::Result<PiiResult> {
                panic!("classifier must not be reached for reserved-delimiter input");
            }
        }

        let text =
            format!("value {RESERVED_DLP_TOKEN_OPEN}MOA_DLP_1_2_3{RESERVED_DLP_TOKEN_CLOSE}");
        assert_eq!(
            sanitize_with(&NeverCalled, &text).await.unwrap_err(),
            SanitizationRejection::ReservedDlpToken
        );
    }

    #[tokio::test]
    async fn classifier_error_refuses_without_carrying_the_source_error() {
        // Pins: a detector failure refuses, and the reason code carries no trace of
        // the underlying error text (which can embed the input it failed on).
        struct Failing;
        #[async_trait::async_trait]
        impl PiiClassifier for Failing {
            async fn classify(&self, _text: &str) -> crate::Result<PiiResult> {
                Err(crate::Error::Inference(
                    "upstream said: alice@example.com".to_string(),
                ))
            }
        }

        let rejection = sanitize_with(&Failing, "alice@example.com")
            .await
            .unwrap_err();
        assert_eq!(rejection, SanitizationRejection::ClassifierError);
        assert_eq!(rejection.code(), "classifier_error");
        assert!(!rejection.to_string().contains("alice@example.com"));
    }

    #[tokio::test]
    async fn heuristic_pii_is_redacted_and_provenance_survives() {
        // Pins: the production heuristic path redacts email/phone content, keeps the
        // original class and detector version as provenance, and yields text that no
        // longer contains the original identifier.
        let sanitized = sanitize_with(
            &crate::HeuristicPiiClassifier,
            "ping alice@example.com about the migration",
        )
        .await
        .expect("clean PII text sanitizes");

        assert!(!sanitized.redacted().contains("alice@example.com"));
        assert!(sanitized.redacted().contains("[EMAIL_REDACTED]"));
        assert_eq!(sanitized.class(), SensitivityClass::Pii);
        assert_eq!(sanitized.categories(), &[PiiCategory::Email]);
        assert_eq!(sanitized.detector_version(), "moa-heuristic:v1");
    }

    #[tokio::test]
    async fn residual_sensitivity_refuses_when_the_detector_misses_an_occurrence() {
        // Pins: a detector that finds only the first of two identifiers is caught by
        // the re-classification pass, so partially-redacted text never ships.
        struct FirstOccurrenceOnly;
        #[async_trait::async_trait]
        impl PiiClassifier for FirstOccurrenceOnly {
            async fn classify(&self, text: &str) -> crate::Result<PiiResult> {
                let spans = text
                    .find("alice@example.com")
                    .map(|start| {
                        vec![PiiSpan::new(
                            start,
                            start + "alice@example.com".len(),
                            PiiCategory::Email,
                            0.9,
                        )]
                    })
                    .unwrap_or_default();
                let class = if spans.is_empty() {
                    SensitivityClass::None
                } else {
                    SensitivityClass::Pii
                };
                Ok(PiiResult {
                    class,
                    spans,
                    model_version: "first-only:v1".to_string(),
                    abstained: false,
                })
            }
        }

        assert_eq!(
            sanitize_with(
                &FirstOccurrenceOnly,
                "alice@example.com cc alice@example.com",
            )
            .await
            .unwrap_err(),
            SanitizationRejection::ResidualSensitivity
        );
    }

    #[tokio::test]
    async fn label_adjacent_placeholder_is_not_mistaken_for_residual_leakage() {
        // Pins: the medical-record heuristic flags the token AFTER an `MRN` label, so
        // re-classification re-flags the placeholder that replaced the identifier.
        // That is proof the redaction landed, and must not refuse the sanitization.
        let sanitized = sanitize_with(&crate::HeuristicPiiClassifier, "patient MRN: 8891233")
            .await
            .expect("label-adjacent redaction is not residual leakage");

        assert!(!sanitized.redacted().contains("8891233"));
        assert!(sanitized.redacted().contains("[MEDICAL_RECORD_REDACTED]"));
    }

    #[test]
    fn debug_rendering_never_prints_the_content() {
        // Pins: `Debug` shows provenance only, so an incidental log line cannot
        // become a copy of the learning corpus.
        let sanitized =
            sanitize_classified("hello world", &result(SensitivityClass::None, Vec::new()))
                .expect("clean text sanitizes");

        let rendered = format!("{sanitized:?}");
        assert!(!rendered.contains("hello world"), "{rendered}");
        assert!(rendered.contains("redacted_len"), "{rendered}");
    }
}
