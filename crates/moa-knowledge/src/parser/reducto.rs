//! Reducto parser adapter.

use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};

use crate::{
    domain::{DocumentElement, DocumentElementKind, ElementLayout, ParseInput, ParsedDocument},
    error::{Error, Result},
    normalize::normalize_text,
    parser::DocumentParser,
    providers::http::{self, string_field, value_field},
};

const POLL_ATTEMPTS: usize = 30;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// HTTP adapter for Reducto Parse.
#[derive(Clone)]
pub struct ReductoParser {
    client: Client,
    base_url: String,
    api_key: String,
    parse_mode: String,
    async_enabled: bool,
    chunk_mode: String,
    force_url_result: bool,
}

impl ReductoParser {
    /// Creates a Reducto parser with the default HTTP client.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        parse_mode: impl Into<String>,
        async_enabled: bool,
        chunk_mode: impl Into<String>,
        force_url_result: bool,
    ) -> Result<Self> {
        Ok(Self::with_client(
            http::build_http_client()?,
            base_url,
            api_key,
            parse_mode,
            async_enabled,
            chunk_mode,
            force_url_result,
        ))
    }

    /// Creates a Reducto parser with an injected HTTP client.
    #[must_use]
    pub fn with_client(
        client: Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        parse_mode: impl Into<String>,
        async_enabled: bool,
        chunk_mode: impl Into<String>,
        force_url_result: bool,
    ) -> Self {
        Self {
            client,
            base_url: http::trim_base_url(base_url.into()),
            api_key: api_key.into(),
            parse_mode: parse_mode.into(),
            async_enabled,
            chunk_mode: chunk_mode.into(),
            force_url_result,
        }
    }

    fn url(&self, path: &str) -> String {
        http::join_url(&self.base_url, path)
    }
}

