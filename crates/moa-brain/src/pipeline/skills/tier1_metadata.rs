//! Ranking, budgeting, and formatting for Tier 1 skill metadata.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use moa_artifacts::{document::ArtifactKind, reference::ArtifactRef};
use moa_core::{
    types::context::ExcludedItem, types::experience::TaskStrategySuccessRate,
    types::memory::SkillMetadata,
};

use crate::pipeline::memory::extract_search_keywords;

pub(super) const MANIFEST_PREAMBLE: &str = "\
<available_skills>
Use this compact manifest for skill selection. Activate a skill only when its description,
tags, or trigger conditions match the current task. When multiple skills apply, prefer the
most specific match; compose skills only when the task genuinely requires multiple workflows.
Do not invent skills not listed here. To activate a skill, read the exact file named in its
[activate: <path>] tag; do not guess a different path. Read it once — the content stays in your
context; do not re-read it during the turn.

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
    task_strategy_rates: &HashMap<String, TaskStrategySuccessRate>,
) -> Vec<RankedSkill> {
    let mut ranked = skills
        .iter()
        .cloned()
        .map(|metadata| {
            let keyword_overlap = keyword_overlap_score(query_keywords, &metadata);
            let manifest_entry = format_manifest_entry(&metadata, budget);
            let global_rate = resolution_rates
                .get(&metadata.name)
                .copied()
                .unwrap_or(0.5)
                .clamp(0.0, 1.0);
            let rate = task_strategy_rates.get(&metadata.name);
            let task_rate = rate.map(smoothed_task_rate).unwrap_or(0.0);
            let task_weight = rate.map(task_rate_weight).unwrap_or(0.0);
            let base_score = if task_weight > 0.0 {
                (0.45 * keyword_overlap) + (0.45 * task_rate * task_weight) + (0.10 * global_rate)
            } else if !task_strategy_rates.is_empty() {
                (0.60 * keyword_overlap) + (0.15 * global_rate)
            } else if resolution_rates.contains_key(&metadata.name) {
                (0.45 * keyword_overlap) + (0.55 * global_rate)
            } else {
                keyword_overlap
            };
            // A skill injected often under this fingerprint but rarely engaged is weak
            // negative-relevance evidence; subtract a small capped penalty so it can
            // demote a skill without ever overpowering keyword relevance.
            let penalty = rate.map(unused_injection_penalty).unwrap_or(0.0);
            let score = (base_score - penalty).max(0.0);

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

/// Weight of the attribution `effect_score` when blended with `success_rate`.
///
/// The two coincide except when a used skill's tool call failed (effect
/// downgraded) or the outcome was `Unknown`, so a minority weight lets the
/// effect signal nudge ranking without overriding the outcome rate.
const EFFECT_SCORE_WEIGHT: f64 = 0.25;

/// Maximum score subtracted for an all-unused-injection skill under a fingerprint.
///
/// Bounded well below the keyword-overlap term so a strongly relevant skill can
/// never be buried by injection history alone.
const UNUSED_INJECTION_PENALTY_CAP: f64 = 0.15;

/// Laplace-smoothed success rate blended with the attribution effect score.
///
/// The base rate blends the outcome-derived `success_rate` with the effect-derived
/// `effect_score` (which carries independent signal from failed skill engagements),
/// then applies add-one smoothing over `uses` so a low-evidence skill is pulled
/// toward the 0.5 prior.
fn smoothed_task_rate(rate: &TaskStrategySuccessRate) -> f64 {
    let base_rate = ((1.0 - EFFECT_SCORE_WEIGHT) * rate.success_rate.clamp(0.0, 1.0)
        + EFFECT_SCORE_WEIGHT * rate.effect_score.clamp(0.0, 1.0))
    .clamp(0.0, 1.0);
    let successes = base_rate * rate.uses as f64;
    ((1.0 + successes) / (2.0 + rate.uses as f64)).clamp(0.0, 1.0)
}

fn task_rate_weight(rate: &TaskStrategySuccessRate) -> f64 {
    let sample_weight = (rate.uses as f64 / 5.0).clamp(0.0, 1.0);
    sample_weight * rate.avg_confidence.clamp(0.0, 1.0)
}

/// Capped penalty for skills injected but rarely engaged under the fingerprint.
///
/// The penalty scales with the fraction of this subject's rows that are
/// unused injections and is capped at [`UNUSED_INJECTION_PENALTY_CAP`]. Returns
/// zero when the subject has no attribution rows at all.
fn unused_injection_penalty(rate: &TaskStrategySuccessRate) -> f64 {
    let total = rate.uses + rate.unused_injections;
    if total == 0 {
        return 0.0;
    }
    let unused_ratio = rate.unused_injections as f64 / total as f64;
    (UNUSED_INJECTION_PENALTY_CAP * unused_ratio).clamp(0.0, UNUSED_INJECTION_PENALTY_CAP)
}

#[cfg(test)]
pub(super) fn select_skills_within_budget(
    ranked: &[RankedSkill],
    max_manifest_chars: usize,
) -> SkillSelection {
    select_skills_within_budget_and_limit(ranked, max_manifest_chars, None, &[])
}

pub(super) fn select_skills_within_budget_and_limit(
    ranked: &[RankedSkill],
    max_manifest_chars: usize,
    max_selected: Option<usize>,
    pinned_names: &[String],
) -> SkillSelection {
    let mut selected = Vec::new();
    let mut selected_names = HashSet::new();
    let mut chars_used = MANIFEST_PREAMBLE.chars().count() + MANIFEST_FOOTER.chars().count();

    for pinned_name in pinned_names {
        if max_selected.is_some_and(|limit| selected.len() >= limit) {
            break;
        }
        let Some(skill) = ranked
            .iter()
            .find(|skill| skill.metadata.name == *pinned_name)
        else {
            continue;
        };
        if selected_names.contains(&skill.metadata.name) {
            continue;
        }
        let entry_cost = skill.manifest_entry.chars().count() + 1;
        if chars_used + entry_cost > max_manifest_chars {
            break;
        }

        chars_used += entry_cost;
        selected_names.insert(skill.metadata.name.clone());
        selected.push(skill.clone());
    }

    for skill in ranked {
        if max_selected.is_some_and(|limit| selected.len() >= limit) {
            break;
        }
        if selected_names.contains(&skill.metadata.name) {
            continue;
        }
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
            reason: if max_selected.is_some_and(|limit| selected_names.len() >= limit) {
                "excluded by agent skill policy max_visible after relevance ranking".to_string()
            } else {
                "excluded by manifest budget after relevance ranking".to_string()
            },
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
    let name =
        crate::text::truncate_chars(&normalize_inline_text(&metadata.name), MAX_SKILL_NAME_CHARS)
            .into_owned();
    let description = crate::text::truncate_chars(
        &normalize_inline_text(&metadata.description),
        MAX_SKILL_DESCRIPTION_CHARS,
    )
    .into_owned();
    let tags = normalized_tags(&metadata.tags);
    let tags = if tags.is_empty() {
        "none".to_string()
    } else {
        tags.join(", ")
    };

    // The activation path is the exact materialized package file the model must
    // read to load the skill (`.moa/skills/<slug>/SKILL.md`). It comes straight
    // from `SkillMetadata::path`, which the skill materializer already slugified,
    // so the manifest never forks a second slug convention or lets the model guess.
    let activate = normalize_inline_text(&metadata.path);
    let mut entry = format!("- {name}: {description} [activate: {activate}] [tags: {tags}]");
    let actions = normalized_action_names(&metadata.actions);
    if !actions.is_empty() {
        entry.push_str(&format!(" [actions: {}]", actions.join(", ")));
    }
    if metadata.has_execution_plan
        && let Some(revision_uid) = metadata.artifact_revision_uid
    {
        let artifact_ref =
            ArtifactRef::artifact(ArtifactKind::Skill, metadata.name.clone()).to_string();
        entry.push_str(&format!(
            " [execution-plan: ref={artifact_ref}, revision_uid={revision_uid}]"
        ));
    }
    if budget.show_token_estimates {
        entry.push_str(&format!(" (est. {} tok)", metadata.estimated_tokens));
    }

    crate::text::truncate_chars(&entry, budget.max_per_skill_chars).into_owned()
}

fn normalized_tags(tags: &[String]) -> Vec<String> {
    normalized_inline_values(tags)
}

fn normalized_action_names(actions: &[String]) -> Vec<String> {
    normalized_inline_values(actions)
}

fn normalized_inline_values(values: &[String]) -> Vec<String> {
    let mut normalized = values
        .iter()
        .map(|value| normalize_inline_text(value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    normalized.sort_by(|left, right| alphabetical_name_cmp(left, right));
    normalized.dedup();
    normalized
}

fn keyword_overlap_score(query_keywords: &[String], metadata: &SkillMetadata) -> f64 {
    if query_keywords.is_empty() {
        return 0.0;
    }

    let haystack = format!(
        "{} {} {} {}",
        metadata.name,
        metadata.description,
        metadata.tags.join(" "),
        metadata.actions.join(" ")
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use moa_core::{
        types::experience::AttributionSubjectType, types::experience::TaskStrategySuccessRate,
        types::identifiers::TenantId,
    };

    use super::{
        DEFAULT_MIN_MANIFEST_CHARS, MANIFEST_FOOTER, MANIFEST_PREAMBLE, ResolvedSkillBudget,
        format_manifest_entry, format_skill_manifest, rank_skills, select_skills_within_budget,
    };
    use crate::pipeline::skills::test_support::{
        resolved_budget, test_skill, test_skill_with_execution_plan,
    };

    #[test]
    fn rank_skills_pins_all_four_scoring_branches_exactly() {
        // Pins: the four score formulas selected by which rate maps are populated. Empty
        // query keywords zero the keyword term so each branch's remaining terms are exact:
        //   1. task-conditioned:  0.45*kw + 0.45*smoothed*weight + 0.10*global
        //   2. task data exists for others only: 0.60*kw + 0.15*global
        //   3. tenant resolution rate only:      0.45*kw + 0.55*global
        //   4. no outcome data at all:           kw
        // The smoothed task rate blends success_rate with effect_score at
        // EFFECT_SCORE_WEIGHT before add-one smoothing.
        let skill = test_skill("branch-skill", "Branch formula pin");
        let skills = vec![skill];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let task_rate = TaskStrategySuccessRate {
            tenant_id: TenantId::new(),
            task_fingerprint: "task-hash".to_string(),
            subject_type: AttributionSubjectType::Skill,
            subject_id: "branch-skill".to_string(),
            uses: 10,
            success_rate: 0.8,
            avg_confidence: 1.0,
            // Distinct from success_rate so the blend is exercised, not a no-op.
            effect_score: 0.4,
            unused_injections: 0,
        };

        // Branch 1: base_rate = 0.75*0.8 + 0.25*0.4 = 0.7; smoothed = (1 + 0.7*10)/(2 + 10)
        // = 8/12; weight = min(10/5,1)*1.0 = 1.0; global defaults to the 0.5 prior when the
        // skill has no resolution row; no unused injections so the penalty is zero.
        let task_rates = HashMap::from([("branch-skill".to_string(), task_rate.clone())]);
        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new(), &task_rates);
        assert!(
            (ranked[0].score - (0.45 * (8.0 / 12.0) + 0.10 * 0.5)).abs() < 1e-9,
            "task-conditioned branch: {}",
            ranked[0].score
        );

        // Branch 2: task data exists for a different skill, so this skill takes the
        // keyword+global fallback with the 0.5 unrated prior.
        let other_rates = HashMap::from([(
            "someone-else".to_string(),
            TaskStrategySuccessRate {
                subject_id: "someone-else".to_string(),
                ..task_rate
            },
        )]);
        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new(), &other_rates);
        assert!(
            (ranked[0].score - 0.15 * 0.5).abs() < 1e-9,
            "task-data-elsewhere branch: {}",
            ranked[0].score
        );

        // Branch 3: no task-conditioned data at all; a tenant resolution rate blends in.
        let resolution = HashMap::from([("branch-skill".to_string(), 0.9)]);
        let ranked = rank_skills(&skills, &[], &budget, &resolution, &HashMap::new());
        assert!(
            (ranked[0].score - 0.55 * 0.9).abs() < 1e-9,
            "resolution-rate branch: {}",
            ranked[0].score
        );

        // Branch 4: no outcome data anywhere falls back to pure keyword overlap.
        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new(), &HashMap::new());
        assert_eq!(ranked[0].score, 0.0, "keyword-only branch");
    }

    #[test]
    fn low_effect_score_demotes_a_skill_with_a_high_success_rate() {
        // Pins: effect_score carries signal beyond success_rate. Two skills with the same
        // success_rate/uses/confidence but different effect_score rank by effect_score,
        // which diverges from the outcome rate when a used skill's tool call failed.
        let skills = vec![
            test_skill("clean-skill", "General workflow"),
            test_skill("erroring-skill", "General workflow"),
        ];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let base = TaskStrategySuccessRate {
            tenant_id: TenantId::new(),
            task_fingerprint: "task-hash".to_string(),
            subject_type: AttributionSubjectType::Skill,
            subject_id: String::new(),
            uses: 5,
            success_rate: 1.0,
            avg_confidence: 1.0,
            effect_score: 1.0,
            unused_injections: 0,
        };
        let task_rates = HashMap::from([
            (
                "clean-skill".to_string(),
                TaskStrategySuccessRate {
                    subject_id: "clean-skill".to_string(),
                    effect_score: 1.0,
                    ..base.clone()
                },
            ),
            (
                "erroring-skill".to_string(),
                TaskStrategySuccessRate {
                    subject_id: "erroring-skill".to_string(),
                    effect_score: 0.0,
                    ..base
                },
            ),
        ]);

        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new(), &task_rates);
        assert_eq!(ranked[0].metadata.name, "clean-skill");
        assert_eq!(ranked[1].metadata.name, "erroring-skill");
    }

    #[test]
    fn unused_injection_ratio_applies_a_bounded_penalty() {
        // Pins: a skill injected but half the time never engaged loses a penalty equal to
        // UNUSED_INJECTION_PENALTY_CAP * unused_ratio, subtracted after the branch score.
        // Branch 1 base: smoothed = (1 + (0.75*1 + 0.25*1)*2)/(2 + 2) = 0.75;
        // weight = min(2/5,1)*1.0 = 0.4; global default 0.5;
        // base = 0.45*0 + 0.45*0.75*0.4 + 0.10*0.5 = 0.185; penalty = 0.15*0.5 = 0.075.
        let skills = vec![test_skill("branch-skill", "Branch formula pin")];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let task_rates = HashMap::from([(
            "branch-skill".to_string(),
            TaskStrategySuccessRate {
                tenant_id: TenantId::new(),
                task_fingerprint: "task-hash".to_string(),
                subject_type: AttributionSubjectType::Skill,
                subject_id: "branch-skill".to_string(),
                uses: 2,
                success_rate: 1.0,
                avg_confidence: 1.0,
                effect_score: 1.0,
                unused_injections: 2,
            },
        )]);

        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new(), &task_rates);
        assert!(
            (ranked[0].score - (0.45 * 0.75 * 0.4 + 0.10 * 0.5 - 0.15 * 0.5)).abs() < 1e-9,
            "penalized score: {}",
            ranked[0].score
        );
    }

    #[test]
    fn selects_top_ranked_skills_then_resorts_emission_alphabetically() {
        let skills = (0..30)
            .map(|index| {
                test_skill(
                    &format!("skill-{index:02}"),
                    &format!("Workflow number {index:02}"),
                )
            })
            .collect::<Vec<_>>();
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new(), &HashMap::new());
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
        let skill = test_skill("very-long-skill", &"x".repeat(4_000));
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
    fn manifest_entry_includes_actions_only_when_present() {
        // Pins: artifact-backed skill actions are visible in the compact skill manifest.
        let mut skill = test_skill("refund-helper", "Refund workflow");
        let budget = ResolvedSkillBudget {
            max_manifest_chars: DEFAULT_MIN_MANIFEST_CHARS,
            max_per_skill_chars: 512,
            show_token_estimates: false,
        };

        let without_actions = format_manifest_entry(&skill, &budget);
        skill.actions = vec!["issue_refund".to_string(), "lookup_order".to_string()];
        let with_actions = format_manifest_entry(&skill, &budget);

        assert!(!without_actions.contains("[actions:"));
        assert!(with_actions.contains("[actions: issue_refund, lookup_order]"));
    }

    #[test]
    fn manifest_entry_includes_the_exact_materialized_activation_path() {
        // Pins: the compact manifest carries each skill's exact activation file so the
        // model reads `.moa/skills/<slug>/SKILL.md` instead of guessing a bare
        // `.moa/skills/<name>.md` (the live S085/S090 tool-loop failure). The rendered
        // path is `SkillMetadata::path` verbatim, i.e. the materializer's slug, and a
        // name with spaces/uppercase still shows the slugified directory.
        let mut skill = test_skill("Memory Privacy Check", "Check stored facts for PII");
        skill.path = ".moa/skills/memory-privacy-check/SKILL.md".to_string();
        let budget = ResolvedSkillBudget {
            max_manifest_chars: DEFAULT_MIN_MANIFEST_CHARS,
            max_per_skill_chars: 512,
            show_token_estimates: false,
        };

        let entry = format_manifest_entry(&skill, &budget);

        assert!(
            entry.contains("[activate: .moa/skills/memory-privacy-check/SKILL.md]"),
            "entry must name the exact activation path: {entry}"
        );
        assert!(
            !entry.contains(".moa/skills/memory-privacy-check.md"),
            "entry must not present the bare guessed path: {entry}"
        );
    }

    #[test]
    fn manifest_entry_marks_execution_plan_with_exact_ref_and_revision() {
        // Pins: an optional execution template is identified by its canonical skill
        // artifact ref and exact immutable revision; instruction-only skills stay valid.
        let budget = ResolvedSkillBudget {
            max_manifest_chars: DEFAULT_MIN_MANIFEST_CHARS,
            max_per_skill_chars: 512,
            show_token_estimates: false,
        };

        let without =
            format_manifest_entry(&test_skill("agentic", "Agent-mediated skill"), &budget);
        let with = format_manifest_entry(
            &test_skill_with_execution_plan("refund", "Refund execution plan"),
            &budget,
        );

        assert!(!without.contains("[execution-plan:"));
        assert!(with.contains(
            "[execution-plan: ref=skill://refund, revision_uid=00000000-0000-0000-0000-000000000001]"
        ));
    }

    #[test]
    fn selection_reports_excluded_items_with_reasons() {
        let skills = vec![
            test_skill("alpha", "Alpha workflow"),
            test_skill("beta", "Beta workflow"),
            test_skill("gamma", "Gamma workflow"),
        ];
        // Size the budget to fit exactly one ranked entry (plus its newline) so the
        // remaining two are excluded, independent of the exact entry length.
        let sizing_budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let sized = rank_skills(
            &skills,
            &[],
            &sizing_budget,
            &HashMap::new(),
            &HashMap::new(),
        );
        let one_entry_cost = sized[0].manifest_entry.chars().count() + 1;
        let budget = resolved_budget(
            MANIFEST_PREAMBLE.chars().count() + MANIFEST_FOOTER.chars().count() + one_entry_cost,
        );
        let ranked = rank_skills(&skills, &[], &budget, &HashMap::new(), &HashMap::new());
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
            test_skill("alpha-auth", "Handle auth failures"),
            test_skill("beta-db", "Handle database failures"),
        ];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);

        let ranked = rank_skills(
            &skills,
            &["auth".to_string()],
            &budget,
            &HashMap::new(),
            &HashMap::new(),
        );

        assert_eq!(ranked[0].metadata.name, "alpha-auth");
        assert_eq!(ranked[1].metadata.name, "beta-db");
    }

    #[test]
    fn ranking_uses_resolution_rate_when_available() {
        let skills = vec![
            test_skill("high-use", "General workflow"),
            test_skill("high-resolution", "General workflow"),
        ];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let resolution_rates = HashMap::from([
            ("high-use".to_string(), 0.0),
            ("high-resolution".to_string(), 1.0),
        ]);

        let ranked = rank_skills(&skills, &[], &budget, &resolution_rates, &HashMap::new());

        assert_eq!(ranked[0].metadata.name, "high-resolution");
    }

    #[test]
    fn task_conditioned_skill_ranking_can_beat_higher_global_rate_with_enough_evidence() {
        // Pins: task-specific skill success can outrank a globally better skill when evidence is strong.
        let skills = vec![
            test_skill("global-winner", "Rust auth workflow"),
            test_skill("task-winner", "Rust auth workflow"),
        ];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let resolution_rates = HashMap::from([
            ("global-winner".to_string(), 0.95),
            ("task-winner".to_string(), 0.40),
        ]);
        let task_rates = HashMap::from([(
            "task-winner".to_string(),
            TaskStrategySuccessRate {
                tenant_id: TenantId::new(),
                task_fingerprint: "task-hash".to_string(),
                subject_type: AttributionSubjectType::Skill,
                subject_id: "task-winner".to_string(),
                uses: 8,
                success_rate: 1.0,
                avg_confidence: 0.95,
                effect_score: 1.0,
                unused_injections: 0,
            },
        )]);

        let ranked = rank_skills(
            &skills,
            &["rust".to_string(), "auth".to_string()],
            &budget,
            &resolution_rates,
            &task_rates,
        );

        assert_eq!(ranked[0].metadata.name, "task-winner");
        assert_eq!(ranked[1].metadata.name, "global-winner");
    }

    #[test]
    fn emitted_manifest_entries_are_alphabetical_even_when_ranked_input_is_not() {
        let skills = vec![
            test_skill("zeta", "Zeta workflow"),
            test_skill("alpha", "Alpha workflow"),
        ];
        let budget = resolved_budget(DEFAULT_MIN_MANIFEST_CHARS);
        let resolution_rates = HashMap::from([("zeta".to_string(), 1.0)]);
        let ranked = rank_skills(&skills, &[], &budget, &resolution_rates, &HashMap::new());
        assert_eq!(ranked[0].metadata.name, "zeta");
        assert_eq!(ranked[1].metadata.name, "alpha");

        let selection = select_skills_within_budget(&ranked, budget.max_manifest_chars);
        let manifest = format_skill_manifest(&selection.selected);

        assert!(
            manifest.find("- alpha:").expect("alpha") < manifest.find("- zeta:").expect("zeta")
        );
    }

    #[test]
    fn manifest_preamble_pins_activation_boundary() {
        // Pins: the compact skill manifest tells the model when to activate listed skills.
        assert!(MANIFEST_PREAMBLE.contains("Activate a skill only when"));
        assert!(MANIFEST_PREAMBLE.contains("most specific match"));
        assert!(MANIFEST_PREAMBLE.contains("Do not invent skills not listed here"));
        // Pins: the preamble tells the model to read the exact [activate: <path>] file
        // rather than guess a path, complementing the per-entry activation path.
        assert!(MANIFEST_PREAMBLE.contains("[activate: <path>]"));
        assert!(MANIFEST_PREAMBLE.contains("do not guess a different path"));
        // Pins: the preamble tells the model an activated skill's content persists in
        // context, so it does not re-read the same file during the turn.
        assert!(MANIFEST_PREAMBLE.contains("do not re-read it during the turn"));
    }
}
