//! Cohere chat-backed fact extraction prompt and response parsing.

use serde::Deserialize;

use super::{LlmChatClient, LlmChatError};

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

/// Transcript chunk sent to the memory fact extraction model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmFactExtractionChunk {
    /// Stable chunk index from the upstream transcript chunker.
    pub index: usize,
    /// Transcript text for this chunk.
    pub text: String,
}

/// Structured fact returned by the memory fact extraction model.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmExtractedFact {
    /// Concise subject noun phrase.
    pub subject: String,
    /// Concise relation phrase.
    pub predicate: String,
    /// Concise object noun phrase or value.
    pub object: String,
    /// One sentence restating the fact.
    pub summary: String,
    /// Model-selected scope hint.
    pub scope: String,
    /// Optional model confidence.
    pub confidence: Option<f64>,
    /// Source transcript chunk index.
    pub source_chunk: usize,
}

/// Cohere chat-backed client for extracting memory facts.
#[derive(Clone)]
pub struct LlmFactExtractionClient {
    client: LlmChatClient,
    max_facts_per_chunk: usize,
}

impl LlmFactExtractionClient {
    /// Creates a fact extraction client from a configured chat transport.
    #[must_use]
    pub fn new(client: LlmChatClient, max_facts_per_chunk: usize) -> Self {
        Self {
            client,
            max_facts_per_chunk,
        }
    }

    /// Returns structured facts extracted from the provided chunks.
    pub async fn extract(
        &self,
        chunks: &[LlmFactExtractionChunk],
    ) -> Result<Vec<LlmExtractedFact>, LlmChatError> {
        let mut facts = Vec::new();
        for chunk in chunks {
            let response = self
                .client
                .chat(EXTRACTION_SYSTEM_PROMPT, &self.user_prompt(chunk))
                .await?;
            let parsed = parse_extraction_response(&response)?;
            facts.extend(
                parsed
                    .into_iter()
                    .take(self.max_facts_per_chunk)
                    .map(|fact| LlmExtractedFact {
                        subject: fact.subject,
                        predicate: fact.predicate,
                        object: fact.object,
                        summary: fact.summary,
                        scope: fact.scope,
                        confidence: fact.confidence,
                        source_chunk: chunk.index,
                    }),
            );
        }
        Ok(facts)
    }

    fn user_prompt(&self, chunk: &LlmFactExtractionChunk) -> String {
        format!(
            "prompt_version: {EXTRACTION_PROMPT_VERSION}\nmax_facts_per_chunk: {}\nchunk_index: {}\n\nTRANSCRIPT:\n{}",
            self.max_facts_per_chunk, chunk.index, chunk.text
        )
    }
}

fn parse_extraction_response(response: &str) -> Result<Vec<ParsedLlmExtractedFact>, LlmChatError> {
    let stripped = strip_json_code_fence(response);
    serde_json::from_str::<Vec<ParsedLlmExtractedFact>>(stripped).map_err(|error| {
        LlmChatError::Malformed {
            message: format!("failed to parse LLM extraction JSON array: {error}"),
        }
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
struct ParsedLlmExtractedFact {
    subject: String,
    predicate: String,
    object: String,
    summary: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    confidence: Option<f64>,
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn extraction_client_sends_prompt_version_and_parses_fenced_json() {
        // Pins: provider-owned memory extraction prompt and JSON response contract.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/chat"))
            .and(body_string_contains(EXTRACTION_PROMPT_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": [{"type": "text", "text": r#"```json
[
  {
    "subject": "contact",
    "predicate": "prefers",
    "object": "Linear",
    "summary": "The contact prefers Linear.",
    "scope": "contact",
    "confidence": 0.91
  }
]
```"#}]}
            })))
            .mount(&server)
            .await;
        let client =
            LlmChatClient::from_api_key(SecretString::from("test-key"), "command-test", 1_000)
                .with_endpoint(format!("{}/v2/chat", server.uri()));
        let extractor = LlmFactExtractionClient::new(client, 5);

        let facts = extractor
            .extract(&[LlmFactExtractionChunk {
                index: 3,
                text: "user: I prefer Linear.".to_string(),
            }])
            .await
            .expect("extract facts");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].subject, "contact");
        assert_eq!(facts[0].predicate, "prefers");
        assert_eq!(facts[0].object, "Linear");
        assert_eq!(facts[0].source_chunk, 3);
        assert_eq!(facts[0].confidence, Some(0.91));
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
}
