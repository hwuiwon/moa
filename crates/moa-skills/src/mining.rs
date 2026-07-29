//! Deterministic weakness mining: cluster eval and session failure signals into
//! recurrence-thresholded [`LearningCandidate`] intents without any model call.
//!
//! The pass is pure and offline. [`mine_failure_patterns`] clusters raw failure
//! signals into recurrence-counted patterns; [`file_candidates`] turns patterns
//! that cross a threshold into immutable learning candidates, deduplicating
//! against open candidates for the same pattern key. No LLM is involved: candidate
//! descriptions are assembled from the pattern key and its counts.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use moa_core::{
    types::experience::LearningCandidate, types::experience::LearningCandidateSourceRef,
    types::experience::LearningCandidateStatus, types::experience::LearningCandidateType,
    types::experience::LearningProposalKind, types::experience::LearningRiskClass,
    types::identifiers::SessionId, types::identifiers::TenantId,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::proposals::EditableSurface;

/// Default recurrence threshold: a pattern files a candidate once it recurs this many times.
pub const DEFAULT_MINING_THRESHOLD: usize = 3;

/// Maximum evidence references retained per pattern (stable ids only, never transcripts).
const MAX_EVIDENCE_REFS: usize = 20;

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

/// Cluster key: the verifier signal paired with the failing surface subject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FailurePatternKey {
    /// Verifier signal: a session failure kind.
    pub signal: String,
    /// Failing surface subject: a tool name or turn tag.
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
    /// Bounded, sorted source event identifiers.
    pub evidence: Vec<String>,
    /// Typed database references behind the evidence, in the same bounded order.
    ///
    /// Every mined signal is an event, so a filed candidate always has typed
    /// provenance a privacy erasure can follow.
    pub sources: Vec<LearningCandidateSourceRef>,
    /// Editable surface this pattern implicates, derived deterministically from the signal.
    pub surface: EditableSurface,
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

/// Mines a session event window's failure signals and files new candidates.
///
/// The store-coupled application of the pure passes below: extract signals,
/// cluster by recurrence, then file one reviewable candidate per pattern key.
/// A deterministic ID makes every previously filed candidate immutable: later
/// observations remain in their source events instead of rewriting review state.
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
    let patterns = mine_failure_patterns(&session_failures);

    let tenant_key = tenant_id.to_string();
    let open = store
        .list_learning_candidates(
            &tenant_key,
            Some(LearningCandidateStatus::NeedsAuthoring),
            200,
        )
        .await?;

    let mut applied = 0usize;
    for candidate in file_candidates(&patterns, DEFAULT_MINING_THRESHOLD, &open, tenant_id, now) {
        applied += usize::from(
            store
                .insert_learning_candidate_if_absent(&candidate)
                .await?,
        );
    }
    Ok(applied)
}

/// Clusters raw failure signals into recurrence-counted patterns.
///
/// Clustering is deterministic and model-free: signals group by
/// `(signal, subject)`, evidence references are sorted, deduplicated, and capped
/// at [`MAX_EVIDENCE_REFS`], and the returned patterns are ordered by key.
#[must_use]
pub fn mine_failure_patterns(session_failures: &[SessionFailure]) -> Vec<FailurePattern> {
    let mut clusters: BTreeMap<FailurePatternKey, FailureCluster> = BTreeMap::new();

    for failure in session_failures {
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
/// `pattern_key`) is left unchanged. New observations remain in their source
/// events rather than mutating filed candidate evidence without adding matching
/// typed source rows.
#[must_use]
pub fn file_candidates(
    patterns: &[FailurePattern],
    threshold: usize,
    open_candidates: &[LearningCandidate],
    tenant_id: TenantId,
    now: DateTime<Utc>,
) -> Vec<LearningCandidate> {
    patterns
        .iter()
        .filter(|pattern| pattern.occurrences >= threshold)
        .filter(|pattern| !pattern.sources.is_empty())
        .filter(|pattern| {
            open_candidate_for_key(open_candidates, &pattern.key.as_string()).is_none()
        })
        .map(|pattern| {
            let key = pattern.key.as_string();
            mining_candidate(tenant_id, pattern, &key, now)
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

    fn failure(event_seed: u128, kind: SessionFailureKind, subject: &str) -> SessionFailure {
        SessionFailure {
            event_id: Uuid::from_u128(event_seed),
            session_id: SessionId(Uuid::from_u128(9_000)),
            kind,
            subject: subject.to_string(),
        }
    }

    #[test]
    fn mining_clusters_events_with_bounded_sorted_typed_sources() {
        // Pins: recurrence counting and the evidence bound are derived from the
        // same typed event rows, so no retained evidence can lack erasure provenance.
        let failures = (1..=25)
            .map(|event_seed| failure(event_seed, SessionFailureKind::DurableToolError, "bash"))
            .collect::<Vec<_>>();

        let patterns = mine_failure_patterns(&failures);

        assert_eq!(patterns.len(), 1);
        let pattern = &patterns[0];
        assert_eq!(pattern.key.as_string(), "durable_tool_error:bash");
        assert_eq!(pattern.occurrences, 25);
        assert_eq!(pattern.evidence.len(), MAX_EVIDENCE_REFS);
        assert_eq!(pattern.sources.len(), MAX_EVIDENCE_REFS);
        assert_eq!(
            pattern.sources[0],
            LearningCandidateSourceRef::Event {
                event_id: Uuid::from_u128(1),
                session_id: SessionId(Uuid::from_u128(9_000)),
            }
        );
    }

    #[test]
    fn filed_candidate_is_immutable_when_the_pattern_recurs() {
        // Pins: re-observing an open pattern files no update. The new evidence
        // remains in source events instead of being copied into candidate JSON
        // without matching typed source rows.
        let failures = vec![
            failure(1, SessionFailureKind::DurableToolError, "bash"),
            failure(2, SessionFailureKind::DurableToolError, "bash"),
            failure(3, SessionFailureKind::DurableToolError, "bash"),
        ];
        let patterns = mine_failure_patterns(&failures);
        let now = Utc::now();
        let first = file_candidates(&patterns, 3, &[], tenant(), now);
        assert_eq!(first.len(), 1);
        let filed = first[0].clone();
        assert_eq!(filed.sources.len(), 3);

        let repeated = file_candidates(&patterns, 3, std::slice::from_ref(&filed), tenant(), now);

        assert!(
            repeated.is_empty(),
            "an open filed candidate must not be rewritten with untracked evidence"
        );
        assert_eq!(filed.evaluation_payload, None);
        assert_eq!(filed.payload["pattern_occurrences"], 3);
    }

    #[test]
    fn filing_requires_the_recurrence_threshold() {
        // Pins: two observations stay below the default threshold while the third
        // files exactly one authoring candidate on the implicated surface.
        let two = vec![
            failure(1, SessionFailureKind::DurableToolError, "bash"),
            failure(2, SessionFailureKind::DurableToolError, "bash"),
        ];
        assert!(
            file_candidates(
                &mine_failure_patterns(&two),
                DEFAULT_MINING_THRESHOLD,
                &[],
                tenant(),
                Utc::now(),
            )
            .is_empty()
        );

        let mut three = two;
        three.push(failure(3, SessionFailureKind::DurableToolError, "bash"));
        let filed = file_candidates(
            &mine_failure_patterns(&three),
            DEFAULT_MINING_THRESHOLD,
            &[],
            tenant(),
            Utc::now(),
        );
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].proposal_kind, LearningProposalKind::SkillAuthoring);
        assert_eq!(filed[0].status, LearningCandidateStatus::NeedsAuthoring);
    }
}
