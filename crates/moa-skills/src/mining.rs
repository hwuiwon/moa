//! Deterministic weakness mining: cluster eval and session failure signals into
//! recurrence-thresholded [`LearningCandidate`] intents without any model call.
//!
//! The pass is pure and offline. [`mine_failure_patterns`] clusters raw failure
//! signals into recurrence-counted patterns; [`file_candidates`] turns patterns
//! that cross a threshold into learning candidates, deduplicating against open
//! candidates for the same pattern key so re-mining bumps an occurrence counter
//! instead of filing duplicates. No LLM is involved at any step: candidate
//! descriptions are assembled from the pattern key and its counts.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use moa_core::{
    LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, LearningRiskClass, TenantId,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::proposals::EditableSurface;

/// Default recurrence threshold: a pattern files a candidate once it recurs this many times.
pub const DEFAULT_MINING_THRESHOLD: usize = 3;

/// Maximum evidence references retained per pattern (stable ids only, never transcripts).
const MAX_EVIDENCE_REFS: usize = 20;

/// One failed evaluation probe fed into weakness mining.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedProbe {
    /// Stable probe identifier used as an evidence reference.
    pub probe_id: String,
    /// Probe-type slice key, such as `point_recall` or `multi_hop`.
    pub probe_type: String,
    /// Metric that failed for this probe, such as `recall_at_4`.
    pub failing_metric: String,
}

/// Failure signal kind recorded from a session event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFailureKind {
    /// A terminal, non-retryable durable tool error.
    DurableToolError,
    /// A human reviewer rejected an approval request.
    RejectedApproval,
    /// A turn retrieved no relevant memory.
    ZeroRecallTurn,
    /// A produced citation could not be verified against evidence.
    UnverifiedCitation,
}

impl SessionFailureKind {
    /// Returns the stable snake_case signal key used for clustering.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DurableToolError => "durable_tool_error",
            Self::RejectedApproval => "rejected_approval",
            Self::ZeroRecallTurn => "zero_recall_turn",
            Self::UnverifiedCitation => "unverified_citation",
        }
    }
}

/// One failure signal recorded from a session event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFailure {
    /// Stable event identifier used as an evidence reference.
    pub event_id: String,
    /// Failure signal kind.
    pub kind: SessionFailureKind,
    /// Failing surface subject: the tool name, or a turn tag when no tool applies.
    pub subject: String,
}

/// Raw failure signals mined in one deterministic pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MiningInputs {
    /// Failed evaluation probes by slice.
    pub failed_probes: Vec<FailedProbe>,
    /// Session-event failure records.
    pub session_failures: Vec<SessionFailure>,
}

/// Cluster key: the verifier signal paired with the failing surface subject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FailurePatternKey {
    /// Verifier signal: a failing metric name or a session failure kind.
    pub signal: String,
    /// Failing surface subject: a probe-type slice or a tool name.
    pub subject: String,
}

impl FailurePatternKey {
    /// Returns the stable `signal:subject` string used for dedup and candidate keying.
    #[must_use]
    pub fn as_string(&self) -> String {
        format!("{}:{}", self.signal, self.subject)
    }
}

/// A recurrence-counted failure cluster with bounded evidence references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailurePattern {
    /// Cluster key.
    pub key: FailurePatternKey,
    /// Total signals observed for this key; may exceed the retained evidence count.
    pub occurrences: usize,
    /// Bounded, sorted evidence references (event/probe ids only).
    pub evidence: Vec<String>,
    /// Editable surface this pattern implicates, derived deterministically from the signal.
    pub surface: EditableSurface,
}

/// One filing decision produced by [`file_candidates`].
#[derive(Debug, Clone, PartialEq)]
pub enum CandidateFiling {
    /// A new learning candidate to append.
    New(Box<LearningCandidate>),
    /// An occurrence-count bump for an existing open candidate with the same pattern key.
    Bump(LearningCandidateStatusUpdate),
}

