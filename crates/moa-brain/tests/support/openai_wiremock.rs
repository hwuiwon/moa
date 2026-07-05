//! Wiremock helpers for OpenAI Responses API offline tests.

#![allow(dead_code)]

use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mounts a wiremock OpenAI Responses stream that returns the supplied text.
pub async fn mount_openai_text(server: &MockServer, text: impl Into<String>, cached_tokens: usize) {
    Mock::given(any())
        .respond_with(openai_text_response(text.into(), cached_tokens))
        .mount(server)
        .await;
}

/// Mounts a wiremock OpenAI Responses JSON response that returns the supplied text.
pub async fn mount_openai_json_text(
    server: &MockServer,
    text: impl Into<String>,
    cached_tokens: usize,
) {
    Mock::given(any())
        .respond_with(openai_json_response(text.into(), cached_tokens))
        .mount(server)
        .await;
}

/// Returns captured request bodies as JSON values.
pub async fn captured_json_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests")
        .into_iter()
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .collect()
}

fn openai_json_response(text: String, cached_tokens: usize) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
        .set_body_json(json!({
            "id": "resp_offline",
            "object": "response",
            "created_at": 1,
            "model": "gpt-5.4-nano",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                    "logprobs": null
                }]
            }],
            "status": "completed",
            "usage": {
                "input_tokens": 16,
                "input_tokens_details": { "cached_tokens": cached_tokens },
                "output_tokens": 4,
                "output_tokens_details": { "reasoning_tokens": 0 },
                "total_tokens": 20
            }
        }))
}

fn openai_text_response(text: String, cached_tokens: usize) -> ResponseTemplate {
    let events = [
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_offline",
                "object": "response",
                "created_at": 1,
                "model": "gpt-5.4",
                "output": [],
                "status": "in_progress"
            }
        }),
        json!({
            "type": "response.output_text.delta",
            "sequence_number": 1,
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": text,
            "logprobs": null
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "id": "resp_offline",
                "object": "response",
                "created_at": 1,
                "completed_at": 2,
                "model": "gpt-5.4",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": text,
                        "annotations": [],
                        "logprobs": null
                    }]
                }],
                "status": "completed",
                "usage": {
                    "input_tokens": 16,
                    "input_tokens_details": { "cached_tokens": cached_tokens },
                    "output_tokens": 4,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 20
                }
            }
        }),
    ];
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();

    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .insert_header("cache-control", "no-cache")
        .set_body_raw(body, "text/event-stream")
}
