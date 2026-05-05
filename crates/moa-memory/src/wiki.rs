//! Markdown wiki page parsing and serialization helpers.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use moa_core::{ConfidenceLevel, MemoryPath, PageType, Result, WikiPage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::memory_error;

const FRONTMATTER_DELIMITER: &str = "---";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PageFrontmatter {
    #[serde(rename = "type", default)]
    page_type: Option<PageType>,
    #[serde(default)]
    created: Option<DateTime<Utc>>,
    #[serde(default)]
    updated: Option<DateTime<Utc>>,
    #[serde(default)]
    confidence: Option<ConfidenceLevel>,
    #[serde(default)]
    related: Vec<String>,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    auto_generated: Option<bool>,
    #[serde(default)]
    last_referenced: Option<DateTime<Utc>>,
    #[serde(default)]
    reference_count: Option<u64>,
}

const RESERVED_FRONTMATTER_KEYS: &[&str] = &[
    "type",
    "created",
    "updated",
    "confidence",
    "related",
    "sources",
    "tags",
    "auto_generated",
    "last_referenced",
    "reference_count",
];

/// Parses a markdown document into a shared `WikiPage`.
pub fn parse_markdown(path: Option<MemoryPath>, markdown: &str) -> Result<WikiPage> {
    let now = Utc::now();
    let (frontmatter, metadata, content) = split_frontmatter(markdown)?;
    let page_type = frontmatter
        .page_type
        .unwrap_or_else(|| infer_page_type(path.as_ref()));
    let title = extract_title(content).unwrap_or_else(|| fallback_title(path.as_ref()));

    Ok(WikiPage {
        path,
        title,
        page_type,
        content: content.to_string(),
        created: frontmatter.created.unwrap_or(now),
        updated: frontmatter.updated.unwrap_or(now),
        confidence: frontmatter.confidence.unwrap_or(ConfidenceLevel::Medium),
        related: frontmatter.related,
        sources: frontmatter.sources,
        tags: frontmatter.tags,
        auto_generated: frontmatter.auto_generated.unwrap_or(false),
        last_referenced: frontmatter.last_referenced.unwrap_or(now),
        reference_count: frontmatter.reference_count.unwrap_or(0),
        metadata,
    })
}

/// Serializes a shared `WikiPage` into markdown with YAML frontmatter.
pub fn render_markdown(page: &WikiPage) -> Result<String> {
    if matches!(page.page_type, PageType::Index)
        && page
            .path
            .as_ref()
            .is_some_and(|path| path.as_str().eq_ignore_ascii_case("MEMORY.md"))
    {
        return Ok(page.content.clone());
    }

    let frontmatter = PageFrontmatter {
        page_type: Some(page.page_type.clone()),
        created: Some(page.created),
        updated: Some(page.updated),
        confidence: Some(page.confidence.clone()),
        related: page.related.clone(),
        sources: page.sources.clone(),
        tags: page.tags.clone(),
        auto_generated: Some(page.auto_generated),
        last_referenced: Some(page.last_referenced),
        reference_count: Some(page.reference_count),
    };
    let yaml = render_frontmatter(&frontmatter, &page.metadata)?;
    let body = page.content.trim_start_matches('\n');

    Ok(format!(
        "{delimiter}\n{yaml}{delimiter}\n\n{body}",
        delimiter = FRONTMATTER_DELIMITER
    ))
}

fn split_frontmatter(markdown: &str) -> Result<(PageFrontmatter, HashMap<String, Value>, &str)> {
    if !markdown.starts_with(FRONTMATTER_DELIMITER) {
        return Ok((PageFrontmatter::default(), HashMap::new(), markdown));
    }

    let remainder = markdown[FRONTMATTER_DELIMITER.len()..]
        .strip_prefix('\n')
        .or_else(|| markdown[FRONTMATTER_DELIMITER.len()..].strip_prefix("\r\n"));
    let Some(remainder) = remainder else {
        return Ok((PageFrontmatter::default(), HashMap::new(), markdown));
    };

    let Some((yaml_block, body)) = remainder.split_once(&format!("\n{FRONTMATTER_DELIMITER}\n"))
    else {
        return Ok((PageFrontmatter::default(), HashMap::new(), markdown));
    };
    let raw_yaml = serde_yaml::from_str::<serde_yaml::Value>(yaml_block).map_err(memory_error)?;
    let frontmatter =
        serde_yaml::from_value::<PageFrontmatter>(raw_yaml.clone()).map_err(memory_error)?;
    let metadata = extract_extra_metadata(raw_yaml)?;
    let body = body.strip_prefix('\n').unwrap_or(body);

    Ok((frontmatter, metadata, body))
}

fn infer_page_type(path: Option<&MemoryPath>) -> PageType {
    let Some(path) = path else {
        return PageType::Topic;
    };

    match path.as_str() {
        "MEMORY.md" => PageType::Index,
        "_schema.md" => PageType::Schema,
        "_log.md" => PageType::Log,
        value if value.starts_with("topics/") => PageType::Topic,
        value if value.starts_with("entities/") => PageType::Entity,
        value if value.starts_with("decisions/") => PageType::Decision,
        value if value.starts_with("skills/") => PageType::Skill,
        value if value.starts_with("sources/") => PageType::Source,
        _ => PageType::Topic,
    }
}

fn extract_title(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| {
            line.strip_prefix("# ")
                .map(|title| title.trim().to_string())
        })
        .filter(|title| !title.is_empty())
}

fn fallback_title(path: Option<&MemoryPath>) -> String {
    let Some(path) = path else {
        return "Untitled".to_string();
    };

    path.as_str()
        .rsplit('/')
        .next()
        .unwrap_or(path.as_str())
        .trim_end_matches(".md")
        .replace('-', " ")
}