/// Clusters raw failure signals into recurrence-counted patterns.
///
/// Clustering is deterministic and model-free: signals group by
/// `(signal, subject)`, evidence references are sorted, deduplicated, and capped
/// at [`MAX_EVIDENCE_REFS`], and the returned patterns are ordered by key.
#[must_use]
pub fn mine_failure_patterns(inputs: &MiningInputs) -> Vec<FailurePattern> {
    let mut clusters: BTreeMap<FailurePatternKey, (usize, BTreeSet<String>)> = BTreeMap::new();

    for probe in &inputs.failed_probes {
        let key = FailurePatternKey {
            signal: probe.failing_metric.clone(),
            subject: probe.probe_type.clone(),
        };
        let entry = clusters.entry(key).or_default();
        entry.0 += 1;
        entry.1.insert(probe.probe_id.clone());
    }
    for failure in &inputs.session_failures {
        let key = FailurePatternKey {
            signal: failure.kind.as_str().to_string(),
            subject: failure.subject.clone(),
        };
        let entry = clusters.entry(key).or_default();
        entry.0 += 1;
        entry.1.insert(failure.event_id.clone());
    }

    clusters
        .into_iter()
        .map(|(key, (occurrences, evidence))| {
            let surface = surface_for_signal(&key.signal);
            FailurePattern {
                key,
                occurrences,
                evidence: evidence.into_iter().take(MAX_EVIDENCE_REFS).collect(),
                surface,
            }
        })
        .collect()
}

/// Files candidates for patterns that cross `threshold`, deduplicating against open candidates.
///
/// A pattern already represented by an open candidate (matched on the payload
/// `pattern_key`) yields a [`CandidateFiling::Bump`] carrying the new occurrence
/// count; otherwise a fresh [`CandidateFiling::New`] candidate is produced. The
/// candidate description is assembled from the pattern key and counts, never a model.
#[must_use]
pub fn file_candidates(
    patterns: &[FailurePattern],
    threshold: usize,
    open_candidates: &[LearningCandidate],
    tenant_id: TenantId,
    now: DateTime<Utc>,
) -> Vec<CandidateFiling> {
    patterns
        .iter()
        .filter(|pattern| pattern.occurrences >= threshold)
        .map(|pattern| {
            let key = pattern.key.as_string();
            match open_candidate_for_key(open_candidates, &key) {
                Some(existing) => CandidateFiling::Bump(bump_update(existing, pattern, now)),
                None => {
                    CandidateFiling::New(Box::new(mining_candidate(tenant_id, pattern, &key, now)))
                }
            }
        })
        .collect()
}

fn open_candidate_for_key<'a>(
    open_candidates: &'a [LearningCandidate],
    key: &str,
) -> Option<&'a LearningCandidate> {
    open_candidates.iter().find(|candidate| {
        matches!(
            candidate.status,
            LearningCandidateStatus::Proposed | LearningCandidateStatus::Evaluating
        ) && candidate
            .payload
            .get("pattern_key")
            .and_then(|value| value.as_str())
            == Some(key)
    })
}

fn bump_update(
    existing: &LearningCandidate,
    pattern: &FailurePattern,
    now: DateTime<Utc>,
) -> LearningCandidateStatusUpdate {
    LearningCandidateStatusUpdate {
        candidate_id: existing.id,
        status: LearningCandidateStatus::Proposed,
        status_reason: Some(format!(
            "weakness pattern re-observed: {} now at {} occurrences",
            pattern.key.as_string(),
            pattern.occurrences
        )),
        evaluation_payload: Some(json!({
            "pattern_key": pattern.key.as_string(),
            "occurrence_count": pattern.occurrences,
            "evidence": pattern.evidence,
        })),
        updated_at: now,
    }
}

fn mining_candidate(
    tenant_id: TenantId,
    pattern: &FailurePattern,
    key: &str,
    now: DateTime<Utc>,
) -> LearningCandidate {
    let description = format!(
        "Recurring {} failures on {} ({} occurrences); implicates {} surface",
        pattern.key.signal,
        pattern.key.subject,
        pattern.occurrences,
        pattern.surface.as_str()
    );
    LearningCandidate {
        id: mining_candidate_id(tenant_id, key),
        tenant_id,
        user_id: None,
        candidate_type: candidate_type_for_surface(pattern.surface),
        status: LearningCandidateStatus::Proposed,
        target_id: None,
        target_label: Some(pattern.surface.as_str().to_string()),
        task_fingerprint: None,
        task_facets: None,
        payload: json!({
            "kind": "weakness_mining_pattern",
            "pattern_key": key,
            "pattern_occurrences": pattern.occurrences,
            "surface": pattern.surface,
            "signal": pattern.key.signal,
            "subject": pattern.key.subject,
            "evidence": pattern.evidence,
            "description": description,
        }),
        evaluation_payload: None,
        source_experience_ids: Vec::new(),
        confidence: None,
        risk_class: LearningRiskClass::Medium,
        promotion_requirements: vec!["human_review".to_string()],
        status_reason: Some(description),
        batch_id: None,
        created_at: now,
        updated_at: now,
    }
}

