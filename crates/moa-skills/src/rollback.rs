//! Post-promotion skill-regression detection and rollback-proposal filing.
//!
//! Promotion is all-or-nothing and irreversible today: a skill that regresses
//! after it is published only shows up as a slowly declining resolution rate.
//! This module closes that loop deterministically and model-free. The pure
//! passes below compare a promoted skill's post-promotion resolution rate against
//! a baseline — the same skill's pre-promotion rate for an improved skill, or an
//! absolute floor for a newly created one — and, on regression, construct a
//! `Proposed` rollback [`LearningCandidate`]. [`monitor_and_file_skill_regressions`]
//! is the store-coupled driver that runs the comparison per tenant and files or
//! bumps one open proposal per `(tenant, skill)`, mirroring the mining split of
//! pure logic from I/O. Executing a filed proposal stays a human-reviewed
//! operation; nothing here changes what is served.

use chrono::{DateTime, Duration, Utc};
use moa_config::RegressionMonitorConfig;
use moa_core::types::experience::{
    LearningCandidate, LearningCandidateStatus, LearningCandidateStatusUpdate,
    LearningCandidateType, LearningRiskClass,
};
use moa_core::types::identifiers::TenantId;
use moa_session::{PostgresSessionStore, RecentSkillPromotion, SkillResolutionSample};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Learning-log operation recorded when an existing skill was improved.
pub const OPERATION_SKILL_IMPROVED: &str = "skill_improved";
/// Learning-log operation recorded when a new skill was created.
pub const OPERATION_SKILL_CREATED: &str = "skill_created";
/// Payload discriminator marking a rollback proposal candidate.
pub const ROLLBACK_PROPOSAL_KIND: &str = "skill_rollback_proposal";

/// Thresholds governing regression detection, projected from configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollbackThresholds {
    /// Minimum post-promotion used-segment count before a skill is judged.
    pub min_samples: u64,
    /// Regression margin below the pre-promotion baseline for improved skills.
    pub regression_delta: f64,
    /// Absolute resolution-rate floor for created skills with no history.
    pub created_floor: f64,
    /// Lookback window in days, recorded in the proposal evidence.
    pub lookback_days: i64,
}

impl RollbackThresholds {
    /// Projects detection thresholds from the runtime monitor configuration.
    #[must_use]
    pub fn from_config(config: &RegressionMonitorConfig) -> Self {
        Self {
            min_samples: config.min_samples as u64,
            regression_delta: config.regression_delta,
            created_floor: config.created_floor,
            lookback_days: config.lookback_days,
        }
    }
}

/// A detected regression: a promotion whose post-promotion rate fell below its baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct RollbackDecision {
    /// Promotion that regressed.
    pub promotion: RecentSkillPromotion,
    /// Post-promotion resolution sample.
    pub post: SkillResolutionSample,
    /// Pre-promotion baseline sample, absent for created skills.
    pub baseline: Option<SkillResolutionSample>,
    /// Lookback window in days used for the comparison.
    pub lookback_days: i64,
}

/// One filing decision produced by [`file_rollback_candidates`].
#[derive(Debug, Clone, PartialEq)]
pub enum RollbackFiling {
    /// A new rollback proposal to append.
    New(Box<LearningCandidate>),
    /// An evidence refresh for an existing open proposal on the same skill.
    Bump(LearningCandidateStatusUpdate),
}

/// Decides whether one promotion regressed, given its post/baseline samples.
///
/// Abstains (returns `None`) until the skill accumulates at least
/// `thresholds.min_samples` post-promotion used segments, so a rollback is never
/// proposed on noise. An improved skill regressed when its post-promotion rate
/// falls below its pre-promotion baseline by more than `regression_delta`, and
/// only when a baseline with evidence exists; a created skill, which has no
/// history, regressed when its post-promotion rate falls below `created_floor`.
#[must_use]
pub fn evaluate_regression(
    promotion: &RecentSkillPromotion,
    post: SkillResolutionSample,
    baseline: Option<SkillResolutionSample>,
    thresholds: &RollbackThresholds,
) -> Option<RollbackDecision> {
    if post.samples < thresholds.min_samples {
        return None;
    }
    let regressed = match promotion.operation.as_str() {
        OPERATION_SKILL_IMPROVED => match baseline {
            Some(baseline) if baseline.samples > 0 => {
                post.rate < baseline.rate - thresholds.regression_delta
            }
            // An improved skill with no measurable pre-promotion baseline cannot
            // be judged against its own history; abstain rather than guess.
            _ => false,
        },
        OPERATION_SKILL_CREATED => post.rate < thresholds.created_floor,
        _ => false,
    };
    if !regressed {
        return None;
    }
    Some(RollbackDecision {
        promotion: promotion.clone(),
        post,
        baseline,
        lookback_days: thresholds.lookback_days,
    })
}

