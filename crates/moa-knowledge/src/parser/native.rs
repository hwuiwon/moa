//! Native document parser backed by deterministic local parsing and liteparse.

use liteparse::{LiteParse, LiteParseConfig, config::OutputFormat, types::PdfInput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    domain::{DocumentElement, DocumentElementKind, ElementLayout, ParseInput, ParsedDocument},
    error::{Error, Result},
    normalize::{normalize_line_endings_and_unicode, normalize_text},
    parser::DocumentParser,
};

/// Local native parser for text-like inputs and liteparse-supported PDFs.
#[derive(Debug, Clone, Default)]
pub struct NativeDocumentParser;

impl NativeDocumentParser {
    /// Creates a native document parser.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl DocumentParser for NativeDocumentParser {
    async fn parse(&self, input: ParseInput) -> Result<ParsedDocument> {
        let mime_type = input.mime_type.as_deref().unwrap_or_default();
        let file_name = input.file_name.as_deref().unwrap_or_default();

        if let Some(text) = input.text {
            return parse_text_like(text, mime_type, file_name);
        }

        if is_native_text_like(mime_type, file_name) {
            let bytes = input.bytes.ok_or_else(|| {
                Error::UnsupportedFormat("native text parsing requires bytes or text".to_string())
            })?;
            let text = String::from_utf8(bytes).map_err(|error| {
                Error::parser("native", format!("text input was not valid UTF-8: {error}"))
            })?;
            return parse_text_like(text, mime_type, file_name);
        }

        if mime_type == "application/pdf" || file_name.ends_with(".pdf") {
            let bytes = input.bytes.ok_or_else(|| {
                Error::UnsupportedFormat(
                    "native PDF parsing requires bytes; use an external parser for URL-only input"
                        .to_string(),
                )
            })?;
            return parse_pdf_bytes(bytes).await;
        }

        Err(Error::UnsupportedFormat(format!(
            "native parser does not support `{}`; configure llamaparse, unstructured, or reducto",
            if mime_type.is_empty() {
                file_name
            } else {
                mime_type
            }
        )))
    }
}

/// Liteparse-shaped document used by tests and adapters with local layout output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeLiteparseDocument {
    /// Rendered parser text.
    pub text: String,
    /// Parsed pages in document order.
    #[serde(default)]
    pub pages: Vec<NativeLiteparsePage>,
}

/// Liteparse-shaped page used by the native adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeLiteparsePage {
    /// One-based page number.
    pub page_number: u32,
    /// Page width in parser coordinates.
    pub page_width: f32,
    /// Page height in parser coordinates.
    pub page_height: f32,
    /// Page rendered text.
    pub text: String,
    /// Text items in parser order.
    #[serde(default)]
    pub text_items: Vec<NativeLiteparseTextItem>,
}

/// Liteparse-shaped text item used by the native adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeLiteparseTextItem {
    /// Extracted text.
    pub text: String,
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
    /// OCR confidence when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// Adapts liteparse layout output into MOA document elements.
#[must_use]
pub fn map_liteparse_document(document: NativeLiteparseDocument) -> ParsedDocument {
    let mut elements = Vec::new();
    let mut ordinal = 0u32;
    for page in &document.pages {
        for item in &page.text_items {
            let text = normalize_text(&item.text);
            if text.is_empty() {
                continue;
            }
            elements.push(DocumentElement {
                element_id: format!("liteparse:p{}:{ordinal}", page.page_number),
                kind: DocumentElementKind::Paragraph,
                text,
                heading_path: Vec::new(),
                ordinal,
                page_number: Some(page.page_number),
                layout: Some(ElementLayout {
                    x: item.x,
                    y: item.y,
                    width: item.width,
                    height: item.height,
                    page_width: Some(page.page_width),
                    page_height: Some(page.page_height),
                    confidence: item.confidence,
                }),
                metadata: json!({
                    "parser_output_format": "markdown",
                    "source": "liteparse"
                }),
            });
            ordinal = ordinal.saturating_add(1);
        }
    }

    ParsedDocument {
        parser: "native".to_string(),
        parser_job_id: None,
        text: normalize_text(&document.text),
        elements,
        metadata: json!({
            "parser_output_format": "markdown",
            "source": "liteparse",
            "pages": document.pages.len()
        }),
    }
}

