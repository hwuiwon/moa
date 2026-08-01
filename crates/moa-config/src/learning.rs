//! Automated learning configuration.

use serde::{Deserialize, Serialize};

/// Runtime learning-loop configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LearningConfig {
    /// Skill draft proposal generation controls.
    pub skills: SkillLearningConfig,
    /// Deterministic task-segment boundary controls.
    pub segments: SegmentBoundaryConfig,
    /// Post-promotion skill regression monitor controls.
    pub regression_monitor: RegressionMonitorConfig,
    /// Background learning-embedding backfill controls.
    pub embeddings: EmbeddingBackfillConfig,
    /// Exact-fingerprint recurrence-mining controls.
    pub recurrence: RecurrenceConfig,
}

/// Skill self-learning proposal generation configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillLearningConfig {
    /// Minimum tool-call count a segment must contain before it is eligible for
    /// skill distillation.
    ///
    /// This is a cheap pre-LLM filter: a segment shorter than this cannot hold a
    /// reusable multi-step procedure worth distilling, so it is rejected before
    /// any paid distillation call. Set high enough to exclude trivial
    /// three-to-five-call tasks.
    pub min_tool_calls: usize,
    /// Cosine-similarity floor at which filing-time routing sends a distilled
    /// experience to *improvement* of the nearest existing skill instead of
    /// creating a new one.
    ///
    /// Compared against the semantic similarity of the experience's task-summary
    /// embedding to the nearest serving skill-identity embedding. Because
    /// pgvector's `<=>` operator returns cosine *distance* `d = 1 - cosine_sim`,
    /// this similarity `s` maps to the distance ceiling `1 - s`: a neighbor at
    /// distance `<= 1 - s` clears the floor. This is the primary improve-vs-create
    /// signal; the lexical Jaccard fallback is consulted only when no embedding is
    /// available (provider down, or the skill has no embedding yet).
    pub improve_route_similarity: f64,
    /// Cosine-similarity floor at which a new distilled experience is treated as a
    /// duplicate of an *open* proposal and accumulated as a sibling rather than
    /// filed as its own near-duplicate draft.
    ///
    /// Compared against the semantic similarity between the experience's
    /// task-summary embedding and the source-experience embedding behind each open
    /// `Proposed` skill candidate. Same distance mapping as
    /// [`Self::improve_route_similarity`]: similarity `s` clears at distance
    /// `<= 1 - s`. Set high so only genuinely-duplicate work dedupes.
    pub proposal_dedup_similarity: f64,
}

impl Default for SkillLearningConfig {
    fn default() -> Self {
        Self {
            min_tool_calls: 8,
            improve_route_similarity: 0.80,
            proposal_dedup_similarity: 0.85,
        }
    }
}

/// Deterministic task-segment boundary fallback configuration.
///
/// Used when the query-rewrite LLM produced no explicit task-boundary signal
/// (the rewrite gate skipped, rewriting is disabled, or a fallback path stored
/// the original query). In that case the segment tracker decides boundaries
/// deterministically, and this configures the idle-gap threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SegmentBoundaryConfig {
    /// Idle gap, in minutes, between the previous session event and a new user
    /// message that starts a new task segment when no LLM boundary signal is
    /// present. A long pause is treated as a task boundary.
    pub idle_gap_minutes: u64,
}

impl Default for SegmentBoundaryConfig {
    fn default() -> Self {
        Self {
            idle_gap_minutes: 30,
        }
    }
}

/// Post-promotion skill regression monitor configuration.
///
/// Drives the background monitor that compares each recently promoted skill's
/// post-promotion resolution rate against a baseline and files a rollback
/// proposal when the skill regressed. All thresholds are tenant-agnostic; the
/// monitor is deterministic given the same segment history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RegressionMonitorConfig {
    /// Minimum number of post-promotion segments that actually used a skill
    /// before its resolution rate is judged. Below this the skill has too little
    /// evidence and the monitor abstains, avoiding a rollback proposal on noise.
    pub min_samples: usize,
    /// Lookback window, in days, over which recent promotions are monitored. Also
    /// bounds the pre-promotion baseline window for improved skills.
    pub lookback_days: i64,
    /// Regression margin for an improved skill: the skill regressed when its
    /// post-promotion resolution rate falls below its pre-promotion baseline by
    /// more than this delta. Guards against re-filing on ordinary noise.
    pub regression_delta: f64,
    /// Absolute resolution-rate floor for a created skill, which has no
    /// pre-promotion history. A created skill regressed when its post-promotion
    /// rate falls below this floor.
    pub created_floor: f64,
}

