//! Exact-fingerprint recurrence qualification for skill learning.
//!
//! The single-session dispatch gate only generates a skill when one session
//! clears it (learnable outcome, enough tool calls). A task that recurs across
//! many sessions where each instance individually falls below that gate never
//! generates anything — recurrence itself is not treated as evidence. This module
//! is the pure, model-free core that closes that gap: given a fingerprint's
//! resolved/partial experiences (grouped in the store) and its skill-candidate
//! decision history, it decides whether the recurrence qualifies for dispatch,
//! picks the exemplar to distill, and orders the remaining cluster members as
//! siblings. It mirrors the rollback monitor's split — pure logic here,
//! store I/O in `moa-session`, cron wiring in the orchestrator.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use moa_config::RecurrenceConfig;
use moa_core::types::experience::LearningCandidateStatus;
use moa_core::types::segment_assessment::SegmentOutcome;
use moa_session::{
    ExperienceEmbeddingNeighbor, RecurrenceExperienceMember, RecurringExperienceCluster,
    SkillCandidateDecision,
};

use crate::semantic::similarity_to_max_distance;

/// Confidence floor (per-mille) an exemplar resolved experience must meet.
const RESOLVED_EXEMPLAR_CONFIDENCE_MILLI: i64 = 700;
/// Confidence floor (per-mille) an exemplar partial experience must meet.
const PARTIAL_EXEMPLAR_CONFIDENCE_MILLI: i64 = 850;

/// Thresholds governing recurrence qualification, projected from configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecurrenceThresholds {
    /// Minimum resolved/partial occurrences before recurrence dispatches.
    pub min_occurrences: usize,
    /// Lookback window in days (bounds the store grouping and rides the evidence).
    pub lookback_days: i64,
    /// Relaxed per-session tool-call floor applied to the exemplar at dispatch.
    pub relaxed_min_tool_calls: usize,
    /// Suppression window in days after a reviewer rejects a fingerprint.
    pub rejection_cooldown_days: i64,
}

impl RecurrenceThresholds {
    /// Projects qualification thresholds from the runtime recurrence configuration.
    #[must_use]
    pub fn from_config(config: &RecurrenceConfig) -> Self {
        Self {
            min_occurrences: config.min_occurrences,
            lookback_days: config.lookback_days,
            relaxed_min_tool_calls: config.relaxed_min_tool_calls,
            rejection_cooldown_days: config.rejection_cooldown_days,
        }
    }
}

/// A semantic recurrence cluster: one or more exact-fingerprint groups whose task
/// summaries are close enough to be the same recurring work.
///
/// Produced by [`cluster_recurrence_groups`]. A cluster with a single member
/// fingerprint is the graceful-degradation case: either embeddings were absent or
/// no other group was within the similarity threshold, so it behaves exactly like
/// the exact-fingerprint grouping did before R2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedRecurrenceCluster {
    /// Canonical fingerprint hash for the cluster: the lexicographically smallest
    /// of the merged fingerprints, so the identity is order-independent.
    pub fingerprint_hash: String,
    /// Every exact-fingerprint hash merged into this cluster, sorted and deduped.
    /// A reviewer sees these on the dispatched proposal's recurrence evidence.
    pub merged_fingerprints: Vec<String>,
    /// Union of all member experiences across the merged groups, ordered by
    /// creation time then id.
    pub members: Vec<RecurrenceExperienceMember>,
}

impl MergedRecurrenceCluster {
    /// Builds a single-fingerprint cluster from one exact-fingerprint group.
    ///
    /// This is the identity/degradation form used when a group merges with no
    /// other group (or embeddings are unavailable), and the constructor tests use
    /// to exercise qualification independently of clustering.
    #[must_use]
    pub fn single(group: &RecurringExperienceCluster) -> Self {
        Self {
            fingerprint_hash: group.fingerprint_hash.clone(),
            merged_fingerprints: vec![group.fingerprint_hash.clone()],
            members: group.members.clone(),
        }
    }
}

