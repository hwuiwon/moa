//! Types the release-evaluation dispatch owns.
//!
//! Three of them carry the workstream's fail-closed rules in the type system
//! rather than in a check somewhere downstream:
//!
//! * [`ScenarioSource`] has no variant that can hold conversational content, so a
//!   raw transcript is unrepresentable as scenario or persona input. A `learned`
//!   source must carry [`SanitizedScenarioEvidence`], whose four fields are the
//!   contribution, retention, consent, and erasure provenance the plan demands.
//! * [`ReleaseCase`] denies unknown fields, so a case body that smuggles a
//!   `transcript` key fails to deserialize before it ever reaches an assertion --
//!   the same rule the `V000374` check constraint states in the schema.
//! * [`CohortVisibility`] separates the authoring cases a tenant may see from the
//!   hidden release cohort that decides, and [`MergedCasePlan`] accepts only the
//!   platform-owned plan revision shared by both packs.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use moa_artifacts::release::{ActivationTargetClass, Digest32};
use moa_artifacts::simulation::MAX_PLAN_TRIALS_PER_COMBINATION;
use moa_core::types::identifiers::TenantId;
use moa_eval_core::assertion::{AssertionCategory, AssertionSpec, GateEffect};
use moa_eval_core::evaluators::action_assertions::ProhibitedActionsConfig;
use moa_eval_core::evaluators::{PROHIBITED_ACTIONS_EVALUATOR_ID, TEXT_MATCH_EVALUATOR_ID};
use moa_eval_core::types::ExpectedOutput;
use moa_wire::experiments::ArtifactReleaseExperimentCase;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Error;

/// Which arm of a paired release evaluation a row belongs to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArmRole {
    /// The unpublished candidate under evaluation.
    Candidate,
    /// The revision that was serving when the candidate was submitted.
    Baseline,
}

impl ArmRole {
    /// Returns the lowercase database label for this arm.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Baseline => "baseline",
        }
    }
}

impl fmt::Display for ArmRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Lifecycle of one durable dispatch record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchStatus {
    /// Enqueued by the submission transaction; no run started yet.
    Pending,
    /// The evaluation workflow claimed it and started at least one arm.
    Dispatched,
    /// A deterministic decision consumed it.
    Settled,
    /// The dispatch can decide nothing because its subject was superseded or its
    /// evaluation failed terminally before producing evidence.
    Abandoned,
}

impl DispatchStatus {
    /// Returns the lowercase database label for this status.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Dispatched => "dispatched",
            Self::Settled => "settled",
            Self::Abandoned => "abandoned",
        }
    }

    /// Returns whether a result for this record may still decide a release.
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Pending | Self::Dispatched)
    }
}

impl FromStr for DispatchStatus {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        match value {
            "pending" => Ok(Self::Pending),
            "dispatched" => Ok(Self::Dispatched),
            "settled" => Ok(Self::Settled),
            "abandoned" => Ok(Self::Abandoned),
            other => Err(Error::Storage(format!(
                "`{other}` is not a release dispatch status"
            ))),
        }
    }
}

impl fmt::Display for DispatchStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A draft dependency the submitter pinned for evaluation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PinnedDependency {
    /// Artifact whose resolution the overlay overrides.
    pub artifact_uid: Uuid,
    /// Exact revision the overlay resolves that artifact to.
    pub revision_uid: Uuid,
}

/// Whether a case pack is tenant-visible authoring signal or the hidden gate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CohortVisibility {
    /// Visible to the tenant; exists so iteration has feedback.
    Authoring,
    /// The release cohort. Never returned by a tenant-facing handler.
    Hidden,
}

impl CohortVisibility {
    /// Returns the lowercase database label for this visibility.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Authoring => "authoring",
            Self::Hidden => "hidden",
        }
    }
}

impl FromStr for CohortVisibility {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        match value {
            "authoring" => Ok(Self::Authoring),
            "hidden" => Ok(Self::Hidden),
            other => Err(Error::Storage(format!(
                "`{other}` is not a release cohort visibility"
            ))),
        }
    }
}