fn is_native_text_like(mime_type: &str, file_name: &str) -> bool {
    mime_type.starts_with("text/")
        || matches!(mime_type, "application/json" | "application/csv")
        || file_name.ends_with(".md")
        || file_name.ends_with(".txt")
        || file_name.ends_with(".csv")
        || file_name.ends_with(".json")
        || file_name.ends_with(".html")
}

fn parse_text_like(text: String, mime_type: &str, file_name: &str) -> Result<ParsedDocument> {
    if file_name.ends_with(".md") || mime_type == "text/markdown" {
        return Ok(markdown_document(text));
    }
    if file_name.ends_with(".html") || mime_type == "text/html" {
        return Ok(html_document(text));
    }
    if file_name.ends_with(".json") || mime_type == "application/json" {
        return json_document(text);
    }
    if file_name.ends_with(".csv") || mime_type == "text/csv" || mime_type == "application/csv" {
        return Ok(csv_document(text));
    }
    Ok(plain_text_document(text))
}

fn plain_text_document(text: String) -> ParsedDocument {
    let normalized = normalize_text(&text);
    ParsedDocument {
        parser: "native".to_string(),
        parser_job_id: None,
        text: normalized.clone(),
        elements: normalized
            .split("\n\n")
            .enumerate()
            .filter_map(|(ordinal, paragraph)| {
                let paragraph = paragraph.trim();
                if paragraph.is_empty() {
                    return None;
                }
                Some(DocumentElement {
                    element_id: format!("native:text:{ordinal}"),
                    kind: DocumentElementKind::Paragraph,
                    text: paragraph.to_string(),
                    heading_path: Vec::new(),
                    ordinal: ordinal as u32,
                    page_number: None,
                    layout: None,
                    metadata: json!({ "parser_output_format": "text" }),
                })
            })
            .collect(),
        metadata: json!({ "parser_output_format": "text" }),
    }
}

fn markdown_document(text: String) -> ParsedDocument {
    let normalized = normalize_line_endings_and_unicode(&text);
    let mut elements = Vec::new();
    let mut heading_path = Vec::<String>::new();
    let mut paragraph = Vec::<String>::new();
    let mut ordinal = 0u32;
    let mut in_fence = false;

    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            paragraph.push(line.to_string());
            continue;
        }
        if !in_fence && trimmed.starts_with('#') {
            flush_paragraph(
                &mut elements,
                &mut paragraph,
                &heading_path,
                &mut ordinal,
                "markdown",
            );
            let level = trimmed
                .chars()
                .take_while(|ch| *ch == '#')
                .count()
                .clamp(1, 6);
            let title = normalize_text(trimmed[level..].trim());
            if !title.is_empty() {
                heading_path.truncate(level.saturating_sub(1));
                heading_path.push(title.clone());
                push_element(
                    &mut elements,
                    DocumentElementKind::Heading,
                    title,
                    heading_path.clone(),
                    &mut ordinal,
                    "markdown",
                );
            }
        } else if !in_fence && (trimmed.starts_with("- ") || trimmed.starts_with("* ")) {
            flush_paragraph(
                &mut elements,
                &mut paragraph,
                &heading_path,
                &mut ordinal,
                "markdown",
            );
            push_element(
                &mut elements,
                DocumentElementKind::ListItem,
                trimmed[2..].to_string(),
                heading_path.clone(),
                &mut ordinal,
                "markdown",
            );
        } else if !in_fence && trimmed.contains('|') {
            flush_paragraph(
                &mut elements,
                &mut paragraph,
                &heading_path,
                &mut ordinal,
                "markdown",
            );
            push_element(
                &mut elements,
                DocumentElementKind::Table,
                trimmed.to_string(),
                heading_path.clone(),
                &mut ordinal,
                "markdown",
            );
        } else if trimmed.is_empty() {
            flush_paragraph(
                &mut elements,
                &mut paragraph,
                &heading_path,
                &mut ordinal,
                "markdown",
            );
        } else {
            paragraph.push(line.to_string());
        }
    }
    flush_paragraph(
        &mut elements,
        &mut paragraph,
        &heading_path,
        &mut ordinal,
        "markdown",
    );
    parsed_from_elements("markdown", elements)
}

