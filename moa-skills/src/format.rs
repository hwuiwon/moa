//! Agent Skill markdown parsing and rendering utilities.

use std::collections::HashMap;

use chrono::{DateTime, SecondsFormat, Utc};
use moa_core::{ConfidenceLevel, MemoryPath, MoaError, PageType, Result, SkillMetadata, WikiPage};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use tracing::warn;

const FRONTMATTER_DELIMITER: &str = "---";
const DEFAULT_VERSION: &str = "1.0";
const DEFAULT_SUCCESS_RATE: f32 = 1.0;
const META_VERSION: &str = "moa-version";
const META_ONE_LINER: &str = "moa-one-liner";
const META_TAGS: &str = "moa-tags";
const META_CREATED: &str = "moa-created";
const META_UPDATED: &str = "moa-updated";
const META_AUTO_GENERATED: &str = "moa-auto-generated";
const META_SOURCE_SESSION: &str = "moa-source-session";
const META_USE_COUNT: &str = "moa-use-count";
const META_LAST_USED: &str = "moa-last-used";
const META_SUCCESS_RATE: &str = "moa-success-rate";
const META_ESTIMATED_TOKENS: &str = "moa-estimated-tokens";
const META_IMPROVED_FROM: &str = "moa-improved-from";
const META_REGRESSION_COUNT: &str = "moa-regression-count";

/// Fully parsed Agent Skill document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillDocument {
    /// YAML frontmatter fields.
    pub frontmatter: SkillFrontmatter,
    /// Markdown instructions body without the YAML frontmatter.
    pub body: String,
}

/// Parsed Agent Skills frontmatter as stored on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    /// Stable skill name.
    pub name: String,
    /// Longer human-readable description.
    pub description: String,
    /// Optional license declaration from the Agent Skills spec.
    #[serde(default)]
    pub license: Option<String>,
    /// Optional compatibility note from the Agent Skills spec.
    #[serde(default)]
    pub compatibility: Option<String>,
    /// Optional allowlist of tools the skill expects to use.
    #[serde(
        default,
        rename = "allowed-tools",
        deserialize_with = "deserialize_allowed_tools",
        serialize_with = "serialize_allowed_tools",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allowed_tools: Vec<String>,
    /// Arbitrary metadata preserved from the Agent Skills spec.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    metadata: HashMap<String, String>,
}

impl SkillFrontmatter {
    /// Returns the MOA version for the skill, defaulting to the base format version.
    pub fn version(&self) -> String {
        self.metadata_string(META_VERSION)
            .unwrap_or_else(|| DEFAULT_VERSION.to_string())
    }

    pub(crate) fn set_version(&mut self, value: impl Into<String>) {
        self.insert_metadata(META_VERSION, value.into());
    }

    /// Returns the concise one-line summary used in MOA UIs.
    pub fn one_liner(&self) -> String {
        self.metadata_string(META_ONE_LINER)
            .unwrap_or_else(|| self.description.clone())
    }

    /// Returns the normalized skill tags.
    pub fn tags(&self) -> Vec<String> {
        metadata_csv(&self.metadata, META_TAGS)
    }

    pub(crate) fn set_tags(&mut self, tags: &[String]) {
        self.set_metadata_csv(META_TAGS, tags);
    }

    /// Returns the creation timestamp tracked by MOA.
    pub fn created(&self) -> DateTime<Utc> {
        self.metadata_timestamp(META_CREATED)
            .unwrap_or_else(Utc::now)
    }

    pub(crate) fn set_created(&mut self, value: DateTime<Utc>) {
        self.insert_metadata(META_CREATED, format_timestamp(value));
    }

    /// Returns the last-updated timestamp tracked by MOA.
    pub fn updated(&self) -> DateTime<Utc> {
        self.metadata_timestamp(META_UPDATED)
            .unwrap_or_else(|| self.created())
    }

    pub(crate) fn set_updated(&mut self, value: DateTime<Utc>) {
        self.insert_metadata(META_UPDATED, format_timestamp(value));
    }