/// Provenance a learned scenario or persona input must carry.
///
/// Every field is a question an erasure request will ask later: which
/// contribution produced this, how long may it be kept, on what basis was it
/// collected, and where does its erasure get recorded. A learned input without
/// all four is not weaker evidence, it is unerasable evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SanitizedScenarioEvidence {
    /// Contribution row the sanitized evidence came from.
    pub contribution_uid: Uuid,
    /// Retention class governing how long the derived case may be kept.
    pub retention_class: String,
    /// Legal basis under which the source was collected.
    pub consent_basis: String,
    /// Where erasure of the source is recorded.
    pub erasure_provenance: String,
}

/// Where a case pack's scenario and persona content came from.
///
/// There is deliberately no variant carrying transcript, message, event, or turn
/// content. A pack that wants conversational context names a persona and points at
/// sanitized evidence; it cannot embed the conversation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScenarioSource {
    /// Platform-authored pack. Has no data subject, so it needs no provenance.
    ApprovedPack,
    /// Derived from sanitized learning evidence, with full provenance.
    Learned {
        /// Contribution, retention, consent, and erasure provenance.
        evidence: SanitizedScenarioEvidence,
    },
}

/// One case in an approved pack.
///
/// `deny_unknown_fields` is the point: a case body carrying `transcript`,
/// `messages`, `events`, or `turns` fails to deserialize here, which is the Rust
/// half of the `V000374` schema check.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseCase {
    /// Stable case identifier, unique within a pack.
    pub case_id: String,
    /// Persona the simulator drives for this case.
    pub persona_ref: String,
    /// Named execution profile the case runs under.
    pub profile: String,
    /// How many paired repetitions this case contributes.
    pub repetitions: i32,
    /// Versioned, data-only assertions this case evaluates.
    pub assertions: Vec<AssertionSpec>,
}

/// A versioned, server-resolved approved plan/scenario pack.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReleaseCasePack {
    /// Pack row identifier.
    pub pack_uid: Uuid,
    /// Operator-facing pack name.
    pub name: String,
    /// Pack revision; an edit is a new revision, never an in-place change.
    pub revision: i32,
    /// Whether this pack is authoring signal or the hidden cohort.
    pub visibility: CohortVisibility,
    /// Rotation epoch of the hidden cohort.
    pub cohort_epoch: i32,
    /// How wide a window of `cases` one epoch exposes, for hidden packs.
    pub cohort_size: Option<i32>,
    /// When this hidden cohort must rotate.
    pub rotates_at: Option<DateTime<Utc>>,
    /// How many attempts one artifact may spend against one epoch.
    pub max_attempts_per_epoch: Option<i32>,
    /// Pinned experiment plan revision the pack executes.
    pub plan_revision_uid: Uuid,
    /// Cases held by this pack. For a hidden pack this is the reserve, not the
    /// per-epoch cohort.
    pub cases: Vec<ReleaseCase>,
    /// Typed assertions every case in this pack must evaluate.
    pub mandatory_assertions: Vec<AssertionSpec>,
    /// Where the pack's scenario content came from.
    pub scenario_source: ScenarioSource,
    /// Canonical hash over the pack body.
    pub pack_hash: Digest32,
}

/// The exact case plan one release attempt runs.
///
/// Both arms run this same plan with the same seeds, which is what makes the
/// comparison paired rather than two independent samples.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MergedCasePlan {
    /// Approved platform authoring pack identity.
    pub authoring_pack_uid: Uuid,
    /// Hidden cohort pack identity.
    pub hidden_pack_uid: Uuid,
    /// Hidden cohort epoch the attempt was measured against.
    pub cohort_epoch: i32,
    /// Pinned experiment plan revision to execute.
    pub plan_revision_uid: Uuid,
    /// Tenant-visible authoring cases, platform first.
    pub authoring_cases: Vec<ReleaseCase>,
    /// Hidden cohort cases for this epoch. Never surfaced to a tenant.
    pub hidden_cases: Vec<ReleaseCase>,
    /// Union of every mandatory assertion, platform first.
    pub mandatory_assertions: Vec<AssertionSpec>,
}