/// Merges exact-fingerprint groups whose task summaries are semantically close.
///
/// `neighbor_lists` is index-aligned with `groups`: entry `i` is the nearest
/// task-embedding neighbors of group `i`'s representative experience (the store
/// probes one representative per group), or `None` when that representative has no
/// embedding yet. Two groups merge when a representative's neighbor list contains
/// an experience belonging to the other group within the cosine distance ceiling
/// derived from `cluster_similarity`. Merging is transitive (union-find), so a
/// chain of pairwise-close groups collapses into one cluster.
///
/// The result is deterministic given the inputs: the neighbor lists are
/// distance-then-id ordered by the store, the merge partition does not depend on
/// scan order, each cluster's canonical hash is the smallest merged fingerprint,
/// members are sorted by `(created_at, experience_id)`, and clusters are returned
/// sorted by canonical hash. A group whose representative is unembedded (`None`)
/// contributes no merges and stays on its own — the NULL-degrades-to-exact
/// contract — but can still be pulled into a cluster if *another* group's
/// representative names one of its members as a close neighbor.
#[must_use]
pub fn cluster_recurrence_groups(
    groups: &[RecurringExperienceCluster],
    neighbor_lists: &[Option<Vec<ExperienceEmbeddingNeighbor>>],
    cluster_similarity: f64,
) -> Vec<MergedRecurrenceCluster> {
    debug_assert_eq!(
        groups.len(),
        neighbor_lists.len(),
        "neighbor lists must be index-aligned with groups"
    );
    let ceiling = similarity_to_max_distance(cluster_similarity);

    // Map every grouped experience to the group index that owns it, so a neighbor
    // id can be resolved to its group. An experience belongs to exactly one exact
    // fingerprint group, so this map is unambiguous.
    let mut experience_to_group: HashMap<uuid::Uuid, usize> = HashMap::new();
    for (index, group) in groups.iter().enumerate() {
        for member in &group.members {
            experience_to_group.insert(member.experience_id, index);
        }
    }

    let mut parents: Vec<usize> = (0..groups.len()).collect();
    for (index, neighbors) in neighbor_lists.iter().enumerate() {
        let Some(neighbors) = neighbors else {
            continue;
        };
        for neighbor in neighbors {
            if neighbor.distance > ceiling {
                continue;
            }
            if let Some(&other) = experience_to_group.get(&neighbor.id)
                && other != index
            {
                union(&mut parents, index, other);
            }
        }
    }

    // Collect merged group indices by their union-find root.
    let mut by_root: HashMap<usize, Vec<usize>> = HashMap::new();
    for index in 0..groups.len() {
        let root = find(&mut parents, index);
        by_root.entry(root).or_default().push(index);
    }

    let mut clusters: Vec<MergedRecurrenceCluster> = by_root
        .into_values()
        .map(|group_indices| {
            let mut merged_fingerprints: Vec<String> = group_indices
                .iter()
                .map(|&i| groups[i].fingerprint_hash.clone())
                .collect();
            merged_fingerprints.sort();
            merged_fingerprints.dedup();
            let fingerprint_hash = merged_fingerprints.first().cloned().unwrap_or_default();
            let mut members: Vec<RecurrenceExperienceMember> = group_indices
                .iter()
                .flat_map(|&i| groups[i].members.iter().cloned())
                .collect();
            members.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.experience_id.cmp(&b.experience_id))
            });
            MergedRecurrenceCluster {
                fingerprint_hash,
                merged_fingerprints,
                members,
            }
        })
        .collect();
    clusters.sort_by(|a, b| a.fingerprint_hash.cmp(&b.fingerprint_hash));
    clusters
}

/// Union-find `find` with path halving over group indices.
fn find(parents: &mut [usize], mut node: usize) -> usize {
    while parents[node] != node {
        parents[node] = parents[parents[node]];
        node = parents[node];
    }
    node
}

/// Union-find `union` that always roots the merged set at the smaller index, so
/// the resulting partition is independent of union order.
fn union(parents: &mut [usize], left: usize, right: usize) {
    let left_root = find(parents, left);
    let right_root = find(parents, right);
    if left_root == right_root {
        return;
    }
    let (keep, drop) = (left_root.min(right_root), left_root.max(right_root));
    parents[drop] = keep;
}

