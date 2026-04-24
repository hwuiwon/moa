//! Stage 4: injects a budgeted skill manifest and marks the stable cache breakpoint.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    CacheTtl, ContextProcessor, Event, EventRange, ExcludedItem, MemoryPath, MemoryScope,
    MemoryStore, PageType, ProcessorOutput, Result, SessionStore, SkillBudgetConfig, SkillMetadata,
    SkillResolutionRate, WikiPage, WorkingContext, WorkspaceId,
};
use serde_json::{Value, json};

use super::memory::extract_search_keywords;

const MANIFEST_PREAMBLE: &str = "\
<available_skills>
When multiple skills apply, prefer the one whose trigger conditions most specifically match the current task.
Skills can be composed - use multiple if the task requires steps from different skills.
To activate a skill, call memory_read with the canonical skill path skills/<skill-name>/SKILL.md.

";
const MANIFEST_FOOTER: &str = "</available_skills>";
const DEFAULT_MIN_MANIFEST_CHARS: usize = 8_000;
const DEFAULT_MANIFEST_WINDOW_RATIO: f64 = 0.01;
const MAX_SKILL_NAME_CHARS: usize = 64;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 256;
const RECENT_EVENT_LIMIT: usize = 32;
const EXCLUDED_ITEMS_METADATA_KEY: &str = "excluded_items";
const QUERY_KEYWORDS_METADATA_KEY: &str = "query_keywords";
const MANIFEST_BUDGET_METADATA_KEY: &str = "manifest_budget_chars";
const MANIFEST_CHARS_USED_METADATA_KEY: &str = "manifest_chars_used";

/// Injects workspace skill metadata into the stable prompt prefix.
pub struct SkillInjector {
    memory_store: Arc<dyn MemoryStore>,
    session_store: Option<Arc<dyn SessionStore>>,
    budget_config: SkillBudgetConfig,
}

impl SkillInjector {
    /// Creates a skill injector backed by the shared memory store.
    pub fn new(memory_store: Arc<dyn MemoryStore>) -> Self {
        Self {
            memory_store,
            session_store: None,
            budget_config: SkillBudgetConfig::default(),
        }
    }

    /// Creates a skill injector from a memory store.
    pub fn from_memory(memory_store: Arc<dyn MemoryStore>) -> Self {
        Self::new(memory_store)
    }

    /// Configures the injector to derive query keywords from recent session events.
    pub fn with_session_store(mut self, session_store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(session_store);
        self
    }

    /// Overrides the manifest budgeting controls.
    pub fn with_budget_config(mut self, budget_config: SkillBudgetConfig) -> Self {
        self.budget_config = budget_config;
        self
    }

    async fn load_skill_metadata(&self, ctx: &WorkingContext) -> Result<Vec<SkillMetadata>> {
        load_skills(self.memory_store.as_ref(), &ctx.workspace_id).await
    }

    async fn query_keywords(&self, ctx: &WorkingContext) -> Result<Vec<String>> {
        if let Some(message) = ctx.last_user_message() {
            let keywords = extract_search_keywords(message);
            if !keywords.is_empty() {
                return Ok(keywords);
            }
        }

        let Some(session_store) = &self.session_store else {
            return Ok(Vec::new());
        };
        let events = session_store
            .get_events(ctx.session_id, EventRange::recent(RECENT_EVENT_LIMIT))
            .await?;
        Ok(extract_query_keywords_from_events(&events))
    }

    async fn skill_resolution_rates(&self, ctx: &WorkingContext) -> Result<HashMap<String, f64>> {
        let Some(session_store) = &self.session_store else {
            return Ok(HashMap::new());
        };
        let rates = session_store
            .list_skill_resolution_rates(ctx.workspace_id.as_str(), None)
            .await?;
        Ok(skill_resolution_rate_map(&rates))
    }

    fn compute_budget(&self, context_window: usize) -> ResolvedSkillBudget {
        let default_chars =
            ((context_window as f64) * DEFAULT_MANIFEST_WINDOW_RATIO).round() as usize;
        ResolvedSkillBudget {
            max_manifest_chars: self
                .budget_config
                .max_manifest_chars
                .unwrap_or(default_chars.max(DEFAULT_MIN_MANIFEST_CHARS)),
            max_per_skill_chars: self.budget_config.max_per_skill_chars,
            show_token_estimates: self.budget_config.show_token_estimates,
        }
    }
}