impl MergedCasePlan {
    /// Iterates the exact authoring and hidden cases in execution order.
    pub fn cases(&self) -> impl Iterator<Item = &ReleaseCase> {
        self.authoring_cases.iter().chain(self.hidden_cases.iter())
    }

    /// Projects the approved cases onto the internal experiment-run binding.
    pub fn experiment_cases(&self) -> Result<Vec<ArtifactReleaseExperimentCase>, Error> {
        self.cases()
            .map(|case| {
                let repetitions = u32::try_from(case.repetitions).map_err(|_| {
                    Error::CasePackInvalid(format!(
                        "release case `{}` has a non-positive repetition count",
                        case.case_id
                    ))
                })?;
                if repetitions == 0 {
                    return Err(Error::CasePackInvalid(format!(
                        "release case `{}` has a non-positive repetition count",
                        case.case_id
                    )));
                }
                Ok(ArtifactReleaseExperimentCase {
                    scenario_id: case.case_id.clone(),
                    persona_id: case.persona_ref.clone(),
                    profile_id: case.profile.clone(),
                    repetitions,
                    assertions: merged_assertions(case, &self.mandatory_assertions)?,
                })
            })
            .collect()
    }

    /// Returns the total repetition count across every case in the plan.
    #[must_use]
    pub fn total_repetitions(&self) -> i64 {
        self.cases()
            .map(|case| i64::from(case.repetitions.max(0)))
            .sum()
    }
}

fn merged_assertions(
    case: &ReleaseCase,
    mandatory: &[AssertionSpec],
) -> Result<Vec<AssertionSpec>, Error> {
    let mut assertions = Vec::with_capacity(case.assertions.len() + mandatory.len());
    for assertion in case.assertions.iter().chain(mandatory) {
        if let Some(existing) = assertions
            .iter()
            .find(|existing: &&AssertionSpec| existing.id == assertion.id)
        {
            if *existing != *assertion {
                return Err(Error::CasePackInvalid(format!(
                    "release case `{}` conflicts with mandatory assertion `{}`",
                    case.case_id, assertion.id
                )));
            }
        } else {
            assertions.push(assertion.clone());
        }
    }
    validate_scenario_assertions(&case.case_id, &assertions, true)?;
    Ok(assertions)
}

/// Merges the immutable platform authoring pack and current hidden cohort.
///
/// Both packs must name the same server-owned experiment plan. There is no
/// tenant-supplied plan or case supplement in the release authority path.
pub fn merge_case_packs(
    authoring: &ReleaseCasePack,
    hidden: &ReleaseCasePack,
    hidden_cases: Vec<ReleaseCase>,
) -> Result<MergedCasePlan, Error> {
    if authoring.visibility != CohortVisibility::Authoring {
        return Err(Error::CasePackInvalid(format!(
            "pack {} is not an authoring pack",
            authoring.pack_uid
        )));
    }
    if hidden.visibility != CohortVisibility::Hidden {
        return Err(Error::CasePackInvalid(format!(
            "pack {} is not a hidden cohort",
            hidden.pack_uid
        )));
    }
    if hidden_cases.is_empty() {
        return Err(Error::CasePackInvalid(format!(
            "hidden cohort {} exposed no cases for epoch {}",
            hidden.pack_uid, hidden.cohort_epoch
        )));
    }
    if authoring.plan_revision_uid != hidden.plan_revision_uid {
        return Err(Error::CasePackInvalid(format!(
            "platform authoring pack {} and hidden pack {} name different experiment plans",
            authoring.pack_uid, hidden.pack_uid
        )));
    }

    let authoring_cases = authoring.cases.clone();
    let mut mandatory_assertions = authoring.mandatory_assertions.clone();
    merge_assertion_sets(
        "platform mandatory assertions",
        &mut mandatory_assertions,
        &hidden.mandatory_assertions,
    )?;

    validate_release_cases(authoring_cases.iter().chain(hidden_cases.iter()))?;
    validate_scenario_assertions("mandatory release assertions", &mandatory_assertions, false)?;

    let merged = MergedCasePlan {
        authoring_pack_uid: authoring.pack_uid,
        hidden_pack_uid: hidden.pack_uid,
        cohort_epoch: hidden.cohort_epoch,
        plan_revision_uid: authoring.plan_revision_uid,
        authoring_cases,
        hidden_cases,
        mandatory_assertions,
    };
    merged.experiment_cases()?;
    Ok(merged)
}

