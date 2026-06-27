//! LlamaParse document parser adapter.

use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};

use crate::{
    domain::{DocumentElement, DocumentElementKind, ParseInput, ParsedDocument},
    error::{Error, Result},
    normalize::normalize_text,
    parser::DocumentParser,
    providers::http,
};

const POLL_ATTEMPTS: usize = 30;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// HTTP adapter for LlamaParse.
#[derive(Clone)]
pub struct LlamaParseParser {
    client: Client,
    base_url: String,
    api_key: String,
    tier: String,
    version: String,
    expand: Vec<String>,
}

impl LlamaParseParser {
    /// Creates a parser with the default HTTP client.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        tier: impl Into<String>,
        expand: Vec<String>,
    ) -> Result<Self> {
        Ok(Self::with_client(
            http::build_http_client()?,
            base_url,
            api_key,
            tier,
            "latest",
            expand,
        ))
    }

    /// Creates a parser with an injected HTTP client.
    #[must_use]
    pub fn with_client(
        client: Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        tier: impl Into<String>,
        version: impl Into<String>,
        expand: Vec<String>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            tier: tier.into(),
            version: version.into(),
            expand,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }
}

#[async_trait::async_trait]
impl DocumentParser for LlamaParseParser {
    async fn parse(&self, input: ParseInput) -> Result<ParsedDocument> {
        self.validate_config()?;
        let expand = if self.expand.is_empty() {
            vec![
                "markdown".to_string(),
                "markdown_full".to_string(),
                "items".to_string(),
                "page_metadata".to_string(),
                "metadata".to_string(),
                "job_metadata".to_string(),
            ]
        } else {
            self.expand.clone()
        };
        let source_url = input.source_url.clone();
        let file_id = input
            .options
            .get("file_id")
            .or_else(|| input.options.get("llamaparse_file_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if source_url.is_none() && file_id.is_none() {
            return Err(Error::parser(
                "llamaparse",
                "parse input requires source_url or file_id",
            ));
        }
        let mut body = serde_json::Map::new();
        if let Some(source_url) = source_url {
            body.insert("source_url".to_string(), Value::String(source_url));
        }
        if let Some(file_id) = file_id {
            body.insert("file_id".to_string(), Value::String(file_id));
        }
        body.insert("tier".to_string(), Value::String(self.tier.clone()));
        body.insert("version".to_string(), Value::String(self.version.clone()));
        if cost_optimizer_supported(&self.tier) {
            body.insert(
                "processing_options".to_string(),
                json!({ "cost_optimizer": { "enable": true } }),
            );
        }
        let response = self
            .client
            .post(self.url("/api/v2/parse"))
            .bearer_auth(&self.api_key)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|error| {
                Error::parser("llamaparse", format!("parse submission failed: {error}"))
            })?;
        let submitted: Value = http::json_response(response).await?;
        let job_id = string_field(&submitted, &["id", "job.id", "job_id"])
            .ok_or_else(|| Error::parser("llamaparse", "parse response missing job id"))?;
        let result = if submitted.get("markdown").is_some() || submitted.get("items").is_some() {
            submitted
        } else {
            let mut url = parse_url(&self.url(&format!("/api/v2/parse/{job_id}")))?;
            url.query_pairs_mut()
                .append_pair("expand", &expand.join(","));
            let mut result = Value::Null;
            for attempt in 0..POLL_ATTEMPTS {
                let response = self
                    .client
                    .get(url.clone())
                    .bearer_auth(&self.api_key)
                    .send()
                    .await
                    .map_err(|error| {
                        Error::parser("llamaparse", format!("result retrieval failed: {error}"))
                    })?;
                result = http::json_response(response).await?;
                ensure_llamaparse_not_failed(&result)?;
                if !is_pending_status(&result) {
                    break;
                }
                if attempt + 1 == POLL_ATTEMPTS {
                    return Err(Error::parser("llamaparse", "parse job did not complete"));
                }
                sleep(POLL_INTERVAL).await;
            }
            result
        };
        ensure_llamaparse_not_failed(&result)?;
        Ok(map_llamaparse_result(job_id, result))
    }
}

impl LlamaParseParser {
    fn validate_config(&self) -> Result<()> {
        if self.base_url.trim().is_empty() {
            return Err(Error::Config(
                "llamaparse parser requires api_base_url".to_string(),
            ));
        }
        if self.api_key.trim().is_empty() {
            return Err(Error::Config(
                "llamaparse parser requires api_key".to_string(),
            ));
        }
        if self.tier.trim().is_empty() {
            return Err(Error::Config("llamaparse parser requires tier".to_string()));
        }
        if self.version.trim().is_empty() {
            return Err(Error::Config(
                "llamaparse parser requires version".to_string(),
            ));
        }
        Ok(())
    }
}

