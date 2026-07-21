//! Contract tests for the shared deterministic PII heuristic.

use moa_core::types::security::SensitivityClass;
use moa_memory_pii::{PiiCategory, classify_heuristic, redact_text, redaction_replacement};

#[test]
fn heuristic_classifier_returns_offsets_categories_confidence_and_redactions() {
    // Pins: the shared fallback classifier emits stable spans and replacements for known PII forms.
    let cases = [
        (
            "Email alice@example.com now",
            "alice@example.com",
            PiiCategory::Email,
            0.80,
            "Email [EMAIL_REDACTED] now",
        ),
        (
            "Call 555-123-4567 today",
            "555-123-4567",
            PiiCategory::Phone,
            0.90,
            "Call [PHONE_REDACTED] today",
        ),
        (
            "SSN 123-45-6789 confirmed",
            "123-45-6789",
            PiiCategory::Ssn,
            0.90,
            "SSN [SSN_REDACTED] confirmed",
        ),
        (
            "Card 4242-4242-4242-4242 expires",
            "4242-4242-4242-4242",
            PiiCategory::FinancialAccount,
            0.95,
            "Card [FINANCIAL_ACCOUNT_REDACTED] expires",
        ),
        (
            "Patient MRN: A12345 checked",
            "A12345",
            PiiCategory::MedicalRecord,
            0.90,
            "Patient MRN: [MEDICAL_RECORD_REDACTED] checked",
        ),
        (
            "Key sk-test-123 is active",
            "sk-test-123",
            PiiCategory::Secret,
            0.80,
            "Key [SECRET_REDACTED] is active",
        ),
    ];

    for (text, needle, category, confidence, expected_redacted) in cases {
        let result = classify_heuristic(text);
        let start = text.find(needle).expect("sample needle is present");
        let span = result
            .spans
            .first()
            .unwrap_or_else(|| panic!("expected a span for {text:?}"));

        assert_eq!(result.spans.len(), 1, "{text}");
        assert_eq!(span.start, start, "{text}");
        assert_eq!(span.end, start + needle.len(), "{text}");
        assert_eq!(span.category, category, "{text}");
        assert_eq!(span.confidence, confidence, "{text}");
        assert!(
            expected_redacted.contains(redaction_replacement(category)),
            "{text}"
        );
        assert_eq!(
            redact_text(text, &result.spans),
            expected_redacted,
            "{text}"
        );
    }
}

#[test]
fn heuristic_classifier_aggregates_detected_spans_into_privacy_class() {
    // Pins: the aggregate `PiiResult::class` for each detected category, not just
    // the span offsets. Secret -> Restricted, SSN -> Phi, every other detected
    // category -> Pii; this is the privacy class downstream storage trusts.
    let cases = [
        ("Email alice@example.com now", SensitivityClass::Pii),
        ("Call 555-123-4567 today", SensitivityClass::Pii),
        ("SSN 123-45-6789 confirmed", SensitivityClass::Phi),
        ("Card 4242-4242-4242-4242 expires", SensitivityClass::Pii),
        ("Patient MRN: A12345 checked", SensitivityClass::Pii),
        ("Key sk-test-123 is active", SensitivityClass::Restricted),
        (r#"{"key":"sk-test-123"}"#, SensitivityClass::Restricted),
    ];

    for (text, expected_class) in cases {
        let result = classify_heuristic(text);
        assert!(!result.spans.is_empty(), "expected a span for {text:?}");
        assert_eq!(result.class, expected_class, "{text}");
        assert!(!result.abstained, "{text}");
    }
}

#[test]
fn heuristic_classifier_leaves_non_pii_text_unclassified() {
    // Pins the privacy NEGATIVE space: clean text must produce zero spans and a
    // `SensitivityClass::None` so unredacted, fully-readable memory is not silently
    // restricted/encrypted. Each case targets a specific historical over-match:
    //   - "secretary" embeds "secret" but is not a credential.
    //   - "task-lifecycle" contains the byte sequence `sk-` inside an ordinary word.
    //   - a bare 10-digit order/tracking number is not a phone number.
    //   - a UUID and a git SHA are opaque identifiers, not PII.
    let clean_cases = [
        "Ask the secretary to confirm the meeting",
        r#"{"case":"task-lifecycle"}"#,
        "Order 1234567890 shipped to the warehouse",
        "Tracking number 9400110200830000000000",
        "Run id 550e8400-e29b-41d4-a716-446655440000 completed",
        "Commit da39a3ee5e6b4b0d3255bfef95601890afd80709 reverted",
    ];

    for text in clean_cases {
        let result = classify_heuristic(text);
        assert_eq!(
            result.spans,
            Vec::new(),
            "expected no spans for clean text {text:?}, got {:?}",
            result.spans
        );
        assert_eq!(
            result.class,
            SensitivityClass::None,
            "expected SensitivityClass::None for clean text {text:?}",
        );
        assert!(!result.abstained, "{text}");
    }
}

#[test]
fn heuristic_classifier_still_detects_secret_keyword_as_standalone_word() {
    // Pins: the word-boundary fix that suppresses "secretary" must not regress
    // the real positive — a standalone "secret" credential keyword still maps to
    // SensitivityClass::Restricted.
    let result = classify_heuristic("The deploy secret rotated overnight");
    assert_eq!(result.class, SensitivityClass::Restricted, "{result:?}");
    assert!(
        result
            .spans
            .iter()
            .any(|span| span.category == PiiCategory::Secret),
        "{result:?}"
    );
}