fn merge_assertion_sets(
    owner: &str,
    destination: &mut Vec<AssertionSpec>,
    additions: &[AssertionSpec],
) -> Result<(), Error> {
    for assertion in additions {
        if let Some(existing) = destination.iter().find(|entry| entry.id == assertion.id) {
            if existing != assertion {
                return Err(Error::CasePackInvalid(format!(
                    "{owner} conflict on assertion id `{}`",
                    assertion.id
                )));
            }
        } else {
            destination.push(assertion.clone());
        }
    }
    Ok(())
}

fn validate_release_cases<'a>(cases: impl Iterator<Item = &'a ReleaseCase>) -> Result<(), Error> {
    let mut case_ids = BTreeSet::new();
    for case in cases {
        for (field, value) in [
            ("case_id", case.case_id.as_str()),
            ("persona_ref", case.persona_ref.as_str()),
            ("profile", case.profile.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(Error::CasePackInvalid(format!(
                    "release case `{}` has an empty {field}",
                    case.case_id
                )));
            }
        }
        if !case_ids.insert(case.case_id.as_str()) {
            return Err(Error::CasePackInvalid(format!(
                "release case id `{}` is duplicated across the approved packs",
                case.case_id
            )));
        }
        let repetitions = u32::try_from(case.repetitions).ok();
        if repetitions.is_none_or(|value| value > MAX_PLAN_TRIALS_PER_COMBINATION) {
            return Err(Error::CasePackInvalid(format!(
                "release case `{}` repetitions must be between 1 and {MAX_PLAN_TRIALS_PER_COMBINATION}",
                case.case_id
            )));
        }
        validate_scenario_assertions(&case.case_id, &case.assertions, false)?;
    }
    Ok(())
}

fn validate_scenario_assertions(
    owner: &str,
    assertions: &[AssertionSpec],
    require_positive: bool,
) -> Result<(), Error> {
    if assertions.is_empty() {
        if !require_positive {
            return Ok(());
        }
        return Err(Error::CasePackInvalid(format!(
            "release case `{owner}` declares no deterministic scenario assertions"
        )));
    }
    let mut seen = BTreeSet::new();
    let mut has_positive = false;
    for assertion in assertions {
        if !seen.insert(assertion.id.as_str()) {
            return Err(Error::CasePackInvalid(format!(
                "release case `{owner}` duplicates scenario assertion `{}`",
                assertion.id
            )));
        }
        moa_eval_core::assertion::builtin_registry()
            .check_spec(assertion)
            .map_err(|error| {
                Error::CasePackInvalid(format!(
                    "release case `{owner}` declares unusable scenario assertion `{}`: {error}",
                    assertion.id
                ))
            })?;
        if assertion.gate_effect != GateEffect::Blocking {
            return Err(Error::CasePackInvalid(format!(
                "release case `{owner}` scenario assertion `{}` is not blocking",
                assertion.id
            )));
        }
        match (assertion.category, assertion.evaluator.id.as_str()) {
            (AssertionCategory::Action, PROHIBITED_ACTIONS_EVALUATOR_ID) => {
                let config: ProhibitedActionsConfig = scenario_assertion_config(owner, assertion)?;
                if config.names.is_empty() || config.names.iter().any(|name| name.trim().is_empty())
                {
                    return Err(Error::CasePackInvalid(format!(
                        "release case `{owner}` scenario assertion `{}` declares no usable prohibited action names",
                        assertion.id
                    )));
                }
            }
            (AssertionCategory::Communication, TEXT_MATCH_EVALUATOR_ID) => {
                let config: ExpectedOutput = scenario_assertion_config(owner, assertion)?;
                let positive_rules = config.contains.len()
                    + config.facts.len()
                    + usize::from(config.regex.is_some())
                    + usize::from(config.exact.is_some());
                let has_blank_rule = config
                    .contains
                    .iter()
                    .chain(&config.facts)
                    .any(|rule| rule.trim().is_empty())
                    || config
                        .regex
                        .as_deref()
                        .is_some_and(|rule| rule.trim().is_empty())
                    || config
                        .exact
                        .as_deref()
                        .is_some_and(|rule| rule.trim().is_empty());
                if positive_rules == 0 || has_blank_rule {
                    return Err(Error::CasePackInvalid(format!(
                        "release case `{owner}` scenario assertion `{}` has no usable positive response rule",
                        assertion.id
                    )));
                }
                has_positive = true;
            }
            _ => {
                return Err(Error::CasePackInvalid(format!(
                    "release case `{owner}` scenario assertion `{}` is not supported by complete production evidence; supported evaluators are text_match@1 and prohibited_actions@1",
                    assertion.id
                )));
            }
        }
    }
    if require_positive && !has_positive {
        return Err(Error::CasePackInvalid(format!(
            "release case `{owner}` declares safety assertions but no positive deterministic scenario assertion"
        )));
    }
    Ok(())
}

