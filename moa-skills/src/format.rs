//! Agent Skill markdown parsing and rendering utilities.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use moa_core::{
    ConfidenceLevel, MemoryPath, MoaError, PageType, Result, SandboxTier, SkillMetadata, WikiPage,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use tracing::warn;

const FRONTMATTER_DELIMITER: &str = "---";
const DEFAULT_VERSION: &str = "1.0";
const DEFAULT_ESTIMATED_TOKENS: usize = 256;
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
const META_SOURCE: &str = "moa-source";
const META_BRAIN_AFFINITY: &str = "moa-brain-affinity";
const META_SANDBOX_TIER: &str = "moa-sandbox-tier";
const META_ESTIMATED_TOKENS: &str = "moa-estimated-tokens";
const META_IMPROVED_FROM: &str = "moa-improved-from";

/// Fully parsed Agent Skill document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillDocument {
    /// YAML frontmatter fields.
    pub frontmatter: SkillFrontmatter,
    /// Markdown instructions body without the YAML frontmatter.
    pub body: String,
}

/// Parsed Agent Skill frontmatter.
///
/// This canonical representation follows the Agent Skills `name` + `description`
/// model while retaining MOA-specific bookkeeping fields internally.
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
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Arbitrary metadata preserved from the Agent Skills spec.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    /// Semantic or dotted version string.
    pub version: String,
    /// Concise single-line summary.
    pub one_liner: String,
    /// User-defined tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Tools referenced by the skill.
    #[serde(default)]
    pub tools_required: Vec<String>,
    /// Creation timestamp.
    pub created: DateTime<Utc>,
    /// Update timestamp.
    pub updated: DateTime<Utc>,
    /// Whether the skill was auto-generated.
    #[serde(default)]
    pub auto_generated: bool,
    /// Source session identifier when auto-generated.
    #[serde(default)]
    pub source_session: Option<String>,
    /// Number of successful or attempted uses recorded for the skill.
    #[serde(default)]
    pub use_count: u32,
    /// Last time the skill was used.
    #[serde(default)]
    pub last_used: Option<DateTime<Utc>>,
    /// Historical success rate between `0.0` and `1.0`.
    #[serde(default = "default_success_rate")]
    pub success_rate: f32,
    /// Optional source label for imported community skills.
    #[serde(default)]
    pub source: Option<String>,
    /// MOA-specific extensions.
    #[serde(default)]
    pub moa: SkillMoaFrontmatter,
}

/// MOA-specific Skill frontmatter extensions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SkillMoaFrontmatter {
    /// Optional brain specialization hint.
    #[serde(default)]
    pub brain_affinity: Option<String>,
    /// Preferred sandbox tier for the skill.
    #[serde(default)]
    pub sandbox_tier: Option<SandboxTier>,
    /// Estimated token cost for the full skill body.
    #[serde(default = "default_estimated_tokens")]
    pub estimated_tokens: usize,
    /// Previous version or skill lineage when self-improved.
    #[serde(default)]
    pub improved_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct ParsedSkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(
        default,
        rename = "allowed-tools",
        deserialize_with = "deserialize_allowed_tools"
    )]
    allowed_tools: Vec<String>,
    #[serde(default)]
    metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedSkillFrontmatter {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compatibility: Option<String>,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        rename = "allowed-tools",
        serialize_with = "serialize_allowed_tools"
    )]
    allowed_tools: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    metadata: HashMap<String, String>,
}

/// Parses a `SKILL.md` document into a structured skill representation.
pub fn parse_skill_markdown(markdown: &str) -> Result<SkillDocument> {
    let (yaml_block, body) = split_frontmatter(markdown)?;
    let parsed = serde_yaml::from_str::<ParsedSkillFrontmatter>(yaml_block)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    let frontmatter = canonicalize_frontmatter(parsed, body)?;
    validate_skill_frontmatter(&frontmatter)?;

    Ok(SkillDocument {
        frontmatter,
        body: body.trim_start_matches('\n').to_string(),
    })
}

