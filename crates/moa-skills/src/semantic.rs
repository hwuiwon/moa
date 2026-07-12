//! Pure decision logic for the semantic (R2) layer of the skill-learning loop.
//!
//! This module holds the model-free, deterministic core of two filing-time
//! decisions and one shared distance helper; all I/O (embedding a probe, running
//! nearest-neighbor queries, resolving an artifact to a skill name) stays in the
//! distiller and its store, which feed the results here. Keeping the decisions
//! pure makes routing precedence and dedup tie-breaks unit-testable without a
//! database or a provider.
//!
//! ## Cosine distance vs similarity
//!
//! pgvector's `<=>` operator returns cosine *distance* `d = 1 - cosine_similarity`,
//! with `d` in `[0, 2]` because cosine similarity is in `[-1, 1]`. Every threshold
//! in the learning config is expressed as a *similarity* `s` in `[0, 1]` (higher =
//! more alike). A neighbor clears the threshold when its distance is at most
//! `1 - s`. [`similarity_to_max_distance`] is the single conversion both this
//! module and the recurrence clustering use, so the mapping lives in exactly one
//! place.

use moa_session::{ExperienceEmbeddingNeighbor, OpenProposalSource};
use std::collections::HashMap;
use uuid::Uuid;

/// Converts a cosine *similarity* threshold into the maximum cosine *distance* a
/// neighbor may have and still clear it.
///
/// pgvector cosine distance is `1 - cosine_similarity`, so a similarity floor `s`
/// admits every neighbor at distance `<= 1 - s`. A similarity of `0.85` admits
/// distance `<= 0.15`; a similarity of `0.80` admits distance `<= 0.20`.
#[must_use]
pub fn similarity_to_max_distance(similarity: f64) -> f64 {
    1.0 - similarity
}

/// Converts a cosine *distance* back into the cosine similarity it represents.
///
/// The inverse of [`similarity_to_max_distance`], used to report the similarity
/// behind a routing/dedup decision in the reviewer-facing evidence payload.
#[must_use]
pub fn distance_to_similarity(distance: f64) -> f64 {
    1.0 - distance
}

/// How the improve-vs-create routing decision was reached.
///
/// Recorded in the proposal's `routing` evidence so a reviewer can see whether
/// the semantic embedding signal or the lexical Jaccard fallback chose the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingMethod {
    /// The nearest published skill-identity embedding cleared the improve floor.
    Embedding,
    /// No embedding signal was available; the lexical Jaccard fallback matched.
    Jaccard,
    /// Neither signal matched an existing skill; the experience creates a new one.
    None,
}

impl RoutingMethod {
    /// Returns the stable wire label for this routing method.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Embedding => "embedding",
            Self::Jaccard => "jaccard",
            Self::None => "none",
        }
    }
}

/// The nearest published skill-identity embedding to a probe.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingSkillMatch {
    /// Name of the nearest skill (resolved from its artifact identity).
    pub skill_name: String,
    /// Cosine distance from the probe in `[0, 2]`.
    pub distance: f64,
}

/// The improve-vs-create routing decision for one distilled experience.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingDecision {
    /// Existing skill to improve, or `None` to create a new skill.
    pub improve_skill: Option<String>,
    /// How the decision was reached.
    pub method: RoutingMethod,
    /// Cosine similarity behind the decision; `0.0` when routing to a new skill.
    pub similarity: f64,
}