fn scenario_assertion_config<T: serde::de::DeserializeOwned>(
    owner: &str,
    assertion: &AssertionSpec,
) -> Result<T, Error> {
    serde_json::from_value(assertion.config.clone()).map_err(|error| {
        Error::CasePackInvalid(format!(
            "release case `{owner}` scenario assertion `{}` has invalid config: {error}",
            assertion.id
        ))
    })
}

/// One durable dispatch record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DispatchRecord {
    /// Dispatch row identifier, also the evaluation workflow key.
    pub outbox_uid: Uuid,
    /// Tenant owning the attempt.
    pub tenant_id: TenantId,
    /// Candidate revision under evaluation.
    pub revision_uid: Uuid,
    /// Artifact whose serving pointer the candidate would move.
    pub artifact_uid: Uuid,
    /// Submission generation this record was created for.
    pub generation: i64,
    /// Exact subject digest this record was created for.
    pub subject_digest: Digest32,
    /// Deterministic idempotency key over the three fields above.
    pub idempotency_key: String,
    /// Current lifecycle status.
    pub status: DispatchStatus,
    /// Seed material both arms run with.
    pub seed_material: String,
    /// Draft dependencies pinned by the submitter.
    pub pinned_dependencies: Vec<PinnedDependency>,
    /// Approved authoring pack resolved for this attempt.
    pub case_pack_uid: Option<Uuid>,
    /// Hidden cohort pack resolved for this attempt.
    pub hidden_pack_uid: Option<Uuid>,
    /// Hidden cohort epoch resolved for this attempt.
    pub cohort_epoch: Option<i32>,
    /// Experiment run started for the candidate arm.
    pub candidate_run_uid: Option<Uuid>,
    /// Experiment run started for the baseline arm.
    pub baseline_run_uid: Option<Uuid>,
    /// Which release attempt for this candidate this record is.
    pub attempt_no: i32,
}

/// One exact release trial provisioned before experiment dispatch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProvisionedTrial {
    /// Canonical plan trial key, including arm and repetition.
    pub trial_key: String,
    /// Which paired arm this trial belongs to.
    pub role: ArmRole,
    /// Exact approved case this trial executes.
    pub case: ArtifactReleaseExperimentCase,
    /// Overlay row identifier.
    pub overlay_uid: Uuid,
    /// Secret that must be presented to resolve the overlay.
    ///
    /// Only the hash is persisted. This plaintext lives in the workflow journal
    /// and in memory, never in Postgres.
    pub overlay_token: String,
    /// Revision this arm resolves the target artifact to.
    pub revision_uid: Uuid,
    /// Eval-owned session identity reserved for this trial only.
    pub eval_session_id: Uuid,
}

/// Everything one release attempt needs in order to dispatch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProvisionedAttempt {
    /// Attempt row identifier on the release review surface.
    pub attempt_uid: Uuid,
    /// Activation class read from the exact fenced release subject.
    pub activation_target: ActivationTargetClass,
    /// The exact case plan both arms run.
    pub plan: MergedCasePlan,
    /// Exact trial bindings, in deterministic plan expansion order.
    pub trials: Vec<ProvisionedTrial>,
}

