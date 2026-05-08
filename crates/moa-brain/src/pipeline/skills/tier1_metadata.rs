//! Ranking, budgeting, and formatting for Tier 1 skill metadata.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use moa_core::{ExcludedItem, SkillMetadata};

use crate::pipeline::memory::extract_search_keywords;

pub(super) const MANIFEST_PREAMBLE: &str = "\
<available_skills>
When multiple skills apply, prefer the one whose trigger conditions most specifically match the current task.
Skills can be composed - use multiple if the task requires steps from different skills.
To activate a skill, follow the listed skill when it matches the task.

";
pub(super) const MANIFEST_FOOTER: &str = "</available_skills>";
pub(super) const DEFAULT_MIN_MANIFEST_CHARS: usize = 8_000;
pub(super) const DEFAULT_MANIFEST_WINDOW_RATIO: f64 = 0.01;

const MAX_SKILL_NAME_CHARS: usize = 64;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedSkillBudget {
    pub(super) max_manifest_chars: usize,
    pub(super) max_per_skill_chars: usize,
    pub(super) show_token_estimates: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RankedSkill {
    pub(super) metadata: SkillMetadata,
    score: f64,
    manifest_entry: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct SkillSelection {
    pub(super) selected: Vec<RankedSkill>,
    pub(super) excluded: Vec<ExcludedItem>,
    pub(super) chars_used: usize,
}

pub(super) fn rank_skills(
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

pub(super) fn select_skills_within_budget(
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

pub(super) fn format_skill_manifest(selected: &[RankedSkill]) -> String {
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

fn compare_ranked_skills(left: &RankedSkill, right: &RankedSkill) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| alphabetical_name_cmp(&left.metadata.name, &right.metadata.name))
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        DEFAULT_MIN_MANIFEST_CHARS, MANIFEST_FOOTER, MANIFEST_PREAMBLE, ResolvedSkillBudget,
        format_manifest_entry, format_skill_manifest, rank_skills, select_skills_within_budget,
    };
    use crate::pipeline::skills::test_support::{resolved_budget, test_skill};

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