/// Files a rollback proposal per regression, bumping an existing open proposal.
///
/// Exactly one open proposal is kept per `(tenant, skill)`: a regression whose
/// skill already has an open (`Proposed` or `Evaluating`) proposal yields a
/// [`RollbackFiling::Bump`] refreshing its evidence, and the bump is conditional
/// on `Proposed`, so a proposal a reviewer already claimed (`Evaluating`) is left
/// untouched and never duplicated. A skill with no open proposal yields a fresh
/// [`RollbackFiling::New`] with a deterministic id.
#[must_use]
pub fn file_rollback_candidates(
    decisions: &[RollbackDecision],
    open_candidates: &[LearningCandidate],
    now: DateTime<Utc>,
) -> Vec<RollbackFiling> {
    decisions
        .iter()
        .map(|decision| {
            match open_rollback_for_skill(open_candidates, &decision.promotion.skill_name) {
                Some(existing) => RollbackFiling::Bump(bump_update(existing, decision, now)),
                None => RollbackFiling::New(Box::new(rollback_candidate(decision, now))),
            }
        })
        .collect()
}

/// Returns the deterministic candidate id for one skill's rollback proposal.
///
/// A pure function of tenant, skill, and the promoted revision, so re-observing
/// the same regression resolves to one candidate id across monitor runs.
#[must_use]
pub fn rollback_candidate_id(
    tenant_id: TenantId,
    skill_name: &str,
    promoted_revision_uid: Uuid,
) -> Uuid {
    let mut hasher = Sha256::new();
    for part in [
        "moa.skill.rollback_proposal.v1",
        &tenant_id.to_string(),
        skill_name,
        &promoted_revision_uid.to_string(),
    ] {
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

fn open_rollback_for_skill<'a>(
    open_candidates: &'a [LearningCandidate],
    skill_name: &str,
) -> Option<&'a LearningCandidate> {
    open_candidates.iter().find(|candidate| {
        matches!(
            candidate.status,
            LearningCandidateStatus::Proposed | LearningCandidateStatus::Evaluating
        ) && candidate.payload.get("kind").and_then(Value::as_str) == Some(ROLLBACK_PROPOSAL_KIND)
            && candidate
                .payload
                .get("rollback_key")
                .and_then(Value::as_str)
                == Some(skill_name)
    })
}

fn evidence_payload(decision: &RollbackDecision) -> Value {
    let baseline_rate = decision.baseline.map(|sample| sample.rate);
    let baseline_samples = decision.baseline.map_or(0, |sample| sample.samples);
    json!({
        "kind": ROLLBACK_PROPOSAL_KIND,
        "rollback_key": decision.promotion.skill_name,
        "skill_name": decision.promotion.skill_name,
        "artifact_uid": decision.promotion.artifact_uid,
        "promoted_revision_uid": decision.promotion.promoted_revision_uid,
        "previous_revision_uid": decision.promotion.previous_revision_uid,
        "promotion_candidate_id": decision.promotion.promotion_candidate_id,
        "regressed_operation": decision.promotion.operation,
        "post_resolution_rate": decision.post.rate,
        "post_samples": decision.post.samples,
        "baseline_resolution_rate": baseline_rate,
        "baseline_samples": baseline_samples,
        "lookback_days": decision.lookback_days,
    })
}

fn regression_description(decision: &RollbackDecision) -> String {
    match decision.baseline {
        Some(baseline) => format!(
            "skill `{}` regressed after promotion: post-promotion resolution {:.3} over {} \
             segment(s) fell below its pre-promotion baseline {:.3}",
            decision.promotion.skill_name, decision.post.rate, decision.post.samples, baseline.rate,
        ),
        None => format!(
            "created skill `{}` underperformed after promotion: resolution {:.3} over {} \
             segment(s) fell below the created-skill floor",
            decision.promotion.skill_name, decision.post.rate, decision.post.samples,
        ),
    }
}

