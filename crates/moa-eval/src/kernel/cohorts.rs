//! Anchor, rolling-freshness, and hidden-seed cohort contracts.
//!
//! A frozen corpus answers "did this change regress anything" and a fresh
//! corpus answers "does this still work on current data". Those are different
//! questions and they need different case sets:
//!
//! - an **anchor cohort** is immutable. It exists so a comparison across months
//!   is genuinely paired. Overwriting it — even "refreshing" it every six months
//!   — destroys every longitudinal claim previously made against it.
//! - a **rolling freshness cohort** is separately versioned and hidden. It moves
//!   on purpose, so its results are reported next to the anchor's, never mixed
//!   into the same paired comparison.
//!
//! A gap between the anchor and the rolling cohort is *not* automatically
//! overfitting: the two case sets differ in content, difficulty, and size. This
//! module refuses to label it, and reports it as a gap with uncertainty
//! attached.
//!
//! Hidden seeds stop being held out once someone reads their per-case failures.
//! [`HiddenSeedLedger`] counts inspections and forces rotation, because a seed
//! that has been debugged against is validation data.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Inspections after which a hidden seed must be rotated out.
///
/// One inspection is a legitimate triage of a failure. By the third the seed has
/// informed development decisions and can no longer be described as held out.
pub const MAX_HIDDEN_SEED_INSPECTIONS: u32 = 2;

/// Errors raised by cohort handling.
#[derive(Debug, Error, PartialEq)]
pub enum CohortError {
    /// A cohort field required for identity was blank.
    #[error("cohort {cohort} is missing a required field: {field}")]
    MissingField {
        /// Offending cohort.
        cohort: String,
        /// Missing field.
        field: &'static str,
    },
    /// An anchor cohort was redefined under the same id.
    #[error(
        "anchor cohort `{anchor_id}` is immutable: manifest hash {existing} cannot become {proposed}"
    )]
    AnchorOverwrite {
        /// Anchor identity.
        anchor_id: String,
        /// Hash already frozen for this anchor.
        existing: String,
        /// Hash the caller tried to install.
        proposed: String,
    },
    /// Two reports do not describe the same anchor and cannot be paired.
    #[error("paired comparison rejected: {reason}")]
    UnpairedComparison {
        /// What differed.
        reason: String,
    },
    /// A hidden seed has been inspected too often to stay held out.
    #[error("hidden seed {seed} was inspected {inspections} times and must be rotated out")]
    HiddenSeedBurned {
        /// Offending seed.
        seed: u64,
        /// Inspections recorded.
        inspections: u32,
    },
    /// A production-shaped case failed its admission requirements.
    #[error("production-shaped case `{case_id}` is not admissible: {reason}")]
    InadmissibleProductionCase {
        /// Offending case.
        case_id: String,
        /// Why it was refused.
        reason: String,
    },
}

/// An immutable cohort used for longitudinal paired comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorCohort {
    /// Stable anchor identity, never reused for different content.
    pub anchor_id: String,
    /// Content hash over the exact case set.
    pub manifest_hash: String,
    /// Corpus this cohort draws from.
    pub corpus_id: String,
    /// Seeds that produced the cohort, when it is generated.
    pub seeds: Vec<u64>,
    /// When the cohort was frozen.
    pub frozen_at: DateTime<Utc>,
    /// Exact case identifiers, in sorted order.
    pub case_ids: BTreeSet<String>,
}

impl AnchorCohort {
    /// Validates that the anchor carries a complete identity.
    pub fn validate(&self) -> Result<(), CohortError> {
        for (field, value) in [
            ("anchor_id", self.anchor_id.as_str()),
            ("manifest_hash", self.manifest_hash.as_str()),
            ("corpus_id", self.corpus_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(CohortError::MissingField {
                    cohort: self.anchor_id.clone(),
                    field,
                });
            }
        }
        if self.case_ids.is_empty() {
            return Err(CohortError::MissingField {
                cohort: self.anchor_id.clone(),
                field: "case_ids",
            });
        }
        Ok(())
    }

