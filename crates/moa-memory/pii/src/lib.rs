//! PII classification helpers for graph-memory ingestion and privacy workflows.

use serde::{Deserialize, Serialize};

pub mod mock;
pub mod openai_filter;

pub use moa_memory_graph::PiiClass;
pub use mock::MockClassifier;
pub use openai_filter::{OpenAiPrivacyFilterClassifier, PrivacyFilterThresholds};

/// Result type returned by PII classifier implementations.
pub type Result<T> = std::result::Result<T, PiiError>;

/// PII categories emitted by `openai/privacy-filter` and normalized into MOA categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PiiCategory {
    /// A person name.
    Person,
    /// An email address.
    Email,
    /// A phone number.
    Phone,
    /// A physical address.
    Address,
    /// A US Social Security number or equivalent national taxpayer identifier.
    Ssn,
    /// A medical record identifier.
    MedicalRecord,
    /// A bank, card, or other financial account identifier.
    FinancialAccount,
    /// A government-issued identifier.
    GovernmentId,
    /// A private URL or web identifier emitted by `openai/privacy-filter`.
    Url,
    /// A private date emitted by `openai/privacy-filter`.
    Date,
    /// A secret token, credential, or similar high-sensitivity value.
    Secret,
}

impl PiiCategory {
    /// Parses common model label forms into the canonical category enum.
    pub fn parse_label(label: &str) -> Option<Self> {
        let normalized = label
            .trim()
            .trim_start_matches("B-")
            .trim_start_matches("I-")
            .trim_start_matches("E-")
            .trim_start_matches("S-")
            .replace(['-', ' '], "_")
            .to_ascii_uppercase();
        match normalized.as_str() {
            "PERSON" | "PRIVATE_PERSON" | "NAME" | "PER" => Some(Self::Person),
            "EMAIL" | "PRIVATE_EMAIL" | "EMAIL_ADDRESS" => Some(Self::Email),
            "PHONE" | "PRIVATE_PHONE" | "PHONE_NUMBER" | "TELEPHONE" => Some(Self::Phone),
            "ADDRESS" | "PRIVATE_ADDRESS" | "LOCATION" | "STREET_ADDRESS" => Some(Self::Address),
            "SSN" | "SOCIAL_SECURITY_NUMBER" => Some(Self::Ssn),
            "MEDICAL_RECORD" | "MEDICAL_RECORD_NUMBER" | "MRN" => Some(Self::MedicalRecord),
            "FINANCIAL_ACCOUNT" | "ACCOUNT_NUMBER" | "BANK_ACCOUNT" | "CREDIT_CARD"
            | "CARD_NUMBER" => Some(Self::FinancialAccount),
            "GOVERNMENT_ID" | "GOV_ID" | "PASSPORT" | "DRIVER_LICENSE" => Some(Self::GovernmentId),
            "URL" | "PRIVATE_URL" => Some(Self::Url),
            "DATE" | "PRIVATE_DATE" => Some(Self::Date),
            "SECRET" => Some(Self::Secret),
            _ => None,
        }
    }
}

/// One detected PII span in UTF-8 byte offsets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PiiSpan {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
    /// Detected PII category.
    pub category: PiiCategory,
    /// Model confidence for this span.
    pub confidence: f32,
}

/// Full classifier result for one input text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PiiResult {
    /// Aggregated privacy class derived from detected spans.
    pub class: PiiClass,
    /// Detected spans, preserving offsets and model categories for later encryption/redaction.
    pub spans: Vec<PiiSpan>,
    /// Model and serving version that produced this result.
    pub model_version: String,
    /// Whether the model abstained or the client produced a fail-closed fallback.
    pub abstained: bool,
}

impl PiiResult {
    /// Builds the fail-closed result used when inference is unavailable.
    pub fn fail_closed(model_version: impl Into<String>) -> Self {
        Self {
            class: PiiClass::Pii,
            spans: Vec::new(),
            model_version: model_version.into(),
            abstained: true,
        }
    }
}

/// Redacts detected PII spans from one UTF-8 string.
#[must_use]
pub fn redact_text(text: &str, spans: &[PiiSpan]) -> String {
    if spans.is_empty() {
        return text.to_string();
    }

    let mut spans = spans
        .iter()
        .filter(|span| {
            span.start < span.end
                && span.end <= text.len()
                && text.is_char_boundary(span.start)
                && text.is_char_boundary(span.end)
        })
        .collect::<Vec<_>>();
    spans.sort_by_key(|span| span.start);

    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    for span in spans {
        if span.start < cursor {
            continue;
        }
        redacted.push_str(&text[cursor..span.start]);
        redacted.push_str(redaction_token(span.category));
        cursor = span.end;
    }
    redacted.push_str(&text[cursor..]);
    redacted
}

fn redaction_token(category: PiiCategory) -> &'static str {
    match category {
        PiiCategory::Person => "[PERSON_REDACTED]",
        PiiCategory::Email => "[EMAIL_REDACTED]",
        PiiCategory::Phone => "[PHONE_REDACTED]",
        PiiCategory::Address => "[ADDRESS_REDACTED]",
        PiiCategory::Ssn => "[SSN_REDACTED]",
        PiiCategory::MedicalRecord => "[MEDICAL_RECORD_REDACTED]",
        PiiCategory::FinancialAccount => "[FINANCIAL_ACCOUNT_REDACTED]",
        PiiCategory::GovernmentId => "[GOVERNMENT_ID_REDACTED]",
        PiiCategory::Url => "[URL_REDACTED]",
        PiiCategory::Date => "[DATE_REDACTED]",
        PiiCategory::Secret => "[SECRET_REDACTED]",
    }
}

/// Async PII classification abstraction used by ingestion and privacy workflows.
#[async_trait::async_trait]
pub trait PiiClassifier: Send + Sync {
    /// Classifies one input string and returns spans plus the aggregate privacy class.
    async fn classify(&self, text: &str) -> Result<PiiResult>;
}

/// Errors returned by PII classification helpers.
#[derive(Debug, thiserror::Error)]
pub enum PiiError {
    /// The inference service returned a non-network failure.
    #[error("inference: {0}")]
    Inference(String),
    /// The inference service request failed.
    #[error("network: {0}")]
    Network(#[from] reqwest::Error),
    /// The inference response could not be parsed.
    #[error("parse: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_text_replaces_detected_spans() {
        let text = "Email alice@example.com and SSN 123-45-6789";
        let spans = vec![
            PiiSpan {
                start: 6,
                end: 23,
                category: PiiCategory::Email,
                confidence: 0.99,
            },
            PiiSpan {
                start: 32,
                end: 43,
                category: PiiCategory::Ssn,
                confidence: 0.99,
            },
        ];

        assert_eq!(
            redact_text(text, &spans),
            "Email [EMAIL_REDACTED] and SSN [SSN_REDACTED]"
        );
    }

    #[test]
    fn redact_text_ignores_invalid_offsets() {
        let text = "safe text";
        let spans = vec![PiiSpan {
            start: 99,
            end: 100,
            category: PiiCategory::Secret,
            confidence: 0.99,
        }];

        assert_eq!(redact_text(text, &spans), text);
    }
}
