//! Built-in memory tool implementations backed by `MemoryStore`.

use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    BuiltInTool, Event, IngestReport, MemoryPath, MemoryScope, MemorySearchMode, MoaError,
    PageType, PolicyAction, Result, RiskLevel, ToolContent, ToolContext, ToolDiffStrategy,
    ToolInputShape, ToolOutput, ToolPolicySpec, WikiPage, read_tool_policy, write_tool_policy,
};
use serde::Deserialize;
use serde_json::json;

/// Built-in memory read tool.
pub struct MemoryReadTool;

#[async_trait]
impl BuiltInTool for MemoryReadTool {
    fn name(&self) -> &'static str {
        "memory_read"
    }

    fn description(&self) -> &'static str {
        "Read a memory wiki page by logical path. Optionally specify `scope` as `workspace` or `user`; otherwise MOA checks workspace first and then user scope."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Logical wiki path such as skills/deploy/SKILL.md." },
                "scope": { "type": "string", "enum": ["user", "workspace"], "description": "Optional explicit scope. Defaults to workspace and falls back to user when omitted." }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        read_tool_policy(ToolInputShape::Path)
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let params: MemoryReadInput = serde_json::from_value(input.clone())?;
        let started_at = Instant::now();
        let path = MemoryPath::new(params.path);
        let page = match params.scope.as_deref() {
            Some(scope) => {
                let resolved_scope = parse_scope(scope, ctx.session)?;
                ctx.memory_store.read_page(&resolved_scope, &path).await?
            }
            None => read_page_with_fallback(ctx.memory_store, ctx.session, &path).await?,
        };

        Ok(ToolOutput::text(
            format!(
                "# {} ({})\n\n{}",
                page.title,
                path.as_str(),
                page.content.trim()
            ),
            started_at.elapsed(),
        ))
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
                "limit": { "type": "integer", "minimum": 1, "maximum": 10, "default": 5 },
                "mode": { "type": "string", "enum": ["hybrid", "keyword", "semantic"], "default": "hybrid" }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        read_tool_policy(ToolInputShape::Query)
    }

    fn max_output_tokens(&self) -> u32 {
        3_000
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
        let mut structured_results = Vec::new();

        for scope in scopes {
            let scope_label = match &scope {
                MemoryScope::User(_) => "user",
                MemoryScope::Workspace(_) => "workspace",
            };
            let mut results = ctx
                .memory_store
                .search_with_mode(&params.query, &scope, per_scope_limit, params.mode)
                .await?;
            if let Some(page_type) = &type_filter {
                results.retain(|result| &result.page_type == page_type);
            }
            for result in results.into_iter().take(limit) {
                let path = result.path.clone();
                let title = result.title.clone();
                let confidence = result.confidence.clone();
                let updated = result.updated;
                let snippet = result.snippet.clone();
                let page_type = result.page_type.clone();
                let reference_count = result.reference_count;
                structured_results.push(json!({
                    "path": path,
                    "title": title,
                    "scope": scope_label,
                    "confidence": confidence,
                    "updated": updated,
                    "snippet": snippet,
                    "page_type": page_type,
                    "reference_count": reference_count,
                }));
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

        let summary = if rendered.is_empty() {
            "No matching memory pages found.".to_string()
        } else {
            rendered.join("\n")
        };

        Ok(ToolOutput::json(
            summary,
            serde_json::Value::Array(structured_results),
            started_at.elapsed(),
        ))
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
        "Create or update a memory wiki page. Provide `scope` when creating a new page; without `scope`, MOA updates an existing page by checking workspace scope first and then user scope."
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

    fn policy_spec(&self) -> ToolPolicySpec {
        write_tool_policy(ToolInputShape::Path, ToolDiffStrategy::None)
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let params: MemoryWriteInput = serde_json::from_value(input.clone())?;
        let started_at = Instant::now();
        let path = MemoryPath::new(params.path);
        let now = Utc::now();
        let (scope, existing_page) = match params.scope.as_deref() {
            Some(scope) => (parse_scope(scope, ctx.session)?, None),
            None => resolve_existing_scope(ctx.memory_store, ctx.session, &path).await?,
        };

        let page = match existing_page {
            Some(existing_page) => WikiPage {
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
            },
            None => WikiPage {
                path: Some(path.clone()),
                title: params.title.unwrap_or_else(|| infer_page_title(&path)),
                page_type: params
                    .page_type
                    .as_deref()
                    .map(parse_page_type)
                    .transpose()?
                    .unwrap_or_else(|| infer_page_type(&path)),
                content: params.content,
                created: now,
                updated: now,
                confidence: moa_core::ConfidenceLevel::Medium,
                related: params.related.unwrap_or_default(),
                sources: params.sources.unwrap_or_default(),
                tags: params.tags.unwrap_or_default(),
                auto_generated: false,
                last_referenced: now,
                reference_count: 0,
                metadata: std::collections::HashMap::new(),
            },
        };
        ctx.memory_store.write_page(&scope, &path, page).await?;

        Ok(ToolOutput::text(
            format!("Wrote memory page {}", path.as_str()),
            started_at.elapsed(),
        ))
    }
}

/// Built-in memory ingest tool.
pub struct MemoryIngestTool;

#[async_trait]
impl BuiltInTool for MemoryIngestTool {
    fn name(&self) -> &'static str {
        "memory_ingest"
    }

    fn description(&self) -> &'static str {
        "Ingest a source document into workspace memory. Creates a summary page in sources/, extracts entities, topics, and decisions into related wiki pages, and refreshes the workspace index."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "source_name": {
                    "type": "string",
                    "description": "Optional human-readable name for the source document. Defaults to the first markdown heading."
                },
                "content": {
                    "type": "string",
                    "description": "Full text content of the document to ingest."
                }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        ToolPolicySpec {
            risk_level: RiskLevel::Low,
            default_action: PolicyAction::Allow,
            input_shape: ToolInputShape::Json,
            diff_strategy: ToolDiffStrategy::None,
        }
    }

    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let params: MemoryIngestInput = serde_json::from_value(input.clone())?;
        let started_at = Instant::now();
        let source_name = params
            .source_name
            .unwrap_or_else(|| derive_source_name_from_content(&params.content));
        let report = ctx
            .memory_store
            .ingest_source(
                &MemoryScope::Workspace(ctx.session.workspace_id.clone()),
                &source_name,
                &params.content,
            )
            .await?;

        if let Some(session_store) = ctx.session_store {
            session_store
                .emit_event(
                    ctx.session.id,
                    Event::MemoryIngest {
                        source_name: report.source_name.clone(),
                        source_path: report.source_path.to_string(),
                        affected_pages: report
                            .affected_pages
                            .iter()
                            .map(ToString::to_string)
                            .collect(),
                        contradictions: report.contradictions.clone(),
                    },
                )
                .await?;
        }

        Ok(ToolOutput {
            content: vec![ToolContent::Text {
                text: format_ingest_report(&report),
            }],
            is_error: false,
            structured: Some(ingest_report_json(&report)),
            duration: started_at.elapsed(),
            truncated: false,
            original_output_tokens: None,
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
    #[serde(default)]
    mode: MemorySearchMode,
}

#[derive(Debug, Deserialize)]
struct MemoryReadInput {
    path: String,
    #[serde(default)]
    scope: Option<String>,
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

#[derive(Debug, Deserialize)]
struct MemoryIngestInput {
    content: String,
    #[serde(default)]
    source_name: Option<String>,
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

async fn read_page_with_fallback(
    memory_store: &dyn moa_core::MemoryStore,
    session: &moa_core::SessionMeta,
    path: &MemoryPath,
) -> Result<WikiPage> {
    match memory_store
        .read_page(&MemoryScope::Workspace(session.workspace_id.clone()), path)
        .await
    {
        Ok(page) => Ok(page),
        Err(error) if is_memory_not_found(&error) => memory_store
            .read_page(&MemoryScope::User(session.user_id.clone()), path)
            .await
            .map_err(|fallback_error| {
                if is_memory_not_found(&fallback_error) {
                    MoaError::ToolError(format!(
                        "memory page not found in workspace or user scope: {}",
                        path.as_str()
                    ))
                } else {
                    fallback_error
                }
            }),
        Err(error) => Err(error),
    }
}

async fn resolve_existing_scope(
    memory_store: &dyn moa_core::MemoryStore,
    session: &moa_core::SessionMeta,
    path: &MemoryPath,
) -> Result<(MemoryScope, Option<WikiPage>)> {
    let workspace_scope = MemoryScope::Workspace(session.workspace_id.clone());
    match memory_store.read_page(&workspace_scope, path).await {
        Ok(page) => return Ok((workspace_scope, Some(page))),
        Err(error) if !is_memory_not_found(&error) => return Err(error),
        Err(_) => {}
    }

    let user_scope = MemoryScope::User(session.user_id.clone());
    match memory_store.read_page(&user_scope, path).await {
        Ok(page) => Ok((user_scope, Some(page))),
        Err(error) if !is_memory_not_found(&error) => Err(error),
        Err(_) => Err(MoaError::ToolError(format!(
            "memory page {} does not exist in workspace or user scope; specify `scope` as `workspace` or `user` to create it",
            path.as_str()
        ))),
    }
}

fn parse_scope(value: &str, session: &moa_core::SessionMeta) -> Result<MemoryScope> {
    match value {
        "user" => Ok(MemoryScope::User(session.user_id.clone())),
        "workspace" => Ok(MemoryScope::Workspace(session.workspace_id.clone())),
        other => Err(MoaError::ValidationError(format!(
            "unsupported memory scope: {other}"
        ))),
    }
}

fn infer_page_type(path: &MemoryPath) -> PageType {
    match path.as_str().split('/').next() {
        Some("skills") => PageType::Skill,
        Some("entities") => PageType::Entity,
        Some("decisions") => PageType::Decision,
        Some("sources") => PageType::Source,
        Some("schemas") => PageType::Schema,
        Some("logs") => PageType::Log,
        Some("topics") => PageType::Topic,
        _ if path.as_str() == "MEMORY.md" => PageType::Index,
        _ if path.as_str() == "_log.md" => PageType::Log,
        _ => PageType::Topic,
    }
}

fn infer_page_title(path: &MemoryPath) -> String {
    let leaf = path
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or(path.as_str())
        .trim_end_matches(".md");
    leaf.split(['-', '_'])
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn derive_source_name_from_content(content: &str) -> String {
    if let Some(heading) = content
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('#') && !line.trim_start_matches('#').trim().is_empty())
    {
        return heading.trim_start_matches('#').trim().to_string();
    }

    let first_line = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Untitled Source");
    first_line.chars().take(80).collect()
}

fn ingest_report_json(report: &IngestReport) -> serde_json::Value {
    let derived_pages = report
        .affected_pages
        .iter()
        .skip(1)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let (entities, topics, decisions) = count_ingest_pages(report);

    json!({
        "scope": &report.scope,
        "source_name": &report.source_name,
        "source_path": &report.source_path,
        "affected_pages": &report.affected_pages,
        "derived_pages": derived_pages,
        "counts": {
            "entities": entities,
            "topics": topics,
            "decisions": decisions,
        },
        "contradictions": &report.contradictions,
    })
}

fn format_ingest_report(report: &IngestReport) -> String {
    let mut lines = vec![
        format!("Ingested \"{}\" into workspace memory.", report.source_name),
        String::new(),
        format!("Created: {}", report.source_path.as_str()),
    ];

    if report.affected_pages.len() > 1 {
        lines.push(String::new());
        lines.push("Updated pages:".to_string());
        for path in report.affected_pages.iter().skip(1) {
            lines.push(format!("- {}", path.as_str()));
        }
    }

    let (entities, topics, decisions) = count_ingest_pages(report);
    lines.push(String::new());
    lines.push(format!(
        "Extracted: {} entities, {} topics, {} decisions",
        entities, topics, decisions
    ));

    if !report.contradictions.is_empty() {
        lines.push(String::new());
        lines.push("Contradictions detected:".to_string());
        for contradiction in &report.contradictions {
            lines.push(format!("- {contradiction}"));
        }
    }

    lines.join("\n")
}

fn count_ingest_pages(report: &IngestReport) -> (usize, usize, usize) {
    report
        .affected_pages
        .iter()
        .skip(1)
        .fold((0, 0, 0), |(entities, topics, decisions), path| {
            let raw = path.as_str();
            if raw.starts_with("entities/") {
                (entities + 1, topics, decisions)
            } else if raw.starts_with("topics/") {
                (entities, topics + 1, decisions)
            } else if raw.starts_with("decisions/") {
                (entities, topics, decisions + 1)
            } else {
                (entities, topics, decisions)
            }
        })
}

fn is_memory_not_found(error: &MoaError) -> bool {
    matches!(error, MoaError::StorageError(message) if message.starts_with("memory page not found:"))
}