    /// Refuses any redefinition of an already frozen anchor.
    ///
    /// The registry is the enforcement point: an anchor is a promise that a
    /// comparison made a year ago still means what it said.
    pub fn ensure_unchanged(&self, proposed: &Self) -> Result<(), CohortError> {
        self.validate()?;
        proposed.validate()?;
        if self.anchor_id != proposed.anchor_id {
            return Err(CohortError::UnpairedComparison {
                reason: format!(
                    "anchor id {} cannot be compared with {}",
                    self.anchor_id, proposed.anchor_id
                ),
            });
        }
        if self.manifest_hash != proposed.manifest_hash
            || self.corpus_id != proposed.corpus_id
            || self.seeds != proposed.seeds
            || self.case_ids != proposed.case_ids
        {
            return Err(CohortError::AnchorOverwrite {
                anchor_id: self.anchor_id.clone(),
                existing: self.manifest_hash.clone(),
                proposed: proposed.manifest_hash.clone(),
            });
        }
        Ok(())
    }
}

/// A separately versioned, hidden cohort that tracks corpus freshness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollingCohort {
    /// Stable family name, shared across versions.
    pub family: String,
    /// Monotonic version; a new version never overwrites an older one.
    pub version: u32,
    /// Corpus this version draws from.
    pub corpus_id: String,
    /// Seeds that produced this version.
    pub seeds: Vec<u64>,
    /// When this version was cut.
    pub cut_at: DateTime<Utc>,
    /// Exact case identifiers in this version.
    pub case_ids: BTreeSet<String>,
}

impl RollingCohort {
    /// Returns the fully qualified cohort identity, including its version.
    #[must_use]
    pub fn qualified_id(&self) -> String {
        format!("{}@v{}", self.family, self.version)
    }

    /// Validates that a new version supersedes rather than overwrites.
    pub fn ensure_supersedes(&self, previous: &Self) -> Result<(), CohortError> {
        if self.family != previous.family {
            return Err(CohortError::UnpairedComparison {
                reason: format!("rolling family {} is not {}", self.family, previous.family),
            });
        }
        if self.version <= previous.version {
            return Err(CohortError::UnpairedComparison {
                reason: format!(
                    "rolling version {} does not supersede {}",
                    self.version, previous.version
                ),
            });
        }
        Ok(())
    }
}

/// One cohort's measured value with its uncertainty interval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CohortEstimate {
    /// Point estimate.
    pub value: f64,
    /// Lower confidence bound.
    pub lower: f64,
    /// Upper confidence bound.
    pub upper: f64,
    /// Cases behind the estimate.
    pub cases: usize,
}

/// How an anchor-versus-rolling gap may be described.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CohortGapInterpretation {
    /// The intervals overlap; there is no measured gap to explain.
    NoMeasuredGap,
    /// The intervals separate. The cause is not determined by this comparison:
    /// different case sets differ in difficulty as well as in freshness.
    MeasuredGapCauseUndetermined,
}

/// Side-by-side anchor and rolling results, explicitly not a paired comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FreshnessReport {
    /// Metric both cohorts measured.
    pub metric: String,
    /// Anchor identity.
    pub anchor_id: String,
    /// Rolling cohort identity including version.
    pub rolling_id: String,
    /// Anchor estimate.
    pub anchor: CohortEstimate,
    /// Rolling estimate.
    pub rolling: CohortEstimate,
    /// Difference in point estimates, anchor minus rolling.
    pub gap: f64,
    /// The only interpretation this comparison licenses.
    pub interpretation: CohortGapInterpretation,
}

/// Builds a freshness report from two independent cohort estimates.
///
/// Deliberately not paired: the cohorts hold different cases, so the report
/// carries two intervals and a gap rather than a paired delta or a p-value.
#[must_use]
pub fn compare_cohorts(
    metric: &str,
    anchor: &AnchorCohort,
    anchor_estimate: CohortEstimate,
    rolling: &RollingCohort,
    rolling_estimate: CohortEstimate,
) -> FreshnessReport {
    let intervals_overlap = anchor_estimate.lower <= rolling_estimate.upper
        && rolling_estimate.lower <= anchor_estimate.upper;
    FreshnessReport {
        metric: metric.to_string(),
        anchor_id: anchor.anchor_id.clone(),
        rolling_id: rolling.qualified_id(),
        anchor: anchor_estimate,
        rolling: rolling_estimate,
        gap: anchor_estimate.value - rolling_estimate.value,
        interpretation: if intervals_overlap {
            CohortGapInterpretation::NoMeasuredGap
        } else {
            CohortGapInterpretation::MeasuredGapCauseUndetermined
        },
    }
}

