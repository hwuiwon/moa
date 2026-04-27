//! PII classification helpers for graph-memory ingestion and privacy workflows.

use serde::{Deserialize, Serialize};

pub mod mock;
pub mod openai_filter;

pub use mock::MockClassifier;
pub use openai_filter::{OpenAiPrivacyFilterClassifier, PrivacyFilterThresholds};

/// Result type returned by PII classifier implementations.
pub type Result<T> = std::result::Result<T, PiiError>;

/// MOA's four-tier privacy class for memory nodes and audit payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiiClass {
    /// No PII or PHI was detected.
    None,
    /// Personally identifiable information was detected.
    Pii,
    /// Protected health information or high-sensitivity identity data was detected.
    Phi,
    /// Restricted financial or policy-sensitive data was detected.
    Restricted,
}

impl PiiClass {
    /// Returns the canonical storage string for this class.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pii => "pii",
            Self::Phi => "phi",
            Self::Restricted => "restricted",
        }
    }
}

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
