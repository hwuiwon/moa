//! Offline coverage for the irreversible learning-sanitization primitive.
//!
//! The unit tests drive the policy with hand-built classifier results. These
//! drive it end to end through the production heuristic, which is what the
//! learning path actually runs with, so the two layers cannot agree in isolation
//! and disagree in production.

use moa_core::types::security::SensitivityClass;
use moa_memory_pii::sanitized::{SanitizationRejection, sanitize_with};
use moa_memory_pii::{HeuristicPiiClassifier, PiiCategory};

#[tokio::test]
async fn phi_proceeds_through_irreversible_redaction_offline() {
    // Pins: PHI is releasable to learning, but only after the identifier itself is
    // gone. The original class survives as provenance so a reviewer can tell that
    // clean-looking evidence was redacted down from PHI.
    let sanitized = sanitize_with(
        &HeuristicPiiClassifier,
        "the patient record lists 123-45-6789 as the taxpayer id",
    )
    .await
    .expect("PHI proceeds after redaction");

    assert!(!sanitized.redacted().contains("123-45-6789"));
    assert!(sanitized.redacted().contains("[SSN_REDACTED]"));
    assert_eq!(sanitized.class(), SensitivityClass::Phi);
    assert_eq!(sanitized.categories(), &[PiiCategory::Ssn]);
    assert_eq!(sanitized.detector_version(), "moa-heuristic:v1");
}

#[tokio::test]
async fn credential_material_refuses_rather_than_redacting_offline() {
    // Pins: a credential is refused, not redacted. Redaction would still mean the
    // secret reached the learning boundary and was handled there, and the
    // heuristic classifies secret-bearing text as Restricted.
    let rejection = sanitize_with(
        &HeuristicPiiClassifier,
        "export OPENAI_API_KEY=sk-abc123def456 before running",
    )
    .await
    .expect_err("credential material refuses");

    assert_eq!(rejection, SanitizationRejection::RestrictedClass);
    assert_eq!(rejection.code(), "restricted_class");
}

#[tokio::test]
async fn clean_text_passes_through_byte_identical_offline() {
    // Pins: sanitization is not a rewrite. Text with nothing to redact comes back
    // unchanged and classified None, so the learning corpus is not silently
    // reshaped for evidence that never needed it.
    let text = "run the migration and verify the checksum";
    let sanitized = sanitize_with(&HeuristicPiiClassifier, text)
        .await
        .expect("clean text sanitizes");

    assert_eq!(sanitized.redacted(), text);
    assert_eq!(sanitized.class(), SensitivityClass::None);
    assert!(sanitized.categories().is_empty());
}

#[tokio::test]
async fn multiple_categories_are_all_redacted_and_reported_offline() {
    // Pins: a carrier with several kinds of identifier redacts every one and
    // reports the full closed-vocabulary category set, which is what rides into
    // the reviewer-facing provenance.
    let sanitized = sanitize_with(
        &HeuristicPiiClassifier,
        "mail alice@example.com or call +1-555-010-9999 about card 4111111111111111",
    )
    .await
    .expect("multi-category PII proceeds after redaction");

    let redacted = sanitized.redacted();
    assert!(!redacted.contains("alice@example.com"), "{redacted}");
    assert!(!redacted.contains("+1-555-010-9999"), "{redacted}");
    assert!(!redacted.contains("4111111111111111"), "{redacted}");

    let mut categories = sanitized.categories().to_vec();
    categories.sort_unstable_by_key(|category| category.field_name());
    assert_eq!(
        categories,
        vec![
            PiiCategory::Email,
            PiiCategory::FinancialAccount,
            PiiCategory::Phone
        ]
    );
    assert_eq!(sanitized.class(), SensitivityClass::Pii);
}