/// Identity a report must carry for a paired comparison to be legitimate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedRunIdentity {
    /// Anchor the run scored.
    pub anchor_id: String,
    /// Anchor manifest hash the run scored.
    pub manifest_hash: String,
    /// Corpus the anchor draws from.
    pub corpus_id: String,
    /// Seeds behind the case set.
    pub seeds: Vec<u64>,
    /// Exact case identifiers scored.
    pub case_ids: BTreeSet<String>,
}

impl PairedRunIdentity {
    /// Derives a run identity from an anchor cohort.
    #[must_use]
    pub fn from_anchor(anchor: &AnchorCohort) -> Self {
        Self {
            anchor_id: anchor.anchor_id.clone(),
            manifest_hash: anchor.manifest_hash.clone(),
            corpus_id: anchor.corpus_id.clone(),
            seeds: anchor.seeds.clone(),
            case_ids: anchor.case_ids.clone(),
        }
    }
}

/// Rejects a comparison whose two sides are not the same anchor cohort.
///
/// Different corpus ids, different seeds, or a different case set produce
/// unpaired data; treating them as paired invents a delta.
pub fn require_paired(
    baseline: &PairedRunIdentity,
    candidate: &PairedRunIdentity,
) -> Result<(), CohortError> {
    if baseline.anchor_id != candidate.anchor_id {
        return Err(CohortError::UnpairedComparison {
            reason: format!(
                "anchor id {} vs {}",
                baseline.anchor_id, candidate.anchor_id
            ),
        });
    }
    if baseline.manifest_hash != candidate.manifest_hash {
        return Err(CohortError::UnpairedComparison {
            reason: format!(
                "anchor manifest hash {} vs {}",
                baseline.manifest_hash, candidate.manifest_hash
            ),
        });
    }
    if baseline.corpus_id != candidate.corpus_id {
        return Err(CohortError::UnpairedComparison {
            reason: format!(
                "corpus id {} vs {}",
                baseline.corpus_id, candidate.corpus_id
            ),
        });
    }
    if baseline.seeds != candidate.seeds {
        return Err(CohortError::UnpairedComparison {
            reason: format!("seeds {:?} vs {:?}", baseline.seeds, candidate.seeds),
        });
    }
    if baseline.case_ids != candidate.case_ids {
        let missing = baseline
            .case_ids
            .difference(&candidate.case_ids)
            .cloned()
            .collect::<Vec<_>>();
        let added = candidate
            .case_ids
            .difference(&baseline.case_ids)
            .cloned()
            .collect::<Vec<_>>();
        return Err(CohortError::UnpairedComparison {
            reason: format!("case set differs: missing {missing:?}, added {added:?}"),
        });
    }
    Ok(())
}

/// Inspection accounting for hidden rolling-cohort seeds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenSeedLedger {
    /// Inspections recorded per active seed.
    pub inspections: BTreeMap<u64, u32>,
    /// Seeds already rotated out, retained so they are never reused.
    pub retired: BTreeSet<u64>,
}

impl HiddenSeedLedger {
    /// Creates a ledger over an initial hidden seed set.
    #[must_use]
    pub fn new(seeds: impl IntoIterator<Item = u64>) -> Self {
        Self {
            inspections: seeds.into_iter().map(|seed| (seed, 0)).collect(),
            retired: BTreeSet::new(),
        }
    }

    /// Records one inspection of a hidden seed's case-level results.
    ///
    /// Returns an error once the seed has been inspected
    /// [`MAX_HIDDEN_SEED_INSPECTIONS`] times: at that point its results have
    /// informed development and it is validation data, not held-out data.
    pub fn record_inspection(&mut self, seed: u64) -> Result<u32, CohortError> {
        if self.retired.contains(&seed) {
            return Err(CohortError::HiddenSeedBurned {
                seed,
                inspections: MAX_HIDDEN_SEED_INSPECTIONS,
            });
        }
        let count = self.inspections.entry(seed).or_insert(0);
        *count += 1;
        let observed = *count;
        if observed > MAX_HIDDEN_SEED_INSPECTIONS {
            return Err(CohortError::HiddenSeedBurned {
                seed,
                inspections: observed,
            });
        }
        Ok(observed)
    }

