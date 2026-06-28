//! LLM-backed fact extraction through the existing ingestion extractor seam.

use async_trait::async_trait;
use moa_core::config::MemoryExtractionConfig;
use moa_providers::{
    LlmChatClient, LlmExtractedFact, LlmFactExtractionChunk, LlmFactExtractionClient,
};
use uuid::Uuid;

use crate::{
    ExtractedFact, ExtractedFactScopeHint, FactExtractor, IngestError, Result, TurnChunk,
    fact_hash, fact_uid_from_hash,
};

pub use moa_providers::EXTRACTION_PROMPT_VERSION;

/// Fact extractor backed by a Cohere chat model.
#[derive(Clone)]
pub struct LlmFactExtractor {
    client: LlmFactExtractionClient,
}

impl LlmFactExtractor {
    /// Creates an LLM extractor from memory extraction config.
    pub fn from_config(config: &MemoryExtractionConfig) -> Result<Self> {
        let client = LlmChatClient::from_api_key(
            secrecy::SecretString::from(config.api_key.clone()),
            &config.model,
            config.timeout_ms,
        );
        Ok(Self::new(client, config.max_facts_per_chunk))
    }

    /// Creates an LLM extractor from an already configured chat client.
    #[must_use]
    pub fn new(client: LlmChatClient, max_facts_per_chunk: usize) -> Self {
        Self {
            client: LlmFactExtractionClient::new(client, max_facts_per_chunk),
        }
    }
}