/// Chooses improve-vs-create, with the embedding signal replacing Jaccard as the
/// primary decision.
///
/// The embedding signal is authoritative whenever it is *available*
/// (`embedding_nearest` is `Some`, i.e. the probe embedded and at least one
/// published skill embedding existed to compare against):
///
/// - within the improve floor → improve that skill (`method = embedding`);
/// - outside it → create a new skill (`method = none`); Jaccard is **not**
///   consulted, because a conclusive "no semantic match" must not be overridden by
///   a noisy token overlap.
///
/// Jaccard is the fallback only when the embedding signal is *absent*
/// (`embedding_nearest` is `None`: no embedder, a failed embed, no skill
/// embeddings yet, or an unresolvable nearest artifact). Then a lexical match
/// routes to improvement (`method = jaccard`) and no match creates a new skill.
#[must_use]
pub fn route_improve_vs_create(
    embedding_nearest: Option<EmbeddingSkillMatch>,
    jaccard_match: Option<(String, f64)>,
    improve_route_similarity: f64,
) -> RoutingDecision {
    if let Some(nearest) = embedding_nearest {
        if nearest.distance <= similarity_to_max_distance(improve_route_similarity) {
            return RoutingDecision {
                improve_skill: Some(nearest.skill_name),
                method: RoutingMethod::Embedding,
                similarity: distance_to_similarity(nearest.distance),
            };
        }
        return RoutingDecision {
            improve_skill: None,
            method: RoutingMethod::None,
            similarity: 0.0,
        };
    }
    match jaccard_match {
        Some((skill_name, score)) => RoutingDecision {
            improve_skill: Some(skill_name),
            method: RoutingMethod::Jaccard,
            similarity: score,
        },
        None => RoutingDecision {
            improve_skill: None,
            method: RoutingMethod::None,
            similarity: 0.0,
        },
    }
}

