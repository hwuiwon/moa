//! Source-ingest helpers for compiling raw material into wiki pages.

use std::collections::HashSet;

use chrono::Utc;
use moa_core::{ConfidenceLevel, MemoryPath, MemoryScope, PageType, Result, WikiPage};

use crate::FileMemoryStore;
use crate::index::{LogChange, LogEntry};

/// Summary of a single source-ingest operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReport {
    /// Scope receiving the new source-derived pages.
    pub scope: MemoryScope,
    /// Human-readable source name passed by the caller.
    pub source_name: String,
    /// Summary page created for the raw source.
    pub source_path: MemoryPath,
    /// All pages created or updated by the ingest pass.
    pub affected_pages: Vec<MemoryPath>,
    /// Contradiction notes detected in the source text.
    pub contradictions: Vec<String>,
}

/// Ingests a raw source document into a scope-local wiki summary and related pages.
pub async fn ingest_source(
    store: &FileMemoryStore,
    scope: &MemoryScope,
    source_name: &str,
    source: &str,
) -> Result<IngestReport> {
    let slug = slugify(source_name);
    let source_path = MemoryPath::new(format!("sources/{slug}.md"));
    let mut affected_pages = vec![source_path.clone()];

    let source_page = WikiPage {
        path: Some(source_path.clone()),
        title: source_name.to_string(),
        page_type: PageType::Source,
        content: build_source_page(source_name, source),
        created: Utc::now(),
        updated: Utc::now(),
        confidence: ConfidenceLevel::Medium,
        related: Vec::new(),
        sources: Vec::new(),
        tags: extract_tag_candidates(source_name),
        auto_generated: true,
        last_referenced: Utc::now(),
        reference_count: 0,
        metadata: std::collections::HashMap::new(),
    };
    store
        .write_page_in_scope(scope, &source_path, source_page)
        .await?;

    for entity in extract_section_items(source, "entities") {
        let path = MemoryPath::new(format!("entities/{}.md", slugify(&entity)));
        upsert_derived_page(
            store,
            scope,
            &path,
            PageType::Entity,
            &entity,
            source_name,
            &source_path,
        )
        .await?;
        affected_pages.push(path);
    }
    for topic in extract_section_items(source, "topics") {
        let path = MemoryPath::new(format!("topics/{}.md", slugify(&topic)));
        upsert_derived_page(
            store,
            scope,
            &path,
            PageType::Topic,
            &topic,
            source_name,
            &source_path,
        )
        .await?;
        affected_pages.push(path);
    }
    for decision in extract_section_items(source, "decisions") {
        let date = Utc::now().format("%Y-%m-%d");
        let path = MemoryPath::new(format!("decisions/{date}-{}.md", slugify(&decision)));
        upsert_derived_page(
            store,
            scope,
            &path,
            PageType::Decision,
            &decision,
            source_name,
            &source_path,
        )
        .await?;
        affected_pages.push(path);
    }

    let contradictions = extract_section_items(source, "contradictions");
    store.refresh_scope_index(scope).await?;
    store
        .append_scope_log(
            scope,
            LogEntry {
                timestamp: Utc::now(),
                operation: "ingest".to_string(),
                description: format!("Ingested source: {source_name}"),
                changes: affected_pages
                    .iter()
                    .enumerate()
                    .map(|(index, path)| LogChange {
                        action: if index == 0 {
                            "Created".to_string()
                        } else {
                            "Updated".to_string()
                        },
                        path: path.clone(),
                        detail: None,
                    })
                    .collect(),
                brain_session: None,
            },
        )
        .await?;

    Ok(IngestReport {
        scope: scope.clone(),
        source_name: source_name.to_string(),
        source_path,
        affected_pages: dedupe_paths(affected_pages),
        contradictions,
    })
}

