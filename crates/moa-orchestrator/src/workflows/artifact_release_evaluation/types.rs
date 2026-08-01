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
//!   hidden release cohort that decides, and [`MergedCasePlan`] is built so a
//!   tenant supplement can only add.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use moa_artifacts::release::Digest32;
use moa_artifacts::simulation::MAX_PLAN_TRIALS_PER_COMBINATION;
use moa_core::types::identifiers::TenantId;
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
    /// A newer subject replaced it, so its result can decide nothing.
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    /// Registered assertion identifiers this case evaluates.
    pub assertions: Vec<String>,
}

/// A versioned, server-resolved approved plan/scenario pack.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleaseCasePack {
    /// Pack row identifier.
    pub pack_uid: Uuid,
    /// Operator-facing pack name.
    pub name: String,
    /// Pack revision; an edit is a new revision, never an in-place change.
    pub revision: i32,
    /// Owning tenant, or `None` for a platform pack.
    pub tenant_id: Option<TenantId>,
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
    pub plan_revision_uid: Option<Uuid>,
    /// Cases held by this pack. For a hidden pack this is the reserve, not the
    /// per-epoch cohort.
    pub cases: Vec<ReleaseCase>,
    /// Assertion identifiers every case in this pack must evaluate.
    pub mandatory_assertions: Vec<String>,
    /// Where the pack's scenario content came from.
    pub scenario_source: ScenarioSource,
    /// Canonical hash over the pack body.
    pub pack_hash: Digest32,
}

/// The exact case plan one release attempt runs.
///
/// Both arms run this same plan with the same seeds, which is what makes the
/// comparison paired rather than two independent samples.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    pub mandatory_assertions: Vec<String>,
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

/// Merges an optional tenant supplement into the approved platform packs.
///
/// A tenant pack may only add. Every platform case survives, every platform
/// mandatory assertion survives, a tenant case that reuses a platform case id is
/// refused rather than allowed to shadow it, and a tenant pack that declares
/// itself hidden is refused outright -- the hidden cohort is the platform's gate,
/// and a tenant that could contribute to it could weaken it.
pub fn merge_case_packs(
    authoring: &ReleaseCasePack,
    hidden: &ReleaseCasePack,
    hidden_cases: Vec<ReleaseCase>,
    tenant_supplement: Option<&ReleaseCasePack>,
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
    let plan_revision_uid = tenant_supplement
        .and_then(|pack| pack.plan_revision_uid)
        .or(authoring.plan_revision_uid)
        .ok_or_else(|| {
            Error::CasePackInvalid(format!(
                "approved pack {} names no experiment plan revision, so there is nothing to execute",
                authoring.pack_uid
            ))
        })?;

    let mut authoring_cases = authoring.cases.clone();
    let mut mandatory_assertions = authoring.mandatory_assertions.clone();
    for assertion in &hidden.mandatory_assertions {
        if !mandatory_assertions.contains(assertion) {
            mandatory_assertions.push(assertion.clone());
        }
    }

    if let Some(supplement) = tenant_supplement {
        if supplement.visibility != CohortVisibility::Authoring {
            return Err(Error::CasePackInvalid(format!(
                "tenant pack {} declares hidden visibility; the hidden cohort is platform-owned",
                supplement.pack_uid
            )));
        }
        let platform_ids = authoring_cases
            .iter()
            .map(|case| case.case_id.clone())
            .collect::<BTreeSet<_>>();
        for case in &supplement.cases {
            if platform_ids.contains(&case.case_id) {
                return Err(Error::CasePackInvalid(format!(
                    "tenant case `{}` shadows an approved platform case",
                    case.case_id
                )));
            }
            authoring_cases.push(case.clone());
        }
        // Supplement assertions are added, never subtracted: the union is taken in
        // this direction so a tenant pack that omits a platform assertion has not
        // removed it.
        for assertion in &supplement.mandatory_assertions {
            if !mandatory_assertions.contains(assertion) {
                mandatory_assertions.push(assertion.clone());
            }
        }
    }

    validate_release_cases(authoring_cases.iter().chain(hidden_cases.iter()))?;

    Ok(MergedCasePlan {
        authoring_pack_uid: authoring.pack_uid,
        hidden_pack_uid: hidden.pack_uid,
        cohort_epoch: hidden.cohort_epoch,
        plan_revision_uid,
        authoring_cases,
        hidden_cases,
        mandatory_assertions,
    })
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
    }
    Ok(())
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