/// Maps a LlamaParse result payload into a parsed document.
#[must_use]
pub fn map_llamaparse_result(job_id: String, value: Value) -> ParsedDocument {
    let markdown = llamaparse_markdown(&value);
    let items = llamaparse_items(&value);
    let mut elements = Vec::new();
    let mut heading_path = Vec::<String>::new();
    for (ordinal, item) in items.iter().enumerate() {
        let text = string_field(item, &["text", "value", "content", "md"]).unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let raw_type =
            string_field(item, &["type", "item_type"]).unwrap_or_else(|| "paragraph".to_string());
        let kind = map_kind(&raw_type);
        if kind == DocumentElementKind::Heading {
            let level = item
                .get("level")
                .or_else(|| item.get("heading_level"))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 6) as usize;
            heading_path.truncate(level.saturating_sub(1));
            heading_path.push(normalize_text(&text));
        }
        let page_number = item
            .get("page")
            .or_else(|| item.get("page_number"))
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        elements.push(DocumentElement {
            element_id: string_field(item, &["id"])
                .unwrap_or_else(|| format!("llamaparse:{job_id}:{ordinal}")),
            kind,
            text: normalize_text(&text),
            heading_path: heading_path.clone(),
            ordinal: ordinal as u32,
            page_number,
            layout: None,
            metadata: json!({
                "provider": "llamaparse",
                "job_id": job_id,
                "page_number": page_number,
                "item_type": raw_type,
                "parser_timing": value.get("timing").or_else(|| value.get("parser_timing")).cloned().unwrap_or(Value::Null),
                "parser_version": value.get("version").or_else(|| value.pointer("/job/version")).or_else(|| value.pointer("/job_metadata/version")).cloned().unwrap_or(Value::Null),
                "metadata": item.get("metadata").cloned().unwrap_or(Value::Null)
            }),
        });
    }
    if elements.is_empty() && !markdown.trim().is_empty() {
        elements.push(DocumentElement {
            element_id: format!("llamaparse:{job_id}:markdown"),
            kind: DocumentElementKind::Paragraph,
            text: normalize_text(&markdown),
            heading_path: Vec::new(),
            ordinal: 0,
            page_number: None,
            layout: None,
            metadata: json!({ "provider": "llamaparse", "job_id": job_id }),
        });
    }
    ParsedDocument {
        parser: "llamaparse".to_string(),
        parser_job_id: Some(job_id),
        text: normalize_text(&markdown),
        elements,
        metadata: json!({
            "parser_status": value.get("status").or_else(|| value.pointer("/job/status")).or_else(|| value.pointer("/job_metadata/status")).cloned().unwrap_or(Value::Null),
            "parser_errors": value.get("errors").or_else(|| value.pointer("/metadata/errors")).cloned().unwrap_or(Value::Null),
            "metadata": value.get("metadata").cloned().unwrap_or(Value::Null),
            "job_metadata": value.get("job_metadata").cloned().unwrap_or(Value::Null),
            "page_metadata": value.get("page_metadata").or_else(|| value.pointer("/metadata/pages")).or_else(|| value.get("pages")).cloned().unwrap_or(Value::Null),
            "parser_timing": value.get("timing").or_else(|| value.get("parser_timing")).cloned().unwrap_or(Value::Null),
            "parser_version": value.get("version").or_else(|| value.pointer("/job/version")).or_else(|| value.pointer("/job_metadata/version")).cloned().unwrap_or(Value::Null)
        }),
    }
}

fn llamaparse_markdown(value: &Value) -> String {
    if let Some(markdown) = string_field(value, &["markdown_full", "markdown", "result.markdown"]) {
        return markdown;
    }
    value
        .pointer("/markdown/pages")
        .and_then(Value::as_array)
        .map(|pages| {
            pages
                .iter()
                .filter_map(|page| string_field(page, &["markdown", "md"]))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

fn llamaparse_items(value: &Value) -> Vec<Value> {
    if let Some(items) = value
        .get("items")
        .or_else(|| value.pointer("/result/items"))
        .and_then(Value::as_array)
    {
        return items.clone();
    }
    value
        .pointer("/items/pages")
        .and_then(Value::as_array)
        .map(|pages| {
            pages
                .iter()
                .flat_map(|page| {
                    let page_number = page.get("page_number").cloned();
                    page.get("items")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(move |item| {
                            let mut item = item.clone();
                            if let (Some(object), Some(page_number)) =
                                (item.as_object_mut(), page_number.clone())
                            {
                                object
                                    .entry("page_number".to_string())
                                    .or_insert(page_number);
                            }
                            item
                        })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn map_kind(raw: &str) -> DocumentElementKind {
    match raw.to_ascii_lowercase().as_str() {
        "heading" | "title" => DocumentElementKind::Heading,
        "table" | "table_row" => DocumentElementKind::Table,
        "list" | "list_item" => DocumentElementKind::ListItem,
        "figure" | "image" => DocumentElementKind::Figure,
        "page" => DocumentElementKind::Page,
        _ => DocumentElementKind::Paragraph,
    }
}

fn cost_optimizer_supported(tier: &str) -> bool {
    matches!(tier, "agentic" | "agentic_plus")
}

fn ensure_llamaparse_not_failed(value: &Value) -> Result<()> {
    let status = value
        .get("status")
        .or_else(|| value.pointer("/job/status"))
        .or_else(|| value.pointer("/job_metadata/status"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(status.as_str(), "error" | "failed" | "failure") {
        return Err(Error::parser("llamaparse", "parse job failed"));
    }
    Ok(())
}

fn is_pending_status(value: &Value) -> bool {
    let status = value
        .get("status")
        .or_else(|| value.pointer("/job/status"))
        .or_else(|| value.pointer("/job_metadata/status"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        status.as_str(),
        "pending" | "queued" | "running" | "processing"
    )
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

fn parse_url(value: &str) -> Result<reqwest::Url> {
    reqwest::Url::parse(value)
        .map_err(|error| Error::parser("llamaparse", format!("invalid URL `{value}`: {error}")))
}
