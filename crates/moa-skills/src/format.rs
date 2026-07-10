//! Agent Skill markdown parsing and rendering utilities.

use moa_core::{MoaError, Result, SkillMetadata};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use tracing::warn;

const FRONTMATTER_DELIMITER: &str = "---";
const DEFAULT_VERSION: &str = "1.0";
const META_VERSION: &str = "moa-version";
const META_TAGS: &str = "moa-tags";
const META_ESTIMATED_TOKENS: &str = "moa-estimated-tokens";

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

    /// Returns the normalized skill tags.
    pub fn tags(&self) -> Vec<String> {
        metadata_csv(&self.metadata, META_TAGS)
    }

    /// Returns the estimated token cost of loading the full skill body.
    pub fn estimated_tokens(&self, body: &str) -> usize {
        self.metadata_usize(META_ESTIMATED_TOKENS)
            .unwrap_or_else(|| estimate_skill_tokens(body))
    }

    fn metadata_string(&self, key: &str) -> Option<String> {
        self.metadata
            .get(key)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }

    pub(crate) fn metadata_usize(&self, key: &str) -> Option<usize> {
        self.metadata_string(key)
            .and_then(|value| value.parse::<usize>().ok())
    }

    fn insert_metadata(&mut self, key: &str, value: String) {
        self.metadata.insert(key.to_string(), value);
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

/// Builds pipeline metadata for a parsed skill document.
pub fn skill_metadata_from_document(path: String, skill: &SkillDocument) -> SkillMetadata {
    SkillMetadata {
        artifact_revision_uid: None,
        path,
        name: skill.frontmatter.name.clone(),
        description: skill.frontmatter.description.clone(),
        tags: skill.frontmatter.tags(),
        allowed_tools: skill.frontmatter.allowed_tools.clone(),
        actions: Vec::new(),
        // Procedures are defined on the skill artifact, not in the Markdown body,
        // so a document-derived metadata never carries a procedure.
        has_procedure: false,
        estimated_tokens: skill.frontmatter.estimated_tokens(&skill.body),
    }
}

/// Returns the canonical memory path for a skill name.
pub fn build_skill_path(skill_name: &str) -> String {
    format!(".moa/skills/{}/SKILL.md", slugify_skill_name(skill_name))
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

    for key in skill.frontmatter.metadata.keys() {
        if key.starts_with("moa-") && !is_supported_moa_metadata_key(key) {
            return Err(MoaError::ValidationError(format!(
                "unsupported MOA skill metadata key `{key}`"
            )));
        }
    }

    if skill.frontmatter.version().trim().is_empty() {
        return Err(MoaError::ValidationError(
            "skill version metadata must not be empty".to_string(),
        ));
    }

    if skill.frontmatter.estimated_tokens(&skill.body) == 0 {
        return Err(MoaError::ValidationError(
            "skill frontmatter `moa-estimated-tokens` must be greater than zero".to_string(),
        ));
    }

    Ok(())
}

fn estimate_skill_tokens(body: &str) -> usize {
    body.split_whitespace().count().max(1)
}

fn is_supported_moa_metadata_key(key: &str) -> bool {
    matches!(key, META_VERSION | META_TAGS | META_ESTIMATED_TOKENS)
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

#[cfg(test)]
mod tests {
    use super::{SkillDocument, parse_skill_markdown, render_skill_markdown, slugify_skill_name};

    const VALID_SKILL: &str = r#"---
name: deploy-to-staging
description: "Deploy applications to a staging and production environment"
compatibility: "Requires deploy auth and repo write access"
allowed-tools: bash file_read
metadata:
  moa-version: "1.2"
  moa-tags: "deployment, staging, devops"
  moa-estimated-tokens: "1200"
---

# Deploy to Staging

Run the deploy flow.
"#;

    #[test]
    fn parses_valid_skill_markdown() {
        let skill = parse_skill_markdown(VALID_SKILL).unwrap();

        assert_eq!(skill.frontmatter.name, "deploy-to-staging");
        assert_eq!(
            skill.frontmatter.tags(),
            vec!["deployment", "staging", "devops"]
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
    fn rejects_unsupported_moa_metadata_key() {
        // Pins: only the known moa-* metadata keys are accepted so a typo cannot silently persist.
        let invalid = r#"---
name: typo-skill
description: "Skill with an unknown MOA metadata key"
metadata:
  moa-version: "1.0"
  moa-unknown: "oops"
  moa-estimated-tokens: "100"
---

# Typo skill
"#;

        let error = parse_skill_markdown(invalid)
            .expect_err("unsupported moa- metadata key must be rejected");
        assert!(
            error.to_string().contains("moa-unknown"),
            "error names the unsupported key: {error}"
        );
    }

    #[test]
    fn rejects_zero_estimated_tokens() {
        // Pins: an explicit zero moa-estimated-tokens is rejected so skill budgeting stays positive.
        let invalid = r#"---
name: zero-token-skill
description: "Skill that declares zero estimated tokens"
metadata:
  moa-estimated-tokens: "0"
---

# Zero token skill
"#;

        let error =
            parse_skill_markdown(invalid).expect_err("zero estimated tokens must be rejected");
        assert!(
            error.to_string().contains("greater than zero"),
            "error explains the token floor: {error}"
        );
    }

    #[test]
    fn rejects_markdown_without_closing_frontmatter_delimiter() {
        // Pins: frontmatter without a closing delimiter is a hard parse error, not a silent empty body.
        let invalid = "---\nname: unterminated\ndescription: \"No closing delimiter\"\n\n# Body without a fence\n";

        let error = parse_skill_markdown(invalid)
            .expect_err("missing closing frontmatter delimiter must be rejected");
        assert!(
            error.to_string().contains("closing delimiter"),
            "error names the missing closing delimiter: {error}"
        );
    }

    #[test]
    fn rejects_markdown_without_frontmatter() {
        // Pins: a document with no YAML frontmatter is rejected before any YAML parsing.
        let invalid = "# Just a heading\n\nNo frontmatter here.\n";

        let error = parse_skill_markdown(invalid)
            .expect_err("markdown without frontmatter must be rejected");
        assert!(
            error.to_string().contains("frontmatter"),
            "error mentions frontmatter: {error}"
        );
    }

    #[test]
    fn slugifies_skill_names_consistently() {
        assert_eq!(
            slugify_skill_name("Deploy to Staging 2.0"),
            "deploy-to-staging-2-0"
        );
    }

    #[test]
    fn builds_sandbox_skill_path() {
        assert_eq!(
            super::build_skill_path("Deploy to Staging 2.0"),
            ".moa/skills/deploy-to-staging-2-0/SKILL.md"
        );
    }

    #[test]
    fn renders_skill_markdown() {
        let skill = parse_skill_markdown(VALID_SKILL).unwrap();
        let rendered = render_skill_markdown(&SkillDocument {
            frontmatter: skill.frontmatter,
            body: skill.body,
        })
        .unwrap();

        assert!(rendered.contains("name: deploy-to-staging"));
        assert!(rendered.contains("allowed-tools: bash file_read"));
        assert!(
            rendered.contains("moa-version: '1.2'") || rendered.contains("moa-version: \"1.2\"")
        );
        assert!(rendered.contains("# Deploy to Staging"));
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
        assert!(skill.frontmatter.tags().is_empty());
        assert!(skill.frontmatter.allowed_tools.is_empty());
        assert!(skill.frontmatter.estimated_tokens(&skill.body) > 0);
    }
}
