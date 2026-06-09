//! Skill package validation and deterministic package metadata.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use moa_core::{MoaError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::format::{SkillDocument, parse_skill_markdown};

/// Required root file for every skill package.
pub const SKILL_MD_PATH: &str = "SKILL.md";
/// Maximum total package size accepted by the registry.
pub const MAX_SKILL_PACKAGE_BYTES: i64 = 5 * 1024 * 1024;
/// Maximum size accepted for one package file.
pub const MAX_SKILL_FILE_BYTES: i64 = 1024 * 1024;
/// Maximum number of files accepted for one package.
pub const MAX_SKILL_PACKAGE_FILES: usize = 128;

/// One file submitted as part of a skill package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPackageFile {
    /// POSIX relative path inside the skill package.
    pub path: String,
    /// Raw file bytes.
    pub content: Vec<u8>,
    /// Optional media type hint for export and sandbox setup.
    pub content_type: Option<String>,
    /// Whether the file should be executable after sandbox materialization.
    pub executable: bool,
}

impl SkillPackageFile {
    /// Builds a package file with non-executable default permissions.
    pub fn new(path: impl Into<String>, content: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
            content_type: None,
            executable: false,
        }
    }

    /// Adds a media type hint to the package file.
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Marks whether the file should be executable in sandboxes.
    pub fn with_executable(mut self, executable: bool) -> Self {
        self.executable = executable;
        self
    }
}

/// Submitted skill package before validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPackage {
    /// Files that make up the package.
    pub files: Vec<SkillPackageFile>,
}

impl SkillPackage {
    /// Builds a package from raw files.
    pub fn new(files: Vec<SkillPackageFile>) -> Self {
        Self { files }
    }

    /// Builds a minimal package containing only the required `SKILL.md`.
    pub fn from_skill_markdown(markdown: impl Into<String>) -> Self {
        Self {
            files: vec![
                SkillPackageFile::new(SKILL_MD_PATH, markdown.into())
                    .with_content_type("text/markdown; charset=utf-8"),
            ],
        }
    }

    /// Validates the package and returns canonical metadata and sorted files.
    pub fn validate(self) -> Result<ValidatedSkillPackage> {
        ValidatedSkillPackage::from_package(self)
    }
}

/// Canonical package file ready for persistence or sandbox installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSkillPackageFile {
    /// Normalized POSIX relative path inside the package.
    pub path: String,
    /// Raw file bytes.
    pub content: Vec<u8>,
    /// SHA-256 digest of `content`.
    pub content_sha256: Vec<u8>,
    /// Optional media type hint.
    pub content_type: Option<String>,
    /// Whether the file should be executable in sandboxes.
    pub executable: bool,
    /// File size in bytes.
    pub file_size_bytes: i64,
}

/// Canonical validated skill package.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedSkillPackage {
    /// Parsed required `SKILL.md` document.
    pub document: SkillDocument,
    /// UTF-8 `SKILL.md` markdown.
    pub skill_md: String,
    /// Stable skill name from frontmatter.
    pub name: String,
    /// Human-readable description from frontmatter.
    pub description: String,
    /// Search and ranking tags from frontmatter metadata.
    pub tags: Vec<String>,
    /// SHA-256 digest of the whole package tree.
    pub package_hash: Vec<u8>,
    /// SHA-256 digest of the required `SKILL.md`.
    pub skill_md_hash: Vec<u8>,
    /// Number of files in the package.
    pub file_count: i32,
    /// Total package size in bytes.
    pub total_size_bytes: i64,
    /// Estimated token cost for the required `SKILL.md`.
    pub estimated_tokens: usize,
    /// Deterministic package manifest for persistence and pipeline reads.
    pub manifest: SkillPackageManifest,
    /// Sorted validated package files.
    pub files: Vec<ValidatedSkillPackageFile>,
}

impl ValidatedSkillPackage {
    fn from_package(package: SkillPackage) -> Result<Self> {
        if package.files.is_empty() {
            return Err(MoaError::ValidationError(
                "skill package must contain SKILL.md".to_string(),
            ));
        }
        if package.files.len() > MAX_SKILL_PACKAGE_FILES {
            return Err(MoaError::ValidationError(format!(
                "skill package exceeds {MAX_SKILL_PACKAGE_FILES} files"
            )));
        }

        let mut seen_paths = HashSet::new();
        let mut files = Vec::with_capacity(package.files.len());
        let mut total_size_bytes = 0_i64;

        for file in package.files {
            let path = normalize_package_path(&file.path)?;
            if !seen_paths.insert(path.clone()) {
                return Err(MoaError::ValidationError(format!(
                    "duplicate skill package path `{path}`"
                )));
            }

            let file_size_bytes = i64::try_from(file.content.len()).map_err(|_| {
                MoaError::ValidationError("skill package file is too large".to_string())
            })?;
            if file_size_bytes > MAX_SKILL_FILE_BYTES {
                return Err(MoaError::ValidationError(format!(
                    "skill package file `{path}` exceeds {MAX_SKILL_FILE_BYTES} bytes"
                )));
            }
            total_size_bytes = total_size_bytes
                .checked_add(file_size_bytes)
                .ok_or_else(|| {
                    MoaError::ValidationError("skill package size overflow".to_string())
                })?;
            if total_size_bytes > MAX_SKILL_PACKAGE_BYTES {
                return Err(MoaError::ValidationError(format!(
                    "skill package exceeds {MAX_SKILL_PACKAGE_BYTES} bytes"
                )));
            }

            files.push(ValidatedSkillPackageFile {
                path,
                content_sha256: Sha256::digest(&file.content).to_vec(),
                content: file.content,
                content_type: normalize_content_type(file.content_type),
                executable: file.executable,
                file_size_bytes,
            });
        }

        files.sort_by(|left, right| left.path.cmp(&right.path));
        let skill_md = skill_md_content(&files)?;
        let document = parse_skill_markdown(&skill_md)?;
        let skill_md_hash = Sha256::digest(skill_md.as_bytes()).to_vec();
        let package_hash = package_hash(&files);
        let file_count = i32::try_from(files.len()).map_err(|_| {
            MoaError::ValidationError("skill package file count overflow".to_string())
        })?;
        let estimated_tokens = document.frontmatter.estimated_tokens(&document.body);
        let manifest = SkillPackageManifest::from_document_and_files(&document, &files);

        Ok(Self {
            name: document.frontmatter.name.clone(),
            description: document.frontmatter.description.clone(),
            tags: document.frontmatter.tags(),
            document,
            skill_md,
            package_hash,
            skill_md_hash,
            file_count,
            total_size_bytes,
            estimated_tokens,
            manifest,
            files,
        })
    }
}

