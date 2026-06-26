//! Unstructured partitioning parser adapter.

use reqwest::Client;
use serde_json::{Value, json};

use crate::{
    domain::{DocumentElement, DocumentElementKind, ElementLayout, ParseInput, ParsedDocument},
    error::{Error, Result},
    normalize::normalize_text,
    parser::DocumentParser,
    providers::http,
};

/// HTTP adapter for Unstructured partitioning.
#[derive(Debug, Clone)]
pub struct UnstructuredParser {
    client: Client,
    base_url: String,
    api_key: String,
    strategy: String,
    chunking_strategy: String,
}

impl UnstructuredParser {
    /// Creates an Unstructured parser with the default HTTP client.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        strategy: impl Into<String>,
        chunking_strategy: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::with_client(
            http::build_http_client()?,
            base_url,
            api_key,
            strategy,
            chunking_strategy,
        ))
    }

    /// Creates an Unstructured parser with an injected HTTP client.
    #[must_use]
    pub fn with_client(
        client: Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        strategy: impl Into<String>,
        chunking_strategy: impl Into<String>,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            strategy: strategy.into(),
            chunking_strategy: chunking_strategy.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }
}

#[async_trait::async_trait]
impl DocumentParser for UnstructuredParser {
    async fn parse(&self, input: ParseInput) -> Result<ParsedDocument> {
        self.validate_config()?;
        let response = self
            .client
            .post(self.url("/general/v0/general"))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "url": input.source_url,
                "content": input.text,
                "filename": input.file_name,
                "strategy": self.strategy,
                "chunking_strategy": self.chunking_strategy,
                "chunking_options": input.options.get("chunking_options").cloned().unwrap_or(Value::Null),
                "coordinates": true,
            }))
            .send()
            .await
            .map_err(|error| {
                Error::parser("unstructured", format!("partition request failed: {error}"))
            })?;
        let value: Value = http::json_response(response).await?;
        Ok(map_unstructured_elements(value))
    }
}

impl UnstructuredParser {
    fn validate_config(&self) -> Result<()> {
        if self.base_url.trim().is_empty() {
            return Err(Error::Config(
                "unstructured parser requires api_base_url".to_string(),
            ));
        }
        if self.api_key.trim().is_empty() {
            return Err(Error::Config(
                "unstructured parser requires api_key".to_string(),
            ));
        }
        if self.strategy.trim().is_empty() {
            return Err(Error::Config(
                "unstructured parser requires strategy".to_string(),
            ));
        }
        Ok(())
    }
}

/// Maps Unstructured partition elements into a parsed document.
#[must_use]
pub fn map_unstructured_elements(value: Value) -> ParsedDocument {
    let array = value
        .as_array()
        .cloned()
        .or_else(|| {
            value
                .get("elements")
                .or_else(|| value.get("results"))
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default();
    let mut elements = Vec::new();
    let mut heading_path = Vec::<String>::new();
    for (ordinal, item) in array.iter().enumerate() {
        let text = string_field(item, &["text"]).unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        let raw_type = string_field(item, &["type", "category"])
            .unwrap_or_else(|| "NarrativeText".to_string());
        let metadata = item.get("metadata").cloned().unwrap_or(Value::Null);
        let kind = map_kind(&raw_type);
        if kind == DocumentElementKind::Heading {
            let heading = normalize_text(&text);
            if !heading.is_empty() {
                heading_path.truncate(0);
                heading_path.push(heading);
            }
        }
        let page_number = metadata
            .get("page_number")
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        elements.push(DocumentElement {
            element_id: string_field(item, &["element_id", "id"])
                .unwrap_or_else(|| format!("unstructured:{ordinal}")),
            kind,
            text: normalize_text(&text),
            heading_path: heading_path.clone(),
            ordinal: ordinal as u32,
            page_number,
            layout: coordinates_to_layout(&metadata),
            metadata: json!({
                "provider": "unstructured",
                "element_type": raw_type,
                "parent_id": metadata.get("parent_id").or_else(|| item.get("parent_id")).cloned().unwrap_or(Value::Null),
                "filetype": metadata.get("filetype").cloned().unwrap_or(Value::Null),
                "source": metadata.get("source").or_else(|| item.get("source")).cloned().unwrap_or(Value::Null),
                "page_number": page_number,
                "parser_chunk": matches!(kind, DocumentElementKind::ParserChunk),
                "metadata": metadata
            }),
        });
    }
    let text = normalize_text(
        &elements
            .iter()
            .map(|element| element.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    ParsedDocument {
        parser: "unstructured".to_string(),
        parser_job_id: None,
        text,
        elements,
        metadata: json!({ "provider": "unstructured" }),
    }
}

fn map_kind(raw: &str) -> DocumentElementKind {
    match raw {
        "Title" | "Header" => DocumentElementKind::Heading,
        "ListItem" => DocumentElementKind::ListItem,
        "Table" => DocumentElementKind::Table,
        "Footer" | "NarrativeText" | "HeaderFooter" | "Text" => DocumentElementKind::Paragraph,
        "FigureCaption" => DocumentElementKind::Figure,
        "CompositeElement" => DocumentElementKind::ParserChunk,
        _ => DocumentElementKind::Other,
    }
}

fn coordinates_to_layout(metadata: &Value) -> Option<ElementLayout> {
    let points = metadata.get("coordinates")?.get("points")?.as_array()?;
    let xs: Vec<f32> = points
        .iter()
        .filter_map(|point| {
            point
                .as_array()?
                .first()?
                .as_f64()
                .map(|value| value as f32)
        })
        .collect();
    let ys: Vec<f32> = points
        .iter()
        .filter_map(|point| point.as_array()?.get(1)?.as_f64().map(|value| value as f32))
        .collect();
    let min_x = xs.iter().copied().reduce(f32::min)?;
    let max_x = xs.iter().copied().reduce(f32::max)?;
    let min_y = ys.iter().copied().reduce(f32::min)?;
    let max_y = ys.iter().copied().reduce(f32::max)?;
    Some(ElementLayout {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
        page_width: metadata
            .pointer("/coordinates/layout_width")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
        page_height: metadata
            .pointer("/coordinates/layout_height")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
        confidence: metadata
            .get("detection_class_prob")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
    })
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key)?.as_str().map(ToOwned::to_owned))
}