fn html_document(text: String) -> ParsedDocument {
    let mut elements = Vec::new();
    let mut heading_path = Vec::<String>::new();
    let mut ordinal = 0u32;
    for (tag, body) in extract_html_blocks(&text) {
        let kind = match tag.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => DocumentElementKind::Heading,
            "li" => DocumentElementKind::ListItem,
            "table" | "tr" => DocumentElementKind::Table,
            _ => DocumentElementKind::Paragraph,
        };
        let clean = normalize_text(&strip_html_tags(&body));
        if clean.is_empty() {
            continue;
        }
        if kind == DocumentElementKind::Heading {
            let level = tag
                .trim_start_matches('h')
                .parse::<usize>()
                .unwrap_or(1)
                .clamp(1, 6);
            heading_path.truncate(level.saturating_sub(1));
            heading_path.push(clean.clone());
        }
        push_element(
            &mut elements,
            kind,
            clean,
            heading_path.clone(),
            &mut ordinal,
            "html",
        );
    }
    if elements.is_empty() {
        return plain_text_document(strip_html_tags(&text));
    }
    parsed_from_elements("html", elements)
}

fn json_document(text: String) -> Result<ParsedDocument> {
    let value = serde_json::from_str::<Value>(&text).map_err(|error| {
        Error::parser(
            "native",
            format!("JSON input could not be decoded: {error}"),
        )
    })?;
    let mut elements = Vec::new();
    let mut ordinal = 0u32;
    flatten_json_value("$", &value, &mut elements, &mut ordinal);
    Ok(parsed_from_elements("json", elements))
}

fn csv_document(text: String) -> ParsedDocument {
    let mut elements = Vec::new();
    let mut ordinal = 0u32;
    let normalized = normalize_line_endings_and_unicode(&text);
    let mut lines = normalized.lines();
    let header = lines.next().map(parse_csv_line).unwrap_or_default();
    for line in lines {
        let cells = parse_csv_line(line);
        let rendered = cells
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let key = header
                    .get(index)
                    .filter(|value| !value.is_empty())
                    .map_or_else(|| format!("column_{}", index + 1), ToOwned::to_owned);
                format!("{key}: {}", normalize_text(value))
            })
            .collect::<Vec<_>>()
            .join(" | ");
        push_element(
            &mut elements,
            DocumentElementKind::Table,
            rendered,
            Vec::new(),
            &mut ordinal,
            "csv",
        );
    }
    parsed_from_elements("csv", elements)
}

async fn parse_pdf_bytes(bytes: Vec<u8>) -> Result<ParsedDocument> {
    let config = LiteParseConfig {
        output_format: OutputFormat::Markdown,
        ocr_enabled: false,
        quiet: true,
        ..Default::default()
    };
    let result = LiteParse::new(config)
        .parse_input(PdfInput::Bytes(bytes))
        .await
        .map_err(|error| {
            Error::parser("native", format!("liteparse PDF parsing failed: {error}"))
        })?;

    Ok(map_liteparse_document(NativeLiteparseDocument {
        text: result.text,
        pages: result
            .pages
            .into_iter()
            .map(|page| NativeLiteparsePage {
                page_number: page.page_number as u32,
                page_width: page.page_width,
                page_height: page.page_height,
                text: page.text,
                text_items: page
                    .text_items
                    .into_iter()
                    .map(|item| NativeLiteparseTextItem {
                        text: item.text,
                        x: item.x,
                        y: item.y,
                        width: item.width,
                        height: item.height,
                        confidence: item.confidence,
                    })
                    .collect(),
            })
            .collect(),
    }))
}

