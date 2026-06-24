//! Stage 5: injects a budgeted skill manifest as dynamic turn context.

mod activation;
mod registry;
#[cfg(test)]
mod test_support;
mod tier1_metadata;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    AgentSkillPolicy, AgentSkillPolicyMode, ContextMessage, ContextProcessor, ExcludedItem,
    ProcessorOutput, Result, SegmentStore, SessionStore, SkillBudgetConfig, SkillMetadata,
    WorkingContext,
};
use serde_json::json;
use sqlx::PgPool;

use self::tier1_metadata::{
    DEFAULT_MANIFEST_WINDOW_RATIO, DEFAULT_MIN_MANIFEST_CHARS, ResolvedSkillBudget,
    format_skill_manifest, rank_skills, select_skills_within_budget_and_limit,
};

const RECENT_EVENT_LIMIT: usize = 32;
const EXCLUDED_ITEMS_METADATA_KEY: &str = "excluded_items";
const QUERY_KEYWORDS_METADATA_KEY: &str = "query_keywords";
const TASK_STRATEGY_RATES_METADATA_KEY: &str = "task_strategy_rates";
const MANIFEST_BUDGET_METADATA_KEY: &str = "manifest_budget_chars";
const MANIFEST_CHARS_USED_METADATA_KEY: &str = "manifest_chars_used";
/// Context metadata key containing selected skill names.
pub const SELECTED_SKILL_NAMES_METADATA_KEY: &str = "selected_skill_names";
/// Context metadata key containing the selected skill sandbox file count.
pub const SELECTED_SKILL_FILE_COUNT_METADATA_KEY: &str = "selected_skill_sandbox_file_count";

/// Injects workspace skill metadata into dynamic turn context.
pub struct SkillInjector {
    source: SkillSource,
    session_store: Option<Arc<dyn SessionStore>>,
    segment_store: Option<Arc<dyn SegmentStore>>,
    budget_config: SkillBudgetConfig,
}

/// Shared skill-injection stage backed by a process-wide injector.
#[derive(Clone)]
pub struct SharedSkillInjector {
    inner: Arc<SkillInjector>,
}

enum SkillSource {
    Registry(PgPool),
    #[cfg(test)]
    Static(Vec<SkillMetadata>),
}

