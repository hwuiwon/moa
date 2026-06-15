//! Stage 5: injects a budgeted skill manifest as dynamic turn context.

mod activation;
mod registry;
#[cfg(test)]
mod test_support;
mod tier1_metadata;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    ContextMessage, ContextProcessor, ProcessorOutput, Result, SessionStore, SkillBudgetConfig,
    SkillMetadata, WorkingContext,
};
use serde_json::json;
use sqlx::PgPool;

use self::tier1_metadata::{
    DEFAULT_MANIFEST_WINDOW_RATIO, DEFAULT_MIN_MANIFEST_CHARS, ResolvedSkillBudget,
    format_skill_manifest, rank_skills, select_skills_within_budget,
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
    budget_config: SkillBudgetConfig,
}

enum SkillSource {
    Registry(PgPool),
    #[cfg(test)]
    Static(Vec<SkillMetadata>),
}

impl SkillInjector {
    /// Creates a skill injector backed by the Postgres skill registry.
    pub fn new(pool: PgPool) -> Self {
        Self {
            source: SkillSource::Registry(pool),
            session_store: None,
            budget_config: SkillBudgetConfig::default(),
        }
    }

    /// Creates a skill injector from static test metadata.
    #[cfg(test)]
    pub fn from_skills(skills: Vec<SkillMetadata>) -> Self {
        Self {
            source: SkillSource::Static(skills),
            session_store: None,
            budget_config: SkillBudgetConfig::default(),
        }
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
        let ranked = rank_skills(
            &skills,
            &query_keywords,
            &budget,
            &resolution_rates,
            &task_strategy_rates,
        );
        let selection = select_skills_within_budget(&ranked, budget.max_manifest_chars);
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
        let items_excluded = selection
            .excluded
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
                json!(selection.excluded.clone()),
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
            excluded_items: selection.excluded.clone(),
            metadata: output_metadata,
            ..ProcessorOutput::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{ContextMessage, ContextProcessor, SkillBudgetConfig};
    use serde_json::json;

    use super::SkillInjector;
    use super::test_support::{capabilities, session, skills};
    use super::tier1_metadata::{MANIFEST_FOOTER, MANIFEST_PREAMBLE};

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

    #[test]
    fn compute_budget_uses_context_window_percentage_or_default_floor() {
        let injector = SkillInjector::from_skills(Vec::new());

        assert_eq!(injector.compute_budget(200_000).max_manifest_chars, 8_000);
        assert_eq!(
            injector.compute_budget(1_200_000).max_manifest_chars,
            12_000
        );
    }
}