fn flush_paragraph(
    elements: &mut Vec<DocumentElement>,
    paragraph: &mut Vec<String>,
    heading_path: &[String],
    ordinal: &mut u32,
    parser_output_format: &str,
) {
    if paragraph.is_empty() {
        return;
    }
    let text = normalize_text(&paragraph.join("\n"));
    paragraph.clear();
    if text.is_empty() {
        return;
    }
    push_element(
        elements,
        DocumentElementKind::Paragraph,
        text,
        heading_path.to_vec(),
        ordinal,
        parser_output_format,
    );
}

fn push_element(
    elements: &mut Vec<DocumentElement>,
    kind: DocumentElementKind,
    text: String,
    heading_path: Vec<String>,
    ordinal: &mut u32,
    parser_output_format: &str,
) {
    elements.push(DocumentElement {
        element_id: format!("native:{parser_output_format}:{}", *ordinal),
        kind,
        text: normalize_text(&text),
        heading_path,
        ordinal: *ordinal,
        page_number: None,
        layout: None,
        metadata: json!({ "parser_output_format": parser_output_format }),
    });
    *ordinal = ordinal.saturating_add(1);
}

fn parsed_from_elements(
    parser_output_format: &str,
    elements: Vec<DocumentElement>,
) -> ParsedDocument {
    let text = normalize_text(
        &elements
            .iter()
            .map(|element| element.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    ParsedDocument {
        parser: "native".to_string(),
        parser_job_id: None,
        text,
        elements,
        metadata: json!({ "parser_output_format": parser_output_format }),
    }
}

fn extract_html_blocks(input: &str) -> Vec<(String, String)> {
    let tags = ["h1", "h2", "h3", "h4", "h5", "h6", "p", "li", "tr", "table"];
    let mut blocks = Vec::new();
    let mut cursor = 0usize;
    let lowercase = input.to_ascii_lowercase();
    while cursor < input.len() {
        let Some(start_rel) = lowercase[cursor..].find('<') else {
            break;
        };
        let start = cursor + start_rel;
        let Some(end_rel) = lowercase[start..].find('>') else {
            break;
        };
        let tag_text = lowercase[start + 1..start + end_rel]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches('/')
            .to_string();
        cursor = start + end_rel + 1;
        if !tags.contains(&tag_text.as_str()) {
            continue;
        }
        let closing = format!("</{tag_text}>");
        if let Some(close_rel) = lowercase[cursor..].find(&closing) {
            let close = cursor + close_rel;
            blocks.push((tag_text, input[cursor..close].to_string()));
            cursor = close + closing.len();
        }
    }
    blocks
}

fn strip_html_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn flatten_json_value(
    path: &str,
    value: &Value,
    elements: &mut Vec<DocumentElement>,
    ordinal: &mut u32,
) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                flatten_json_value(&format!("{path}.{key}"), value, elements, ordinal);
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                flatten_json_value(&format!("{path}[{index}]"), value, elements, ordinal);
            }
        }
        Value::Null => {}
        Value::String(text) => push_element(
            elements,
            DocumentElementKind::Field,
            format!("{path}: {text}"),
            Vec::new(),
            ordinal,
            "json",
        ),
        other => push_element(
            elements,
            DocumentElementKind::Field,
            format!("{path}: {other}"),
            Vec::new(),
            ordinal,
            "json",
        ),
    }
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                cell.push('"');
                let _ = chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                cells.push(cell.trim().to_string());
                cell.clear();
            }
            _ => cell.push(ch),
        }
    }
    cells.push(cell.trim().to_string());
    cells
}
