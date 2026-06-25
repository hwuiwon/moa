//! LLM-backed fact extraction through the existing ingestion extractor seam.

use async_trait::async_trait;
use moa_core::config::MemoryExtractionConfig;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    ExtractedFact, ExtractedFactScopeHint, FactExtractor, IngestError, Result, TurnChunk,
    fact_hash, fact_uid_from_hash, llm_client::LlmChatClient,
};

/// Version for the LLM extraction prompt and recorded extraction fixtures.
pub const EXTRACTION_PROMPT_VERSION: &str = "v2";

const EXTRACTION_SYSTEM_PROMPT: &str = r#"You extract durable, declarative facts from transcripts.
Skip questions, requests, speculation, small talk, transient scheduling commentary, and provenance-only details like "last sprint" or "per the platform decision".
Do not split one durable preference/value into multiple facts only because the object contains punctuation.
For each fact return one JSON object with keys:
- subject: concise noun phrase
- predicate: concise relation phrase
- object: concise noun phrase or value
- summary: one sentence restating the fact
- scope: "contact" or "tenant" using the rubric below
- confidence: number from 0.0 to 1.0
Use this scope rubric:
scope = "contact" when the fact is about the speaker personally: preferences ("I prefer", "my setup", "for my work"), personal state, individual habits, or anything phrased in first person about themselves.
scope = "tenant" when the fact is about shared systems or team agreements: "we decided", "the team", "our service", infrastructure, ownership, processes that apply to everyone inside the tenant.
When genuinely ambiguous, choose "contact".
Few-shot scope examples:
Transcript: user: I prefer Linear for bug triage.
Fact: {"subject":"contact","predicate":"prefers","object":"Linear for bug triage","summary":"The contact prefers Linear for bug triage.","scope":"contact","confidence":0.95}
Transcript: user: For my work, repo/control-plane is my default repo.
Fact: {"subject":"contact","predicate":"uses as default repository","object":"repo/control-plane","summary":"The contact uses repo/control-plane as their default repository.","scope":"contact","confidence":0.92}
Transcript: user: We decided the API gateway runs on port 8443.
Fact: {"subject":"API gateway","predicate":"runs on port","object":"8443","summary":"The API gateway runs on port 8443.","scope":"tenant","confidence":0.94}
Transcript: user: Our team owns the billing reconciler service.
Fact: {"subject":"team","predicate":"owns","object":"billing reconciler service","summary":"The team owns the billing reconciler service.","scope":"tenant","confidence":0.93}
Return a JSON array and nothing else."#;

/// Fact extractor backed by a Cohere chat model.
#[derive(Clone)]
pub struct LlmFactExtractor {
    client: LlmChatClient,
    max_facts_per_chunk: usize,
}

impl LlmFactExtractor {
    /// Creates an LLM extractor from memory extraction config.
    pub fn from_config(config: &MemoryExtractionConfig) -> Result<Self> {
        let client =
            LlmChatClient::from_env(&config.api_key_env, &config.model, config.timeout_ms)?;
        Ok(Self::new(client, config.max_facts_per_chunk))
    }

    /// Creates an LLM extractor from an already configured chat client.
    #[must_use]
    pub fn new(client: LlmChatClient, max_facts_per_chunk: usize) -> Self {
        Self {
            client,
            max_facts_per_chunk,
        }
    }

    fn user_prompt(&self, chunk: &TurnChunk) -> String {
        format!(
            "prompt_version: {EXTRACTION_PROMPT_VERSION}\nmax_facts_per_chunk: {}\nchunk_index: {}\n\nTRANSCRIPT:\n{}",
            self.max_facts_per_chunk, chunk.index, chunk.text
        )
    }
}

#[async_trait]
impl FactExtractor for LlmFactExtractor {
    async fn extract(&self, chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>> {
        validate_chunks(chunks)?;
        let mut facts = Vec::new();
        for chunk in chunks {
            let response = self
                .client
                .chat(EXTRACTION_SYSTEM_PROMPT, &self.user_prompt(chunk))
                .await?;
            let parsed = parse_extraction_response(&response)?;
            let extracted = parsed
                .into_iter()
                .map(|fact| {
                    fact.into_extracted(chunk.index)
                        .map(normalize_extracted_fact)
                })
                .collect::<Result<Vec<_>>>()?;
            facts.extend(
                extracted
                    .into_iter()
                    .filter(should_keep_extracted_fact)
                    .take(self.max_facts_per_chunk),
            );
        }
        Ok(facts)
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

fn parse_extraction_response(response: &str) -> Result<Vec<LlmExtractedFact>> {
    let stripped = strip_json_code_fence(response);
    serde_json::from_str::<Vec<LlmExtractedFact>>(stripped).map_err(|error| {
        IngestError::Extraction(format!(
            "failed to parse LLM extraction JSON array: {error}"
        ))
    })
}

fn strip_json_code_fence(response: &str) -> &str {
    let trimmed = response.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("json").unwrap_or(rest).trim_start();
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

#[derive(Debug, Clone, Deserialize)]
struct LlmExtractedFact {
    subject: String,
    predicate: String,
    object: String,
    summary: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    confidence: Option<f64>,
}

impl LlmExtractedFact {
    fn into_extracted(self, source_chunk: usize) -> Result<ExtractedFact> {
        let scope_hint = match self.scope.trim().to_ascii_lowercase().as_str() {
            "tenant" => ExtractedFactScopeHint::Tenant,
            _ => ExtractedFactScopeHint::Contact,
        };
        let confidence = self.confidence.map(clamp_confidence);
        let mut fact = ExtractedFact {
            uid: Uuid::nil(),
            subject: self.subject.trim().to_string(),
            predicate: self.predicate.trim().to_string(),
            object: self.object.trim().to_string(),
            summary: self.summary.trim().to_string(),
            source_chunk,
            scope_hint,
            confidence,
        };
        let hash = fact_hash(&fact)?;
        fact.uid = fact_uid_from_hash(&hash);
        Ok(fact)
    }
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

    #[test]
    fn scope_rubric_v2_prompt_contains_few_shot_pairs_and_contact_default() {
        // Pins: v2 extraction prompt makes ambiguous scope privacy-preserving and examples explicit.
        assert_eq!(EXTRACTION_PROMPT_VERSION, "v2");
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("When genuinely ambiguous, choose \"contact\"."));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("I prefer Linear for bug triage"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("For my work, repo/control-plane"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("We decided the API gateway runs on port 8443"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("Our team owns the billing reconciler service"));
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