    /// Returns whether a seed must be rotated out before the next cut.
    #[must_use]
    pub fn must_rotate(&self, seed: u64) -> bool {
        self.retired.contains(&seed)
            || self
                .inspections
                .get(&seed)
                .is_some_and(|count| *count >= MAX_HIDDEN_SEED_INSPECTIONS)
    }

    /// Rotates a burned seed out and installs its replacement.
    pub fn rotate(&mut self, seed: u64, replacement: u64) -> Result<(), CohortError> {
        if !self.must_rotate(seed) {
            return Err(CohortError::UnpairedComparison {
                reason: format!("seed {seed} is not burned and must stay hidden"),
            });
        }
        if self.retired.contains(&replacement) || self.inspections.contains_key(&replacement) {
            return Err(CohortError::UnpairedComparison {
                reason: format!("replacement seed {replacement} has already been used"),
            });
        }
        self.inspections.remove(&seed);
        self.retired.insert(seed);
        self.inspections.insert(replacement, 0);
        Ok(())
    }

    /// Returns the currently held-out seeds.
    #[must_use]
    pub fn active_seeds(&self) -> Vec<u64> {
        self.inspections.keys().copied().collect()
    }
}

/// How a production-shaped case was made safe to evaluate against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductionShapeTreatment {
    /// Generated to resemble production traffic; contains no tenant content.
    Synthetic,
    /// Derived from real traffic with identifiers and content removed.
    Sanitized,
    /// Copied verbatim from tenant traffic. Never admissible.
    RawTenantTraffic,
}

/// A production-shaped case proposed for an eval cohort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductionShapedCase {
    /// Stable case identity.
    pub case_id: String,
    /// How the case was made safe.
    pub treatment: ProductionShapeTreatment,
    /// Consent record covering this material; required.
    pub consent_ref: Option<String>,
    /// Who authored or approved the case; required for attribution.
    pub attributed_to: Option<String>,
    /// Retention and erasure provenance link; required.
    pub retention_provenance_ref: Option<String>,
}