fn render_frontmatter(
    frontmatter: &PageFrontmatter,
    metadata: &HashMap<String, Value>,
) -> Result<String> {
    let mut mapping = serde_yaml::Mapping::new();
    let reserved_keys: HashSet<&str> = RESERVED_FRONTMATTER_KEYS.iter().copied().collect();
    let base = serde_yaml::to_value(frontmatter).map_err(memory_error)?;
    let Some(base_mapping) = base.as_mapping() else {
        return Err(memory_error("page frontmatter must serialize to a mapping"));
    };
    for (key, value) in base_mapping {
        mapping.insert(key.clone(), value.clone());
    }

    for (key, value) in metadata {
        if reserved_keys.contains(key.as_str()) {
            continue;
        }
        mapping.insert(
            serde_yaml::Value::String(key.clone()),
            json_to_yaml_value(value)?,
        );
    }

    serde_yaml::to_string(&mapping).map_err(memory_error)
}

fn extract_extra_metadata(raw_yaml: serde_yaml::Value) -> Result<HashMap<String, Value>> {
    let Some(mapping) = raw_yaml.as_mapping() else {
        return Err(memory_error("page frontmatter must be a YAML mapping"));
    };
    let reserved_keys: HashSet<&str> = RESERVED_FRONTMATTER_KEYS.iter().copied().collect();
    let mut metadata = HashMap::new();

    for (key, value) in mapping {
        let Some(key) = key.as_str() else {
            continue;
        };
        if reserved_keys.contains(key) {
            continue;
        }
        metadata.insert(key.to_string(), yaml_to_json_value(value)?);
    }

    Ok(metadata)
}

fn yaml_to_json_value(value: &serde_yaml::Value) -> Result<Value> {
    match value {
        serde_yaml::Value::Null => Ok(Value::Null),
        serde_yaml::Value::Bool(value) => Ok(Value::Bool(*value)),
        serde_yaml::Value::Number(value) => {
            if let Some(number) = value.as_i64() {
                Ok(Value::Number(number.into()))
            } else if let Some(number) = value.as_u64() {
                Ok(Value::Number(number.into()))
            } else if let Some(number) = value.as_f64() {
                serde_json::Number::from_f64(number)
                    .map(Value::Number)
                    .ok_or_else(|| memory_error("invalid floating-point frontmatter value"))
            } else {
                Err(memory_error("unsupported numeric frontmatter value"))
            }
        }
        serde_yaml::Value::String(value) => Ok(Value::String(value.clone())),
        serde_yaml::Value::Sequence(values) => values
            .iter()
            .map(yaml_to_json_value)
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        serde_yaml::Value::Mapping(values) => {
            let mut object = serde_json::Map::with_capacity(values.len());
            for (key, value) in values {
                let Some(key) = key.as_str() else {
                    return Err(memory_error("frontmatter object keys must be strings"));
                };
                object.insert(key.to_string(), yaml_to_json_value(value)?);
            }
            Ok(Value::Object(object))
        }
        serde_yaml::Value::Tagged(value) => yaml_to_json_value(&value.value),
    }
}

fn json_to_yaml_value(value: &Value) -> Result<serde_yaml::Value> {
    match value {
        Value::Null => Ok(serde_yaml::Value::Null),
        Value::Bool(value) => Ok(serde_yaml::Value::Bool(*value)),
        Value::Number(value) => Ok(serde_yaml::to_value(value).map_err(memory_error)?),
        Value::String(value) => Ok(serde_yaml::Value::String(value.clone())),
        Value::Array(values) => values
            .iter()
            .map(json_to_yaml_value)
            .collect::<Result<Vec<_>>>()
            .map(serde_yaml::Value::Sequence),
        Value::Object(values) => {
            let mut mapping = serde_yaml::Mapping::new();
            for (key, value) in values {
                mapping.insert(
                    serde_yaml::Value::String(key.clone()),
                    json_to_yaml_value(value)?,
                );
            }
            Ok(serde_yaml::Value::Mapping(mapping))
        }
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{ConfidenceLevel, PageType};

    use super::{parse_markdown, render_markdown};

    #[test]
    fn wiki_page_roundtrip() {
        let markdown = r"---
type: topic
created: 2026-04-09T14:30:00Z
updated: 2026-04-09T16:45:00Z
confidence: high
related:
  - entities/auth-service.md
sources:
  - sources/rfc-0042-auth-redesign.md
tags:
  - security
  - auth
auto_generated: false
last_referenced: 2026-04-09T16:00:00Z
reference_count: 7
---

# Authentication Architecture

The auth system uses JWT.
";

        let page = parse_markdown(Some("topics/authentication.md".into()), markdown).unwrap();
        let rendered = render_markdown(&page).unwrap();
        let reparsed = parse_markdown(Some("topics/authentication.md".into()), &rendered).unwrap();

        assert_eq!(page, reparsed);
    }

    #[test]
    fn frontmatter_parsing_reads_expected_fields() {
        let markdown = r"---
type: skill
confidence: low
related:
  - topics/testing.md
sources:
  - sources/playbook.md
tags: [rust, testing]
auto_generated: true
reference_count: 3
---

# Run the tests

Use cargo test.
";

        let page = parse_markdown(Some("skills/run-tests.md".into()), markdown).unwrap();

        assert_eq!(page.page_type, PageType::Skill);
        assert_eq!(page.confidence, ConfidenceLevel::Low);
        assert_eq!(page.related, vec!["topics/testing.md"]);
        assert_eq!(page.sources, vec!["sources/playbook.md"]);
        assert_eq!(page.tags, vec!["rust", "testing"]);
        assert!(page.auto_generated);
        assert_eq!(page.reference_count, 3);
    }
}