#[async_trait]
impl ContextProcessor for SkillInjector {
    fn name(&self) -> &str {
        "skills"
    }

    fn stage(&self) -> u8 {
        4
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let skills = self.load_skill_metadata(ctx).await?;
        let tokens_before = ctx.token_count;

        if skills.is_empty() {
            ctx.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);
            return Ok(ProcessorOutput::default());
        }

        let query_keywords = self.query_keywords(ctx).await?;
        let resolution_rates = self.skill_resolution_rates(ctx).await?;
        let budget = self.compute_budget(ctx.model_capabilities.context_window);
        let ranked = rank_skills(&skills, &query_keywords, &budget, &resolution_rates);
        let selection = select_skills_within_budget(&ranked, budget.max_manifest_chars);
        let manifest = format_skill_manifest(&selection.selected);

        if !manifest.is_empty() {
            ctx.append_system(manifest);
        }
        ctx.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);

        let items_included = selection
            .selected
            .iter()
            .map(|skill| skill.metadata.name.clone())
            .collect::<Vec<_>>();
        let items_excluded = selection
            .excluded
            .iter()
            .map(|item| item.item.clone())
            .collect::<Vec<_>>();

        Ok(ProcessorOutput {
            tokens_added: ctx.token_count.saturating_sub(tokens_before),
            items_included,
            items_excluded,
            excluded_items: selection.excluded.clone(),
            metadata: HashMap::from([
                (
                    QUERY_KEYWORDS_METADATA_KEY.to_string(),
                    json!(query_keywords),
                ),
                (
                    MANIFEST_BUDGET_METADATA_KEY.to_string(),
                    json!(budget.max_manifest_chars),
                ),
                (
                    MANIFEST_CHARS_USED_METADATA_KEY.to_string(),
                    json!(selection.chars_used),
                ),
                (
                    EXCLUDED_ITEMS_METADATA_KEY.to_string(),
                    json!(selection.excluded),
                ),
            ]),
            ..ProcessorOutput::default()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedSkillBudget {
    max_manifest_chars: usize,
    max_per_skill_chars: usize,
    show_token_estimates: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct RankedSkill {
    metadata: SkillMetadata,
    score: f64,
    manifest_entry: String,
}

#[derive(Debug, Clone, PartialEq)]
struct SkillSelection {
    selected: Vec<RankedSkill>,
    excluded: Vec<ExcludedItem>,
    chars_used: usize,
}

fn rank_skills(
    skills: &[SkillMetadata],
    query_keywords: &[String],
    budget: &ResolvedSkillBudget,
    resolution_rates: &HashMap<String, f64>,
) -> Vec<RankedSkill> {
    let max_use_count = skills
        .iter()
        .map(|skill| skill.use_count)
        .max()
        .unwrap_or(0);
    let newest = skills.iter().filter_map(|skill| skill.last_used).max();
    let oldest = skills.iter().filter_map(|skill| skill.last_used).min();

    let mut ranked = skills
        .iter()
        .cloned()
        .map(|metadata| {
            let keyword_overlap = keyword_overlap_score(query_keywords, &metadata);
            let normalized_use_count = if max_use_count == 0 {
                0.0
            } else {
                f64::from(metadata.use_count) / f64::from(max_use_count)
            };
            let recency_score = normalized_recency_score(metadata.last_used, oldest, newest);
            let manifest_entry = format_manifest_entry(&metadata, budget);
            let score = resolution_rates
                .get(&metadata.name)
                .map(|resolution_rate| {
                    (0.3 * keyword_overlap)
                        + (0.4 * resolution_rate)
                        + (0.2 * normalized_use_count)
                        + (0.1 * recency_score)
                })
                .unwrap_or_else(|| {
                    (0.3 * keyword_overlap) + (0.5 * normalized_use_count) + (0.2 * recency_score)
                });

            RankedSkill {
                metadata,
                score,
                manifest_entry,
            }
        })
        .collect::<Vec<_>>();

    ranked.sort_by(compare_ranked_skills);
    ranked
}

fn skill_resolution_rate_map(rates: &[SkillResolutionRate]) -> HashMap<String, f64> {
    rates
        .iter()
        .map(|rate| {
            (
                rate.skill_name.clone(),
                rate.resolution_rate.clamp(0.0, 1.0),
            )
        })
        .collect()
}

fn compare_ranked_skills(left: &RankedSkill, right: &RankedSkill) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| alphabetical_name_cmp(&left.metadata.name, &right.metadata.name))
}

fn select_skills_within_budget(
    ranked: &[RankedSkill],
    max_manifest_chars: usize,
) -> SkillSelection {
    let mut selected = Vec::new();
    let mut selected_names = HashSet::new();
    let mut chars_used = MANIFEST_PREAMBLE.chars().count() + MANIFEST_FOOTER.chars().count();

    for skill in ranked {
        let entry_cost = skill.manifest_entry.chars().count() + 1;
        if chars_used + entry_cost > max_manifest_chars {
            break;
        }

        chars_used += entry_cost;
        selected_names.insert(skill.metadata.name.clone());
        selected.push(skill.clone());
    }

    selected
        .sort_by(|left, right| alphabetical_name_cmp(&left.metadata.name, &right.metadata.name));

    let excluded = ranked
        .iter()
        .filter(|skill| !selected_names.contains(&skill.metadata.name))
        .map(|skill| ExcludedItem {
            item: skill.metadata.name.clone(),
            reason: "excluded by manifest budget after relevance ranking".to_string(),
        })
        .collect::<Vec<_>>();

    SkillSelection {
        selected,
        excluded,
        chars_used,
    }
}

fn format_skill_manifest(selected: &[RankedSkill]) -> String {
    if selected.is_empty() {
        return String::new();
    }

    let entries = selected
        .iter()
        .map(|skill| skill.manifest_entry.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    format!("{MANIFEST_PREAMBLE}{entries}\n{MANIFEST_FOOTER}")
}

fn format_manifest_entry(metadata: &SkillMetadata, budget: &ResolvedSkillBudget) -> String {
    let name = truncate_with_ellipsis(&normalize_inline_text(&metadata.name), MAX_SKILL_NAME_CHARS);
    let description = truncate_with_ellipsis(
        &normalize_inline_text(&metadata.description),
        MAX_SKILL_DESCRIPTION_CHARS,
    );
    let tags = normalized_tags(&metadata.tags);
    let tags = if tags.is_empty() {
        "none".to_string()
    } else {
        tags.join(", ")
    };

    let mut entry = format!("- {name}: {description} [tags: {tags}]");
    if budget.show_token_estimates {
        entry.push_str(&format!(" (est. {} tok)", metadata.estimated_tokens));
    }

    truncate_with_ellipsis(&entry, budget.max_per_skill_chars)
}

fn normalized_tags(tags: &[String]) -> Vec<String> {
    let mut tags = tags
        .iter()
        .map(|tag| normalize_inline_text(tag))
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();
    tags.sort_by(|left, right| alphabetical_name_cmp(left, right));
    tags.dedup();
    tags
}

fn keyword_overlap_score(query_keywords: &[String], metadata: &SkillMetadata) -> f64 {
    if query_keywords.is_empty() {
        return 0.0;
    }

    let haystack = format!(
        "{} {} {}",
        metadata.name,
        metadata.description,
        metadata.tags.join(" ")
    );
    let skill_keywords = extract_search_keywords(&haystack)
        .into_iter()
        .collect::<HashSet<_>>();
    let overlap = query_keywords
        .iter()
        .filter(|keyword| skill_keywords.contains(keyword.as_str()))
        .count();

    overlap as f64 / query_keywords.len() as f64
}

fn normalized_recency_score(
    last_used: Option<DateTime<Utc>>,
    oldest: Option<DateTime<Utc>>,
    newest: Option<DateTime<Utc>>,
) -> f64 {
    match (last_used, oldest, newest) {
        (Some(last_used), Some(oldest), Some(newest)) if newest > oldest => {
            let total_span = (newest - oldest).num_seconds() as f64;
            let distance_from_oldest = (last_used - oldest).num_seconds() as f64;
            (distance_from_oldest / total_span).clamp(0.0, 1.0)
        }
        (Some(_), Some(_), Some(_)) => 1.0,
        (Some(_), _, _) => 1.0,
        _ => 0.0,
    }
}

fn alphabetical_name_cmp(left: &str, right: &str) -> Ordering {
    left.to_ascii_lowercase()
        .cmp(&right.to_ascii_lowercase())
        .then_with(|| left.cmp(right))
}

fn normalize_inline_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }

    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let truncated = value.chars().take(max_chars - 3).collect::<String>();
    format!("{truncated}...")
}