    /// Returns whether MOA auto-generated the skill.
    pub fn auto_generated(&self) -> bool {
        self.metadata_bool(META_AUTO_GENERATED).unwrap_or(false)
    }

    pub(crate) fn set_auto_generated(&mut self, value: bool) {
        self.insert_metadata(META_AUTO_GENERATED, value.to_string());
    }

    pub(crate) fn set_source_session(&mut self, value: Option<String>) {
        self.set_optional_metadata(META_SOURCE_SESSION, value);
    }

    /// Returns how many times MOA has used this skill.
    pub fn use_count(&self) -> u32 {
        self.metadata_u32(META_USE_COUNT).unwrap_or(0)
    }

    pub(crate) fn set_use_count(&mut self, value: u32) {
        self.insert_metadata(META_USE_COUNT, value.to_string());
    }

    /// Returns the last time MOA used this skill, when known.
    pub fn last_used(&self) -> Option<DateTime<Utc>> {
        self.metadata_timestamp(META_LAST_USED)
    }

    pub(crate) fn set_last_used(&mut self, value: Option<DateTime<Utc>>) {
        self.set_optional_metadata(META_LAST_USED, value.map(format_timestamp));
    }

    /// Returns the tracked success rate for this skill.
    pub fn success_rate(&self) -> f32 {
        self.metadata_f32(META_SUCCESS_RATE)
            .unwrap_or(DEFAULT_SUCCESS_RATE)
    }

    pub(crate) fn set_success_rate(&mut self, value: f32) {
        self.insert_metadata(META_SUCCESS_RATE, value.to_string());
    }

    /// Returns the estimated token cost of loading the full skill body.
    pub fn estimated_tokens(&self, body: &str) -> usize {
        self.metadata_usize(META_ESTIMATED_TOKENS)
            .unwrap_or_else(|| estimate_skill_tokens(body))
    }

    pub(crate) fn set_improved_from(&mut self, value: Option<String>) {
        self.set_optional_metadata(META_IMPROVED_FROM, value);
    }

    /// Returns how many candidate improvements were rolled back for this skill.
    pub fn regression_count(&self) -> u32 {
        self.metadata_u32(META_REGRESSION_COUNT).unwrap_or(0)
    }

    pub(crate) fn set_regression_count(&mut self, value: u32) {
        self.insert_metadata(META_REGRESSION_COUNT, value.to_string());
    }

    /// Returns one raw metadata value by key.
    pub fn metadata_value(&self, key: &str) -> Option<&str> {
        self.metadata
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn metadata_string(&self, key: &str) -> Option<String> {
        self.metadata
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }

    pub(crate) fn metadata_timestamp(&self, key: &str) -> Option<DateTime<Utc>> {
        self.metadata_string(key)
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&Utc))
    }

    pub(crate) fn metadata_bool(&self, key: &str) -> Option<bool> {
        self.metadata_string(key)
            .and_then(|value| value.parse::<bool>().ok())
    }

    pub(crate) fn metadata_u32(&self, key: &str) -> Option<u32> {
        self.metadata_string(key)
            .and_then(|value| value.parse::<u32>().ok())
    }

    pub(crate) fn metadata_usize(&self, key: &str) -> Option<usize> {
        self.metadata_string(key)
            .and_then(|value| value.parse::<usize>().ok())
    }

    pub(crate) fn metadata_f32(&self, key: &str) -> Option<f32> {
        self.metadata_string(key)
            .and_then(|value| value.parse::<f32>().ok())
    }

    fn insert_metadata(&mut self, key: &str, value: String) {
        self.metadata.insert(key.to_string(), value);
    }

    fn set_optional_metadata(&mut self, key: &str, value: Option<String>) {
        if let Some(value) = value {
            self.insert_metadata(key, value);
        } else {
            self.metadata.remove(key);
        }
    }

    fn set_metadata_csv(&mut self, key: &str, values: &[String]) {
        if values.is_empty() {
            self.metadata.remove(key);
        } else {
            self.insert_metadata(key, values.join(", "));
        }
    }
}