/// One provisioned evaluation arm.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProvisionedArm {
    /// Which arm this is.
    pub role: ArmRole,
    /// Overlay row identifier.
    pub overlay_uid: Uuid,
    /// Secret that must be presented to resolve the overlay.
    ///
    /// Only the hash is persisted. This plaintext lives in the workflow journal
    /// and in memory, never in Postgres.
    pub overlay_token: String,
    /// Revision this arm resolves the target artifact to.
    pub revision_uid: Uuid,
    /// Eval-owned session identity reserved for this arm.
    pub eval_session_id: Uuid,
    /// Writable copy-on-write fixture environment for this arm.
    pub fixture_uid: Uuid,
}

/// Everything one release attempt needs in order to dispatch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProvisionedAttempt {
    /// Attempt row identifier on the release review surface.
    pub attempt_uid: Uuid,
    /// The exact case plan both arms run.
    pub plan: MergedCasePlan,
    /// Provisioned arms, candidate first. A first activation has no baseline arm.
    pub arms: Vec<ProvisionedArm>,
}

impl ProvisionedAttempt {
    /// Returns the arm with the given role.
    #[must_use]
    pub fn arm(&self, role: ArmRole) -> Option<&ProvisionedArm> {
        self.arms.iter().find(|arm| arm.role == role)
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
            assertions: vec!["target_completed".to_string()],
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
            tenant_id: None,
            visibility,
            cohort_epoch: 1,
            cohort_size: matches!(visibility, CohortVisibility::Hidden).then_some(2),
            rotates_at: None,
            max_attempts_per_epoch: matches!(visibility, CohortVisibility::Hidden).then_some(3),
            plan_revision_uid: Some(Uuid::from_u128(9)),
            cases,
            mandatory_assertions: vec!["privacy_safe_output".to_string()],
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

    // Pins: a tenant supplement can only add. It cannot drop a platform case,
    // shadow one, remove a mandatory assertion, or contribute hidden cases.
    #[test]
    fn tenant_supplement_cannot_replace_or_weaken_the_approved_pack_offline() {
        let authoring = pack(CohortVisibility::Authoring, vec![case("platform.a")]);
        let hidden = pack(CohortVisibility::Hidden, vec![case("hidden.a")]);
        let hidden_cases = vec![case("hidden.a")];

        let mut supplement = pack(CohortVisibility::Authoring, vec![case("tenant.a")]);
        supplement.pack_uid = Uuid::from_u128(3);
        supplement.mandatory_assertions = vec!["tenant.extra".to_string()];
        let merged = merge_case_packs(&authoring, &hidden, hidden_cases.clone(), Some(&supplement))
            .expect("supplement merges");
        assert_eq!(
            merged
                .authoring_cases
                .iter()
                .map(|case| case.case_id.as_str())
                .collect::<Vec<_>>(),
            vec!["platform.a", "tenant.a"]
        );
        assert!(
            merged
                .mandatory_assertions
                .contains(&"privacy_safe_output".to_string()),
            "a supplement that omits a platform assertion has not removed it"
        );
        assert!(
            merged
                .mandatory_assertions
                .contains(&"tenant.extra".to_string())
        );
        assert_eq!(merged.hidden_cases, hidden_cases);
        assert_eq!(merged.total_repetitions(), 9);

        let mut shadowing = supplement.clone();
        shadowing.cases = vec![case("platform.a")];
        assert!(matches!(
            merge_case_packs(&authoring, &hidden, hidden_cases.clone(), Some(&shadowing)),
            Err(Error::CasePackInvalid(_))
        ));

        let mut hidden_supplement = supplement.clone();
        hidden_supplement.visibility = CohortVisibility::Hidden;
        assert!(matches!(
            merge_case_packs(
                &authoring,
                &hidden,
                hidden_cases.clone(),
                Some(&hidden_supplement)
            ),
            Err(Error::CasePackInvalid(_))
        ));

        let mut planless = authoring.clone();
        planless.plan_revision_uid = None;
        let mut planless_supplement = supplement.clone();
        planless_supplement.plan_revision_uid = None;
        assert!(
            matches!(
                merge_case_packs(
                    &planless,
                    &hidden,
                    hidden_cases.clone(),
                    Some(&planless_supplement)
                ),
                Err(Error::CasePackInvalid(_))
            ),
            "a pack with no plan revision has nothing to execute and must fail closed"
        );

        assert!(matches!(
            merge_case_packs(&authoring, &hidden, Vec::new(), None),
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