async fn load_skills(
    memory_store: &dyn MemoryStore,
    workspace_id: &WorkspaceId,
) -> Result<Vec<SkillMetadata>> {
    let scope = MemoryScope::Workspace(workspace_id.clone());
    let summaries = memory_store
        .list_pages(&scope, Some(PageType::Skill))
        .await?;
    let mut skills = Vec::with_capacity(summaries.len());

    for summary in summaries {
        let page = memory_store.read_page(&scope, &summary.path).await?;
        skills.push(skill_metadata_from_page(summary.path, &page));
    }

    Ok(skills)
}

fn extract_query_keywords_from_events(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                Some(extract_search_keywords(text))
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn skill_metadata_from_page(path: MemoryPath, page: &WikiPage) -> SkillMetadata {
    SkillMetadata {
        path,
        name: metadata_string(&page.metadata, "name").unwrap_or_else(|| page.title.clone()),
        description: metadata_string(&page.metadata, "description")
            .unwrap_or_else(|| page.title.clone()),
        tags: skill_tags(page),
        allowed_tools: allowed_tools(page),
        estimated_tokens: metadata_nested_usize(&page.metadata, "metadata", "moa-estimated-tokens")
            .unwrap_or_else(|| estimate_skill_tokens(&page.content)),
        use_count: metadata_nested_u32(&page.metadata, "metadata", "moa-use-count")
            .unwrap_or(page.reference_count.min(u64::from(u32::MAX)) as u32),
        last_used: metadata_nested_timestamp(&page.metadata, "metadata", "moa-last-used")
            .or(Some(page.last_referenced)),
        success_rate: metadata_nested_f32(&page.metadata, "metadata", "moa-success-rate")
            .unwrap_or(1.0),
        auto_generated: page.auto_generated,
    }
}

fn skill_tags(page: &WikiPage) -> Vec<String> {
    if !page.tags.is_empty() {
        return page.tags.clone();
    }

    metadata_nested_csv(&page.metadata, "metadata", "moa-tags")
}

fn allowed_tools(page: &WikiPage) -> Vec<String> {
    match page.metadata.get("allowed-tools") {
        Some(Value::String(value)) => value
            .split_whitespace()
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn metadata_string(metadata: &HashMap<String, Value>, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn metadata_nested_string(
    metadata: &HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Option<String> {
    metadata
        .get(container)
        .and_then(Value::as_object)
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn metadata_nested_csv(
    metadata: &HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Vec<String> {
    metadata_nested_string(metadata, container, key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn metadata_nested_usize(
    metadata: &HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Option<usize> {
    metadata_nested_string(metadata, container, key).and_then(|value| value.parse().ok())
}

fn metadata_nested_u32(
    metadata: &HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Option<u32> {
    metadata_nested_string(metadata, container, key).and_then(|value| value.parse().ok())
}

fn metadata_nested_f32(
    metadata: &HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Option<f32> {
    metadata_nested_string(metadata, container, key).and_then(|value| value.parse().ok())
}

fn metadata_nested_timestamp(
    metadata: &HashMap<String, Value>,
    container: &str,
    key: &str,
) -> Option<DateTime<Utc>> {
    metadata_nested_string(metadata, container, key).and_then(|value| {
        chrono::DateTime::parse_from_rfc3339(&value)
            .ok()
            .map(|timestamp| timestamp.with_timezone(&Utc))
    })
}

fn estimate_skill_tokens(body: &str) -> usize {
    body.split_whitespace().count().max(1)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use moa_core::{
        ContextMessage, ContextProcessor, MemoryPath, MemoryScope, MemoryStore, ModelCapabilities,
        ModelId, PageSummary, PageType, Platform, Result, SessionId, SessionMeta,
        SkillBudgetConfig, SkillMetadata, TokenPricing, ToolCallFormat, UserId, WikiPage,
        WorkspaceId,
    };
    use serde_json::json;

    use super::{
        DEFAULT_MIN_MANIFEST_CHARS, MANIFEST_FOOTER, MANIFEST_PREAMBLE, ResolvedSkillBudget,
        SkillInjector, format_manifest_entry, format_skill_manifest, rank_skills,
        select_skills_within_budget,
    };

    fn fixed_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 16, 12, 0, 0)
            .single()
            .expect("fixed skill timestamp should be valid")
    }

    fn older_time(days: i64) -> chrono::DateTime<Utc> {
        fixed_time() - chrono::Duration::days(days)
    }

    fn capabilities(context_window: usize) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
            context_window,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
            native_tools: Vec::new(),
        }
    }

    fn session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        }
    }

    fn resolved_budget(max_manifest_chars: usize) -> ResolvedSkillBudget {
        ResolvedSkillBudget {
            max_manifest_chars,
            max_per_skill_chars: 1_536,
            show_token_estimates: true,
        }
    }

    fn test_skill(
        name: &str,
        description: &str,
        use_count: u32,
        last_used_days_ago: i64,
    ) -> SkillMetadata {
        SkillMetadata {
            path: MemoryPath::new(format!("skills/{name}/SKILL.md")),
            name: name.to_string(),
            description: description.to_string(),
            tags: vec!["ops".to_string(), "debug".to_string()],
            allowed_tools: vec!["bash".to_string()],
            estimated_tokens: 1_200,
            use_count,
            last_used: Some(older_time(last_used_days_ago)),
            success_rate: 0.9,
            auto_generated: false,
        }
    }

    #[derive(Clone)]
    struct StubSkillMemoryStore {
        pages: HashMap<MemoryPath, WikiPage>,
        summaries: Vec<PageSummary>,
    }

    #[async_trait]
    impl MemoryStore for StubSkillMemoryStore {
        async fn search(
            &self,
            _query: &str,
            _scope: &MemoryScope,
            _limit: usize,
        ) -> Result<Vec<moa_core::MemorySearchResult>> {
            Ok(Vec::new())
        }

        async fn read_page(&self, _scope: &MemoryScope, path: &MemoryPath) -> Result<WikiPage> {
            self.pages
                .get(path)
                .cloned()
                .ok_or_else(|| moa_core::MoaError::StorageError("skill page not found".to_string()))
        }

        async fn write_page(
            &self,
            _scope: &MemoryScope,
            _path: &MemoryPath,
            _page: WikiPage,
        ) -> Result<()> {
            Ok(())
        }

        async fn delete_page(&self, _scope: &MemoryScope, _path: &MemoryPath) -> Result<()> {
            Ok(())
        }

        async fn list_pages(
            &self,
            _scope: &MemoryScope,
            _filter: Option<PageType>,
        ) -> Result<Vec<PageSummary>> {
            Ok(self.summaries.clone())
        }

        async fn get_index(&self, _scope: &MemoryScope) -> Result<String> {
            Ok(String::new())
        }

        async fn rebuild_search_index(&self, _scope: &MemoryScope) -> Result<()> {
            Ok(())
        }
    }

    fn skill_page(
        name: &str,
        description: &str,
        use_count: u32,
        last_used_days_ago: i64,
    ) -> WikiPage {
        WikiPage {
            path: Some(MemoryPath::new(format!("skills/{name}/SKILL.md"))),
            title: name.to_string(),
            page_type: PageType::Skill,
            content: "## When to use\nUse this skill for testing.".to_string(),
            created: fixed_time(),
            updated: fixed_time(),
            confidence: moa_core::ConfidenceLevel::High,
            related: Vec::new(),
            sources: Vec::new(),
            tags: vec!["ops".to_string(), "debug".to_string()],
            auto_generated: false,
            last_referenced: older_time(last_used_days_ago),
            reference_count: u64::from(use_count),
            metadata: HashMap::from([
                ("name".to_string(), serde_json::json!(name)),
                ("description".to_string(), serde_json::json!(description)),
                (
                    "metadata".to_string(),
                    serde_json::json!({
                        "moa-estimated-tokens": "1200",
                        "moa-use-count": use_count.to_string(),
                        "moa-last-used": older_time(last_used_days_ago).to_rfc3339(),
                        "moa-success-rate": "0.9",
                        "moa-tags": "ops, debug",
                    }),
                ),
            ]),
        }
    }

    fn store_with_pages(pages: Vec<(MemoryPath, WikiPage)>) -> StubSkillMemoryStore {
        let summaries = pages
            .iter()
            .map(|(path, page)| PageSummary {
                path: path.clone(),
                title: page.title.clone(),
                page_type: PageType::Skill,
                updated: page.updated,
                confidence: moa_core::ConfidenceLevel::High,
            })
            .collect::<Vec<_>>();
        let page_map = pages.into_iter().collect::<HashMap<_, _>>();
        StubSkillMemoryStore {
            pages: page_map,
            summaries,
        }
    }

    #[tokio::test]
    async fn skill_injector_marks_cache_breakpoint_and_formats_metadata() {
        let mut ctx = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        let skill_path = MemoryPath::new("skills/debug-oauth/SKILL.md");
        let store = store_with_pages(vec![(
            skill_path.clone(),
            skill_page(
                "debug-oauth",
                "OAuth refresh-token debugging workflow",
                3,
                0,
            ),
        )]);

        let output = SkillInjector::from_memory(Arc::new(store))
            .process(&mut ctx)
            .await
            .expect("skill injection should succeed");

        assert_eq!(ctx.cache_breakpoints, vec![1]);
        assert!(ctx.messages[0].content.contains("<available_skills>"));
        assert!(ctx.messages[0].content.contains("debug-oauth"));
        assert!(ctx.messages[0].content.contains("memory_read"));
        assert!(!ctx.messages[0].content.contains("allowed-tools"));
        assert!(output.tokens_added > 0);
        assert_eq!(output.items_included, vec!["debug-oauth"]);
    }

    #[tokio::test]
    async fn skill_injector_marks_breakpoint_without_skills() {
        let mut ctx = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        let store = StubSkillMemoryStore {
            pages: HashMap::new(),
            summaries: Vec::new(),
        };

        let output = SkillInjector::from_memory(Arc::new(store))
            .process(&mut ctx)
            .await
            .expect("skill injection should succeed");

        assert_eq!(ctx.cache_breakpoints, vec![0]);
        assert!(ctx.messages.is_empty());
        assert_eq!(output.tokens_added, 0);
        assert!(output.items_included.is_empty());
    }

    #[tokio::test]
    async fn emits_all_skills_alphabetically_when_budget_allows() {
        let mut ctx = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        let store = store_with_pages(vec![
            (
                MemoryPath::new("skills/zeta/SKILL.md"),
                skill_page("zeta", "Zeta workflow", 1, 2),
            ),
            (
                MemoryPath::new("skills/alpha/SKILL.md"),
                skill_page("alpha", "Alpha workflow", 10, 0),
            ),
            (
                MemoryPath::new("skills/gamma/SKILL.md"),
                skill_page("gamma", "Gamma workflow", 5, 1),
            ),
            (
                MemoryPath::new("skills/beta/SKILL.md"),
                skill_page("beta", "Beta workflow", 7, 3),
            ),
            (
                MemoryPath::new("skills/delta/SKILL.md"),
                skill_page("delta", "Delta workflow", 3, 4),
            ),
        ]);

        let output = SkillInjector::from_memory(Arc::new(store))
            .process(&mut ctx)
            .await
            .expect("skill injection should succeed");
        let manifest = ctx.messages[0].content.clone();

        assert_eq!(
            output.items_included,
            vec!["alpha", "beta", "delta", "gamma", "zeta"]
        );
        assert!(
            manifest.find("- alpha:").expect("alpha") < manifest.find("- beta:").expect("beta")
        );
        assert!(
            manifest.find("- beta:").expect("beta") < manifest.find("- delta:").expect("delta")
        );
        assert!(output.items_excluded.is_empty());
    }

    #[test]
    fn selects_top_ranked_skills_then_resorts_emission_alphabetically() {
        let skills = (0..30)
            .map(|index| {
                test_skill(
                    &format!("skill-{index:02}"),
                    &format!("Workflow number {index:02}"),
                    30 - index as u32,
                    index as i64,
                )
            })
            .collect::<Vec<_>>();
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new());
        let exact_budget = MANIFEST_PREAMBLE.chars().count()
            + MANIFEST_FOOTER.chars().count()
            + ranked
                .iter()
                .take(15)
                .map(|skill| skill.manifest_entry.chars().count() + 1)
                .sum::<usize>();
        let selection = select_skills_within_budget(&ranked, exact_budget);

        assert_eq!(selection.selected.len(), 15);
        let names = selection
            .selected
            .iter()
            .map(|skill| skill.metadata.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "skill-00", "skill-01", "skill-02", "skill-03", "skill-04", "skill-05", "skill-06",
                "skill-07", "skill-08", "skill-09", "skill-10", "skill-11", "skill-12", "skill-13",
                "skill-14",
            ]
        );
        assert_eq!(selection.excluded.len(), 15);
    }

    #[test]
    fn long_skill_entries_are_truncated_with_ellipsis() {
        let skill = test_skill("very-long-skill", &"x".repeat(4_000), 1, 0);
        let budget = ResolvedSkillBudget {
            max_manifest_chars: DEFAULT_MIN_MANIFEST_CHARS,
            max_per_skill_chars: 120,
            show_token_estimates: true,
        };

        let entry = format_manifest_entry(&skill, &budget);

        assert_eq!(entry.chars().count(), 120);
        assert!(entry.ends_with("..."));
    }

    #[tokio::test]
    async fn identical_query_produces_identical_manifest_output() {
        let store = Arc::new(store_with_pages(vec![
            (
                MemoryPath::new("skills/auth/SKILL.md"),
                skill_page("auth", "Handle auth incidents", 9, 0),
            ),
            (
                MemoryPath::new("skills/db/SKILL.md"),
                skill_page("db", "Handle database incidents", 7, 1),
            ),
        ]));

        let mut first = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        first.append_message(ContextMessage::user("Investigate auth failures"));
        SkillInjector::from_memory(store.clone())
            .process(&mut first)
            .await
            .expect("first manifest should render");

        let mut second = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        second.append_message(ContextMessage::user("Investigate auth failures"));
        SkillInjector::from_memory(store)
            .process(&mut second)
            .await
            .expect("second manifest should render");

        assert_eq!(first.messages[1].content, second.messages[1].content);
    }

    #[tokio::test]
    async fn different_queries_keep_manifest_identical_when_selected_set_does_not_change() {
        let store = Arc::new(store_with_pages(vec![
            (
                MemoryPath::new("skills/auth/SKILL.md"),
                skill_page("auth", "Handle auth incidents", 9, 0),
            ),
            (
                MemoryPath::new("skills/db/SKILL.md"),
                skill_page("db", "Handle database incidents", 7, 1),
            ),
            (
                MemoryPath::new("skills/deploy/SKILL.md"),
                skill_page("deploy", "Handle deploy incidents", 5, 2),
            ),
        ]));

        let mut first = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        first.append_message(ContextMessage::user("Investigate auth failures"));
        SkillInjector::from_memory(store.clone())
            .process(&mut first)
            .await
            .expect("first manifest should render");

        let mut second = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        second.append_message(ContextMessage::user("Review database latency"));
        SkillInjector::from_memory(store)
            .process(&mut second)
            .await
            .expect("second manifest should render");

        assert_eq!(first.messages[1].content, second.messages[1].content);
    }

    #[test]
    fn selection_reports_excluded_items_with_reasons() {
        let skills = vec![
            test_skill("alpha", "Alpha workflow", 10, 0),
            test_skill("beta", "Beta workflow", 9, 1),
            test_skill("gamma", "Gamma workflow", 1, 2),
        ];
        let budget = resolved_budget(
            MANIFEST_PREAMBLE.chars().count() + MANIFEST_FOOTER.chars().count() + 60,
        );
        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new());
        let selection = select_skills_within_budget(&ranked, budget.max_manifest_chars);

        assert_eq!(selection.selected.len(), 1);
        assert_eq!(selection.excluded.len(), 2);
        assert!(
            selection
                .excluded
                .iter()
                .all(|item| item.reason.contains("manifest budget"))
        );
    }

    #[test]
    fn format_skill_manifest_is_empty_without_selected_skills() {
        assert!(format_skill_manifest(&[]).is_empty());
    }

    #[tokio::test]
    async fn process_uses_budget_override_and_reports_excluded_skills() {
        let store = store_with_pages(vec![
            (
                MemoryPath::new("skills/alpha/SKILL.md"),
                skill_page("alpha", "Alpha workflow", 10, 0),
            ),
            (
                MemoryPath::new("skills/beta/SKILL.md"),
                skill_page("beta", "Beta workflow", 9, 1),
            ),
            (
                MemoryPath::new("skills/gamma/SKILL.md"),
                skill_page("gamma", "Gamma workflow", 8, 2),
            ),
        ]);
        let mut ctx = moa_core::WorkingContext::new(&session(), capabilities(200_000));

        let output = SkillInjector::from_memory(Arc::new(store))
            .with_budget_config(SkillBudgetConfig {
                max_manifest_chars: Some(
                    MANIFEST_PREAMBLE.chars().count() + MANIFEST_FOOTER.chars().count() + 60,
                ),
                max_per_skill_chars: 1_536,
                show_token_estimates: true,
            })
            .process(&mut ctx)
            .await
            .expect("skill injection should succeed");

        assert_eq!(output.items_included, vec!["alpha"]);
        assert_eq!(output.items_excluded.len(), 2);
        assert_eq!(output.excluded_items.len(), 2);
        assert_eq!(
            output.metadata.get("manifest_budget_chars"),
            Some(&json!(
                MANIFEST_PREAMBLE.chars().count() + MANIFEST_FOOTER.chars().count() + 60
            ))
        );
    }

    #[test]
    fn ranking_prefers_keyword_overlap_then_deterministic_name_tie_breaks() {
        let skills = vec![
            test_skill("alpha-auth", "Handle auth failures", 5, 0),
            test_skill("beta-db", "Handle database failures", 5, 0),
        ];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);

        let ranked = rank_skills(&skills, &["auth".to_string()], &budget, &HashMap::new());

        assert_eq!(ranked[0].metadata.name, "alpha-auth");
        assert_eq!(ranked[1].metadata.name, "beta-db");
    }

    #[test]
    fn ranking_uses_resolution_rate_when_available() {
        let skills = vec![
            test_skill("high-use", "General workflow", 100, 0),
            test_skill("high-resolution", "General workflow", 1, 5),
        ];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let resolution_rates = HashMap::from([
            ("high-use".to_string(), 0.0),
            ("high-resolution".to_string(), 1.0),
        ]);

        let ranked = rank_skills(&skills, &[], &budget, &resolution_rates);

        assert_eq!(ranked[0].metadata.name, "high-resolution");
    }

    #[test]
    fn compute_budget_uses_context_window_percentage_or_default_floor() {
        let injector = SkillInjector::from_memory(Arc::new(StubSkillMemoryStore {
            pages: HashMap::new(),
            summaries: Vec::new(),
        }));

        assert_eq!(injector.compute_budget(200_000).max_manifest_chars, 8_000);
        assert_eq!(
            injector.compute_budget(1_200_000).max_manifest_chars,
            12_000
        );
    }

    #[test]
    fn emitted_manifest_entries_are_alphabetical_even_when_ranked_input_is_not() {
        let skills = vec![
            test_skill("zeta", "Zeta workflow", 10, 0),
            test_skill("alpha", "Alpha workflow", 1, 5),
        ];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new());
        assert_eq!(ranked[0].metadata.name, "zeta");
        assert_eq!(ranked[1].metadata.name, "alpha");

        let selection = select_skills_within_budget(&ranked, budget.max_manifest_chars);
        let manifest = format_skill_manifest(&selection.selected);

        assert!(
            manifest.find("- alpha:").expect("alpha") < manifest.find("- zeta:").expect("zeta")
        );
    }
}