#[async_trait::async_trait]
impl DocumentParser for ReductoParser {
    async fn parse(&self, input: ParseInput) -> Result<ParsedDocument> {
        self.validate_config()?;
        let endpoint = if self.async_enabled {
            "/parse_async"
        } else {
            "/parse"
        };
        let file_id = input
            .options
            .get("file_id")
            .or_else(|| input.options.get("reducto_file_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let parser_input = file_id
            .or_else(|| input.source_url.clone())
            .or_else(|| {
                input
                    .options
                    .get("presigned_url")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .ok_or_else(|| {
                Error::parser("reducto", "parse input requires source_url or file_id")
            })?;
        let mut settings = serde_json::Map::new();
        settings.insert(
            "force_url_result".to_string(),
            Value::Bool(self.force_url_result),
        );
        if matches!(self.parse_mode.as_str(), "ocr" | "hybrid") {
            settings.insert(
                "extraction_mode".to_string(),
                Value::String(self.parse_mode.clone()),
            );
        }
        let response = self
            .client
            .post(self.url(endpoint))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "input": parser_input,
                "options": {
                    "chunking": {
                        "chunk_mode": self.chunk_mode,
                    },
                },
                "settings": settings,
            }))
            .send()
            .await
            .map_err(|error| Error::parser("reducto", format!("parse request failed: {error}")))?;
        let mut value: Value = http::json_response(response).await?;
        if self.async_enabled {
            let job_id = string_field(&value, &["job_id", "id"])
                .ok_or_else(|| Error::parser("reducto", "async parse response missing job_id"))?;
            for attempt in 0..POLL_ATTEMPTS {
                let response = self
                    .client
                    .get(self.url(&format!("/job/{job_id}")))
                    .bearer_auth(&self.api_key)
                    .send()
                    .await
                    .map_err(|error| {
                        Error::parser("reducto", format!("job retrieval failed: {error}"))
                    })?;
                value = http::json_response(response).await?;
                ensure_reducto_not_failed(&value)?;
                if !is_pending_status(&value) {
                    break;
                }
                if attempt + 1 == POLL_ATTEMPTS {
                    return Err(Error::parser("reducto", "async parse job did not complete"));
                }
                sleep(POLL_INTERVAL).await;
            }
        }
        ensure_reducto_not_failed(&value)?;
        if let Some(result_url) = string_field(
            &value,
            &[
                "result_url",
                "url",
                "result_url.url",
                "result.url",
                "result.result_url",
                "result.result.url",
            ],
        ) {
            validate_result_url(&self.base_url, &result_url)?;
            let response = self.client.get(result_url).send().await.map_err(|error| {
                Error::parser("reducto", format!("URL result retrieval failed: {error}"))
            })?;
            let result = http::json_response(response).await?;
            ensure_reducto_not_failed(&result)?;
            value = merge_result_metadata(value, result);
        }
        Ok(map_reducto_result(value))
    }
}

impl ReductoParser {
    fn validate_config(&self) -> Result<()> {
        if self.base_url.trim().is_empty() {
            return Err(Error::Config(
                "reducto parser requires api_base_url".to_string(),
            ));
        }
        if self.api_key.trim().is_empty() {
            return Err(Error::Config("reducto parser requires api_key".to_string()));
        }
        if self.parse_mode.trim().is_empty() {
            return Err(Error::Config(
                "reducto parser requires parse_mode".to_string(),
            ));
        }
        if self.chunk_mode.trim().is_empty() {
            return Err(Error::Config(
                "reducto parser requires chunk_mode".to_string(),
            ));
        }
        Ok(())
    }
}

/// Maps a Reducto parse result into a parsed document.
#[must_use]
pub(crate) fn map_reducto_result(value: Value) -> ParsedDocument {
    let job_id = string_field(&value, &["job_id", "id"]);
    let chunks = value
        .get("chunks")
        .or_else(|| value.pointer("/result/chunks"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut elements = Vec::new();
    let mut ordinal = 0u32;
    let mut heading_path = Vec::<String>::new();
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        let chunk_text = string_field(chunk, &["content", "text", "markdown", "chunk_content"])
            .unwrap_or_default();
        if !chunk_text.trim().is_empty() {
            let page_number = chunk
                .get("page")
                .or_else(|| chunk.get("page_number"))
                .and_then(Value::as_u64)
                .map(|value| value as u32);
            elements.push(DocumentElement {
                element_id: string_field(chunk, &["id", "chunk_id"])
                    .unwrap_or_else(|| format!("reducto:chunk:{chunk_index}")),
                kind: DocumentElementKind::ParserChunk,
                text: normalize_text(&chunk_text),
                heading_path: heading_path.clone(),
                ordinal,
                page_number,
                layout: None,
                metadata: json!({
                    "provider": "reducto",
                    "job_id": job_id,
                    "parse_mode": value.get("parse_mode").or_else(|| value.get("mode")).cloned().unwrap_or(Value::Null),
                    "chunk_content": chunk.get("content").or_else(|| chunk.get("chunk_content")).cloned().unwrap_or(Value::Null),
                    "chunk_metadata": chunk.get("metadata").cloned().unwrap_or(Value::Null),
                    "embedding_content": chunk.get("embedding_content").or_else(|| chunk.get("embedding_optimized_content")).cloned().unwrap_or(Value::Null),
                    "page_number": page_number
                }),
            });
            ordinal = ordinal.saturating_add(1);
        }
        if let Some(blocks) = chunk.get("blocks").and_then(Value::as_array) {
            for (block_index, block) in blocks.iter().enumerate() {
                let block_text = string_field(block, &["content", "text"]).unwrap_or_default();
                if block_text.trim().is_empty() {
                    continue;
                }
                let raw_type = string_field(block, &["type", "block_type"])
                    .unwrap_or_else(|| "paragraph".to_string());
                let kind = map_kind(&raw_type);
                if kind == DocumentElementKind::Heading {
                    heading_path.clear();
                    heading_path.push(normalize_text(&block_text));
                }
                let page_number = block
                    .get("page")
                    .or_else(|| block.get("page_number"))
                    .or_else(|| block.pointer("/bounding_box/page"))
                    .and_then(Value::as_u64)
                    .map(|value| value as u32);
                elements.push(DocumentElement {
                    element_id: string_field(block, &["id"])
                        .unwrap_or_else(|| format!("reducto:block:{chunk_index}:{block_index}")),
                    kind,
                    text: normalize_text(&block_text),
                    heading_path: heading_path.clone(),
                    ordinal,
                    page_number,
                    layout: block_layout(block),
                    metadata: json!({
                        "provider": "reducto",
                        "job_id": job_id,
                        "block_type": raw_type,
                        "block_content": block.get("content").or_else(|| block.get("text")).cloned().unwrap_or(Value::Null),
                        "bounding_box": block.get("bbox").or_else(|| block.get("bounding_box")).cloned().unwrap_or(Value::Null),
                        "page_number": page_number,
                        "confidence": block.get("confidence").cloned().unwrap_or(Value::Null)
                    }),
                });
                ordinal = ordinal.saturating_add(1);
            }
        }
    }
    let text = normalize_text(
        &elements
            .iter()
            .map(|element| element.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    ParsedDocument {
        parser: "reducto".to_string(),
        parser_job_id: job_id.clone(),
        text,
        elements,
        metadata: json!({
            "job_id": job_id,
            "parser_status": value.get("status").or_else(|| value.pointer("/result/status")).cloned().unwrap_or(Value::Null),
            "parser_errors": value.get("errors").or_else(|| value.pointer("/result/errors")).cloned().unwrap_or(Value::Null),
            "processing_duration": value.get("duration").or_else(|| value.get("processing_duration")).cloned().unwrap_or(Value::Null),
            "usage": value.get("usage").cloned().unwrap_or(Value::Null),
            "usage_pages": value.pointer("/usage/pages").or_else(|| value.pointer("/usage/num_pages")).or_else(|| value.get("usage_pages")).cloned().unwrap_or(Value::Null),
            "usage_credits": value.pointer("/usage/credits").or_else(|| value.get("usage_credits")).cloned().unwrap_or(Value::Null),
            "studio_link": value.get("studio_link").cloned().unwrap_or(Value::Null),
            "parse_mode": value.get("parse_mode").or_else(|| value.get("mode")).cloned().unwrap_or(Value::Null)
        }),
    }
}

fn merge_result_metadata(envelope: Value, mut result: Value) -> Value {
    let Some(result_object) = result.as_object_mut() else {
        return envelope;
    };
    for (key, paths) in [
        ("job_id", &["job_id", "id", "result.job_id"][..]),
        ("status", &["status", "result.status"][..]),
        ("duration", &["duration", "result.duration"][..]),
        (
            "processing_duration",
            &[
                "processing_duration",
                "duration",
                "result.processing_duration",
                "result.duration",
            ][..],
        ),
        ("usage", &["usage", "result.usage"][..]),
        ("studio_link", &["studio_link", "result.studio_link"][..]),
        (
            "parse_mode",
            &["parse_mode", "mode", "result.parse_mode", "result.mode"][..],
        ),
    ] {
        if result_object.get(key).is_none()
            && let Some(value) = value_field(&envelope, paths)
        {
            result_object.insert(key.to_string(), value);
        }
    }
    result
}

fn block_layout(block: &Value) -> Option<ElementLayout> {
    let bbox = block.get("bbox").or_else(|| block.get("bounding_box"))?;
    Some(ElementLayout {
        x: bbox.get("x").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        y: bbox.get("y").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        width: bbox.get("width").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        height: bbox.get("height").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        page_width: bbox
            .get("page_width")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
        page_height: bbox
            .get("page_height")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
        confidence: block
            .get("confidence")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
    })
}

const REDUCTO_STATUS_POINTERS: &[&str] = &["/status", "/result/status"];

fn ensure_reducto_not_failed(value: &Value) -> Result<()> {
    if http::status_failed(&http::parse_status(value, REDUCTO_STATUS_POINTERS)) {
        return Err(Error::parser("reducto", "parse job failed"));
    }
    if value.get("errors").is_some() || value.pointer("/result/errors").is_some() {
        return Err(Error::parser("reducto", "parse job returned errors"));
    }
    Ok(())
}

fn is_pending_status(value: &Value) -> bool {
    http::status_pending(&http::parse_status(value, REDUCTO_STATUS_POINTERS))
}

fn validate_result_url(base_url: &str, result_url: &str) -> Result<()> {
    let result = reqwest::Url::parse(result_url)
        .map_err(|error| Error::parser("reducto", format!("result_url was invalid: {error}")))?;
    if result.scheme() == "https" {
        return Ok(());
    }
    let base = reqwest::Url::parse(base_url)
        .map_err(|error| Error::parser("reducto", format!("api_base_url was invalid: {error}")))?;
    if result.scheme() == base.scheme() && result.host_str() == base.host_str() {
        return Ok(());
    }
    Err(Error::parser(
        "reducto",
        "result_url must be HTTPS or same-origin as api_base_url",
    ))
}

fn map_kind(raw: &str) -> DocumentElementKind {
    match raw.to_ascii_lowercase().as_str() {
        "title" | "heading" | "header" => DocumentElementKind::Heading,
        "table" => DocumentElementKind::Table,
        "figure" | "image" => DocumentElementKind::Figure,
        "list" | "list_item" => DocumentElementKind::ListItem,
        _ => DocumentElementKind::Paragraph,
    }
}
