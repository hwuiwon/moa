//! Contract tests for the shared deterministic PII heuristic.

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
        assert_eq!(
            span.redaction_replacement(),
            redaction_replacement(category),
            "{text}"
        );
        assert_eq!(
            redact_text(text, &result.spans),
            expected_redacted,
            "{text}"
        );
    }
}
