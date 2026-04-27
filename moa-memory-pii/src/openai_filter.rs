//! HTTP client for the out-of-process `openai/privacy-filter` inference service.

use std::time::Duration;

use serde::Deserialize;
use tracing::warn;

use crate::{PiiCategory, PiiClass, PiiClassifier, PiiError, PiiResult, PiiSpan, Result};

const DEFAULT_MODEL_VERSION: &str = "openai/privacy-filter:v1.0";

/// Confidence thresholds used to aggregate detected spans into a `PiiClass`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrivacyFilterThresholds {
    /// Threshold for person-name spans.
    pub person: f32,
    /// Threshold for email-address spans.
    pub email: f32,
    /// Threshold for phone-number spans.
    pub phone: f32,
    /// Threshold for address spans.
    pub address: f32,
    /// Threshold for SSN spans.
    pub ssn: f32,
    /// Threshold for medical-record spans.
    pub medical_record: f32,
    /// Threshold for financial-account spans.
    pub financial_account: f32,
    /// Threshold for government-id spans.
    pub government_id: f32,
}

impl Default for PrivacyFilterThresholds {
    fn default() -> Self {
        Self {
            person: 0.50,
            email: 0.50,
            phone: 0.50,
            address: 0.50,
            ssn: 0.85,
            medical_record: 0.85,
            financial_account: 0.90,
            government_id: 0.85,
        }
    }
}

impl PrivacyFilterThresholds {
    /// Returns the operating threshold for one emitted category.
    pub fn threshold_for(self, category: PiiCategory) -> f32 {
        match category {
            PiiCategory::Person => self.person,
            PiiCategory::Email => self.email,
            PiiCategory::Phone => self.phone,
            PiiCategory::Address => self.address,
            PiiCategory::Ssn => self.ssn,
            PiiCategory::MedicalRecord => self.medical_record,
            PiiCategory::FinancialAccount => self.financial_account,
            PiiCategory::GovernmentId => self.government_id,
            PiiCategory::Url => self.email,
            PiiCategory::Date => self.person,
            PiiCategory::Secret => self.financial_account,
        }
    }
}

/// HTTP-backed classifier for the `moa-pii-service` sidecar.
#[derive(Debug, Clone)]
pub struct OpenAiPrivacyFilterClassifier {
    client: reqwest::Client,
    base_url: String,
    model_version: String,
    fail_closed_on_error: bool,
    thresholds: PrivacyFilterThresholds,
}

impl OpenAiPrivacyFilterClassifier {
    /// Creates a classifier with the default timeout and fail-closed behavior.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self::with_client(base_url, client))
    }

    /// Creates a classifier using a caller-provided HTTP client.
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: normalize_base_url(base_url.into()),
            model_version: DEFAULT_MODEL_VERSION.to_string(),
            fail_closed_on_error: true,
            thresholds: PrivacyFilterThresholds::default(),
        }
    }

    /// Overrides the model-version string returned in classifier results.
    pub fn with_model_version(mut self, model_version: impl Into<String>) -> Self {
        self.model_version = model_version.into();
        self
    }

    /// Overrides fail-closed behavior for callers that need hard error propagation.
    pub fn with_fail_closed_on_error(mut self, fail_closed_on_error: bool) -> Self {
        self.fail_closed_on_error = fail_closed_on_error;
        self
    }

    /// Overrides confidence thresholds used to aggregate spans.
    pub fn with_thresholds(mut self, thresholds: PrivacyFilterThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Returns the currently configured model-version string.
    pub fn model_version(&self) -> &str {
        &self.model_version
    }

    /// Returns the currently configured aggregation thresholds.
    pub fn thresholds(&self) -> PrivacyFilterThresholds {
        self.thresholds
    }
}

#[async_trait::async_trait]
impl PiiClassifier for OpenAiPrivacyFilterClassifier {
    async fn classify(&self, text: &str) -> Result<PiiResult> {
        match self.classify_inner(text).await {
            Ok(mut result) => {
                if result.abstained {
                    result.class = PiiClass::Pii;
                }
                Ok(result)
            }
            Err(error) if self.fail_closed_on_error => {
                warn!(%error, "PII classifier failed; returning fail-closed result");
                Ok(PiiResult::fail_closed(self.model_version.clone()))
            }
            Err(error) => Err(error),
        }
    }
}

impl OpenAiPrivacyFilterClassifier {
    async fn classify_inner(&self, text: &str) -> Result<PiiResult> {
        let response = self
            .client
            .post(format!("{}/classify", self.base_url))
            .json(&serde_json::json!({ "text": text, "return_spans": true }))
            .send()
            .await?
            .error_for_status()?;
        let payload: ServiceResponse = response.json().await?;
        let abstained = payload.abstained.unwrap_or(false);
        let model_version = payload
            .model_version
            .clone()
            .unwrap_or_else(|| self.model_version.clone());
        let spans = payload.into_spans()?;
        let class = if abstained {
            PiiClass::Pii
        } else {
            resolve_class(&spans, self.thresholds)
        };
        Ok(PiiResult {
            class,
            spans,
            model_version,
            abstained,
        })
    }
}

/// Resolves MOA's aggregate privacy class from detected spans and thresholds.
pub fn resolve_class(spans: &[PiiSpan], thresholds: PrivacyFilterThresholds) -> PiiClass {
    if spans.iter().any(|span| {
        matches!(
            span.category,
            PiiCategory::FinancialAccount | PiiCategory::Secret
        ) && span.confidence >= thresholds.threshold_for(span.category)
    }) {
        return PiiClass::Restricted;
    }
    if spans.iter().any(|span| {
        matches!(
            span.category,
            PiiCategory::Ssn | PiiCategory::MedicalRecord | PiiCategory::GovernmentId
        ) && span.confidence >= thresholds.threshold_for(span.category)
    }) {
        return PiiClass::Phi;
    }
    if spans
        .iter()
        .any(|span| span.confidence >= thresholds.threshold_for(span.category))
    {
        return PiiClass::Pii;
    }
    PiiClass::None
}

fn normalize_base_url(base_url: String) -> String {
    base_url.trim_end_matches('/').to_string()
}

#[derive(Debug, Deserialize)]
struct ServiceResponse {
    #[serde(default)]
    spans: Vec<ServiceSpan>,
    abstained: Option<bool>,
    model_version: Option<String>,
}

impl ServiceResponse {
    fn into_spans(self) -> Result<Vec<PiiSpan>> {
        self.spans
            .into_iter()
            .map(ServiceSpan::try_into_span)
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct ServiceSpan {
    start: usize,
    end: usize,
    confidence: Option<f32>,
    score: Option<f32>,
    category: Option<String>,
    label: Option<String>,
    entity_group: Option<String>,
    entity: Option<String>,
}

impl ServiceSpan {
    fn try_into_span(self) -> Result<PiiSpan> {
        let confidence = self
            .confidence
            .or(self.score)
            .ok_or_else(|| PiiError::Parse("span missing confidence".to_string()))?;
        let category = self
            .category
            .as_deref()
            .and_then(PiiCategory::parse_label)
            .or_else(|| self.label.as_deref().and_then(PiiCategory::parse_label))
            .or_else(|| {
                self.entity_group
                    .as_deref()
                    .and_then(PiiCategory::parse_label)
            })
            .or_else(|| self.entity.as_deref().and_then(PiiCategory::parse_label))
            .ok_or_else(|| PiiError::Parse("span missing recognized category".to_string()))?;
        Ok(PiiSpan {
            start: self.start,
            end: self.end,
            category,
            confidence,
        })
    }
}