/// Parses a `SKILL.md` document into a structured skill representation.
pub fn parse_skill_markdown(markdown: &str) -> Result<SkillDocument> {
    let (yaml_block, body) = split_frontmatter(markdown)?;
    let skill = SkillDocument {
        frontmatter: serde_yaml::from_str::<SkillFrontmatter>(yaml_block)
            .map_err(|error| MoaError::ValidationError(error.to_string()))?,
        body: body.trim_start_matches('\n').to_string(),
    };
    validate_skill_document(&skill)?;
    Ok(skill)
}

/// Renders a structured skill representation back into `SKILL.md` markdown.
pub fn render_skill_markdown(skill: &SkillDocument) -> Result<String> {
    validate_skill_document(skill)?;
    let yaml = serde_yaml::to_string(&skill.frontmatter)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    Ok(format!(
        "{delimiter}\n{yaml}{delimiter}\n\n{body}",
        delimiter = FRONTMATTER_DELIMITER,
        body = skill.body.trim_start_matches('\n')
    ))
}

/// Converts a parsed wiki page into a skill document.
pub fn skill_from_wiki_page(page: &WikiPage) -> Result<SkillDocument> {
    let metadata_json = serde_json::to_value(&page.metadata)?;
    let mut frontmatter = serde_json::from_value::<SkillFrontmatter>(metadata_json)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    frontmatter.set_created(page.created);
    frontmatter.set_updated(page.updated);
    frontmatter.set_auto_generated(page.auto_generated);
    if !page.tags.is_empty() && frontmatter.tags().is_empty() {
        frontmatter.set_tags(&page.tags);
    }
    let skill = SkillDocument {
        frontmatter,
        body: page.content.clone(),
    };
    validate_skill_document(&skill)?;
    Ok(skill)
}

/// Builds pipeline metadata for a parsed skill document.
pub fn skill_metadata_from_document(path: MemoryPath, skill: &SkillDocument) -> SkillMetadata {
    SkillMetadata {
        path,
        name: skill.frontmatter.name.clone(),
        description: skill.frontmatter.description.clone(),
        tags: skill.frontmatter.tags(),
        allowed_tools: skill.frontmatter.allowed_tools.clone(),
        estimated_tokens: skill.frontmatter.estimated_tokens(&skill.body),
        use_count: skill.frontmatter.use_count(),
        last_used: skill.frontmatter.last_used(),
        success_rate: skill.frontmatter.success_rate(),
        auto_generated: skill.frontmatter.auto_generated(),
    }
}

/// Builds pipeline metadata directly from a wiki page.
pub fn skill_metadata_from_page(path: MemoryPath, page: &WikiPage) -> Result<SkillMetadata> {
    let skill = skill_from_wiki_page(page)?;
    Ok(skill_metadata_from_document(path, &skill))
}

/// Converts a structured skill document into a shared wiki page.
pub fn wiki_page_from_skill(skill: &SkillDocument, path: Option<MemoryPath>) -> Result<WikiPage> {
    validate_skill_document(skill)?;
    let metadata = serde_json::from_value::<HashMap<String, Value>>(serde_json::to_value(
        &skill.frontmatter,
    )?)?;
    let reference_count = u64::from(skill.frontmatter.use_count());
    let last_referenced = skill
        .frontmatter
        .last_used()
        .unwrap_or_else(|| skill.frontmatter.updated());

    Ok(WikiPage {
        path,
        title: humanize_skill_name(&skill.frontmatter.name),
        page_type: PageType::Skill,
        content: skill.body.clone(),
        created: skill.frontmatter.created(),
        updated: skill.frontmatter.updated(),
        confidence: confidence_for_skill(skill.frontmatter.success_rate()),
        related: Vec::new(),
        sources: Vec::new(),
        tags: skill.frontmatter.tags(),
        auto_generated: skill.frontmatter.auto_generated(),
        last_referenced,
        reference_count,
        metadata,
    })
}

/// Returns the canonical memory path for a skill name.
pub fn build_skill_path(skill_name: &str) -> MemoryPath {
    MemoryPath::new(format!(
        "skills/{}/SKILL.md",
        slugify_skill_name(skill_name)
    ))
}