impl Default for RegressionMonitorConfig {
    fn default() -> Self {
        Self {
            min_samples: 5,
            lookback_days: 14,
            regression_delta: 0.2,
            created_floor: 0.3,
        }
    }
}

/// Background learning-embedding backfill configuration.
///
/// Drives the cron that populates task-summary embeddings on `experience_records`
/// and identity embeddings on serving Skill artifacts. Embeddings are computed
/// out-of-band (never on the turn or persist path), so they lag writes by up to
/// one cron tick; every knob here bounds per-tick provider cost. Provider
/// unavailability leaves rows NULL for the next tick rather than failing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingBackfillConfig {
    /// Maximum number of `experience_records` embedded per tick. Caps the number
    /// of task summaries sent to the embedding provider in one pass so a large
    /// backlog drains over several ticks instead of one oversized burst.
    pub experience_batch_size: usize,
    /// Only `experience_records` created within this many days are eligible for
    /// backfill. Bounds the working set to recent recurrence-relevant rows;
    /// older un-embedded rows are intentionally left NULL.
    pub experience_lookback_days: i64,
    /// Maximum number of serving Skill artifacts embedded per tick.
    pub skill_batch_size: usize,
}

impl Default for EmbeddingBackfillConfig {
    fn default() -> Self {
        Self {
            experience_batch_size: 128,
            experience_lookback_days: 30,
            skill_batch_size: 64,
        }
    }
}

/// Exact-fingerprint recurrence-mining configuration.
///
/// Drives the background cron that treats recurrence itself as skill-learning
/// evidence: a task fingerprint seen enough times across sessions dispatches
/// distillation even when each individual session fell below the single-session
/// dispatch gate. All thresholds are tenant-agnostic; the cron is deterministic
/// given the same experience and candidate history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecurrenceConfig {
    /// Minimum number of resolved/partial experiences sharing one task
    /// fingerprint, within the lookback window, before recurrence dispatches
    /// distillation. N-fold recurrence stands in for the per-session evidence
    /// bar the single-session gate enforces.
    pub min_occurrences: usize,
    /// Lookback window, in days, over which recurring experiences are grouped.
    /// Older experiences are outside the recurrence ledger.
    pub lookback_days: i64,
    /// Relaxed per-session tool-call floor applied to the recurrence exemplar.
    /// Recurrence replaces the evidence the standard `skills.min_tool_calls`
    /// floor stood in for, so the exemplar only needs this many tool calls.
    pub relaxed_min_tool_calls: usize,
    /// Suppression window, in days, after a reviewer rejects a fingerprint's
    /// candidate. A fingerprint rejected within this window is not re-dispatched,
    /// so recurring work a reviewer already declined cannot spam the queue.
    pub rejection_cooldown_days: i64,
    /// Cosine-similarity threshold at which two exact-fingerprint groups are
    /// merged into one semantic recurrence cluster ("same loop, different
    /// wording").
    ///
    /// Applied after exact-fingerprint grouping: two groups merge when a
    /// representative task-summary embedding of one is within this cosine
    /// similarity of the other. Because pgvector's `<=>` operator returns cosine
    /// *distance* `d = 1 - cosine_sim`, a similarity `s` merges groups whose
    /// representatives sit at distance `<= 1 - s`. Members without an embedding
    /// (NULL = not yet embedded) stay in their exact-fingerprint group, so
    /// clustering only ever widens what pools and degrades to exact grouping when
    /// embeddings are absent. Set high so only genuinely-equivalent tasks merge.
    pub cluster_similarity: f64,
    /// Upper bound on the number of exact-fingerprint groups loaded per tenant per
    /// tick as candidates for semantic clustering.
    ///
    /// Occurrence-threshold gating happens *after* clustering (so sub-threshold
    /// aliases that merge into a qualifying cluster are not discarded first), which
    /// means the store must load every group down to a single occurrence. This
    /// bound keeps that load and the per-group neighbor probing cost finite: the
    /// most recently active groups (by latest member time) are loaded first, up to
    /// this many, and older groups fall outside the tick. A cut group only fails to
    /// merge this tick; a later tick with fresh activity re-includes it. Also caps
    /// the per-representative neighbor breadth, which tracks the loaded members.
    pub max_candidate_groups: usize,
}

impl Default for RecurrenceConfig {
    fn default() -> Self {
        Self {
            min_occurrences: 3,
            lookback_days: 30,
            relaxed_min_tool_calls: 3,
            rejection_cooldown_days: 30,
            cluster_similarity: 0.85,
            max_candidate_groups: 200,
        }
    }
}