fn rollback_candidate(decision: &RollbackDecision, now: DateTime<Utc>) -> LearningCandidate {
    let description = regression_description(decision);
    let mut payload = evidence_payload(decision);
    payload["description"] = json!(description);
    LearningCandidate {
        id: rollback_candidate_id(
            decision.promotion.tenant_id,
            &decision.promotion.skill_name,
            decision.promotion.promoted_revision_uid,
        ),
        tenant_id: decision.promotion.tenant_id,
        user_id: None,
        candidate_type: LearningCandidateType::Skill,
        status: LearningCandidateStatus::Proposed,
        target_id: Some(decision.promotion.artifact_uid.to_string()),
        target_label: Some(decision.promotion.skill_name.clone()),
        task_fingerprint: None,
        task_facets: None,
        payload,
        evaluation_payload: None,
        source_experience_ids: Vec::new(),
        confidence: None,
        // Rolling back a live, serving skill has a larger blast radius than a
        // draft promotion, so the proposal is filed at high risk.
        risk_class: LearningRiskClass::High,
        promotion_requirements: vec!["human_review".to_string()],
        status_reason: Some(description),
        batch_id: None,
        created_at: now,
        updated_at: now,
    }
}

fn bump_update(
    existing: &LearningCandidate,
    decision: &RollbackDecision,
    now: DateTime<Utc>,
) -> LearningCandidateStatusUpdate {
    LearningCandidateStatusUpdate {
        candidate_id: existing.id,
        status: LearningCandidateStatus::Proposed,
        status_reason: Some(format!(
            "skill `{}` regression re-observed: post-promotion resolution {:.3} over {} segment(s)",
            decision.promotion.skill_name, decision.post.rate, decision.post.samples,
        )),
        evaluation_payload: Some(evidence_payload(decision)),
        updated_at: now,
    }
}

/// Monitors a lookback window of skill promotions and files rollback proposals.
///
/// The store-coupled driver over the pure passes: for each tenant with a recent
/// promotion, it compares every promoted skill's post-promotion resolution rate
/// against its baseline and files or bumps one open rollback proposal per
/// regressed `(tenant, skill)`. A claimed (`Evaluating`) proposal is never
/// disturbed. Returns the number of proposals filed or bumped.
pub async fn monitor_and_file_skill_regressions(
    store: &PostgresSessionStore,
    config: &RegressionMonitorConfig,
    now: DateTime<Utc>,
) -> moa_core::error::Result<usize> {
    let thresholds = RollbackThresholds::from_config(config);
    let since = now - Duration::days(config.lookback_days.max(0));
    let tenants = store
        .list_tenants_with_recent_skill_promotions(since)
        .await?;

    let mut applied = 0usize;
    for tenant_id in tenants {
        let promotions = store
            .list_recent_skill_promotions(&tenant_id, since)
            .await?;
        let mut decisions = Vec::new();
        for promotion in promotions {
            let post = store
                .skill_resolution_rate_in_window(
                    &tenant_id,
                    &promotion.skill_name,
                    promotion.promoted_at,
                    None,
                )
                .await?;
            let baseline = if promotion.operation == OPERATION_SKILL_IMPROVED {
                let baseline_start =
                    promotion.promoted_at - Duration::days(config.lookback_days.max(0));
                Some(
                    store
                        .skill_resolution_rate_in_window(
                            &tenant_id,
                            &promotion.skill_name,
                            baseline_start,
                            Some(promotion.promoted_at),
                        )
                        .await?,
                )
            } else {
                None
            };
            if let Some(decision) = evaluate_regression(&promotion, post, baseline, &thresholds) {
                decisions.push(decision);
            }
        }
        if decisions.is_empty() {
            continue;
        }

        let open = open_rollback_candidates(store, &tenant_id).await?;
        for filing in file_rollback_candidates(&decisions, &open, now) {
            match filing {
                RollbackFiling::New(candidate) => {
                    store.append_learning_candidate(&candidate).await?;
                    applied += 1;
                    tracing::info!(
                        tenant_id = %tenant_id,
                        skill = %candidate.target_label.as_deref().unwrap_or_default(),
                        candidate_id = %candidate.id,
                        "skill_regression_rollback_proposal_filed"
                    );
                }
                RollbackFiling::Bump(update) => {
                    // Conditional on Proposed: a claimed proposal keeps its state.
                    if store
                        .update_learning_candidate_status_from(
                            &update,
                            LearningCandidateStatus::Proposed,
                        )
                        .await?
                    {
                        applied += 1;
                        tracing::info!(
                            tenant_id = %tenant_id,
                            candidate_id = %update.candidate_id,
                            "skill_regression_rollback_proposal_bumped"
                        );
                    }
                }
            }
        }
    }
    Ok(applied)
}