/// A qualified recurrence ready to dispatch distillation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceDispatchPlan {
    /// Canonical task-fingerprint hash for the cluster.
    pub fingerprint_hash: String,
    /// Every exact fingerprint merged into this cluster, for reviewer evidence.
    pub merged_fingerprints: Vec<String>,
    /// Best exemplar to distill from (highest confidence, then tool count, then recency).
    pub exemplar: RecurrenceExperienceMember,
    /// Remaining cluster members ordered by recency, capped at the sibling cap.
    pub siblings: Vec<RecurrenceExperienceMember>,
    /// Total resolved/partial occurrences observed in the window.
    pub occurrences: usize,
    /// Earliest member creation time in the cluster.
    pub first_seen: DateTime<Utc>,
    /// Latest member creation time in the cluster.
    pub last_seen: DateTime<Utc>,
}

/// Decides whether a recurring cluster qualifies for dispatch.
///
/// Abstains (returns `None`) unless the cluster has at least
/// `thresholds.min_occurrences` members, has no open (`Proposed`/`Evaluating`)
/// skill candidate, has no `Promoted` candidate (the improve path owns
/// evolution), and has no `Rejected` candidate whose decision is still inside the
/// rejection cooldown. Suppression is evaluated per cluster: `decisions` must be
/// the union of the candidate history across *every* merged fingerprint, so any
/// one member fingerprint with an open/promoted/cooldown candidate suppresses the
/// whole cluster. When it qualifies, the exemplar is the learnable-eligible
/// member with the highest confidence, breaking ties by distinct-tool count then
/// recency then id, and the siblings are every other member ordered by recency
/// and capped at [`MAX_RECURRENCE_SIBLINGS`]. A cluster whose members are all
/// below the exemplar confidence floor never dispatches, so a fingerprint that
/// only ever half-succeeds does not spin every tick.
#[must_use]
pub fn qualify_recurrence_cluster(
    cluster: &MergedRecurrenceCluster,
    decisions: &[SkillCandidateDecision],
    thresholds: &RecurrenceThresholds,
    now: DateTime<Utc>,
) -> Option<RecurrenceDispatchPlan> {
    let occurrences = cluster.members.len();
    if occurrences < thresholds.min_occurrences {
        return None;
    }
    if fingerprint_is_suppressed(decisions, thresholds, now) {
        return None;
    }

    let exemplar_index = select_exemplar_index(&cluster.members)?;
    let exemplar = cluster.members[exemplar_index].clone();

    let mut siblings: Vec<RecurrenceExperienceMember> = cluster
        .members
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != exemplar_index)
        .map(|(_, member)| member.clone())
        .collect();
    // Process siblings deterministically, most recent first, and cap at the same
    // bound the open proposal accumulates so the dispatch payload stays bounded.
    siblings.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.experience_id.cmp(&a.experience_id))
    });
    siblings.truncate(MAX_RECURRENCE_SIBLINGS);

    let first_seen = cluster
        .members
        .iter()
        .map(|member| member.created_at)
        .min()
        .unwrap_or(now);
    let last_seen = cluster
        .members
        .iter()
        .map(|member| member.created_at)
        .max()
        .unwrap_or(now);

    Some(RecurrenceDispatchPlan {
        fingerprint_hash: cluster.fingerprint_hash.clone(),
        merged_fingerprints: cluster.merged_fingerprints.clone(),
        exemplar,
        siblings,
        occurrences,
        first_seen,
        last_seen,
    })
}

/// Maximum sibling members threaded into one recurrence dispatch.
///
/// Matches the open-proposal sibling-suite cap so the exemplar's proposal cannot
/// be handed more siblings than the accumulation path will accept.
pub const MAX_RECURRENCE_SIBLINGS: usize = crate::proposals::MAX_ACCUMULATED_SIBLING_SUITES;

/// Whether any decided-or-open candidate suppresses a fingerprint's dispatch.
fn fingerprint_is_suppressed(
    decisions: &[SkillCandidateDecision],
    thresholds: &RecurrenceThresholds,
    now: DateTime<Utc>,
) -> bool {
    let cooldown = Duration::days(thresholds.rejection_cooldown_days.max(0));
    decisions.iter().any(|decision| match decision.status {
        // An open proposal already routes this recurrence through review, and a
        // promotion means the skill exists (the improve path owns its evolution).
        LearningCandidateStatus::Proposed
        | LearningCandidateStatus::Evaluating
        | LearningCandidateStatus::Promoted => true,
        // A reviewer already declined this work, or a promoted skill for it was
        // rolled back: honor the cooldown so recurring rejection/rollback cannot
        // spam the queue. Today's open-proposal dedup does not cover decided
        // candidates, which is why this check exists.
        LearningCandidateStatus::Rejected | LearningCandidateStatus::RolledBack => {
            decision.updated_at >= now - cooldown
        }
    })
}