impl ProductionShapedCase {
    /// Rejects any case that is not consented, safe, attributable, and erasable.
    ///
    /// Raw tenant traffic is refused outright rather than gated on paperwork.
    pub fn validate(&self) -> Result<(), CohortError> {
        if self.case_id.trim().is_empty() {
            return Err(CohortError::InadmissibleProductionCase {
                case_id: self.case_id.clone(),
                reason: "case id must not be blank".to_string(),
            });
        }
        if self.treatment == ProductionShapeTreatment::RawTenantTraffic {
            return Err(CohortError::InadmissibleProductionCase {
                case_id: self.case_id.clone(),
                reason: "raw tenant traffic is never admissible".to_string(),
            });
        }
        for (field, value) in [
            ("consent_ref", self.consent_ref.as_deref()),
            ("attributed_to", self.attributed_to.as_deref()),
            (
                "retention_provenance_ref",
                self.retention_provenance_ref.as_deref(),
            ),
        ] {
            if value.is_none_or(|value| value.trim().is_empty()) {
                return Err(CohortError::InadmissibleProductionCase {
                    case_id: self.case_id.clone(),
                    reason: format!("{field} is required"),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn frozen_at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn anchor() -> AnchorCohort {
        AnchorCohort {
            anchor_id: "retrieval-anchor-2026-01".to_string(),
            manifest_hash: "a".repeat(64),
            corpus_id: "pr-corpus-1".to_string(),
            seeds: vec![1, 2, 3],
            frozen_at: frozen_at(),
            case_ids: ["c1".to_string(), "c2".to_string()].into_iter().collect(),
        }
    }

    fn rolling(version: u32) -> RollingCohort {
        RollingCohort {
            family: "retrieval-rolling".to_string(),
            version,
            corpus_id: "rolling-corpus-1".to_string(),
            seeds: vec![9001],
            cut_at: frozen_at(),
            case_ids: ["r1".to_string()].into_iter().collect(),
        }
    }

    #[test]
    fn an_anchor_cohort_cannot_be_overwritten_under_its_own_id() {
        // Pins: refreshing an anchor in place is refused, so old longitudinal
        // comparisons keep meaning what they claimed.
        let existing = anchor();
        let mut refreshed = anchor();
        refreshed.manifest_hash = "b".repeat(64);
        refreshed.case_ids.insert("c3".to_string());

        assert_eq!(
            existing
                .ensure_unchanged(&refreshed)
                .expect_err("overwrite"),
            CohortError::AnchorOverwrite {
                anchor_id: existing.anchor_id.clone(),
                existing: existing.manifest_hash.clone(),
                proposed: refreshed.manifest_hash.clone(),
            }
        );
        existing
            .ensure_unchanged(&anchor())
            .expect("an identical anchor is accepted");
    }

    #[test]
    fn each_anchor_identity_field_alone_blocks_a_redefinition() {
        // Pins: every identity field is load-bearing on its own. A change to only
        // the hash, only the corpus, only the seeds, or only the case set is still
        // an overwrite, so the immutability check cannot be weakened to require
        // several fields to differ at once.
        type Mutation = (&'static str, fn(&mut AnchorCohort));
        let existing = anchor();
        let mutations: [Mutation; 4] = [
            ("manifest_hash", |cohort| {
                cohort.manifest_hash = "b".repeat(64);
            }),
            ("corpus_id", |cohort| {
                cohort.corpus_id = "pr-corpus-2".to_string();
            }),
            ("seeds", |cohort| cohort.seeds = vec![1, 2, 4]),
            ("case_ids", |cohort| {
                cohort.case_ids.insert("c3".to_string());
            }),
        ];
        for (field, mutate) in mutations {
            let mut proposed = anchor();
            mutate(&mut proposed);
            assert!(
                matches!(
                    existing.ensure_unchanged(&proposed),
                    Err(CohortError::AnchorOverwrite { .. })
                ),
                "changing only {field} must be refused"
            );
        }
    }

    #[test]
    fn an_anchor_missing_any_identity_field_is_invalid() {
        // Pins: each required field is checked independently, so a blank anchor id,
        // hash, corpus, or empty case set cannot pass by borrowing another field.
        type Blank = (&'static str, fn(&mut AnchorCohort));
        let blanks: [Blank; 4] = [
            ("anchor_id", |cohort| cohort.anchor_id = "  ".to_string()),
            ("manifest_hash", |cohort| {
                cohort.manifest_hash = String::new();
            }),
            ("corpus_id", |cohort| cohort.corpus_id = String::new()),
            ("case_ids", |cohort| cohort.case_ids.clear()),
        ];
        for (field, mutate) in blanks {
            let mut cohort = anchor();
            mutate(&mut cohort);
            let error = cohort.validate().expect_err("blank field must be refused");
            assert!(
                matches!(&error, CohortError::MissingField { field: missing, .. } if *missing == field),
                "expected {field} to be reported, got {error}"
            );
        }
        anchor().validate().expect("a complete anchor validates");
    }

    #[test]
    fn a_rolling_family_mismatch_and_a_stale_version_are_reported_separately() {
        // Pins: both guards fire on their own condition.
        let previous = rolling(3);
        let mut other_family = rolling(9);
        other_family.family = "other-rolling".to_string();
        let error = other_family
            .ensure_supersedes(&previous)
            .expect_err("family mismatch");
        assert!(
            matches!(&error, CohortError::UnpairedComparison { reason } if reason.contains("family")),
            "got {error}"
        );

        let error = rolling(2)
            .ensure_supersedes(&previous)
            .expect_err("older version");
        assert!(
            matches!(&error, CohortError::UnpairedComparison { reason } if reason.contains("supersede")),
            "got {error}"
        );
    }

    #[test]
    fn a_gap_is_measured_whichever_cohort_scores_higher() {
        // Pins: the overlap test is symmetric. Both conjuncts matter, so neither
        // direction of separation can be dropped.
        let higher_anchor = compare_cohorts(
            "recall_at_4",
            &anchor(),
            CohortEstimate {
                value: 0.90,
                lower: 0.88,
                upper: 0.92,
                cases: 100,
            },
            &rolling(4),
            CohortEstimate {
                value: 0.50,
                lower: 0.45,
                upper: 0.55,
                cases: 40,
            },
        );
        assert_eq!(
            higher_anchor.interpretation,
            CohortGapInterpretation::MeasuredGapCauseUndetermined
        );
        assert!(higher_anchor.gap > 0.0);

        let higher_rolling = compare_cohorts(
            "recall_at_4",
            &anchor(),
            CohortEstimate {
                value: 0.50,
                lower: 0.45,
                upper: 0.55,
                cases: 100,
            },
            &rolling(4),
            CohortEstimate {
                value: 0.90,
                lower: 0.88,
                upper: 0.92,
                cases: 40,
            },
        );
        assert_eq!(
            higher_rolling.interpretation,
            CohortGapInterpretation::MeasuredGapCauseUndetermined
        );
        assert!(higher_rolling.gap < 0.0);
    }

    #[test]
    fn a_retired_seed_must_rotate_even_though_it_holds_no_inspection_count() {
        // Pins: both arms of the rotation predicate. A retired seed is removed from
        // the inspection map, so only the retired check can catch it.
        let mut ledger = HiddenSeedLedger::new([1, 2]);
        ledger.record_inspection(1).expect("first");
        ledger.record_inspection(1).expect("second");
        ledger.rotate(1, 3).expect("rotate");

        assert!(!ledger.inspections.contains_key(&1));
        assert!(ledger.must_rotate(1), "a retired seed must never come back");
        assert!(!ledger.must_rotate(2));
        assert!(!ledger.must_rotate(3));
    }

    #[test]
    fn comparison_rejects_a_different_anchor_manifest_seed_or_case_set() {
        // Pins: three separate ways of faking a paired comparison all fail.
        let baseline = PairedRunIdentity::from_anchor(&anchor());

        let mut other_hash = baseline.clone();
        other_hash.manifest_hash = "c".repeat(64);
        assert!(matches!(
            require_paired(&baseline, &other_hash).expect_err("hash"),
            CohortError::UnpairedComparison { .. }
        ));

        let mut other_seeds = baseline.clone();
        other_seeds.seeds = vec![4, 5, 6];
        assert!(matches!(
            require_paired(&baseline, &other_seeds).expect_err("seeds"),
            CohortError::UnpairedComparison { .. }
        ));

        let mut other_corpus = baseline.clone();
        other_corpus.corpus_id = "pr-corpus-2".to_string();
        assert!(matches!(
            require_paired(&baseline, &other_corpus).expect_err("corpus"),
            CohortError::UnpairedComparison { .. }
        ));

        let mut other_anchor = baseline.clone();
        other_anchor.anchor_id = "retrieval-anchor-2026-02".to_string();
        assert!(matches!(
            require_paired(&baseline, &other_anchor).expect_err("anchor id"),
            CohortError::UnpairedComparison { .. }
        ));

        let mut other_cases = baseline.clone();
        other_cases.case_ids.insert("c9".to_string());
        assert!(matches!(
            require_paired(&baseline, &other_cases).expect_err("cases"),
            CohortError::UnpairedComparison { .. }
        ));

        require_paired(&baseline, &baseline).expect("identical identities pair");
    }

    #[test]
    fn a_rolling_cohort_version_must_supersede_rather_than_replace() {
        // Pins: rolling cuts are versioned, so a report always names which cut
        // it measured.
        let previous = rolling(3);
        assert!(matches!(
            rolling(3).ensure_supersedes(&previous).expect_err("same"),
            CohortError::UnpairedComparison { .. }
        ));
        rolling(4)
            .ensure_supersedes(&previous)
            .expect("a higher version supersedes");
        assert_eq!(rolling(4).qualified_id(), "retrieval-rolling@v4");
    }

    #[test]
    fn a_freshness_gap_is_reported_with_uncertainty_and_never_called_overfitting() {
        // Pins: separated intervals produce an undetermined-cause verdict, not
        // an overfitting label the data cannot support.
        let report = compare_cohorts(
            "recall_at_4",
            &anchor(),
            CohortEstimate {
                value: 0.90,
                lower: 0.86,
                upper: 0.94,
                cases: 120,
            },
            &rolling(4),
            CohortEstimate {
                value: 0.70,
                lower: 0.62,
                upper: 0.78,
                cases: 40,
            },
        );

        assert_eq!(report.rolling_id, "retrieval-rolling@v4");
        assert!((report.gap - 0.20).abs() < 1e-12);
        assert_eq!(
            report.interpretation,
            CohortGapInterpretation::MeasuredGapCauseUndetermined
        );

        let overlapping = compare_cohorts(
            "recall_at_4",
            &anchor(),
            CohortEstimate {
                value: 0.90,
                lower: 0.84,
                upper: 0.95,
                cases: 120,
            },
            &rolling(4),
            CohortEstimate {
                value: 0.87,
                lower: 0.80,
                upper: 0.93,
                cases: 40,
            },
        );
        assert_eq!(
            overlapping.interpretation,
            CohortGapInterpretation::NoMeasuredGap
        );
    }

    #[test]
    fn a_repeatedly_inspected_hidden_seed_must_be_rotated_out() {
        // Pins: a seed that has been debugged against stops counting as hidden.
        let mut ledger = HiddenSeedLedger::new([9001, 9002]);
        assert_eq!(ledger.record_inspection(9001).expect("first"), 1);
        assert!(!ledger.must_rotate(9001));
        assert_eq!(ledger.record_inspection(9001).expect("second"), 2);
        assert!(ledger.must_rotate(9001));
        assert_eq!(
            ledger.record_inspection(9001).expect_err("third"),
            CohortError::HiddenSeedBurned {
                seed: 9001,
                inspections: 3
            }
        );

        ledger.rotate(9001, 9003).expect("rotate");
        assert_eq!(ledger.active_seeds(), vec![9002, 9003]);
        assert!(ledger.retired.contains(&9001));
        assert!(matches!(
            ledger.rotate(9002, 9004).expect_err("not burned"),
            CohortError::UnpairedComparison { .. }
        ));
        assert!(matches!(
            ledger.record_inspection(9001).expect_err("retired"),
            CohortError::HiddenSeedBurned { .. }
        ));
    }

    #[test]
    fn a_retired_hidden_seed_is_never_reinstalled_as_a_replacement() {
        // Pins: rotation cannot cycle back to a seed whose cases are known.
        let mut ledger = HiddenSeedLedger::new([1]);
        ledger.record_inspection(1).expect("first");
        ledger.record_inspection(1).expect("second");
        ledger.rotate(1, 2).expect("rotate");
        ledger.record_inspection(2).expect("first");
        ledger.record_inspection(2).expect("second");
        assert!(matches!(
            ledger.rotate(2, 1).expect_err("reuse"),
            CohortError::UnpairedComparison { .. }
        ));
    }

    #[test]
    fn raw_tenant_traffic_is_never_an_admissible_production_shaped_case() {
        // Pins: the treatment check comes before the paperwork check, so a fully
        // documented copy of tenant traffic is still refused.
        let case = ProductionShapedCase {
            case_id: "prod-1".to_string(),
            treatment: ProductionShapeTreatment::RawTenantTraffic,
            consent_ref: Some("consent-1".to_string()),
            attributed_to: Some("release-desk".to_string()),
            retention_provenance_ref: Some("retention-1".to_string()),
        };
        assert_eq!(
            case.validate().expect_err("raw traffic"),
            CohortError::InadmissibleProductionCase {
                case_id: "prod-1".to_string(),
                reason: "raw tenant traffic is never admissible".to_string(),
            }
        );
    }

    #[test]
    fn a_production_shaped_case_needs_consent_attribution_and_retention_links() {
        // Pins: each required provenance field is enforced independently.
        let admissible = ProductionShapedCase {
            case_id: "prod-2".to_string(),
            treatment: ProductionShapeTreatment::Sanitized,
            consent_ref: Some("consent-2".to_string()),
            attributed_to: Some("release-desk".to_string()),
            retention_provenance_ref: Some("retention-2".to_string()),
        };
        admissible.validate().expect("fully provenanced case");

        for mutate in [
            (|case: &mut ProductionShapedCase| case.consent_ref = None) as fn(&mut _),
            |case: &mut ProductionShapedCase| case.attributed_to = Some("  ".to_string()),
            |case: &mut ProductionShapedCase| case.retention_provenance_ref = None,
        ] {
            let mut case = admissible.clone();
            mutate(&mut case);
            assert!(matches!(
                case.validate().expect_err("missing provenance"),
                CohortError::InadmissibleProductionCase { .. }
            ));
        }

        let synthetic = ProductionShapedCase {
            treatment: ProductionShapeTreatment::Synthetic,
            ..admissible
        };
        synthetic.validate().expect("synthetic case is admissible");
    }
}