/// Lists a tenant's open (proposed or claimed) rollback proposals.
async fn open_rollback_candidates(
    store: &PostgresSessionStore,
    tenant_id: &TenantId,
) -> moa_core::error::Result<Vec<LearningCandidate>> {
    let tenant_key = tenant_id.to_string();
    let mut open = store
        .list_learning_candidates(&tenant_key, Some(LearningCandidateStatus::Proposed), 200)
        .await?;
    open.extend(
        store
            .list_learning_candidates(&tenant_key, Some(LearningCandidateStatus::Evaluating), 200)
            .await?,
    );
    open.retain(|candidate| {
        candidate.payload.get("kind").and_then(Value::as_str) == Some(ROLLBACK_PROPOSAL_KIND)
    });
    Ok(open)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant() -> TenantId {
        TenantId::from(Uuid::from_u128(9))
    }

    fn thresholds() -> RollbackThresholds {
        RollbackThresholds::from_config(&RegressionMonitorConfig::default())
    }

    fn promotion(operation: &str) -> RecentSkillPromotion {
        RecentSkillPromotion {
            tenant_id: tenant(),
            skill_name: "deploy-runbook".to_string(),
            operation: operation.to_string(),
            artifact_uid: Uuid::from_u128(0xA1),
            promoted_revision_uid: Uuid::from_u128(0xB2),
            previous_revision_uid: Some(Uuid::from_u128(0xC3)),
            promotion_candidate_id: Uuid::from_u128(0xD4),
            promoted_at: Utc::now(),
        }
    }

    fn sample(rate: f64, samples: u64) -> SkillResolutionSample {
        SkillResolutionSample { rate, samples }
    }

    #[test]
    fn improved_skill_regresses_only_below_baseline_minus_delta() {
        // Pins: an improved skill regresses when its post rate falls more than
        // regression_delta below its baseline, and not merely below it.
        let promotion = promotion(OPERATION_SKILL_IMPROVED);
        let thresholds = thresholds();

        // Baseline 0.9, delta 0.2 → regression threshold is 0.7.
        let regressed = evaluate_regression(
            &promotion,
            sample(0.6, 8),
            Some(sample(0.9, 10)),
            &thresholds,
        );
        assert!(regressed.is_some(), "0.6 < 0.9 - 0.2 must regress");

        let within_margin = evaluate_regression(
            &promotion,
            sample(0.75, 8),
            Some(sample(0.9, 10)),
            &thresholds,
        );
        assert!(
            within_margin.is_none(),
            "0.75 is within the delta margin and must not regress"
        );
    }

    #[test]
    fn improved_skill_without_baseline_evidence_abstains() {
        // Pins: with no measurable pre-promotion baseline an improved skill cannot
        // be judged against its own history and the monitor abstains.
        let promotion = promotion(OPERATION_SKILL_IMPROVED);
        assert!(
            evaluate_regression(
                &promotion,
                sample(0.0, 8),
                Some(sample(0.0, 0)),
                &thresholds()
            )
            .is_none()
        );
        assert!(evaluate_regression(&promotion, sample(0.0, 8), None, &thresholds()).is_none());
    }

    #[test]
    fn created_skill_regresses_below_absolute_floor() {
        // Pins: a created skill with no history regresses against the absolute
        // floor (0.3), independent of any baseline.
        let promotion = promotion(OPERATION_SKILL_CREATED);
        assert!(
            evaluate_regression(&promotion, sample(0.2, 6), None, &thresholds()).is_some(),
            "0.2 < 0.3 floor must regress"
        );
        assert!(
            evaluate_regression(&promotion, sample(0.4, 6), None, &thresholds()).is_none(),
            "0.4 >= 0.3 floor must not regress"
        );
    }

    #[test]
    fn below_min_samples_never_regresses() {
        // Pins: a skill with too few post-promotion used segments is never judged,
        // regardless of how bad the rate looks.
        let promotion = promotion(OPERATION_SKILL_CREATED);
        assert!(
            evaluate_regression(&promotion, sample(0.0, 4), None, &thresholds()).is_none(),
            "4 samples is below the default min of 5"
        );
    }

    #[test]
    fn rollback_candidate_id_is_stable_per_skill_and_revision() {
        // Pins: the candidate id is a pure function of tenant, skill, and promoted
        // revision, so the same regression resolves to one id across runs.
        let first = rollback_candidate_id(tenant(), "deploy-runbook", Uuid::from_u128(0xB2));
        let second = rollback_candidate_id(tenant(), "deploy-runbook", Uuid::from_u128(0xB2));
        let other = rollback_candidate_id(tenant(), "deploy-runbook", Uuid::from_u128(0xB3));
        assert_eq!(first, second);
        assert_ne!(first, other, "a different revision yields a different id");
    }

    #[test]
    fn new_regression_files_a_proposed_candidate_with_full_evidence() {
        // Pins: a first regression files a Proposed skill candidate carrying the
        // revisions to archive and restore plus the observed rates.
        let decision = evaluate_regression(
            &promotion(OPERATION_SKILL_IMPROVED),
            sample(0.5, 9),
            Some(sample(0.9, 12)),
            &thresholds(),
        )
        .expect("regression detected");
        let filings = file_rollback_candidates(&[decision], &[], Utc::now());

        assert_eq!(filings.len(), 1);
        let RollbackFiling::New(candidate) = &filings[0] else {
            panic!("expected a new candidate");
        };
        assert_eq!(candidate.candidate_type, LearningCandidateType::Skill);
        assert_eq!(candidate.status, LearningCandidateStatus::Proposed);
        assert_eq!(candidate.risk_class, LearningRiskClass::High);
        assert_eq!(
            candidate.payload.get("kind").and_then(Value::as_str),
            Some(ROLLBACK_PROPOSAL_KIND)
        );
        assert_eq!(
            candidate
                .payload
                .get("promoted_revision_uid")
                .and_then(Value::as_str),
            Some(Uuid::from_u128(0xB2).to_string().as_str())
        );
        assert_eq!(
            candidate
                .payload
                .get("previous_revision_uid")
                .and_then(Value::as_str),
            Some(Uuid::from_u128(0xC3).to_string().as_str())
        );
        assert_eq!(
            candidate
                .payload
                .get("post_samples")
                .and_then(Value::as_u64),
            Some(9)
        );
    }

    #[test]
    fn reobserved_regression_bumps_open_proposal_without_duplicating() {
        // Pins: a regression whose skill already has an open proposal bumps that
        // proposal's evidence instead of filing a duplicate.
        let decision = evaluate_regression(
            &promotion(OPERATION_SKILL_CREATED),
            sample(0.1, 8),
            None,
            &thresholds(),
        )
        .expect("regression detected");
        let existing =
            match &file_rollback_candidates(std::slice::from_ref(&decision), &[], Utc::now())[0] {
                RollbackFiling::New(candidate) => (**candidate).clone(),
                RollbackFiling::Bump(_) => panic!("first filing must be new"),
            };

        let filings =
            file_rollback_candidates(&[decision], std::slice::from_ref(&existing), Utc::now());
        assert_eq!(filings.len(), 1);
        let RollbackFiling::Bump(update) = &filings[0] else {
            panic!("re-observed regression must bump");
        };
        assert_eq!(update.candidate_id, existing.id);
        assert_eq!(update.status, LearningCandidateStatus::Proposed);
    }

    #[test]
    fn claimed_proposal_is_bumped_conditionally_not_reopened() {
        // Pins: a proposal a reviewer already claimed (Evaluating) matches as the
        // open proposal so no duplicate is filed; the emitted Bump targets it and
        // is compare-and-set against Proposed, leaving the claim untouched at apply.
        let decision = evaluate_regression(
            &promotion(OPERATION_SKILL_CREATED),
            sample(0.1, 8),
            None,
            &thresholds(),
        )
        .expect("regression detected");
        let mut claimed =
            match &file_rollback_candidates(std::slice::from_ref(&decision), &[], Utc::now())[0] {
                RollbackFiling::New(candidate) => (**candidate).clone(),
                RollbackFiling::Bump(_) => panic!("first filing must be new"),
            };
        claimed.status = LearningCandidateStatus::Evaluating;

        let filings =
            file_rollback_candidates(&[decision], std::slice::from_ref(&claimed), Utc::now());
        let RollbackFiling::Bump(update) = &filings[0] else {
            panic!("a claimed proposal must still dedupe to a bump, not a new candidate");
        };
        assert_eq!(update.candidate_id, claimed.id);
        assert_eq!(
            update.status,
            LearningCandidateStatus::Proposed,
            "the bump is compare-and-set against Proposed so an Evaluating claim is untouched"
        );
    }
}
