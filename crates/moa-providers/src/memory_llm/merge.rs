//! Cohere chat-backed entity merge verification prompt.

use super::{LlmChatClient, LlmChatError};

/// Merge-verifier prompt version used for recorded fixtures.
pub const MERGE_PROMPT_VERSION: &str = "v1";

const MERGE_SYSTEM_PROMPT: &str = r#"You decide whether two extracted entity mentions refer to the same real entity in graph memory.
Answer with exactly one lowercase word: yes or no.
Say yes only when the mention is a paraphrase, abbreviation, casing variant, or punctuation variant of the candidate.
Say no when the terms could name different services, repositories, people, teams, credentials, or documents.
When ambiguous, answer no."#;

/// Cohere chat-backed client for verifying whether two entity mentions should merge.
#[derive(Clone)]
pub struct LlmEntityMergeClient {
    client: LlmChatClient,
}

impl LlmEntityMergeClient {
    /// Creates a merge verifier client from the shared chat transport.
    #[must_use]
    pub fn new(client: LlmChatClient) -> Self {
        Self { client }
    }

    /// Returns whether a mention should merge into a candidate entity.
    pub async fn should_merge(
        &self,
        mention: &str,
        candidate_name: &str,
        normalized_candidate_name: &str,
    ) -> Result<bool, LlmChatError> {
        let user = format!(
            "Mention: {}\nCandidate: {}\nCandidate normalized name: {}\n",
            mention.trim(),
            candidate_name.trim(),
            normalized_candidate_name
        );
        let answer = self.client.chat(MERGE_SYSTEM_PROMPT, &user).await?;
        Ok(parse_merge_answer(&answer))
    }
}

fn parse_merge_answer(answer: &str) -> bool {
    match answer.trim().to_ascii_lowercase().as_str() {
        "yes" => true,
        "no" => false,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn verifier_false_or_malformed_means_no_merge() {
        // Pins: ambiguous merge-verifier output is fail-closed to avoid corrupting entity links.
        assert!(parse_merge_answer("yes"));
        assert!(!parse_merge_answer("no"));
        assert!(!parse_merge_answer("maybe"));
        assert!(!parse_merge_answer("yes, probably"));
    }

    #[tokio::test]
    async fn merge_client_sends_mention_and_candidate_context() {
        // Pins: provider-owned merge prompt sends raw and normalized candidate context.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/chat"))
            .and(body_string_contains("Mention: checkout service"))
            .and(body_string_contains("Candidate: CheckoutSvc"))
            .and(body_string_contains(
                "Candidate normalized name: checkoutsvc",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": [{"type": "text", "text": "yes"}]}
            })))
            .mount(&server)
            .await;
        let client =
            LlmChatClient::from_api_key(SecretString::from("test-key"), "command-test", 1_000)
                .with_endpoint(format!("{}/v2/chat", server.uri()));
        let verifier = LlmEntityMergeClient::new(client);

        let should_merge = verifier
            .should_merge("checkout service", "CheckoutSvc", "checkoutsvc")
            .await
            .expect("merge decision");

        assert!(should_merge);
    }
}