async fn upsert_derived_page(
    store: &FileMemoryStore,
    scope: &MemoryScope,
    path: &MemoryPath,
    page_type: PageType,
    title: &str,
    source_name: &str,
    source_path: &MemoryPath,
) -> Result<()> {
    let now = Utc::now();
    let update_block = format!(
        "## Source update\n- Source: [[{}]]\n- Summary: Added from {source_name}\n",
        source_path.as_str()
    );

    let mut page = match store.read_page_in_scope(scope, path).await {
        Ok(mut existing) => {
            if !existing.content.contains(update_block.trim()) {
                if !existing.content.trim().is_empty() {
                    existing.content.push_str("\n\n");
                }
                existing.content.push_str(&update_block);
            }
            existing.updated = now;
            existing.last_referenced = now;
            existing.reference_count = existing.reference_count.saturating_add(1);
            existing.sources.push(source_path.as_str().to_string());
            existing.sources.sort();
            existing.sources.dedup();
            existing.related.push(source_path.as_str().to_string());
            existing.related.sort();
            existing.related.dedup();
            existing
        }
        Err(_) => WikiPage {
            path: Some(path.clone()),
            title: title.to_string(),
            page_type,
            content: format!(
                "# {title}\n\n{update_block}\nThis page was created from source [[{}]].",
                source_path.as_str()
            ),
            created: now,
            updated: now,
            confidence: ConfidenceLevel::Medium,
            related: vec![source_path.as_str().to_string()],
            sources: vec![source_path.as_str().to_string()],
            tags: extract_tag_candidates(title),
            auto_generated: true,
            last_referenced: now,
            reference_count: 1,
            metadata: std::collections::HashMap::new(),
        },
    };

    page.path = Some(path.clone());
    store.write_page_in_scope(scope, path, page).await
}

fn build_source_page(source_name: &str, source: &str) -> String {
    let summary = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .take(8)
        .collect::<Vec<_>>()
        .join("\n");

    if summary.is_empty() {
        format!("# {source_name}\n\n{source}")
    } else {
        format!("# {source_name}\n\n## Summary\n{summary}\n\n## Raw source\n{source}")
    }
}

fn extract_section_items(source: &str, section: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut in_section = false;
    let heading = section.to_ascii_lowercase();

    for line in source.lines() {
        let trimmed = line.trim();
        let heading_text = trimmed
            .trim_start_matches('#')
            .trim()
            .trim_end_matches(':')
            .to_ascii_lowercase();

        if trimmed.starts_with('#') {
            in_section = heading_text == heading;
            continue;
        }

        if !in_section && heading_text.starts_with(&format!("{heading}:")) {
            let remainder = trimmed
                .split_once(':')
                .map(|(_, rest)| rest.trim())
                .unwrap_or("");
            values.extend(split_inline_items(remainder));
            continue;
        }

        if in_section {
            if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                values.push(trimmed[2..].trim().to_string());
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            break;
        }
    }

    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn split_inline_items(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim().to_string())
        .collect()
}

fn extract_tag_candidates(value: &str) -> Vec<String> {
    let mut tags = HashSet::new();

    for token in value
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| token.len() > 2)
    {
        tags.insert(token.to_ascii_lowercase());
    }

    let mut tags = tags.into_iter().collect::<Vec<_>>();
    tags.sort();
    tags
}

fn dedupe_paths(paths: Vec<MemoryPath>) -> Vec<MemoryPath> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();

    for path in paths {
        if seen.insert(path.as_str().to_string()) {
            deduped.push(path);
        }
    }

    deduped
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }

    slug.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use moa_core::{MemoryScope, PageType};
    use tempfile::tempdir;

    use super::{extract_section_items, ingest_source};
    use crate::FileMemoryStore;

    #[test]
    fn extracts_markdown_list_sections() {
        let source = r#"
## Entities
- Auth Service
- Token Store

## Decisions
- Adopt single-use refresh tokens
"#;

        assert_eq!(
            extract_section_items(source, "entities"),
            vec!["Auth Service".to_string(), "Token Store".to_string()]
        );
        assert_eq!(
            extract_section_items(source, "decisions"),
            vec!["Adopt single-use refresh tokens".to_string()]
        );
    }

    #[tokio::test]
    async fn ingest_creates_summary_and_related_pages() {
        let dir = tempdir().unwrap();
        let store = FileMemoryStore::new(dir.path()).await.unwrap();
        let scope = MemoryScope::Workspace("ws1".into());

        let report = ingest_source(
            &store,
            &scope,
            "RFC 0042 Auth Redesign",
            r#"
## Entities
- Auth Service

## Topics
- Token Rotation

## Decisions
- Adopt single-use refresh tokens
"#,
        )
        .await
        .unwrap();

        assert_eq!(
            report.source_path.as_str(),
            "sources/rfc-0042-auth-redesign.md"
        );
        let source_page = store
            .read_page_in_scope(&scope, &report.source_path)
            .await
            .unwrap();
        assert_eq!(source_page.page_type, PageType::Source);

        let entity_page = store
            .read_page_in_scope(&scope, &"entities/auth-service.md".into())
            .await
            .unwrap();
        assert!(entity_page.content.contains("Source update"));
    }
}