/// Converts an arbitrary skill name into a stable slug.
pub fn slugify_skill_name(skill_name: &str) -> String {
    let mut slug = String::new();
    let mut previous_was_separator = false;

    for character in skill_name.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !previous_was_separator && !slug.is_empty() {
            slug.push('-');
            previous_was_separator = true;
        }
    }

    slug.trim_matches('-').to_string()
}

fn split_frontmatter(markdown: &str) -> Result<(&str, &str)> {
    if !markdown.starts_with(FRONTMATTER_DELIMITER) {
        return Err(MoaError::ValidationError(
            "skill markdown must start with YAML frontmatter".to_string(),
        ));
    }

    let remainder = markdown[FRONTMATTER_DELIMITER.len()..]
        .strip_prefix('\n')
        .or_else(|| markdown[FRONTMATTER_DELIMITER.len()..].strip_prefix("\r\n"))
        .ok_or_else(|| {
            MoaError::ValidationError("invalid skill frontmatter delimiter".to_string())
        })?;
    let (yaml_block, body) = remainder
        .split_once(&format!("\n{FRONTMATTER_DELIMITER}\n"))
        .ok_or_else(|| {
            MoaError::ValidationError("skill frontmatter closing delimiter missing".to_string())
        })?;
    Ok((yaml_block, body))
}

fn validate_skill_document(skill: &SkillDocument) -> Result<()> {
    for (field_name, value) in [
        ("name", skill.frontmatter.name.trim()),
        ("description", skill.frontmatter.description.trim()),
    ] {
        if value.is_empty() {
            return Err(MoaError::ValidationError(format!(
                "skill frontmatter field `{field_name}` must not be empty"
            )));
        }
    }

    if !is_valid_skill_name(&skill.frontmatter.name) {
        warn!(
            skill = %skill.frontmatter.name,
            "skill name does not follow the recommended Agent Skills slug format"
        );
    }

    if skill.frontmatter.version().trim().is_empty() {
        return Err(MoaError::ValidationError(
            "skill version metadata must not be empty".to_string(),
        ));
    }

    if skill.frontmatter.one_liner().trim().is_empty() {
        return Err(MoaError::ValidationError(
            "skill summary metadata must not be empty".to_string(),
        ));
    }

    if skill.frontmatter.estimated_tokens(&skill.body) == 0 {
        return Err(MoaError::ValidationError(
            "skill frontmatter `moa-estimated-tokens` must be greater than zero".to_string(),
        ));
    }

    if !(0.0..=1.0).contains(&skill.frontmatter.success_rate()) {
        return Err(MoaError::ValidationError(
            "skill `success_rate` must be between 0.0 and 1.0".to_string(),
        ));
    }

    Ok(())
}

fn estimate_skill_tokens(body: &str) -> usize {
    body.split_whitespace().count().max(1)
}