/// Renders a structured skill representation back into `SKILL.md` markdown.
pub fn render_skill_markdown(skill: &SkillDocument) -> Result<String> {
    validate_skill_frontmatter(&skill.frontmatter)?;
    let yaml = serde_yaml::to_string(&rendered_frontmatter(&skill.frontmatter))
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    Ok(format!(
        "{delimiter}\n{yaml}{delimiter}\n\n{body}",
        delimiter = FRONTMATTER_DELIMITER,
        body = skill.body.trim_start_matches('\n')
    ))
}

/// Converts a parsed wiki page into a skill document.
pub fn skill_from_wiki_page(page: &WikiPage) -> Result<SkillDocument> {
    let parsed =
        serde_json::from_value::<ParsedSkillFrontmatter>(serde_json::to_value(&page.metadata)?)
            .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    let mut frontmatter = canonicalize_frontmatter(parsed, &page.content)?;
    frontmatter.created = page.created;
    frontmatter.updated = page.updated;
    frontmatter.auto_generated = page.auto_generated;
    if !page.tags.is_empty() {
        frontmatter.tags = page.tags.clone();
    }
    validate_skill_frontmatter(&frontmatter)?;

    Ok(SkillDocument {
        frontmatter,
        body: page.content.clone(),
    })
}

/// Builds pipeline metadata for a parsed skill document.
pub fn skill_metadata_from_document(path: MemoryPath, skill: &SkillDocument) -> SkillMetadata {
    SkillMetadata {
        path,
        name: skill.frontmatter.name.clone(),
        version: skill.frontmatter.version.clone(),
        one_liner: skill.frontmatter.one_liner.clone(),
        tags: skill.frontmatter.tags.clone(),
        tools_required: skill.frontmatter.tools_required.clone(),
        estimated_tokens: skill.frontmatter.moa.estimated_tokens,
        use_count: skill.frontmatter.use_count,
        success_rate: skill.frontmatter.success_rate,
        auto_generated: skill.frontmatter.auto_generated,
    }
}

/// Builds pipeline metadata directly from a wiki page.
pub fn skill_metadata_from_page(path: MemoryPath, page: &WikiPage) -> Result<SkillMetadata> {
    let skill = skill_from_wiki_page(page)?;
    Ok(skill_metadata_from_document(path, &skill))
}

/// Converts a structured skill document into a shared wiki page.
pub fn wiki_page_from_skill(skill: &SkillDocument, path: Option<MemoryPath>) -> Result<WikiPage> {
    validate_skill_frontmatter(&skill.frontmatter)?;
    let metadata = serde_json::from_value::<HashMap<String, Value>>(
        serde_json::to_value(rendered_frontmatter(&skill.frontmatter))
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
    )?;
    let reference_count = u64::from(skill.frontmatter.use_count);
    let last_referenced = skill
        .frontmatter
        .last_used
        .unwrap_or(skill.frontmatter.updated);

    Ok(WikiPage {
        path,
        title: humanize_skill_name(&skill.frontmatter.name),
        page_type: PageType::Skill,
        content: skill.body.clone(),
        created: skill.frontmatter.created,
        updated: skill.frontmatter.updated,
        confidence: confidence_for_skill(skill.frontmatter.success_rate),
        related: Vec::new(),
        sources: Vec::new(),
        tags: skill.frontmatter.tags.clone(),
        auto_generated: skill.frontmatter.auto_generated,
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

fn default_success_rate() -> f32 {
    1.0
}

fn default_estimated_tokens() -> usize {
    DEFAULT_ESTIMATED_TOKENS
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

fn validate_skill_frontmatter(frontmatter: &SkillFrontmatter) -> Result<()> {
    for (field_name, value) in [
        ("name", frontmatter.name.trim()),
        ("description", frontmatter.description.trim()),
    ] {
        if value.is_empty() {
            return Err(MoaError::ValidationError(format!(
                "skill frontmatter field `{field_name}` must not be empty"
            )));
        }
    }

    if !is_valid_skill_name(&frontmatter.name) {
        warn!(
            skill = %frontmatter.name,
            "skill name does not follow the recommended Agent Skills slug format"
        );
    }

    if frontmatter.version.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "skill version metadata must not be empty".to_string(),
        ));
    }

    if frontmatter.one_liner.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "skill summary metadata must not be empty".to_string(),
        ));
    }

    if frontmatter.moa.estimated_tokens == 0 {
        return Err(MoaError::ValidationError(
            "skill frontmatter `moa.estimated_tokens` must be greater than zero".to_string(),
        ));
    }

    if !(0.0..=1.0).contains(&frontmatter.success_rate) {
        return Err(MoaError::ValidationError(
            "skill `success_rate` must be between 0.0 and 1.0".to_string(),
        ));
    }

    Ok(())
}