/// Maps a cluster signal to the editable surface a fix would most plausibly touch.
fn surface_for_signal(signal: &str) -> EditableSurface {
    if signal == SessionFailureKind::DurableToolError.as_str() {
        EditableSurface::SkillMarkdown
    } else if signal == SessionFailureKind::RejectedApproval.as_str() {
        EditableSurface::RouterRules
    } else {
        // Zero-recall, unverified-citation, and eval retrieval-metric failures all point at
        // retrieval ranking.
        EditableSurface::RankingConfig
    }
}

fn candidate_type_for_surface(surface: EditableSurface) -> LearningCandidateType {
    match surface {
        EditableSurface::SkillMarkdown => LearningCandidateType::Skill,
        EditableSurface::RewritePromptVersion => LearningCandidateType::Prompt,
        EditableSurface::RouterRules | EditableSurface::RankingConfig => {
            LearningCandidateType::Policy
        }
    }
}

fn mining_candidate_id(tenant_id: TenantId, key: &str) -> Uuid {
    let mut hasher = Sha256::new();
    for part in ["moa.skill.weakness_mining.v1", &tenant_id.to_string(), key] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> TenantId {
        TenantId::from(Uuid::from_u128(7))
    }

    fn probe(probe_id: &str, probe_type: &str, failing_metric: &str) -> FailedProbe {
        FailedProbe {
            probe_id: probe_id.to_string(),
            probe_type: probe_type.to_string(),
            failing_metric: failing_metric.to_string(),
        }
    }

    fn failure(event_id: &str, kind: SessionFailureKind, subject: &str) -> SessionFailure {
        SessionFailure {
            event_id: event_id.to_string(),
            kind,
            subject: subject.to_string(),
        }
    }

    #[test]
    fn mining_clusters_by_signal_and_subject_with_bounded_sorted_evidence() {
        // Pins: signals group by (signal, subject); occurrences count every signal while evidence
        // dedups; patterns and evidence are deterministically ordered; the surface is derived.
        let inputs = MiningInputs {
            failed_probes: vec![
                probe("p3", "multi_hop", "recall_at_4"),
                probe("p1", "multi_hop", "recall_at_4"),
                probe("p2", "multi_hop", "recall_at_4"),
                probe("p9", "point_recall", "recall_at_4"),
            ],
            session_failures: vec![
                failure("e2", SessionFailureKind::DurableToolError, "bash"),
                failure("e1", SessionFailureKind::DurableToolError, "bash"),
            ],
        };

        let patterns = mine_failure_patterns(&inputs);

        // BTreeMap key order: durable_tool_error < recall_at_4.
        assert_eq!(patterns.len(), 3);
        let multi_hop = patterns
            .iter()
            .find(|pattern| pattern.key.subject == "multi_hop")
            .expect("multi_hop cluster");
        assert_eq!(multi_hop.occurrences, 3);
        assert_eq!(multi_hop.evidence, vec!["p1", "p2", "p3"]);
        assert_eq!(multi_hop.surface, EditableSurface::RankingConfig);

        let tool = patterns
            .iter()
            .find(|pattern| pattern.key.signal == "durable_tool_error")
            .expect("tool cluster");
        assert_eq!(tool.occurrences, 2);
        assert_eq!(tool.evidence, vec!["e1", "e2"]);
        assert_eq!(tool.surface, EditableSurface::SkillMarkdown);
    }

    #[test]
    fn mining_caps_evidence_at_twenty_but_counts_all_occurrences() {
        // Pins: evidence is bounded to twenty ids while occurrences reflects the full signal count.
        let failed_probes = (0..25)
            .map(|index| probe(&format!("p{index:02}"), "multi_hop", "recall_at_4"))
            .collect();
        let patterns = mine_failure_patterns(&MiningInputs {
            failed_probes,
            session_failures: Vec::new(),
        });

        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].occurrences, 25);
        assert_eq!(patterns[0].evidence.len(), MAX_EVIDENCE_REFS);
        assert_eq!(
            patterns[0].evidence.first().map(String::as_str),
            Some("p00")
        );
    }

    #[test]
    fn file_candidates_only_files_patterns_at_or_above_threshold() {
        // Pins: the recurrence threshold gates filing; below-threshold patterns produce nothing.
        let inputs = MiningInputs {
            failed_probes: vec![
                probe("p1", "multi_hop", "recall_at_4"),
                probe("p2", "multi_hop", "recall_at_4"),
                probe("p3", "multi_hop", "recall_at_4"),
                probe("q1", "point_recall", "recall_at_4"),
                probe("q2", "point_recall", "recall_at_4"),
            ],
            session_failures: Vec::new(),
        };
        let patterns = mine_failure_patterns(&inputs);

        let filings = file_candidates(
            &patterns,
            DEFAULT_MINING_THRESHOLD,
            &[],
            tenant(),
            Utc::now(),
        );

        assert_eq!(filings.len(), 1);
        let CandidateFiling::New(candidate) = &filings[0] else {
            panic!("expected a new candidate");
        };
        assert_eq!(candidate.candidate_type, LearningCandidateType::Policy);
        assert_eq!(
            candidate
                .payload
                .get("pattern_key")
                .and_then(|value| value.as_str()),
            Some("recall_at_4:multi_hop")
        );
        assert_eq!(
            candidate
                .payload
                .get("pattern_occurrences")
                .and_then(|value| value.as_u64()),
            Some(3)
        );
    }

    #[test]
    fn refiling_an_open_pattern_bumps_the_counter_instead_of_duplicating() {
        // Pins: an open candidate for the same pattern key yields an occurrence bump, not a
        // duplicate new candidate.
        let inputs = MiningInputs {
            failed_probes: vec![
                probe("p1", "multi_hop", "recall_at_4"),
                probe("p2", "multi_hop", "recall_at_4"),
                probe("p3", "multi_hop", "recall_at_4"),
                probe("p4", "multi_hop", "recall_at_4"),
            ],
            session_failures: Vec::new(),
        };
        let patterns = mine_failure_patterns(&inputs);
        let existing = match &file_candidates(&patterns, 3, &[], tenant(), Utc::now())[0] {
            CandidateFiling::New(candidate) => (**candidate).clone(),
            CandidateFiling::Bump(_) => panic!("first filing must be a new candidate"),
        };

        let filings = file_candidates(
            &patterns,
            3,
            std::slice::from_ref(&existing),
            tenant(),
            Utc::now(),
        );

        assert_eq!(filings.len(), 1);
        let CandidateFiling::Bump(update) = &filings[0] else {
            panic!("re-filing an open pattern must bump, not duplicate");
        };
        assert_eq!(update.candidate_id, existing.id);
        assert_eq!(update.status, LearningCandidateStatus::Proposed);
        assert_eq!(
            update
                .evaluation_payload
                .as_ref()
                .and_then(|value| value.get("occurrence_count"))
                .and_then(|value| value.as_u64()),
            Some(4)
        );
    }

    #[test]
    fn mining_candidate_id_is_stable_per_tenant_and_pattern_key() {
        // Pins: candidate ids are a pure function of tenant + pattern key, so re-mining the same
        // weakness resolves to one candidate id across runs.
        let inputs = MiningInputs {
            failed_probes: vec![
                probe("p1", "multi_hop", "recall_at_4"),
                probe("p2", "multi_hop", "recall_at_4"),
                probe("p3", "multi_hop", "recall_at_4"),
            ],
            session_failures: Vec::new(),
        };
        let patterns = mine_failure_patterns(&inputs);
        let first = file_candidates(&patterns, 3, &[], tenant(), Utc::now());
        let second = file_candidates(&patterns, 3, &[], tenant(), Utc::now());

        let id_of = |filings: &[CandidateFiling]| match &filings[0] {
            CandidateFiling::New(candidate) => candidate.id,
            CandidateFiling::Bump(_) => panic!("expected new candidate"),
        };
        assert_eq!(id_of(&first), id_of(&second));
    }
}