/// Picks the exemplar member index, or `None` when no member is learnable-eligible.
///
/// Only members whose confidence clears the outcome-specific exemplar floor are
/// eligible; among those the winner has the highest confidence, then the most
/// distinct tools, then the most recent creation, then the largest id. The full
/// learnability check (partial verification/attribution) still runs in the
/// distiller — this floor only keeps the cron from repeatedly dispatching an
/// exemplar the distiller would reject.
fn select_exemplar_index(members: &[RecurrenceExperienceMember]) -> Option<usize> {
    members
        .iter()
        .enumerate()
        .filter(|(_, member)| member_is_exemplar_eligible(member))
        .max_by(|(_, left), (_, right)| {
            left.confidence_milli
                .cmp(&right.confidence_milli)
                .then_with(|| left.tool_count.cmp(&right.tool_count))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.experience_id.cmp(&right.experience_id))
        })
        .map(|(index, _)| index)
}

/// Whether a member clears the outcome-specific exemplar confidence floor.
fn member_is_exemplar_eligible(member: &RecurrenceExperienceMember) -> bool {
    match member.outcome {
        SegmentOutcome::Resolved => member.confidence_milli >= RESOLVED_EXEMPLAR_CONFIDENCE_MILLI,
        SegmentOutcome::Partial => member.confidence_milli >= PARTIAL_EXEMPLAR_CONFIDENCE_MILLI,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::identifiers::SessionId;
    use uuid::Uuid;

    fn thresholds() -> RecurrenceThresholds {
        RecurrenceThresholds::from_config(&RecurrenceConfig::default())
    }

    fn member(
        confidence_milli: i64,
        tool_count: usize,
        outcome: SegmentOutcome,
        created_at: DateTime<Utc>,
    ) -> RecurrenceExperienceMember {
        RecurrenceExperienceMember {
            experience_id: Uuid::now_v7(),
            session_id: SessionId::new(),
            outcome,
            confidence_milli,
            tool_count,
            created_at,
        }
    }

    fn group(members: Vec<RecurrenceExperienceMember>) -> RecurringExperienceCluster {
        RecurringExperienceCluster {
            fingerprint_hash: "fp".to_string(),
            members,
        }
    }

    /// A single-fingerprint merged cluster, the qualification input when no
    /// semantic merge applies.
    fn cluster(members: Vec<RecurrenceExperienceMember>) -> MergedRecurrenceCluster {
        MergedRecurrenceCluster::single(&group(members))
    }

    fn member_with_id(
        experience_id: uuid::Uuid,
        confidence_milli: i64,
        created_at: DateTime<Utc>,
    ) -> RecurrenceExperienceMember {
        RecurrenceExperienceMember {
            experience_id,
            session_id: SessionId::new(),
            outcome: SegmentOutcome::Resolved,
            confidence_milli,
            tool_count: 5,
            created_at,
        }
    }

    fn named_group(
        hash: &str,
        members: Vec<RecurrenceExperienceMember>,
    ) -> RecurringExperienceCluster {
        RecurringExperienceCluster {
            fingerprint_hash: hash.to_string(),
            members,
        }
    }

    fn neighbor(id: uuid::Uuid, distance: f64) -> ExperienceEmbeddingNeighbor {
        ExperienceEmbeddingNeighbor { id, distance }
    }

    fn resolved(confidence_milli: i64, tool_count: usize) -> RecurrenceExperienceMember {
        member(
            confidence_milli,
            tool_count,
            SegmentOutcome::Resolved,
            Utc::now(),
        )
    }

    #[test]
    fn below_min_occurrences_never_qualifies() {
        // Pins: a fingerprint seen fewer than min_occurrences times is not
        // recurrence evidence, regardless of how strong each instance is.
        let cluster = cluster(vec![resolved(900, 5), resolved(950, 6)]);
        assert!(qualify_recurrence_cluster(&cluster, &[], &thresholds(), Utc::now()).is_none());
    }

    #[test]
    fn three_resolved_occurrences_qualify_and_pick_the_strongest_exemplar() {
        // Pins: at the occurrence floor a cluster qualifies and the exemplar is the
        // highest-confidence member; the rest become siblings and the span is
        // reported for the reviewer.
        let now = Utc::now();
        let weak = member(720, 4, SegmentOutcome::Resolved, now - Duration::days(5));
        let strong = member(980, 9, SegmentOutcome::Resolved, now - Duration::days(2));
        let mid = member(800, 6, SegmentOutcome::Resolved, now - Duration::days(1));
        let plan = qualify_recurrence_cluster(
            &cluster(vec![weak.clone(), strong.clone(), mid.clone()]),
            &[],
            &thresholds(),
            now,
        )
        .expect("cluster qualifies");
        assert_eq!(plan.occurrences, 3);
        assert_eq!(plan.exemplar.experience_id, strong.experience_id);
        assert_eq!(plan.siblings.len(), 2);
        // Siblings are ordered most-recent first.
        assert_eq!(plan.siblings[0].experience_id, mid.experience_id);
        assert_eq!(plan.siblings[1].experience_id, weak.experience_id);
        assert_eq!(plan.first_seen, weak.created_at);
        assert_eq!(plan.last_seen, mid.created_at);
    }

    #[test]
    fn exemplar_tiebreak_prefers_more_tools_then_recency() {
        // Pins: equal confidence breaks to the higher distinct-tool count, and equal
        // confidence and tool count breaks to the more recent experience.
        let now = Utc::now();
        let fewer_tools = member(900, 4, SegmentOutcome::Resolved, now - Duration::days(3));
        let more_tools = member(900, 8, SegmentOutcome::Resolved, now - Duration::days(3));
        let filler = member(900, 4, SegmentOutcome::Resolved, now - Duration::days(9));
        let plan = qualify_recurrence_cluster(
            &cluster(vec![fewer_tools, more_tools.clone(), filler]),
            &[],
            &thresholds(),
            now,
        )
        .expect("qualifies");
        assert_eq!(plan.exemplar.experience_id, more_tools.experience_id);
    }

    #[test]
    fn open_or_promoted_candidate_suppresses_dispatch() {
        // Pins: an open proposal (already in review) or a promoted candidate (skill
        // exists; improve path owns evolution) suppresses recurrence dispatch.
        let cluster = cluster(vec![resolved(900, 5), resolved(910, 5), resolved(920, 5)]);
        for status in [
            LearningCandidateStatus::Proposed,
            LearningCandidateStatus::Evaluating,
            LearningCandidateStatus::Promoted,
        ] {
            let decisions = vec![SkillCandidateDecision {
                status,
                updated_at: Utc::now() - Duration::days(400),
            }];
            assert!(
                qualify_recurrence_cluster(&cluster, &decisions, &thresholds(), Utc::now())
                    .is_none(),
                "status {status:?} must suppress"
            );
        }
    }

    #[test]
    fn recent_rejection_suppresses_but_stale_rejection_does_not() {
        // Pins: a rejection inside the cooldown suppresses the fingerprint; once the
        // cooldown lapses the same recurrence may dispatch again.
        let now = Utc::now();
        let cluster = cluster(vec![resolved(900, 5), resolved(910, 5), resolved(920, 5)]);
        let recent = vec![SkillCandidateDecision {
            status: LearningCandidateStatus::Rejected,
            updated_at: now - Duration::days(5),
        }];
        assert!(qualify_recurrence_cluster(&cluster, &recent, &thresholds(), now).is_none());
        let stale = vec![SkillCandidateDecision {
            status: LearningCandidateStatus::Rejected,
            updated_at: now - Duration::days(45),
        }];
        assert!(qualify_recurrence_cluster(&cluster, &stale, &thresholds(), now).is_some());
    }

    #[test]
    fn cluster_without_a_learnable_exemplar_does_not_dispatch() {
        // Pins: enough occurrences but every member below its confidence floor
        // yields no dispatch, so a chronically-weak fingerprint never spins.
        let low = vec![
            resolved(650, 5),
            resolved(680, 5),
            member(800, 5, SegmentOutcome::Partial, Utc::now()),
        ];
        assert!(
            qualify_recurrence_cluster(&cluster(low), &[], &thresholds(), Utc::now()).is_none()
        );
    }

    #[test]
    fn siblings_are_capped_at_the_sibling_cap() {
        // Pins: a large cluster hands at most MAX_RECURRENCE_SIBLINGS siblings to the
        // exemplar, matching the open-proposal accumulation cap.
        let now = Utc::now();
        let members: Vec<_> = (0..MAX_RECURRENCE_SIBLINGS + 4)
            .map(|index| {
                member(
                    900,
                    5,
                    SegmentOutcome::Resolved,
                    now - Duration::hours(index as i64),
                )
            })
            .collect();
        let plan = qualify_recurrence_cluster(&cluster(members), &[], &thresholds(), now)
            .expect("qualifies");
        assert_eq!(plan.siblings.len(), MAX_RECURRENCE_SIBLINGS);
    }

    #[test]
    fn single_group_qualification_reports_its_lone_fingerprint() {
        // Pins: a cluster that merged with nothing carries exactly its own
        // fingerprint, so single-fingerprint recurrence behaves as before R2.
        let plan = qualify_recurrence_cluster(
            &cluster(vec![resolved(900, 5), resolved(910, 6), resolved(920, 7)]),
            &[],
            &thresholds(),
            Utc::now(),
        )
        .expect("qualifies");
        assert_eq!(plan.fingerprint_hash, "fp");
        assert_eq!(plan.merged_fingerprints, vec!["fp".to_string()]);
    }

    #[test]
    fn close_groups_merge_and_qualify_as_one_cluster() {
        // Pins: two exact-fingerprint groups whose representatives are within the
        // similarity threshold merge into one cluster whose members and merged
        // fingerprints span both groups, and qualify as a single dispatch.
        let now = Utc::now();
        let a1 = member_with_id(uuid::Uuid::now_v7(), 900, now - Duration::days(3));
        let a2 = member_with_id(uuid::Uuid::now_v7(), 910, now - Duration::days(2));
        let b1 = member_with_id(uuid::Uuid::now_v7(), 980, now - Duration::days(1));
        let groups = vec![
            named_group("aaa", vec![a1.clone(), a2.clone()]),
            named_group("bbb", vec![b1.clone()]),
        ];
        // Group "aaa" representative (a1) names b1 as a close neighbor (0.05 <=
        // 0.15 ceiling for 0.85 similarity). Group "bbb" has no cross neighbor.
        let neighbor_lists = vec![
            Some(vec![neighbor(b1.experience_id, 0.05)]),
            Some(Vec::new()),
        ];
        let clusters = cluster_recurrence_groups(&groups, &neighbor_lists, 0.85);
        assert_eq!(clusters.len(), 1);
        let merged = &clusters[0];
        assert_eq!(merged.fingerprint_hash, "aaa");
        assert_eq!(
            merged.merged_fingerprints,
            vec!["aaa".to_string(), "bbb".to_string()]
        );
        assert_eq!(merged.members.len(), 3);
        let plan = qualify_recurrence_cluster(merged, &[], &thresholds(), now).expect("qualifies");
        // Exemplar is the strongest member across the union.
        assert_eq!(plan.exemplar.experience_id, b1.experience_id);
        assert_eq!(plan.occurrences, 3);
    }

    #[test]
    fn distant_groups_do_not_merge() {
        // Pins: groups whose representatives sit beyond the similarity threshold
        // stay separate, so unrelated tasks never pool.
        let now = Utc::now();
        let a1 = member_with_id(uuid::Uuid::now_v7(), 900, now);
        let b1 = member_with_id(uuid::Uuid::now_v7(), 900, now);
        let groups = vec![
            named_group("aaa", vec![a1.clone()]),
            named_group("bbb", vec![b1.clone()]),
        ];
        // 0.5 distance = 0.5 similarity, below the 0.85 threshold (ceiling 0.15).
        let neighbor_lists = vec![
            Some(vec![neighbor(b1.experience_id, 0.5)]),
            Some(vec![neighbor(a1.experience_id, 0.5)]),
        ];
        let clusters = cluster_recurrence_groups(&groups, &neighbor_lists, 0.85);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].fingerprint_hash, "aaa");
        assert_eq!(clusters[1].fingerprint_hash, "bbb");
    }

    #[test]
    fn unembedded_representative_stays_in_its_own_group() {
        // Pins: NULL degradation — a group whose representative has no embedding
        // contributes no merges and remains a standalone cluster, so absent
        // embeddings reduce to exact grouping.
        let now = Utc::now();
        let a1 = member_with_id(uuid::Uuid::now_v7(), 900, now);
        let b1 = member_with_id(uuid::Uuid::now_v7(), 900, now);
        let groups = vec![named_group("aaa", vec![a1]), named_group("bbb", vec![b1])];
        let neighbor_lists = vec![None, None];
        let clusters = cluster_recurrence_groups(&groups, &neighbor_lists, 0.85);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn one_sided_neighbor_pulls_unembedded_group_into_the_cluster() {
        // Pins: a merge is directional-safe — if group A's representative names a
        // member of group B (even one whose own representative is unembedded), B
        // still joins the cluster.
        let now = Utc::now();
        let a1 = member_with_id(uuid::Uuid::now_v7(), 900, now);
        let b1 = member_with_id(uuid::Uuid::now_v7(), 900, now);
        let groups = vec![
            named_group("aaa", vec![a1]),
            named_group("bbb", vec![b1.clone()]),
        ];
        let neighbor_lists = vec![Some(vec![neighbor(b1.experience_id, 0.05)]), None];
        let clusters = cluster_recurrence_groups(&groups, &neighbor_lists, 0.85);
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].merged_fingerprints,
            vec!["aaa".to_string(), "bbb".to_string()]
        );
    }

    #[test]
    fn transitive_neighbors_collapse_into_one_cluster() {
        // Pins: clustering is transitive — A~B and B~C merges A, B, and C even
        // though A and C were never directly compared.
        let now = Utc::now();
        let a1 = member_with_id(uuid::Uuid::now_v7(), 900, now);
        let b1 = member_with_id(uuid::Uuid::now_v7(), 900, now);
        let c1 = member_with_id(uuid::Uuid::now_v7(), 900, now);
        let groups = vec![
            named_group("aaa", vec![a1.clone()]),
            named_group("bbb", vec![b1.clone()]),
            named_group("ccc", vec![c1.clone()]),
        ];
        let neighbor_lists = vec![
            Some(vec![neighbor(b1.experience_id, 0.05)]),
            Some(vec![neighbor(c1.experience_id, 0.05)]),
            Some(Vec::new()),
        ];
        let clusters = cluster_recurrence_groups(&groups, &neighbor_lists, 0.85);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 3);
        assert_eq!(
            clusters[0].merged_fingerprints,
            vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()]
        );
    }

    #[test]
    fn any_merged_fingerprint_open_candidate_suppresses_the_cluster() {
        // Pins: per-cluster suppression — decisions are the union across merged
        // fingerprints, so an open candidate on any one member fingerprint
        // suppresses the whole merged cluster.
        let now = Utc::now();
        let a1 = member_with_id(uuid::Uuid::now_v7(), 900, now - Duration::days(2));
        let b1 = member_with_id(uuid::Uuid::now_v7(), 910, now - Duration::days(1));
        let b2 = member_with_id(uuid::Uuid::now_v7(), 905, now);
        let groups = vec![
            named_group("aaa", vec![a1.clone()]),
            named_group("bbb", vec![b1.clone(), b2.clone()]),
        ];
        let neighbor_lists = vec![
            Some(vec![neighbor(b1.experience_id, 0.05)]),
            Some(Vec::new()),
        ];
        let clusters = cluster_recurrence_groups(&groups, &neighbor_lists, 0.85);
        let merged = &clusters[0];
        // An open proposal exists for fingerprint "bbb" (merged in): the whole
        // cluster is suppressed.
        let decisions = vec![SkillCandidateDecision {
            status: LearningCandidateStatus::Proposed,
            updated_at: now,
        }];
        assert!(qualify_recurrence_cluster(merged, &decisions, &thresholds(), now).is_none());
    }
}