/// Serializable package manifest stored with the skill row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillPackageManifest {
    /// Package manifest schema version.
    pub schema_version: u32,
    /// Required `SKILL.md` path.
    pub skill_md_path: String,
    /// Estimated token cost for loading `SKILL.md`.
    pub skill_md_estimated_tokens: usize,
    /// Allowed tools declared by the skill frontmatter.
    pub allowed_tools: Vec<String>,
    /// Persisted use count declared by MOA skill metadata.
    pub use_count: u32,
    /// Last use time declared by MOA skill metadata.
    pub last_used: Option<DateTime<Utc>>,
    /// Persisted success rate declared by MOA skill metadata.
    pub success_rate: f32,
    /// Whether this skill was generated by MOA.
    pub auto_generated: bool,
    /// Deterministically sorted package files.
    pub files: Vec<SkillPackageManifestFile>,
}

impl SkillPackageManifest {
    fn from_document_and_files(
        document: &SkillDocument,
        files: &[ValidatedSkillPackageFile],
    ) -> Self {
        Self {
            schema_version: 1,
            skill_md_path: SKILL_MD_PATH.to_string(),
            skill_md_estimated_tokens: document.frontmatter.estimated_tokens(&document.body),
            allowed_tools: document.frontmatter.allowed_tools.clone(),
            use_count: document.frontmatter.use_count(),
            last_used: document.frontmatter.last_used(),
            success_rate: document.frontmatter.success_rate(),
            auto_generated: document.frontmatter.auto_generated(),
            files: files
                .iter()
                .map(|file| SkillPackageManifestFile {
                    path: file.path.clone(),
                    size_bytes: file.file_size_bytes,
                    sha256: encode_hex(&file.content_sha256),
                    content_type: file.content_type.clone(),
                    executable: file.executable,
                })
                .collect(),
        }
    }
}

/// Serializable metadata for one package file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillPackageManifestFile {
    /// Normalized POSIX relative path.
    pub path: String,
    /// File size in bytes.
    pub size_bytes: i64,
    /// Hex-encoded SHA-256 digest of the file content.
    pub sha256: String,
    /// Optional media type hint.
    pub content_type: Option<String>,
    /// Whether the file should be executable in sandboxes.
    pub executable: bool,
}

/// Returns a lowercase hex string for bytes.
pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn normalize_package_path(path: &str) -> Result<String> {
    if path.is_empty() || path.contains('\0') {
        return Err(MoaError::ValidationError(
            "skill package path must not be empty".to_string(),
        ));
    }
    if path.starts_with('/') || path.contains('\\') {
        return Err(MoaError::ValidationError(format!(
            "skill package path `{path}` must be a POSIX relative path"
        )));
    }

    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(MoaError::ValidationError(format!(
                "skill package path `{path}` contains an invalid segment"
            )));
        }
        segments.push(segment);
    }

    Ok(segments.join("/"))
}

fn normalize_content_type(content_type: Option<String>) -> Option<String> {
    content_type
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn skill_md_content(files: &[ValidatedSkillPackageFile]) -> Result<String> {
    let file = files
        .iter()
        .find(|file| file.path == SKILL_MD_PATH)
        .ok_or_else(|| {
            MoaError::ValidationError("skill package must contain root SKILL.md".to_string())
        })?;
    std::str::from_utf8(&file.content)
        .map(str::to_string)
        .map_err(|error| {
            MoaError::ValidationError(format!("skill package SKILL.md must be UTF-8: {error}"))
        })
}

fn package_hash(files: &[ValidatedSkillPackageFile]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.path.as_bytes());
        hasher.update([0]);
        hasher.update(file.file_size_bytes.to_be_bytes());
        hasher.update([u8::from(file.executable)]);
        if let Some(content_type) = &file.content_type {
            hasher.update(content_type.as_bytes());
        }
        hasher.update([0]);
        hasher.update(&file.content_sha256);
        hasher.update([0xff]);
    }
    hasher.finalize().to_vec()
}