impl ProvisionedAttempt {
    /// Returns whether at least one trial was provisioned for the given role.
    #[must_use]
    pub fn has_role(&self, role: ArmRole) -> bool {
        self.trials.iter().any(|trial| trial.role == role)
    }
}

/// One row of the release-attempt review surface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleaseAttemptRow {
    /// Attempt row identifier.
    pub attempt_uid: Uuid,
    /// Dispatch record the attempt belongs to.
    pub outbox_uid: Uuid,
    /// Candidate revision.
    pub revision_uid: Uuid,
    /// Artifact whose pointer the candidate would move.
    pub artifact_uid: Uuid,
    /// Submission generation.
    pub generation: i64,
    /// Subject digest, hex encoded.
    pub subject_digest: String,
    /// Activation target class.
    pub activation_target: String,
    /// Experiment run for the candidate arm.
    pub candidate_run_uid: Option<Uuid>,
    /// Experiment run for the baseline arm.
    pub baseline_run_uid: Option<Uuid>,
    /// Hidden cohort epoch the attempt faced. The cases themselves are never
    /// reported: a tenant that could read them could overfit them.
    pub cohort_epoch: Option<i32>,
    /// Deterministic verdict, when one was recorded.
    pub verdict: Option<String>,
    /// Attestation minted by a passing verdict.
    pub attestation_uid: Option<Uuid>,
    /// Whether a superseded result was refused for this attempt.
    pub fenced_out: bool,
    /// Why the attempt was fenced out.
    pub fence_reason: Option<String>,
    /// Review state on the artifact-release surface.
    pub review_state: String,
    /// Who reviewed the attempt.
    pub reviewed_by: Option<String>,
    /// When the attempt was reviewed.
    pub reviewed_at: Option<DateTime<Utc>>,
    /// Reviewer note.
    pub review_note: Option<String>,
    /// When the attempt was created.
    pub created_at: DateTime<Utc>,
}

/// Review outcome an operator may record against a release attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptReviewState {
    /// No review recorded.
    Unreviewed,
    /// Reviewed and accepted as reported.
    Acknowledged,
    /// Reviewed and disputed.
    Disputed,
}

impl AttemptReviewState {
    /// Returns the lowercase database label for this review state.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unreviewed => "unreviewed",
            Self::Acknowledged => "acknowledged",
            Self::Disputed => "disputed",
        }
    }
}

impl FromStr for AttemptReviewState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        match value {
            "unreviewed" => Ok(Self::Unreviewed),
            "acknowledged" => Ok(Self::Acknowledged),
            "disputed" => Ok(Self::Disputed),
            other => Err(Error::Storage(format!(
                "`{other}` is not a release attempt review state"
            ))),
        }
    }
}