#[async_trait]
impl FactExtractor for LlmFactExtractor {
    async fn extract(&self, chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>> {
        validate_chunks(chunks)?;
        let provider_chunks = chunks
            .iter()
            .map(|chunk| LlmFactExtractionChunk {
                index: chunk.index,
                text: chunk.text.clone(),
            })
            .collect::<Vec<_>>();
        let extracted = self
            .client
            .extract(&provider_chunks)
            .await?
            .into_iter()
            .map(|fact| provider_fact_into_extracted(fact).map(normalize_extracted_fact))
            .collect::<Result<Vec<_>>>()?;
        Ok(extracted
            .into_iter()
            .filter(should_keep_extracted_fact)
            .collect())
    }
}

fn validate_chunks(chunks: &[TurnChunk]) -> Result<()> {
    for chunk in chunks {
        let actual_chars = chunk.text.chars().count();
        if actual_chars > crate::extract::MAX_EXTRACT_CHUNK_CHARS {
            return Err(IngestError::ChunkTooLarge {
                index: chunk.index,
                actual_chars,
                max_chars: crate::extract::MAX_EXTRACT_CHUNK_CHARS,
            });
        }
    }
    Ok(())
}

fn provider_fact_into_extracted(fact: LlmExtractedFact) -> Result<ExtractedFact> {
    let scope_hint = match fact.scope.trim().to_ascii_lowercase().as_str() {
        "tenant" => ExtractedFactScopeHint::Tenant,
        _ => ExtractedFactScopeHint::Contact,
    };
    let confidence = fact.confidence.map(clamp_confidence);
    let mut extracted = ExtractedFact {
        uid: Uuid::nil(),
        subject: fact.subject.trim().to_string(),
        predicate: fact.predicate.trim().to_string(),
        object: fact.object.trim().to_string(),
        summary: fact.summary.trim().to_string(),
        source_chunk: fact.source_chunk,
        scope_hint,
        confidence,
    };
    let hash = fact_hash(&extracted)?;
    extracted.uid = fact_uid_from_hash(&hash);
    Ok(extracted)
}

fn clamp_confidence(confidence: f64) -> f64 {
    confidence.clamp(0.0, 1.0)
}

pub(crate) fn normalize_extracted_fact(mut fact: ExtractedFact) -> ExtractedFact {
    if is_user_scoped_fact(&fact) {
        fact.scope_hint = ExtractedFactScopeHint::Contact;
    }
    fact
}

pub(crate) fn should_keep_extracted_fact(fact: &ExtractedFact) -> bool {
    let subject = normalize_filter_text(&fact.subject);
    let predicate = normalize_filter_text(&fact.predicate);
    let object = normalize_filter_text(&fact.object);
    let summary = normalize_filter_text(&fact.summary);
    let combined = format!("{subject} {predicate} {object} {summary}");

    if combined.contains("busy week")
        || combined.contains("nothing in particular")
        || combined.contains("reasonable to me")
        || combined.contains("many meetings without a specific focus")
    {
        return false;
    }

    if matches!(
        object.as_str(),
        "last sprint" | "platform decision" | "user s schedule" | "by the team"
    ) {
        return false;
    }

    if predicate.contains("occurred")
        || predicate.contains("completed")
        || predicate.contains("determined by")
        || predicate.contains("based on")
        || predicate.contains("defined by")
        || predicate == "follows"
    {
        return false;
    }

    !(subject == "week" || subject == "meetings")
}

fn is_user_scoped_fact(fact: &ExtractedFact) -> bool {
    let subject = normalize_filter_text(&fact.subject);
    let predicate = normalize_filter_text(&fact.predicate);
    let object = normalize_filter_text(&fact.object);
    let summary = normalize_filter_text(&fact.summary);
    subject == "user"
        || subject.starts_with("user ")
        || predicate.contains("contact email")
        || predicate.contains("response style")
        || predicate.contains("private repository")
        || predicate == "prefers"
        || predicate == "should use"
        || predicate.contains("switched to")
        || (summary.starts_with("user ") && object.starts_with("repo "))
}

fn normalize_filter_text(text: &str) -> String {
    let mut normalized = String::new();
    let mut previous_was_space = true;
    for character in text.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            normalized.push(character);
            previous_was_space = false;
        } else if !previous_was_space {
            normalized.push(' ');
            previous_was_space = true;
        }
    }
    normalized.trim().to_string()
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn chunk() -> TurnChunk {
        TurnChunk {
            index: 7,
            text: "user: I prefer Linear for bug triage.".to_string(),
            token_estimate: 10,
        }
    }

    async fn extractor_for_response(response: &str, max_facts: usize) -> LlmFactExtractor {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/chat"))
            .and(body_string_contains(EXTRACTION_PROMPT_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": [{"type": "text", "text": response}]}
            })))
            .mount(&server)
            .await;
        let client =
            LlmChatClient::from_api_key(SecretString::from("test-key"), "command-test", 1_000)
                .with_endpoint(format!("{}/v2/chat", server.uri()));
        LlmFactExtractor::new(client, max_facts)
    }

    #[tokio::test]
    async fn llm_extractor_parses_json_array_and_strips_code_fences() {
        // Pins: the LLM extractor accepts fenced JSON arrays and preserves structured fields.
        let extractor = extractor_for_response(
            r#"```json
[
  {
    "subject": "contact",
    "predicate": "prefers",
    "object": "Linear for bug triage",
    "summary": "The contact prefers Linear for bug triage.",
    "scope": "contact",
    "confidence": 0.91
  }
]
```"#,
            12,
        )
        .await;

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "contact");
        assert_eq!(facts[0].predicate, "prefers");
        assert_eq!(facts[0].object, "Linear for bug triage");
        assert_eq!(facts[0].source_chunk, 7);
        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Contact);
        assert_eq!(facts[0].confidence, Some(0.91));
    }

    #[tokio::test]
    async fn llm_extractor_clamps_to_max_facts_per_chunk() {
        // Pins: configured fact cap bounds model output per chunk.
        let extractor = extractor_for_response(
            r#"[
{"subject":"a","predicate":"uses","object":"b","summary":"a uses b","scope":"tenant","confidence":0.8},
{"subject":"c","predicate":"uses","object":"d","summary":"c uses d","scope":"tenant","confidence":0.8}
]"#,
            1,
        )
        .await;

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].summary, "a uses b");
    }

    #[tokio::test]
    async fn llm_extractor_maps_unknown_scope_to_contact() {
        // Pins: unknown model scope values fail closed to contact-scoped memory.
        let extractor = extractor_for_response(
            r#"[{"subject":"a","predicate":"uses","object":"b","summary":"a uses b","scope":"planet","confidence":2.0}]"#,
            12,
        )
        .await;

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Contact);
        assert_eq!(facts[0].confidence, Some(1.0));
    }

    #[tokio::test]
    async fn llm_extractor_corrects_user_subject_scope() {
        // Pins: model tenant scope is corrected for user-subject preference facts.
        let extractor = extractor_for_response(
            r#"[{"subject":"User 04","predicate":"should use","object":"repo/control-plane","summary":"User 04 should use repo/control-plane.","scope":"tenant","confidence":0.9}]"#,
            12,
        )
        .await;

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Contact);
    }

    #[tokio::test]
    async fn llm_extractor_filters_small_talk_and_meta_facts() {
        // Pins: the LLM path drops non-durable model artifacts before ingestion.
        let extractor = extractor_for_response(
            r#"[
{"subject":"week","predicate":"was busy","object":"user","summary":"The user had a busy week.","scope":"user","confidence":0.8},
{"subject":"standardization","predicate":"occurred during","object":"last sprint","summary":"The standardization occurred during last sprint.","scope":"tenant","confidence":0.8},
{"subject":"auth","predicate":"uses","object":"JWT","summary":"auth uses JWT.","scope":"tenant","confidence":0.9}
]"#,
            12,
        )
        .await;

        let facts = extractor.extract(&[chunk()]).await.expect("extract facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "auth");
        assert_eq!(facts[0].predicate, "uses");
        assert_eq!(facts[0].object, "JWT");
    }
}
