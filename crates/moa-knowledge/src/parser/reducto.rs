//! Reducto parser adapter.

use reqwest::Client;
use serde_json::{Value, json};

use crate::{
    domain::{DocumentElement, DocumentElementKind, ElementLayout, ParseInput, ParsedDocument},
    error::{Error, Result},
    normalize::normalize_text,
    parser::DocumentParser,
    providers::http,
};

/// HTTP adapter for Reducto Parse.
#[derive(Debug, Clone)]
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
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            parse_mode: parse_mode.into(),
            async_enabled,
            chunk_mode: chunk_mode.into(),
            force_url_result,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
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
            .cloned();
        let presigned_url = input.options.get("presigned_url").cloned();
        let response = self
            .client
            .post(self.url(endpoint))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "document_url": input.source_url,
                "presigned_url": presigned_url,
                "file_id": file_id,
                "mode": self.parse_mode,
                "chunk_mode": self.chunk_mode,
                "force_url_result": self.force_url_result,
            }))
            .send()
            .await
            .map_err(|error| Error::parser("reducto", format!("parse request failed: {error}")))?;
        let mut value: Value = http::json_response(response).await?;
        if self.async_enabled {
            let job_id = string_field(&value, &["job_id", "id"])
                .ok_or_else(|| Error::parser("reducto", "async parse response missing job_id"))?;
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
        }
        if let Some(result_url) = string_field(&value, &["result_url", "url", "result_url.url"]) {
            let response = self.client.get(result_url).send().await.map_err(|error| {
                Error::parser("reducto", format!("URL result retrieval failed: {error}"))
            })?;
            value = http::json_response(response).await?;
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
pub fn map_reducto_result(value: Value) -> ParsedDocument {
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
                    heading_path.truncate(0);
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
            "processing_duration": value.get("duration").or_else(|| value.get("processing_duration")).cloned().unwrap_or(Value::Null),
            "usage": value.get("usage").cloned().unwrap_or(Value::Null),
            "usage_pages": value.pointer("/usage/pages").or_else(|| value.get("usage_pages")).cloned().unwrap_or(Value::Null),
            "usage_credits": value.pointer("/usage/credits").or_else(|| value.get("usage_credits")).cloned().unwrap_or(Value::Null),
            "studio_link": value.get("studio_link").cloned().unwrap_or(Value::Null),
            "parse_mode": value.get("parse_mode").or_else(|| value.get("mode")).cloned().unwrap_or(Value::Null)
        }),
    }
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

fn map_kind(raw: &str) -> DocumentElementKind {
    match raw.to_ascii_lowercase().as_str() {
        "title" | "heading" | "header" => DocumentElementKind::Heading,
        "table" => DocumentElementKind::Table,
        "figure" | "image" => DocumentElementKind::Figure,
        "list" | "list_item" => DocumentElementKind::ListItem,
        _ => DocumentElementKind::Paragraph,
    }
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let mut current = value;
        for segment in key.split('.') {
            current = current.get(segment)?;
        }
        current.as_str().map(ToOwned::to_owned)
    })
}