/// Picks the open proposal a new experience is a semantic duplicate of, if any.
///
/// `neighbors` is the new experience's nearest task-embedding neighbors ordered by
/// ascending distance (the store's contract). `open_sources` are the origin
/// experiences behind the tenant's open `Proposed` skill candidates. A candidate
/// is a dedupe target when one of its source experiences appears among the
/// neighbors within the dedup distance ceiling.
///
/// Tie-breaks are deterministic: neighbors are scanned nearest-first, so the
/// closest qualifying experience wins; and when a single experience backs more
/// than one open candidate, the oldest candidate (then smallest id) is kept, so
/// the result never depends on row order.
#[must_use]
pub fn select_proposal_dedupe_hit(
    neighbors: &[ExperienceEmbeddingNeighbor],
    open_sources: &[OpenProposalSource],
    proposal_dedup_similarity: f64,
) -> Option<Uuid> {
    let ceiling = similarity_to_max_distance(proposal_dedup_similarity);
    // experience id -> the oldest open candidate that lists it as a source.
    let mut experience_to_candidate: HashMap<Uuid, (chrono::DateTime<chrono::Utc>, Uuid)> =
        HashMap::new();
    for source in open_sources {
        for experience_id in &source.source_experience_ids {
            experience_to_candidate
                .entry(*experience_id)
                .and_modify(|current| {
                    if (source.created_at, source.candidate_id) < *current {
                        *current = (source.created_at, source.candidate_id);
                    }
                })
                .or_insert((source.created_at, source.candidate_id));
        }
    }
    neighbors
        .iter()
        .filter(|neighbor| neighbor.distance <= ceiling)
        .find_map(|neighbor| {
            experience_to_candidate
                .get(&neighbor.id)
                .map(|(_, candidate_id)| *candidate_id)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn neighbor(id: Uuid, distance: f64) -> ExperienceEmbeddingNeighbor {
        ExperienceEmbeddingNeighbor { id, distance }
    }

    #[test]
    fn similarity_and_distance_are_exact_inverses() {
        // Pins: the one place cosine similarity thresholds convert to pgvector
        // distance ceilings. 0.85 similarity admits distance <= 0.15; the inverse
        // recovers the similarity a distance represents.
        assert!((similarity_to_max_distance(0.85) - 0.15).abs() < 1e-9);
        assert!((similarity_to_max_distance(0.80) - 0.20).abs() < 1e-9);
        assert!((distance_to_similarity(0.15) - 0.85).abs() < 1e-9);
    }

    #[test]
    fn embedding_match_within_floor_beats_jaccard() {
        // Pins: when the embedding signal is available and clears the improve
        // floor, it routes to that skill even though a different Jaccard match
        // exists — embedding is the primary signal.
        let decision = route_improve_vs_create(
            Some(EmbeddingSkillMatch {
                skill_name: "deploy-rollback".to_string(),
                distance: 0.1,
            }),
            Some(("some-other-skill".to_string(), 0.9)),
            0.80,
        );
        assert_eq!(decision.improve_skill.as_deref(), Some("deploy-rollback"));
        assert_eq!(decision.method, RoutingMethod::Embedding);
        assert!((decision.similarity - 0.9).abs() < 1e-9);
    }

    #[test]
    fn embedding_below_floor_creates_new_and_ignores_jaccard() {
        // Pins: an available-but-below-floor embedding signal is conclusive — it
        // creates a new skill and never falls back to a noisy Jaccard overlap.
        let decision = route_improve_vs_create(
            Some(EmbeddingSkillMatch {
                skill_name: "deploy-rollback".to_string(),
                distance: 0.5,
            }),
            Some(("token-overlap-skill".to_string(), 0.95)),
            0.80,
        );
        assert_eq!(decision.improve_skill, None);
        assert_eq!(decision.method, RoutingMethod::None);
    }

    #[test]
    fn jaccard_is_used_only_when_embedding_absent() {
        // Pins: with no embedding signal (provider down, or no skill embeddings),
        // a lexical match routes to improvement; no lexical match creates new.
        let matched = route_improve_vs_create(None, Some(("legacy-skill".to_string(), 0.6)), 0.80);
        assert_eq!(matched.improve_skill.as_deref(), Some("legacy-skill"));
        assert_eq!(matched.method, RoutingMethod::Jaccard);
        assert!((matched.similarity - 0.6).abs() < 1e-9);

        let unmatched = route_improve_vs_create(None, None, 0.80);
        assert_eq!(unmatched.improve_skill, None);
        assert_eq!(unmatched.method, RoutingMethod::None);
    }

    #[test]
    fn dedupe_hit_picks_nearest_qualifying_open_candidate() {
        // Pins: neighbors are scanned nearest-first, so the closest source
        // experience within the dedup ceiling selects its candidate; farther
        // matches and above-ceiling matches are ignored.
        let near_exp = Uuid::now_v7();
        let far_exp = Uuid::now_v7();
        let candidate_near = Uuid::now_v7();
        let candidate_far = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap();
        let open = vec![
            OpenProposalSource {
                candidate_id: candidate_far,
                created_at: now,
                source_experience_ids: vec![far_exp],
            },
            OpenProposalSource {
                candidate_id: candidate_near,
                created_at: now,
                source_experience_ids: vec![near_exp],
            },
        ];
        // ceiling for 0.85 similarity is 0.15.
        let neighbors = vec![neighbor(near_exp, 0.05), neighbor(far_exp, 0.10)];
        assert_eq!(
            select_proposal_dedupe_hit(&neighbors, &open, 0.85),
            Some(candidate_near)
        );
    }

    #[test]
    fn dedupe_ignores_neighbors_beyond_the_ceiling() {
        // Pins: a source experience that exists but sits beyond the dedup ceiling
        // is not a duplicate, so the experience files its own proposal.
        let exp = Uuid::now_v7();
        let candidate = Uuid::now_v7();
        let open = vec![OpenProposalSource {
            candidate_id: candidate,
            created_at: Utc::now(),
            source_experience_ids: vec![exp],
        }];
        // 0.3 distance = 0.7 similarity, below the 0.85 floor (ceiling 0.15).
        let neighbors = vec![neighbor(exp, 0.30)];
        assert_eq!(select_proposal_dedupe_hit(&neighbors, &open, 0.85), None);
    }

    #[test]
    fn dedupe_breaks_shared_experience_ties_to_the_oldest_candidate() {
        // Pins: when one experience backs two open candidates, the oldest wins,
        // regardless of the order the sources are listed in.
        let exp = Uuid::now_v7();
        let older = Uuid::now_v7();
        let newer = Uuid::now_v7();
        let older_time = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let newer_time = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap();
        let open = vec![
            OpenProposalSource {
                candidate_id: newer,
                created_at: newer_time,
                source_experience_ids: vec![exp],
            },
            OpenProposalSource {
                candidate_id: older,
                created_at: older_time,
                source_experience_ids: vec![exp],
            },
        ];
        let neighbors = vec![neighbor(exp, 0.05)];
        assert_eq!(
            select_proposal_dedupe_hit(&neighbors, &open, 0.85),
            Some(older)
        );
    }
}