fn canonicalize_frontmatter(
    parsed: ParsedSkillFrontmatter,
    body: &str,
) -> Result<SkillFrontmatter> {
    let mut metadata = parsed.metadata;
    let now = Utc::now();
    let created = take_timestamp(&mut metadata, META_CREATED).unwrap_or(now);
    let updated = take_timestamp(&mut metadata, META_UPDATED).unwrap_or(created);
    let allowed_tools = parsed.allowed_tools;
    let tools_required = allowed_tools.clone();
    let estimated_tokens = take_usize(&mut metadata, META_ESTIMATED_TOKENS)
        .unwrap_or_else(|| estimate_skill_tokens(body));
    let brain_affinity = take_string(&mut metadata, META_BRAIN_AFFINITY);
    let sandbox_tier = take_sandbox_tier(&mut metadata, META_SANDBOX_TIER);
    let improved_from = take_string(&mut metadata, META_IMPROVED_FROM);
    let version =
        take_string(&mut metadata, META_VERSION).unwrap_or_else(|| DEFAULT_VERSION.to_string());
    let one_liner =
        take_string(&mut metadata, META_ONE_LINER).unwrap_or_else(|| parsed.description.clone());
    let tags = take_csv(&mut metadata, META_TAGS);
    let auto_generated = take_bool(&mut metadata, META_AUTO_GENERATED).unwrap_or(false);
    let source_session = take_string(&mut metadata, META_SOURCE_SESSION);
    let use_count = take_u32(&mut metadata, META_USE_COUNT).unwrap_or(0);
    let last_used = take_timestamp(&mut metadata, META_LAST_USED);
    let success_rate =
        take_f32(&mut metadata, META_SUCCESS_RATE).unwrap_or_else(default_success_rate);
    let source = take_string(&mut metadata, META_SOURCE);

    Ok(SkillFrontmatter {
        name: parsed.name,
        description: parsed.description.clone(),
        license: parsed.license,
        compatibility: parsed.compatibility,
        allowed_tools,
        metadata,
        version,
        one_liner,
        tags,
        tools_required,
        created,
        updated,
        auto_generated,
        source_session,
        use_count,
        last_used,
        success_rate,
        source,
        moa: SkillMoaFrontmatter {
            brain_affinity,
            sandbox_tier,
            estimated_tokens,
            improved_from,
        },
    })
}

fn rendered_frontmatter(frontmatter: &SkillFrontmatter) -> RenderedSkillFrontmatter {
    let mut metadata = frontmatter.metadata.clone();
    insert_metadata(&mut metadata, META_VERSION, frontmatter.version.clone());
    insert_metadata(&mut metadata, META_ONE_LINER, frontmatter.one_liner.clone());
    insert_metadata(
        &mut metadata,
        META_CREATED,
        frontmatter.created.to_rfc3339(),
    );
    insert_metadata(
        &mut metadata,
        META_UPDATED,
        frontmatter.updated.to_rfc3339(),
    );
    insert_metadata(
        &mut metadata,
        META_AUTO_GENERATED,
        frontmatter.auto_generated.to_string(),
    );
    insert_optional_metadata(
        &mut metadata,
        META_SOURCE_SESSION,
        frontmatter.source_session.clone(),
    );
    insert_metadata(
        &mut metadata,
        META_USE_COUNT,
        frontmatter.use_count.to_string(),
    );
    insert_optional_metadata(
        &mut metadata,
        META_LAST_USED,
        frontmatter.last_used.map(|value| value.to_rfc3339()),
    );
    insert_metadata(
        &mut metadata,
        META_SUCCESS_RATE,
        frontmatter.success_rate.to_string(),
    );
    insert_optional_metadata(&mut metadata, META_SOURCE, frontmatter.source.clone());
    if !frontmatter.tags.is_empty() {
        insert_metadata(&mut metadata, META_TAGS, frontmatter.tags.join(", "));
    }
    insert_optional_metadata(
        &mut metadata,
        META_BRAIN_AFFINITY,
        frontmatter.moa.brain_affinity.clone(),
    );
    insert_optional_metadata(
        &mut metadata,
        META_SANDBOX_TIER,
        frontmatter
            .moa
            .sandbox_tier
            .as_ref()
            .map(|tier| format!("{tier:?}").to_ascii_lowercase()),
    );
    insert_metadata(
        &mut metadata,
        META_ESTIMATED_TOKENS,
        frontmatter.moa.estimated_tokens.to_string(),
    );
    insert_optional_metadata(
        &mut metadata,
        META_IMPROVED_FROM,
        frontmatter.moa.improved_from.clone(),
    );

    let allowed_tools = if frontmatter.allowed_tools.is_empty() {
        frontmatter.tools_required.clone()
    } else {
        frontmatter.allowed_tools.clone()
    };

    RenderedSkillFrontmatter {
        name: frontmatter.name.clone(),
        description: frontmatter.description.clone(),
        license: frontmatter.license.clone(),
        compatibility: frontmatter.compatibility.clone(),
        allowed_tools,
        metadata,
    }
}

