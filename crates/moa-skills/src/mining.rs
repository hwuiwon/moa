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
    types::experience::LearningCandidate, types::experience::LearningCandidateSourceRef,
    types::experience::LearningCandidateStatus, types::experience::LearningCandidateStatusUpdate,
    types::experience::LearningCandidateType, types::experience::LearningProposalKind,
    types::experience::LearningRiskClass, types::identifiers::SessionId,
    types::identifiers::TenantId,
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
    /// Event that carried the failure signal.
    ///
    /// Events are hash-partitioned by session, so naming one takes both halves
    /// of its key. Carrying them separately (rather than as a display string) is
    /// what lets the filed candidate reference the exact event row instead of
    /// quoting an id into JSON that nothing can join or erase.
    pub event_id: Uuid,
    /// Session that owns the event.
    pub session_id: SessionId,
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
    /// Typed database references behind the evidence, in the same bounded order.
    ///
    /// Only session-event signals produce these; an evaluation probe id names no
    /// row in this database. A pattern with no typed sources therefore cannot be
    /// filed as a candidate at all, which is the point: an unattributable
    /// candidate is one privacy erasure can never reach.
    pub sources: Vec<LearningCandidateSourceRef>,
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

/// Extracts mineable failure signals from a bounded slice of session events.
///
/// Deterministic and model-free: durable (non-retryable) tool errors mine as
/// [`SessionFailureKind::DurableToolError`] keyed by tool name, and denied
/// action reviews mine as [`SessionFailureKind::RejectedApproval`] keyed by the
/// reviewed tool (correlated through the matching review-request event).
/// Retryable tool errors and cleared reviews are not failures and are skipped.
#[must_use]
pub fn session_failures_from_events(
    events: &[moa_core::types::events_stream::EventRecord],
) -> Vec<SessionFailure> {
    use moa_core::{events::Event, types::action_policy::ActionReviewDecision};

    let mut review_subjects: BTreeMap<Uuid, String> = BTreeMap::new();
    let mut failures = Vec::new();
    for record in events {
        match &record.event {
            Event::ToolError {
                tool_name,
                retryable: false,
                ..
            } => failures.push(SessionFailure {
                event_id: record.id,
                session_id: record.session_id,
                kind: SessionFailureKind::DurableToolError,
                subject: tool_name.clone(),
            }),
            Event::ActionReviewRequested {
                review_id,
                envelope,
                ..
            } => {
                review_subjects.insert(*review_id, envelope.tool_name.clone());
            }
            Event::ActionReviewDecided {
                review_id,
                decision: ActionReviewDecision::Denied { .. },
                ..
            } => failures.push(SessionFailure {
                event_id: record.id,
                session_id: record.session_id,
                kind: SessionFailureKind::RejectedApproval,
                subject: review_subjects
                    .get(review_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown_tool".to_string()),
            }),
            _ => {}
        }
    }
    failures
}

/// Mines a session event window's failure signals and files or bumps candidates.
///
/// The store-coupled application of the pure passes below: extract signals,
/// cluster by recurrence, then file one reviewable candidate per pattern key.
/// Candidates a reviewer already claimed (`Evaluating`) keep their state — the
/// conditional bump only applies while a candidate is still `Proposed`.
/// Returns the number of filings applied.
pub async fn mine_and_file_session_failures(
    store: &moa_session::PostgresSessionStore,
    tenant_id: TenantId,
    events: &[moa_core::types::events_stream::EventRecord],
    now: DateTime<Utc>,
) -> moa_core::error::Result<usize> {
    let session_failures = session_failures_from_events(events);
    if session_failures.is_empty() {
        return Ok(0);
    }
    let patterns = mine_failure_patterns(&MiningInputs {
        failed_probes: Vec::new(),
        session_failures,
    });

    let tenant_key = tenant_id.to_string();
    let open = store
        .list_learning_candidates(
            &tenant_key,
            Some(LearningCandidateStatus::NeedsAuthoring),
            200,
        )
        .await?;

    let mut applied = 0usize;
    for filing in file_candidates(&patterns, DEFAULT_MINING_THRESHOLD, &open, tenant_id, now) {
        match filing {
            CandidateFiling::New(candidate) => {
                store.append_learning_candidate(&candidate).await?;
                applied += 1;
            }
            CandidateFiling::Bump(update) => {
                // Conditional on Proposed: a claimed candidate keeps its review state.
                if store
                    .update_learning_candidate_status_from(
                        &update,
                        LearningCandidateStatus::NeedsAuthoring,
                    )
                    .await?
                {
                    applied += 1;
                }
            }
        }
    }
    Ok(applied)
}

/// Clusters raw failure signals into recurrence-counted patterns.
///
/// Clustering is deterministic and model-free: signals group by
/// `(signal, subject)`, evidence references are sorted, deduplicated, and capped
/// at [`MAX_EVIDENCE_REFS`], and the returned patterns are ordered by key.
#[must_use]
pub fn mine_failure_patterns(inputs: &MiningInputs) -> Vec<FailurePattern> {
    let mut clusters: BTreeMap<FailurePatternKey, FailureCluster> = BTreeMap::new();

    for probe in &inputs.failed_probes {
        let key = FailurePatternKey {
            signal: probe.failing_metric.clone(),
            subject: probe.probe_type.clone(),
        };
        let entry = clusters.entry(key).or_default();
        entry.occurrences += 1;
        entry.evidence.insert(probe.probe_id.clone());
    }
    for failure in &inputs.session_failures {
        let key = FailurePatternKey {
            signal: failure.kind.as_str().to_string(),
            subject: failure.subject.clone(),
        };
        let entry = clusters.entry(key).or_default();
        entry.occurrences += 1;
        entry.evidence.insert(failure.event_id.to_string());
        entry
            .events
            .insert((failure.event_id, failure.session_id.0));
    }

    clusters
        .into_iter()
        .map(|(key, cluster)| {
            let surface = surface_for_signal(&key.signal);
            FailurePattern {
                key,
                occurrences: cluster.occurrences,
                evidence: cluster
                    .evidence
                    .into_iter()
                    .take(MAX_EVIDENCE_REFS)
                    .collect(),
                sources: cluster
                    .events
                    .into_iter()
                    .take(MAX_EVIDENCE_REFS)
                    .map(|(event_id, session_id)| LearningCandidateSourceRef::Event {
                        event_id,
                        session_id: SessionId(session_id),
                    })
                    .collect(),
                surface,
            }
        })
        .collect()
}

/// Accumulator for one `(signal, subject)` cluster during a mining pass.
#[derive(Default)]
struct FailureCluster {
    occurrences: usize,
    evidence: BTreeSet<String>,
    events: BTreeSet<(Uuid, Uuid)>,
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
        .filter(|pattern| {
            if pattern.sources.is_empty() {
                // Probe-only clusters name no row in this database, so a filed
                // candidate could never be attributed to a data subject or
                // reached by an erasure. Skipping is the honest outcome; filing
                // an unattributable candidate is not.
                tracing::warn!(
                    pattern_key = %pattern.key.as_string(),
                    occurrences = pattern.occurrences,
                    "skipping weakness-mining candidate with no typed source references"
                );
                return false;
            }
            true
        })
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
        candidate.status == LearningCandidateStatus::NeedsAuthoring
            && candidate
                .payload
                .get("pattern_key")
                .and_then(|value| value.as_str())
                == Some(key)
    })
}

