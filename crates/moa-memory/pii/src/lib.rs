//! PII classification helpers for graph-memory ingestion and privacy workflows.

use serde::{Deserialize, Serialize};

pub mod erasure;
pub mod mock;
pub mod openai_filter;

use moa_memory_graph::PiiClass;
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

    /// Returns the stable lowercase field name used by redaction event streams.
    #[must_use]
    pub const fn field_name(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Email => "email",
            Self::Phone => "phone",
            Self::Address => "address",
            Self::Ssn => "ssn",
            Self::MedicalRecord => "medical_record",
            Self::FinancialAccount => "financial_account",
            Self::GovernmentId => "government_id",
            Self::Url => "url",
            Self::Date => "date",
            Self::Secret => "secret",
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
    /// Optional caller-facing replacement text for redaction.
    ///
    /// Older serialized spans may omit this field; callers should use
    /// [`PiiSpan::redaction_replacement`] rather than reading it directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
}

impl PiiSpan {
    /// Builds a span with the canonical replacement text for its category.
    #[must_use]
    pub fn new(start: usize, end: usize, category: PiiCategory, confidence: f32) -> Self {
        Self::with_replacement(
            start,
            end,
            category,
            confidence,
            redaction_replacement(category),
        )
    }

    /// Builds a span with an explicit replacement text.
    #[must_use]
    pub fn with_replacement(
        start: usize,
        end: usize,
        category: PiiCategory,
        confidence: f32,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            start,
            end,
            category,
            confidence,
            replacement: Some(replacement.into()),
        }
    }

    /// Returns the replacement text to use when redacting this span.
    #[must_use]
    pub fn redaction_replacement(&self) -> &str {
        self.replacement
            .as_deref()
            .unwrap_or(redaction_replacement(self.category))
    }
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
        redacted.push_str(span.redaction_replacement());
        cursor = span.end;
    }
    redacted.push_str(&text[cursor..]);
    redacted
}

/// Returns the canonical bracketed replacement for a PII category.
#[must_use]
pub const fn redaction_replacement(category: PiiCategory) -> &'static str {
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

/// Deterministic local classifier used when no external PII service is configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicPiiClassifier;

/// Classifies text with MOA's deterministic local PII heuristics.
///
/// The heuristic model is intentionally conservative and journal-stable: it
/// detects the same token-level email, secret, SSN-like, card-like, and MRN-like
/// samples that the ingestion fallback historically detected, plus the phone
/// tokens used by the audit vault fallback.
#[must_use]
pub fn classify_heuristic(text: &str) -> PiiResult {
    let tokens = token_spans(text);
    let mut spans = Vec::new();

    for (index, token) in tokens.iter().enumerate() {
        if token.text.contains('@') {
            push_heuristic_span(token, PiiCategory::Email, 0.80, &mut spans);
        } else if token.text.contains("sk-") || contains_secret_keyword(token.text) {
            push_heuristic_span(token, PiiCategory::Secret, 0.80, &mut spans);
        } else if looks_like_ssn(token.text) {
            push_heuristic_span(token, PiiCategory::Ssn, 0.90, &mut spans);
        } else if looks_like_card(token.text) {
            push_heuristic_span(token, PiiCategory::FinancialAccount, 0.95, &mut spans);
        } else if looks_like_phone(token.text) {
            push_heuristic_span(token, PiiCategory::Phone, 0.90, &mut spans);
        } else if token.text.trim_matches(':').eq_ignore_ascii_case("MRN")
            && let Some(next) = tokens.get(index + 1)
        {
            push_heuristic_span(next, PiiCategory::MedicalRecord, 0.90, &mut spans);
        }
    }

    let class = if spans
        .iter()
        .any(|span| matches!(span.category, PiiCategory::Secret))
    {
        PiiClass::Restricted
    } else if spans
        .iter()
        .any(|span| matches!(span.category, PiiCategory::Ssn))
    {
        PiiClass::Phi
    } else if spans.is_empty() {
        PiiClass::None
    } else {
        PiiClass::Pii
    };

    PiiResult {
        class,
        spans,
        model_version: "moa-heuristic:v1".to_string(),
        abstained: false,
    }
}

#[derive(Debug, Clone, Copy)]
struct TokenSpan<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn token_spans(text: &str) -> Vec<TokenSpan<'_>> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(token_start) = start.take() {
                tokens.push(TokenSpan {
                    text: &text[token_start..index],
                    start: token_start,
                    end: index,
                });
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(token_start) = start {
        tokens.push(TokenSpan {
            text: &text[token_start..],
            start: token_start,
            end: text.len(),
        });
    }
    tokens
}

fn push_heuristic_span(
    token: &TokenSpan<'_>,
    category: PiiCategory,
    confidence: f32,
    spans: &mut Vec<PiiSpan>,
) {
    spans.push(PiiSpan::new(token.start, token.end, category, confidence));
}

fn looks_like_ssn(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 11
        && bytes[3] == b'-'
        && bytes[6] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 3 || index == 6 || byte.is_ascii_digit())
}

fn looks_like_card(token: &str) -> bool {
    let digits = token
        .bytes()
        .filter(|byte| byte.is_ascii_digit())
        .collect::<Vec<_>>();
    digits.len() >= 13
        && digits.len() <= 19
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-' || byte == b' ')
}

fn looks_like_phone(token: &str) -> bool {
    let digits = token.chars().filter(|ch| ch.is_ascii_digit()).count();
    // Require at least one phone-style separator (`+ - ( ) .`) so that a bare run
    // of digits such as an order or tracking number is not misread as a phone
    // number. Real phone tokens carry a country prefix or grouping punctuation.
    let has_separator = token.chars().any(|ch| "+-().".contains(ch));
    digits >= 10
        && has_separator
        && token
            .chars()
            .all(|ch| ch.is_ascii_digit() || "+-().".contains(ch))
}

/// Reports whether `token` contains the keyword `secret` as a standalone word.
///
/// Plain substring matching also flags ordinary English words such as
/// "secretary", so the match must be bounded by non-alphabetic characters. A
/// secret value (`secret`, `secret:abc123`, `client_secret`) is still detected
/// while a word that merely embeds the letters is not.
fn contains_secret_keyword(token: &str) -> bool {
    const NEEDLE: &str = "secret";
    let lower = token.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(offset) = lower[from..].find(NEEDLE) {
        let start = from + offset;
        let end = start + NEEDLE.len();
        let preceded_by_letter = start > 0 && bytes[start - 1].is_ascii_alphabetic();
        let followed_by_letter = bytes.get(end).is_some_and(u8::is_ascii_alphabetic);
        if !preceded_by_letter && !followed_by_letter {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Async PII classification abstraction used by ingestion and privacy workflows.
#[async_trait::async_trait]
pub trait PiiClassifier: Send + Sync {
    /// Classifies one input string and returns spans plus the aggregate privacy class.
    async fn classify(&self, text: &str) -> Result<PiiResult>;
}

#[async_trait::async_trait]
impl PiiClassifier for HeuristicPiiClassifier {
    async fn classify(&self, text: &str) -> Result<PiiResult> {
        Ok(classify_heuristic(text))
    }
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
                replacement: Some(redaction_replacement(PiiCategory::Email).to_string()),
            },
            PiiSpan {
                start: 32,
                end: 43,
                category: PiiCategory::Ssn,
                confidence: 0.99,
                replacement: Some(redaction_replacement(PiiCategory::Ssn).to_string()),
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
            replacement: Some(redaction_replacement(PiiCategory::Secret).to_string()),
        }];

        assert_eq!(redact_text(text, &spans), text);
    }
}
