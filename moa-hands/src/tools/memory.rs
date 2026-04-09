//! Built-in memory tool implementations backed by `MemoryStore`.

use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{MemoryPath, MemoryScope, MoaError, PageType, Result, ToolOutput, WikiPage};
use serde::Deserialize;

use crate::router::{BuiltInTool, ToolContext};

/// Built-in memory read tool.
pub struct MemoryReadTool;

#[async_trait]
impl BuiltInTool for MemoryReadTool {
    fn name(&self) -> &'static str {
        "memory_read"
    }

    fn description(&self) -> &'static str {
        "Read a memory wiki page by logical path."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Logical wiki path such as skills/deploy/SKILL.md." }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn risk_level(&self) -> moa_core::RiskLevel {
        moa_core::RiskLevel::Low
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let params: MemoryReadInput = serde_json::from_value(input.clone())?;
        let started_at = Instant::now();
        let path = MemoryPath::new(params.path);
        let page = ctx.memory_store.read_page(&path).await?;

        Ok(ToolOutput {
            stdout: format!(
                "# {} ({})\n\n{}",
                page.title,
                path.as_str(),
                page.content.trim()
            ),
            stderr: String::new(),
            exit_code: 0,
            duration: started_at.elapsed(),
        })
    }
}

/// Built-in memory search tool.
pub struct MemorySearchTool;

#[async_trait]
impl BuiltInTool for MemorySearchTool {
    fn name(&self) -> &'static str {
        "memory_search"
    }

    fn description(&self) -> &'static str {
        "Search the file-backed memory wiki for relevant pages."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search terms." },
                "scope": { "type": "string", "enum": ["user", "workspace", "both"], "default": "both" },
                "type_filter": { "type": "string", "enum": ["index", "topic", "entity", "decision", "skill", "source", "schema", "log"] },
                "limit": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn risk_level(&self) -> moa_core::RiskLevel {
        moa_core::RiskLevel::Low
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let params: MemorySearchInput = serde_json::from_value(input.clone())?;
        let started_at = Instant::now();
        let limit = params.limit.unwrap_or(5).clamp(1, 10);
        let type_filter = params
            .type_filter
            .as_deref()
            .map(parse_page_type)
            .transpose()?;
        let scopes = params.scope.scopes(ctx.session);
        let per_scope_limit = limit.max(1);
        let mut rendered = Vec::new();

        for scope in scopes {
            let scope_label = match &scope {
                MemoryScope::User(_) => "user",
                MemoryScope::Workspace(_) => "workspace",
            };
            let mut results = ctx
                .memory_store
                .search(&params.query, scope, per_scope_limit)
                .await?;
            if let Some(page_type) = &type_filter {
                results.retain(|result| &result.page_type == page_type);
            }
            for result in results.into_iter().take(limit) {
                rendered.push(format!(
                    "## {} ({})\nScope: {} | Confidence: {:?} | Updated: {}\n{}\n",
                    result.title,
                    result.path,
                    scope_label,
                    result.confidence,
                    result.updated.to_rfc3339(),
                    result.snippet
                ));
            }
        }

        let stdout = if rendered.is_empty() {
            "No matching memory pages found.".to_string()
        } else {
            rendered.join("\n")
        };

        Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
            duration: started_at.elapsed(),
        })
    }
}

/// Built-in memory write tool.
pub struct MemoryWriteTool;

#[async_trait]
impl BuiltInTool for MemoryWriteTool {
    fn name(&self) -> &'static str {
        "memory_write"
    }

    fn description(&self) -> &'static str {
        "Update an existing memory wiki page."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Logical wiki path such as topics/auth.md." },
                "title": { "type": "string", "description": "Optional title override." },
                "content": { "type": "string", "description": "Full markdown body." },
                "scope": { "type": "string", "enum": ["user", "workspace"], "description": "Intended scope for the write." },
                "page_type": { "type": "string", "enum": ["index", "topic", "entity", "decision", "skill", "source", "schema", "log"] },
                "related": { "type": "array", "items": { "type": "string" } },
                "sources": { "type": "array", "items": { "type": "string" } },
                "tags": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    fn risk_level(&self) -> moa_core::RiskLevel {
        moa_core::RiskLevel::Medium
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let params: MemoryWriteInput = serde_json::from_value(input.clone())?;
        let started_at = Instant::now();
        let path = MemoryPath::new(params.path);
        let existing_page = ctx.memory_store.read_page(&path).await.map_err(|error| {
            MoaError::ToolError(format!(
                "memory_write currently requires an existing uniquely scoped page: {error}"
            ))
        })?;
        let now = Utc::now();
        if params.scope.is_some() {
            tracing::debug!(
                path = path.as_str(),
                requested_scope = ?params.scope,
                "memory_write received a scope hint that cannot be enforced through the current MemoryStore trait"
            );
        }

        let page = WikiPage {
            path: Some(path.clone()),
            title: params.title.unwrap_or(existing_page.title),
            page_type: params
                .page_type
                .as_deref()
                .map(parse_page_type)
                .transpose()?
                .unwrap_or(existing_page.page_type),
            content: params.content,
            created: existing_page.created,
            updated: now,
            confidence: existing_page.confidence,
            related: params.related.unwrap_or(existing_page.related),
            sources: params.sources.unwrap_or(existing_page.sources),
            tags: params.tags.unwrap_or(existing_page.tags),
            auto_generated: existing_page.auto_generated,
            last_referenced: existing_page.last_referenced,
            reference_count: existing_page.reference_count,
            metadata: existing_page.metadata,
        };
        ctx.memory_store.write_page(&path, page).await?;

        Ok(ToolOutput {
            stdout: format!("Updated memory page {}", path.as_str()),
            stderr: String::new(),
            exit_code: 0,
            duration: started_at.elapsed(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct MemorySearchInput {
    query: String,
    #[serde(default)]
    scope: MemorySearchScope,
    #[serde(default)]
    type_filter: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct MemoryReadInput {
    path: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MemorySearchScope {
    User,
    Workspace,
    #[default]
    Both,
}

impl MemorySearchScope {
    fn scopes(&self, session: &moa_core::SessionMeta) -> Vec<MemoryScope> {
        match self {
            Self::User => vec![MemoryScope::User(session.user_id.clone())],
            Self::Workspace => vec![MemoryScope::Workspace(session.workspace_id.clone())],
            Self::Both => vec![
                MemoryScope::User(session.user_id.clone()),
                MemoryScope::Workspace(session.workspace_id.clone()),
            ],
        }
    }
}

#[derive(Debug, Deserialize)]
struct MemoryWriteInput {
    path: String,
    #[serde(default)]
    title: Option<String>,
    content: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    page_type: Option<String>,
    #[serde(default)]
    related: Option<Vec<String>>,
    #[serde(default)]
    sources: Option<Vec<String>>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

fn parse_page_type(value: &str) -> Result<PageType> {
    match value {
        "index" => Ok(PageType::Index),
        "topic" => Ok(PageType::Topic),
        "entity" => Ok(PageType::Entity),
        "decision" => Ok(PageType::Decision),
        "skill" => Ok(PageType::Skill),
        "source" => Ok(PageType::Source),
        "schema" => Ok(PageType::Schema),
        "log" => Ok(PageType::Log),
        other => Err(MoaError::ValidationError(format!(
            "unsupported memory page type: {other}"
        ))),
    }
}