/// Re-states the candidate's own status rather than choosing one.
///
/// A recurrence bump is new evidence, not a review decision. Every mined
/// candidate is an authoring item, so naming any other status here would be a
/// transition the database rejects — and before this task, naming `Proposed`
/// silently moved mined items onto the reviewable queue.
fn bump_update(
    existing: &LearningCandidate,
    pattern: &FailurePattern,
    now: DateTime<Utc>,
) -> LearningCandidateStatusUpdate {
    LearningCandidateStatusUpdate {
        candidate_id: existing.id,
        status: existing.status,
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
        proposal_kind: proposal_kind_for_surface(pattern.surface),
        status: proposal_kind_for_surface(pattern.surface).initial_status(),
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
        sources: pattern.sources.clone(),
        confidence: None,
        risk_class: LearningRiskClass::Medium,
        promotion_requirements: vec!["human_authoring".to_string()],
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

/// Maps an implicated surface to its authoring kind.
///
/// Every mined pattern is authoring work: mining observes that something keeps
/// failing, it does not produce a change anything can apply. None of these kinds
/// is reviewable, so none of them can reach `Promoted`.
fn proposal_kind_for_surface(surface: EditableSurface) -> LearningProposalKind {
    match surface {
        EditableSurface::SkillMarkdown => LearningProposalKind::SkillAuthoring,
        EditableSurface::RewritePromptVersion => LearningProposalKind::PromptAuthoring,
        EditableSurface::RouterRules | EditableSurface::RankingConfig => {
            LearningProposalKind::PolicyAuthoring
        }
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

    fn failure(event_seed: u128, kind: SessionFailureKind, subject: &str) -> SessionFailure {
        SessionFailure {
            event_id: Uuid::from_u128(event_seed),
            session_id: SessionId(Uuid::from_u128(9_000)),
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
                failure(2, SessionFailureKind::DurableToolError, "bash"),
                failure(1, SessionFailureKind::DurableToolError, "bash"),
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
        assert_eq!(
            tool.evidence,
            vec![
                Uuid::from_u128(1).to_string(),
                Uuid::from_u128(2).to_string(),
            ]
        );
        // The typed sources carry the same two events with their session halves, so
        // the derivation is joinable rather than only readable.
        assert_eq!(
            tool.sources,
            vec![
                LearningCandidateSourceRef::Event {
                    event_id: Uuid::from_u128(1),
                    session_id: SessionId(Uuid::from_u128(9_000)),
                },
                LearningCandidateSourceRef::Event {
                    event_id: Uuid::from_u128(2),
                    session_id: SessionId(Uuid::from_u128(9_000)),
                },
            ]
        );
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
                probe("q1", "point_recall", "recall_at_4"),
                probe("q2", "point_recall", "recall_at_4"),
            ],
            session_failures: vec![
                failure(1, SessionFailureKind::DurableToolError, "bash"),
                failure(2, SessionFailureKind::DurableToolError, "bash"),
                failure(3, SessionFailureKind::DurableToolError, "bash"),
            ],
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
        assert_eq!(candidate.candidate_type, LearningCandidateType::Skill);
        assert_eq!(
            candidate.proposal_kind,
            LearningProposalKind::SkillAuthoring
        );
        assert_eq!(
            candidate
                .payload
                .get("pattern_key")
                .and_then(|value| value.as_str()),
            Some("durable_tool_error:bash")
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
            failed_probes: Vec::new(),
            session_failures: vec![
                failure(1, SessionFailureKind::DurableToolError, "bash"),
                failure(2, SessionFailureKind::DurableToolError, "bash"),
                failure(3, SessionFailureKind::DurableToolError, "bash"),
                failure(4, SessionFailureKind::DurableToolError, "bash"),
            ],
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
        // A recurrence bump is new evidence, not a review decision: it restates the
        // authoring status rather than moving the item onto the reviewable queue.
        assert_eq!(update.status, LearningCandidateStatus::NeedsAuthoring);
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
    fn a_probe_only_pattern_is_never_filed_because_it_names_no_row() {
        // Pins: an evaluation probe id names nothing in this database, so a cluster
        // built only from probes cannot be attributed to a data subject and could
        // never be reached by a privacy erasure. It is skipped rather than filed as
        // an unattributable candidate. The identically-sized session-failure cluster
        // beside it proves the threshold is not what rejected it.
        let probe_only = MiningInputs {
            failed_probes: vec![
                probe("p1", "multi_hop", "recall_at_4"),
                probe("p2", "multi_hop", "recall_at_4"),
                probe("p3", "multi_hop", "recall_at_4"),
            ],
            session_failures: Vec::new(),
        };
        let attributable = MiningInputs {
            failed_probes: Vec::new(),
            session_failures: vec![
                failure(1, SessionFailureKind::DurableToolError, "bash"),
                failure(2, SessionFailureKind::DurableToolError, "bash"),
                failure(3, SessionFailureKind::DurableToolError, "bash"),
            ],
        };

        let probe_patterns = mine_failure_patterns(&probe_only);
        assert_eq!(probe_patterns.len(), 1);
        assert_eq!(probe_patterns[0].occurrences, 3);
        assert!(probe_patterns[0].sources.is_empty());
        assert!(
            file_candidates(&probe_patterns, 3, &[], tenant(), Utc::now()).is_empty(),
            "a pattern with no typed source must not become a candidate"
        );

        assert_eq!(
            file_candidates(
                &mine_failure_patterns(&attributable),
                3,
                &[],
                tenant(),
                Utc::now()
            )
            .len(),
            1
        );
    }

    #[test]
    fn mined_candidates_carry_the_exact_source_events_they_were_mined_from() {
        // Pins: the filed candidate references the real event rows, both halves of
        // each partitioned key. Before this, mining stringified event ids into a
        // payload array, so an erasure walking a subject's sessions could not tell
        // that this candidate was built from them.
        let inputs = MiningInputs {
            failed_probes: Vec::new(),
            session_failures: vec![
                failure(1, SessionFailureKind::DurableToolError, "bash"),
                failure(2, SessionFailureKind::DurableToolError, "bash"),
                failure(3, SessionFailureKind::DurableToolError, "bash"),
            ],
        };

        let filings = file_candidates(
            &mine_failure_patterns(&inputs),
            3,
            &[],
            tenant(),
            Utc::now(),
        );
        let CandidateFiling::New(candidate) = &filings[0] else {
            panic!("expected a new candidate");
        };

        assert_eq!(candidate.sources.len(), 3);
        for seed in 1..=3u128 {
            assert!(
                candidate
                    .sources
                    .contains(&LearningCandidateSourceRef::Event {
                        event_id: Uuid::from_u128(seed),
                        session_id: SessionId(Uuid::from_u128(9_000)),
                    })
            );
        }
    }

    #[test]
    fn mining_candidate_id_is_stable_per_tenant_and_pattern_key() {
        // Pins: candidate ids are a pure function of tenant + pattern key, so re-mining the same
        // weakness resolves to one candidate id across runs.
        let inputs = MiningInputs {
            failed_probes: Vec::new(),
            session_failures: vec![
                failure(1, SessionFailureKind::DurableToolError, "bash"),
                failure(2, SessionFailureKind::DurableToolError, "bash"),
                failure(3, SessionFailureKind::DurableToolError, "bash"),
            ],
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

#[cfg(test)]
mod extraction_tests {
    use chrono::Utc;
    use moa_core::{
        events::Event, types::action_policy::ActionClass, types::action_policy::ActionEnvelope,
        types::action_policy::ActionReviewDecision, types::action_policy::ActionReviewField,
        types::action_policy::ActionReviewOwner, types::action_policy::ActionReviewPreview,
        types::action_policy::RiskLevel, types::contact::SessionActorRef,
        types::events_stream::EventRecord, types::identifiers::SessionId,
        types::identifiers::TenantId, types::identifiers::ToolCallId,
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn session_failures_extract_durable_errors_and_denied_reviews_only() {
        // Pins: retryable tool errors and cleared reviews are not failure signals, and a
        // denied review resolves its subject tool through the matching request envelope.
        let session_id = SessionId::new();
        let review_id = Uuid::from_u128(0x51);
        let events = vec![
            record(session_id, 1, tool_error("bash", false)),
            record(session_id, 2, tool_error("bash", true)),
            record(
                session_id,
                3,
                Event::ActionReviewRequested {
                    review_id,
                    envelope: envelope(review_id, "file_write"),
                    preview: preview(),
                },
            ),
            record(
                session_id,
                4,
                Event::ActionReviewDecided {
                    review_id,
                    decision: ActionReviewDecision::Denied { reason: None },
                    decided_by: "admin".to_string(),
                    decided_at: Utc::now(),
                },
            ),
        ];

        let failures = session_failures_from_events(&events);

        assert_eq!(
            failures.len(),
            2,
            "retryable error must not mine: {failures:?}"
        );
        assert_eq!(failures[0].kind, SessionFailureKind::DurableToolError);
        assert_eq!(failures[0].subject, "bash");
        assert_eq!(failures[1].kind, SessionFailureKind::RejectedApproval);
        assert_eq!(
            failures[1].subject, "file_write",
            "denied review subject comes from the request envelope"
        );
    }

    #[test]
    fn session_failures_ignore_cleared_reviews() {
        // Pins: a cleared review is a success signal and must not mine as a failure.
        let session_id = SessionId::new();
        let review_id = Uuid::from_u128(0x52);
        let events = vec![
            record(
                session_id,
                1,
                Event::ActionReviewRequested {
                    review_id,
                    envelope: envelope(review_id, "file_write"),
                    preview: preview(),
                },
            ),
            record(
                session_id,
                2,
                Event::ActionReviewDecided {
                    review_id,
                    decision: ActionReviewDecision::Cleared,
                    decided_by: "admin".to_string(),
                    decided_at: Utc::now(),
                },
            ),
        ];

        assert!(session_failures_from_events(&events).is_empty());
    }

    fn tool_error(tool_name: &str, retryable: bool) -> Event {
        Event::ToolError {
            tool_id: ToolCallId::new(),
            provider_tool_use_id: None,
            tool_name: tool_name.to_string(),
            error: "boom".to_string(),
            retryable,
        }
    }

    fn envelope(review_id: Uuid, tool_name: &str) -> ActionEnvelope {
        ActionEnvelope {
            review_id,
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            requested_by: SessionActorRef::Identity {
                id: Uuid::from_u128(2),
            },
            owner: ActionReviewOwner::Coordinator {
                session_id: SessionId::new(),
                turn_id: format!("turn-{review_id}"),
                generation: 1,
            },
            tool_call_id: ToolCallId::from(review_id),
            tool_name: tool_name.to_string(),
            normalized_input: "input".to_string(),
            input_summary: "input".to_string(),
            risk_level: RiskLevel::Medium,
            action_class: ActionClass::LocalWrite,
            origin_kind: None,
            origin_id: None,
            origin_step_id: None,
            idempotency_key: None,
            created_at: Utc::now(),
        }
    }

    fn preview() -> ActionReviewPreview {
        ActionReviewPreview {
            fields: vec![ActionReviewField {
                label: "Path".to_string(),
                value: "input".to_string(),
            }],
            file_diffs: Vec::new(),
        }
    }

    fn record(session_id: SessionId, sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::from_u128(0x9100 + u128::from(sequence_num)),
            session_id,
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }
}