fn format_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn metadata_csv(metadata: &HashMap<String, String>, key: &str) -> Vec<String> {
    metadata
        .get(key)
        .map(String::as_str)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn is_valid_skill_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 64
        && !trimmed.starts_with('-')
        && !trimmed.ends_with('-')
        && !trimmed.contains("--")
        && trimmed.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

fn deserialize_allowed_tools<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    Ok(value
        .split_whitespace()
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn serialize_allowed_tools<S>(
    allowed_tools: &[String],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&allowed_tools.join(" "))
}

fn humanize_skill_name(skill_name: &str) -> String {
    skill_name
        .split(['-', '_', ' '])
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut characters = segment.chars();
            match characters.next() {
                Some(first) => format!(
                    "{}{}",
                    first.to_ascii_uppercase(),
                    characters.as_str().to_ascii_lowercase()
                ),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn confidence_for_skill(success_rate: f32) -> ConfidenceLevel {
    if success_rate >= 0.85 {
        ConfidenceLevel::High
    } else if success_rate >= 0.6 {
        ConfidenceLevel::Medium
    } else {
        ConfidenceLevel::Low
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SkillDocument, build_skill_path, parse_skill_markdown, render_skill_markdown,
        slugify_skill_name, wiki_page_from_skill,
    };

    const VALID_SKILL: &str = r#"---
name: deploy-to-fly
description: "Deploy applications to Fly.io staging and production"
compatibility: "Requires flyctl auth and repo write access"
allowed-tools: bash file_read
metadata:
  moa-version: "1.2"
  moa-one-liner: "Fly.io deploy workflow with health checks"
  moa-tags: "deployment, fly, devops"
  moa-created: "2026-04-09T14:30:00Z"
  moa-updated: "2026-04-09T16:00:00Z"
  moa-auto-generated: "true"
  moa-source-session: "abc123"
  moa-use-count: "7"
  moa-last-used: "2026-04-09T16:00:00Z"
  moa-success-rate: "0.86"
  moa-brain-affinity: "devops"
  moa-sandbox-tier: "container"
  moa-estimated-tokens: "1200"
---

# Deploy to Fly.io

Run the deploy flow.
"#;

    #[test]
    fn parses_valid_skill_markdown() {
        let skill = parse_skill_markdown(VALID_SKILL).unwrap();

        assert_eq!(skill.frontmatter.name, "deploy-to-fly");
        assert_eq!(
            skill.frontmatter.one_liner(),
            "Fly.io deploy workflow with health checks"
        );
        assert_eq!(
            skill.frontmatter.tags(),
            vec!["deployment", "fly", "devops"]
        );
        assert_eq!(skill.frontmatter.allowed_tools, vec!["bash", "file_read"]);
        assert_eq!(skill.frontmatter.estimated_tokens(&skill.body), 1200);
    }

    #[test]
    fn rejects_invalid_skill_markdown() {
        let invalid = r#"---
name: ""
description: "Missing content"
---

Broken
"#;

        assert!(parse_skill_markdown(invalid).is_err());
    }

    #[test]
    fn roundtrips_skill_markdown_through_wiki_page() {
        let skill = parse_skill_markdown(VALID_SKILL).unwrap();
        let path = build_skill_path(&skill.frontmatter.name);
        let page = wiki_page_from_skill(&skill, Some(path.clone())).unwrap();
        let reparsed = super::skill_from_wiki_page(&page).unwrap();
        let rendered = render_skill_markdown(&reparsed).unwrap();
        let reparsed_markdown = parse_skill_markdown(&rendered).unwrap();

        assert_eq!(reparsed.frontmatter, skill.frontmatter);
        assert_eq!(reparsed_markdown.body, skill.body);
        assert_eq!(page.path, Some(path));
    }

    #[test]
    fn slugifies_skill_names_consistently() {
        assert_eq!(slugify_skill_name("Deploy to Fly.io"), "deploy-to-fly-io");
    }

    #[test]
    fn renders_skill_markdown() {
        let skill = parse_skill_markdown(VALID_SKILL).unwrap();
        let rendered = render_skill_markdown(&SkillDocument {
            frontmatter: skill.frontmatter,
            body: skill.body,
        })
        .unwrap();

        assert!(rendered.contains("name: deploy-to-fly"));
        assert!(rendered.contains("allowed-tools: bash file_read"));
        assert!(
            rendered.contains("moa-version: '1.2'") || rendered.contains("moa-version: \"1.2\"")
        );
        assert!(rendered.contains("# Deploy to Fly.io"));
    }

    #[test]
    fn defaults_missing_moa_metadata() {
        let minimal = r#"---
name: minimal-skill
description: "Minimal Agent Skills document"
---

# Minimal skill
"#;
        let skill = parse_skill_markdown(minimal).unwrap();

        assert_eq!(skill.frontmatter.version(), "1.0");
        assert_eq!(
            skill.frontmatter.one_liner(),
            "Minimal Agent Skills document"
        );
        assert!(skill.frontmatter.tags().is_empty());
        assert!(skill.frontmatter.allowed_tools.is_empty());
        assert!(skill.frontmatter.estimated_tokens(&skill.body) > 0);
    }
}