/// Derives the seed material both arms of one attempt run with.
///
/// It is a pure function of the fenced identity of the attempt, which gives two
/// properties at once: the candidate and the baseline provably share every
/// case, persona, profile, and repetition seed, and a Restate replay of the same
/// attempt derives the same seeds instead of resampling.
#[must_use]
pub fn release_seed_material(
    tenant_id: TenantId,
    revision_uid: Uuid,
    generation: i64,
    subject_digest: &Digest32,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa.release.seed.v1\0");
    hasher.update(tenant_id.0.as_bytes());
    hasher.update(revision_uid.as_bytes());
    hasher.update(&generation.to_be_bytes());
    hasher.update(subject_digest.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Derives the deterministic idempotency key for one dispatch record.
#[must_use]
pub fn dispatch_idempotency_key(
    revision_uid: Uuid,
    generation: i64,
    subject_digest: &Digest32,
) -> String {
    format!("release:{revision_uid}:{generation}:{subject_digest}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn case(id: &str) -> ReleaseCase {
        ReleaseCase {
            case_id: id.to_string(),
            persona_ref: "persona://platform/cooperative".to_string(),
            profile: "default".to_string(),
            repetitions: 3,
            assertions: vec![positive("visible_result", "result")],
        }
    }

    fn positive(id: &str, text: &str) -> AssertionSpec {
        AssertionSpec {
            id: id.to_string(),
            category: AssertionCategory::Communication,
            gate_effect: GateEffect::Blocking,
            evaluator: moa_eval_core::assertion::EvaluatorRef::deterministic(
                TEXT_MATCH_EVALUATOR_ID,
                1,
            ),
            config: json!({ "contains": [text] }),
        }
    }

    fn prohibited(id: &str, action: &str) -> AssertionSpec {
        AssertionSpec {
            id: id.to_string(),
            category: AssertionCategory::Action,
            gate_effect: GateEffect::Blocking,
            evaluator: moa_eval_core::assertion::EvaluatorRef::deterministic(
                PROHIBITED_ACTIONS_EVALUATOR_ID,
                1,
            ),
            config: json!({ "names": [action] }),
        }
    }

    fn pack(visibility: CohortVisibility, cases: Vec<ReleaseCase>) -> ReleaseCasePack {
        ReleaseCasePack {
            pack_uid: Uuid::from_u128(match visibility {
                CohortVisibility::Authoring => 1,
                CohortVisibility::Hidden => 2,
            }),
            name: "pack".to_string(),
            revision: 1,
            visibility,
            cohort_epoch: 1,
            cohort_size: matches!(visibility, CohortVisibility::Hidden).then_some(2),
            rotates_at: None,
            max_attempts_per_epoch: matches!(visibility, CohortVisibility::Hidden).then_some(3),
            plan_revision_uid: Uuid::from_u128(9),
            cases,
            mandatory_assertions: vec![prohibited("no_email", "send_email")],
            scenario_source: ScenarioSource::ApprovedPack,
            pack_hash: Digest32([7_u8; 32]),
        }
    }

    // Pins: a raw transcript cannot be represented as scenario or persona input,
    // and a learned input without full provenance cannot either.
    #[test]
    fn raw_transcript_scenario_input_is_unrepresentable_offline() {
        let transcript = json!({
            "kind": "learned",
            "evidence": {
                "contribution_uid": Uuid::from_u128(1),
                "retention_class": "short",
                "consent_basis": "contract",
                "erasure_provenance": "moa.artifact_revision_contribution",
                "transcript": [{"role": "user", "content": "hello"}]
            }
        });
        assert!(
            serde_json::from_value::<ScenarioSource>(transcript).is_err(),
            "evidence carrying a transcript must not deserialize"
        );

        let missing_provenance = json!({
            "kind": "learned",
            "evidence": {
                "contribution_uid": Uuid::from_u128(1),
                "retention_class": "short",
                "consent_basis": "contract"
            }
        });
        assert!(
            serde_json::from_value::<ScenarioSource>(missing_provenance).is_err(),
            "learned evidence without erasure provenance must not deserialize"
        );

        let transcript_variant = json!({ "kind": "transcript", "turns": [] });
        assert!(
            serde_json::from_value::<ScenarioSource>(transcript_variant).is_err(),
            "there is no transcript scenario source"
        );

        let complete = json!({
            "kind": "learned",
            "evidence": {
                "contribution_uid": Uuid::from_u128(1),
                "retention_class": "short",
                "consent_basis": "contract",
                "erasure_provenance": "moa.artifact_revision_contribution"
            }
        });
        let decoded: ScenarioSource = serde_json::from_value(complete).expect("decode");
        match decoded {
            ScenarioSource::Learned { evidence } => {
                assert_eq!(evidence.retention_class, "short");
            }
            ScenarioSource::ApprovedPack => panic!("expected a learned source"),
        }

        let case_with_transcript = json!({
            "case_id": "c",
            "persona_ref": "persona://p",
            "profile": "default",
            "repetitions": 1,
            "assertions": [],
            "transcript": "raw"
        });
        assert!(
            serde_json::from_value::<ReleaseCase>(case_with_transcript).is_err(),
            "a case body carrying a transcript must not deserialize"
        );
    }

    // Pins: release cases and their executable plan are platform-owned; the
    // authoring and hidden packs cannot disagree on the plan or weaken evidence.
    #[test]
    fn platform_case_packs_are_immutable_executable_authority_offline() {
        let authoring = pack(CohortVisibility::Authoring, vec![case("platform.a")]);
        let hidden = pack(CohortVisibility::Hidden, vec![case("hidden.a")]);
        let hidden_cases = vec![case("hidden.a")];

        let merged = merge_case_packs(&authoring, &hidden, hidden_cases.clone())
            .expect("platform packs merge");
        assert_eq!(
            merged
                .authoring_cases
                .iter()
                .map(|case| case.case_id.as_str())
                .collect::<Vec<_>>(),
            vec!["platform.a"]
        );
        assert!(
            merged
                .mandatory_assertions
                .iter()
                .any(|assertion| assertion.id == "no_email"),
            "the platform mandatory assertion must survive the merge"
        );
        assert_eq!(merged.hidden_cases, hidden_cases);
        assert_eq!(merged.total_repetitions(), 6);
        assert_eq!(
            merged.experiment_cases().expect("project executable cases")[0].assertions,
            vec![
                positive("visible_result", "result"),
                prohibited("no_email", "send_email"),
            ]
        );

        let mut safety_only = authoring.clone();
        safety_only.cases[0].assertions.clear();
        assert!(
            matches!(
                merge_case_packs(&safety_only, &hidden, hidden_cases.clone()),
                Err(Error::CasePackInvalid(_))
            ),
            "successful completion plus only a prohibited-action assertion cannot prove the case succeeded"
        );

        let mut duplicated = authoring.clone();
        duplicated.cases[0].assertions = vec![
            prohibited("no_email", "send_email"),
            prohibited("no_email", "send_email"),
        ];
        assert!(matches!(
            merge_case_packs(&duplicated, &hidden, hidden_cases.clone()),
            Err(Error::CasePackInvalid(_))
        ));

        let mut unknown = authoring.clone();
        let mut unsupported = prohibited("tenant_assertion", "send_email");
        unsupported.evaluator.id = "tenant.executable_assertion".to_string();
        unknown.cases[0].assertions = vec![unsupported];
        assert!(matches!(
            merge_case_packs(&unknown, &hidden, hidden_cases.clone()),
            Err(Error::CasePackInvalid(_))
        ));

        let mut required_action = authoring.clone();
        let mut unsupported = prohibited("refund_required", "issue_refund");
        unsupported.evaluator.id = "required_actions".to_string();
        unsupported.config = json!({ "actions": [{ "name": "issue_refund" }] });
        required_action.cases[0].assertions = vec![unsupported];
        assert!(
            matches!(
                merge_case_packs(&required_action, &hidden, hidden_cases.clone()),
                Err(Error::CasePackInvalid(_))
            ),
            "required actions cannot block a release until reviewed effects have stable approval correlation"
        );

        let mut divergent_hidden = hidden.clone();
        divergent_hidden.plan_revision_uid = Uuid::from_u128(10);
        assert!(
            matches!(
                merge_case_packs(&authoring, &divergent_hidden, hidden_cases.clone()),
                Err(Error::CasePackInvalid(_))
            ),
            "platform packs cannot select different executable plan revisions"
        );

        assert!(matches!(
            merge_case_packs(&authoring, &hidden, Vec::new()),
            Err(Error::CasePackInvalid(_))
        ));
    }

    // Pins: both arms of one attempt share seeds, and the seeds are a pure
    // function of the fenced attempt identity so a replay reproduces them.
    #[test]
    fn seed_material_is_fenced_and_reproducible_offline() {
        let tenant = TenantId::from(Uuid::from_u128(1));
        let revision = Uuid::from_u128(2);
        let digest = Digest32([3_u8; 32]);
        let first = release_seed_material(tenant, revision, 4, &digest);
        assert_eq!(first, release_seed_material(tenant, revision, 4, &digest));
        assert_ne!(first, release_seed_material(tenant, revision, 5, &digest));
        assert_ne!(
            first,
            release_seed_material(tenant, revision, 4, &Digest32([4_u8; 32]))
        );
        assert_ne!(
            first,
            release_seed_material(TenantId::from(Uuid::from_u128(9)), revision, 4, &digest)
        );

        let key = dispatch_idempotency_key(revision, 4, &digest);
        assert_eq!(key, dispatch_idempotency_key(revision, 4, &digest));
        assert_ne!(key, dispatch_idempotency_key(revision, 5, &digest));
    }
}