fn estimate_skill_tokens(body: &str) -> usize {
    body.split_whitespace().count().max(1)
}

fn insert_metadata(metadata: &mut HashMap<String, String>, key: &str, value: String) {
    metadata.insert(key.to_string(), value);
}

fn insert_optional_metadata(
    metadata: &mut HashMap<String, String>,
    key: &str,
    value: Option<String>,
) {
    if let Some(value) = value {
        insert_metadata(metadata, key, value);
    }
}

fn take_string(metadata: &mut HashMap<String, String>, key: &str) -> Option<String> {
    metadata
        .remove(key)
        .filter(|value| !value.trim().is_empty())
}

fn take_csv(metadata: &mut HashMap<String, String>, key: &str) -> Vec<String> {
    take_string(metadata, key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn take_timestamp(metadata: &mut HashMap<String, String>, key: &str) -> Option<DateTime<Utc>> {
    take_string(metadata, key)
        .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn take_bool(metadata: &mut HashMap<String, String>, key: &str) -> Option<bool> {
    take_string(metadata, key).and_then(|value| value.parse::<bool>().ok())
}

fn take_u32(metadata: &mut HashMap<String, String>, key: &str) -> Option<u32> {
    take_string(metadata, key).and_then(|value| value.parse::<u32>().ok())
}

fn take_usize(metadata: &mut HashMap<String, String>, key: &str) -> Option<usize> {
    take_string(metadata, key).and_then(|value| value.parse::<usize>().ok())
}

fn take_f32(metadata: &mut HashMap<String, String>, key: &str) -> Option<f32> {
    take_string(metadata, key).and_then(|value| value.parse::<f32>().ok())
}

fn take_sandbox_tier(metadata: &mut HashMap<String, String>, key: &str) -> Option<SandboxTier> {
    take_string(metadata, key).and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
        "none" => Some(SandboxTier::None),
        "container" => Some(SandboxTier::Container),
        "microvm" => Some(SandboxTier::MicroVM),
        "local" => Some(SandboxTier::Local),
        _ => None,
    })
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
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum AllowedTools {
        String(String),
        Sequence(Vec<String>),
    }

    Ok(match AllowedTools::deserialize(deserializer)? {
        AllowedTools::String(value) => value
            .split_whitespace()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        AllowedTools::Sequence(values) => values
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect(),
    })
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
            skill.frontmatter.one_liner,
            "Fly.io deploy workflow with health checks"
        );
        assert_eq!(skill.frontmatter.tags, vec!["deployment", "fly", "devops"]);
        assert_eq!(skill.frontmatter.tools_required, vec!["bash", "file_read"]);
        assert_eq!(skill.frontmatter.moa.estimated_tokens, 1200);
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

        assert_eq!(skill.frontmatter.version, "1.0");
        assert_eq!(skill.frontmatter.one_liner, "Minimal Agent Skills document");
        assert!(skill.frontmatter.tags.is_empty());
        assert!(skill.frontmatter.allowed_tools.is_empty());
        assert!(skill.frontmatter.moa.estimated_tokens > 0);
    }
}