impl SharedSkillInjector {
    /// Creates a shared skill processor from a prebuilt injector.
    #[must_use]
    pub fn new(inner: Arc<SkillInjector>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ContextProcessor for SharedSkillInjector {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn stage(&self) -> u8 {
        self.inner.stage()
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        self.inner.process(ctx).await
    }
}

impl SkillInjector {
    /// Creates a skill injector backed by the Postgres skill registry.
    pub fn new(pool: PgPool) -> Self {
        Self {
            source: SkillSource::Registry(pool),
            session_store: None,
            segment_store: None,
            budget_config: SkillBudgetConfig::default(),
        }
    }

    /// Creates a skill injector from static test metadata.
    #[cfg(test)]
    pub fn from_skills(skills: Vec<SkillMetadata>) -> Self {
        Self {
            source: SkillSource::Static(skills),
            session_store: None,
            segment_store: None,
            budget_config: SkillBudgetConfig::default(),
        }
    }

    /// Configures the injector to derive query keywords from recent session events.
    pub fn with_session_store(mut self, session_store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(session_store);
        self
    }

    /// Configures the injector to use segment-derived analytics for skill ranking.
    pub fn with_segment_store(mut self, segment_store: Arc<dyn SegmentStore>) -> Self {
        self.segment_store = Some(segment_store);
        self
    }

    /// Overrides the manifest budgeting controls.
    pub fn with_budget_config(mut self, budget_config: SkillBudgetConfig) -> Self {
        self.budget_config = budget_config;
        self
    }

    async fn load_skill_metadata(&self, ctx: &WorkingContext) -> Result<Vec<SkillMetadata>> {
        match &self.source {
            SkillSource::Registry(pool) => registry::load_skills(pool, ctx).await,
            #[cfg(test)]
            SkillSource::Static(skills) => Ok(skills.clone()),
        }
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
        5
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let skills = self.load_skill_metadata(ctx).await?;
        let tokens_before = ctx.token_count;

        if skills.is_empty() {
            return Ok(ProcessorOutput::default());
        }

        let query_keywords = self.query_keywords(ctx).await?;
        let resolution_rates = self.skill_resolution_rates(ctx).await?;
        let task_strategy_rates = self.task_strategy_success_rates(ctx).await?;
        let budget = self.compute_budget(ctx.model_capabilities.context_window);
        let policy = agent_skill_policy(ctx)?;
        let policy_filtered = filter_skills_by_agent_policy(skills, &policy);
        let ranked = rank_skills(
            &policy_filtered.skills,
            &query_keywords,
            &budget,
            &resolution_rates,
            &task_strategy_rates,
        );
        let selection = select_skills_within_budget_and_limit(
            &ranked,
            budget.max_manifest_chars,
            policy
                .max_visible
                .and_then(|limit| usize::try_from(limit).ok()),
            &pinned_skill_names(&policy),
        );
        let manifest = format_skill_manifest(&selection.selected);
        let selected_metadata = selection
            .selected
            .iter()
            .map(|skill| skill.metadata.clone())
            .collect::<Vec<_>>();
        let selected_files = match &self.source {
            SkillSource::Registry(pool) => {
                registry::load_selected_skill_files(pool, ctx, &selected_metadata).await?
            }
            #[cfg(test)]
            SkillSource::Static(_) => Vec::new(),
        };
        let selected_file_count = selected_files.len();

        if !manifest.is_empty() {
            ctx.append_message(ContextMessage::user(manifest));
        }

        let items_included = selection
            .selected
            .iter()
            .map(|skill| skill.metadata.name.clone())
            .collect::<Vec<_>>();
        let mut excluded_items = policy_filtered.excluded;
        excluded_items.extend(selection.excluded.clone());
        let items_excluded = excluded_items
            .iter()
            .map(|item| item.item.clone())
            .collect::<Vec<_>>();
        ctx.insert_metadata(
            SELECTED_SKILL_NAMES_METADATA_KEY,
            json!(items_included.clone()),
        );
        ctx.insert_metadata(
            SELECTED_SKILL_FILE_COUNT_METADATA_KEY,
            json!(selected_file_count),
        );
        ctx.extend_trusted_sandbox_files(selected_files);
        let output_metadata = HashMap::from([
            (
                QUERY_KEYWORDS_METADATA_KEY.to_string(),
                json!(query_keywords),
            ),
            (
                MANIFEST_BUDGET_METADATA_KEY.to_string(),
                json!(budget.max_manifest_chars),
            ),
            (
                TASK_STRATEGY_RATES_METADATA_KEY.to_string(),
                json!(task_strategy_rates.keys().collect::<Vec<_>>()),
            ),
            (
                MANIFEST_CHARS_USED_METADATA_KEY.to_string(),
                json!(selection.chars_used),
            ),
            (
                EXCLUDED_ITEMS_METADATA_KEY.to_string(),
                json!(excluded_items.clone()),
            ),
            (
                SELECTED_SKILL_NAMES_METADATA_KEY.to_string(),
                json!(items_included.clone()),
            ),
            (
                SELECTED_SKILL_FILE_COUNT_METADATA_KEY.to_string(),
                json!(selected_file_count),
            ),
        ]);

        Ok(ProcessorOutput {
            tokens_added: ctx.token_count.saturating_sub(tokens_before),
            items_included,
            items_excluded,
            excluded_items,
            metadata: output_metadata,
            ..ProcessorOutput::default()
        })
    }
}

struct PolicyFilteredSkills {
    skills: Vec<SkillMetadata>,
    excluded: Vec<ExcludedItem>,
}

fn agent_skill_policy(ctx: &WorkingContext) -> Result<AgentSkillPolicy> {
    Ok(ctx
        .agent_policy_snapshot()?
        .map(|snapshot| snapshot.skill_policy)
        .unwrap_or_default())
}

fn filter_skills_by_agent_policy(
    skills: Vec<SkillMetadata>,
    policy: &AgentSkillPolicy,
) -> PolicyFilteredSkills {
    let refs = policy
        .refs
        .iter()
        .filter_map(|reference| skill_name_from_ref(reference))
        .collect::<HashSet<_>>();
    if refs.is_empty()
        || matches!(
            policy.mode,
            AgentSkillPolicyMode::Auto | AgentSkillPolicyMode::Pinned
        )
    {
        return PolicyFilteredSkills {
            skills,
            excluded: Vec::new(),
        };
    }

    let mut allowed = Vec::new();
    let mut excluded = Vec::new();
    for skill in skills {
        let referenced = refs.contains(skill.name.as_str());
        let include = match policy.mode {
            AgentSkillPolicyMode::Allowlist => referenced,
            AgentSkillPolicyMode::Denylist => !referenced,
            AgentSkillPolicyMode::Auto | AgentSkillPolicyMode::Pinned => true,
        };
        if include {
            allowed.push(skill);
        } else {
            excluded.push(ExcludedItem {
                item: skill.name,
                reason: match policy.mode {
                    AgentSkillPolicyMode::Allowlist => {
                        "excluded by agent skill allowlist".to_string()
                    }
                    AgentSkillPolicyMode::Denylist => {
                        "excluded by agent skill denylist".to_string()
                    }
                    AgentSkillPolicyMode::Auto | AgentSkillPolicyMode::Pinned => {
                        "excluded by agent skill policy".to_string()
                    }
                },
            });
        }
    }

    PolicyFilteredSkills {
        skills: allowed,
        excluded,
    }
}

fn skill_name_from_ref(reference: &str) -> Option<&str> {
    reference
        .strip_prefix("skill://")
        .filter(|name| !name.trim().is_empty())
}

fn pinned_skill_names(policy: &AgentSkillPolicy) -> Vec<String> {
    if policy.mode != AgentSkillPolicyMode::Pinned {
        return Vec::new();
    }
    let mut names = policy
        .refs
        .iter()
        .filter_map(|reference| skill_name_from_ref(reference).map(ToString::to_string))
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use moa_core::{
        AgentContext, AgentPolicySnapshot, AgentSkillPolicy, AgentSkillPolicyMode, ContextMessage,
        ContextProcessor, SYSTEM_DEFAULT_AGENT_POLICY_HASH, SYSTEM_DEFAULT_AGENT_REF,
        SYSTEM_DEFAULT_AGENT_REVISION_UID, SkillBudgetConfig,
    };
    use serde_json::json;

    use super::test_support::{capabilities, session, skills};
    use super::tier1_metadata::{MANIFEST_FOOTER, MANIFEST_PREAMBLE};
    use super::{SharedSkillInjector, SkillInjector};

    #[tokio::test]
    async fn skill_injector_formats_dynamic_metadata() {
        // Pins: selected skill manifests are dynamic context and do not require cache markers.
        let mut ctx = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        let skills = skills(vec![(
            "debug-oauth",
            "OAuth refresh-token debugging workflow",
            3,
            0,
        )]);

        let output = SkillInjector::from_skills(skills)
            .process(&mut ctx)
            .await
            .expect("skill injection should succeed");

        assert_eq!(ctx.messages[0].role, moa_core::MessageRole::User);
        assert!(ctx.messages[0].content.contains("<available_skills>"));
        assert!(ctx.messages[0].content.contains("debug-oauth"));
        assert!(
            ctx.messages[0]
                .content
                .contains("Activate a skill only when")
        );
        assert!(!ctx.messages[0].content.contains("allowed-tools"));
        assert!(output.tokens_added > 0);
        assert_eq!(output.items_included, vec!["debug-oauth"]);
    }

    #[tokio::test]
    async fn skill_injector_injects_nothing_without_skills() {
        // Pins: an empty skill registry leaves the compiled prompt unchanged.
        let mut ctx = moa_core::WorkingContext::new(&session(), capabilities(200_000));

        let output = SkillInjector::from_skills(Vec::new())
            .process(&mut ctx)
            .await
            .expect("skill injection should succeed");

        assert!(ctx.messages.is_empty());
        assert_eq!(output.tokens_added, 0);
        assert!(output.items_included.is_empty());
    }

    #[tokio::test]
    async fn emits_all_skills_alphabetically_when_budget_allows() {
        let mut ctx = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        let skills = skills(vec![
            ("zeta", "Zeta workflow", 1, 2),
            ("alpha", "Alpha workflow", 10, 0),
            ("gamma", "Gamma workflow", 5, 1),
            ("beta", "Beta workflow", 7, 3),
            ("delta", "Delta workflow", 3, 4),
        ]);

        let output = SkillInjector::from_skills(skills)
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

    #[tokio::test]
    async fn identical_query_produces_identical_manifest_output() {
        let static_skills = skills(vec![
            ("auth", "Handle auth incidents", 9, 0),
            ("db", "Handle database incidents", 7, 1),
        ]);

        let mut first = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        first.append_message(ContextMessage::user("Investigate auth failures"));
        SkillInjector::from_skills(static_skills.clone())
            .process(&mut first)
            .await
            .expect("first manifest should render");

        let mut second = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        second.append_message(ContextMessage::user("Investigate auth failures"));
        SkillInjector::from_skills(static_skills)
            .process(&mut second)
            .await
            .expect("second manifest should render");

        assert_eq!(first.messages[1].content, second.messages[1].content);
    }

    #[tokio::test]
    async fn different_queries_keep_manifest_identical_when_selected_set_does_not_change() {
        let static_skills = skills(vec![
            ("auth", "Handle auth incidents", 9, 0),
            ("db", "Handle database incidents", 7, 1),
            ("deploy", "Handle deploy incidents", 5, 2),
        ]);

        let mut first = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        first.append_message(ContextMessage::user("Investigate auth failures"));
        SkillInjector::from_skills(static_skills.clone())
            .process(&mut first)
            .await
            .expect("first manifest should render");

        let mut second = moa_core::WorkingContext::new(&session(), capabilities(200_000));
        second.append_message(ContextMessage::user("Review database latency"));
        SkillInjector::from_skills(static_skills)
            .process(&mut second)
            .await
            .expect("second manifest should render");

        assert_eq!(first.messages[1].content, second.messages[1].content);
    }

    #[tokio::test]
    async fn process_uses_budget_override_and_reports_excluded_skills() {
        let static_skills = skills(vec![
            ("alpha", "Alpha workflow", 10, 0),
            ("beta", "Beta workflow", 9, 1),
            ("gamma", "Gamma workflow", 8, 2),
        ]);
        let mut ctx = moa_core::WorkingContext::new(&session(), capabilities(200_000));

        let output = SkillInjector::from_skills(static_skills)
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

    #[tokio::test]
    async fn pinned_skill_policy_selects_pinned_skill_before_higher_ranked_skill() {
        // Pins: configured-agent pinned skills reserve slots before relevance ranking fills budget.
        let static_skills = skills(vec![
            ("popular", "Popular workflow", 100, 0),
            ("pinned", "Pinned workflow", 0, 1),
        ]);
        let mut session = session();
        session.agent_context = Some(agent_context_with_skill_policy(AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Pinned,
            refs: vec!["skill://pinned".to_string()],
            max_visible: Some(1),
        }));
        let mut ctx = moa_core::WorkingContext::new(&session, capabilities(200_000));

        let output = SkillInjector::from_skills(static_skills)
            .process(&mut ctx)
            .await
            .expect("skill injection should succeed");

        assert_eq!(output.items_included, vec!["pinned"]);
        assert_eq!(output.items_excluded, vec!["popular"]);
    }

    #[test]
    fn compute_budget_uses_context_window_percentage_or_default_floor() {
        let injector = SkillInjector::from_skills(Vec::new());

        assert_eq!(injector.compute_budget(200_000).max_manifest_chars, 8_000);
        assert_eq!(
            injector.compute_budget(1_200_000).max_manifest_chars,
            12_000
        );
    }

    #[tokio::test]
    async fn shared_skill_injector_preserves_processor_identity() {
        // Pins: injected skill runtime remains the stage-5 skills processor.
        let shared =
            SharedSkillInjector::new(std::sync::Arc::new(SkillInjector::from_skills(Vec::new())));

        assert_eq!(shared.name(), "skills");
        assert_eq!(shared.stage(), 5);
    }

    fn agent_context_with_skill_policy(skill_policy: AgentSkillPolicy) -> AgentContext {
        let snapshot = AgentPolicySnapshot {
            skill_policy,
            ..AgentPolicySnapshot::default()
        };
        AgentContext {
            agent_id: None,
            installation_uid: None,
            deployment_uid: None,
            definition_ref: SYSTEM_DEFAULT_AGENT_REF.to_string(),
            revision_uid: SYSTEM_DEFAULT_AGENT_REVISION_UID,
            policy_hash: SYSTEM_DEFAULT_AGENT_POLICY_HASH.to_string(),
            display_name: "Test Agent".to_string(),
            artifact_dependencies: Vec::new(),
            tool_dependencies: Vec::new(),
            policy_snapshot: serde_json::to_value(snapshot).expect("serialize policy snapshot"),
        }
    }
}
