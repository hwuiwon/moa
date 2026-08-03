//! Regression-suite parsing, held-out pooling, and structured execution input resolution.

use moa_artifacts::registry::{StoredSuiteContribution, SuiteContributionKind};
use moa_core::{
    canonical_json::canonical_json_bytes,
    error::{MoaError, Result},
    types::experience::LearningCandidate,
};
use moa_eval_core::TestSuite;
use serde_json::{Value, json};

use super::DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS;

const EXECUTION_INPUT_METADATA_KEY: &str = "execution_input";

/// Held-out evaluation material pooled for one gate run.
pub(super) struct HeldOutPool {
    /// Merged pool suite, when any source contributed cases.
    pub(super) suite: Option<TestSuite>,
    /// Number of distinct suite sources pooled.
    pub(super) source_count: usize,
    /// Pool entries skipped with the reason (for report honesty).
    pub(super) skipped: Vec<String>,
}

impl HeldOutPool {
    /// Base report object describing the pool before any execution results.
    pub(super) fn report_base(&self) -> Value {
        json!({
            "source_count": self.source_count,
            "case_count": self
                .suite
                .as_ref()
                .map(|suite| suite.cases.len())
                .unwrap_or(0),
            "skipped": self.skipped,
            "decision": if self.suite.is_some() { "pending" } else { "no_material" },
        })
    }
}

/// Pools held-out suites: the previous promoted revision's own suite plus any
/// sibling suites accumulated onto the candidate from deduped sessions.
///
/// Sources that fail to parse are skipped with a recorded reason rather than
/// rejecting the candidate — pool corruption is not a property of the draft
/// under review. Case names are prefixed by source so merged cases stay unique.
pub(super) fn collect_held_out_pool(
    previous_package: Option<&moa_skills::registry::StoredSkillPackage>,
    contributions: &[StoredSuiteContribution],
) -> HeldOutPool {
    let mut cases = Vec::new();
    let mut source_count = 0usize;
    let mut skipped = Vec::new();

    if let Some(file) = previous_package.and_then(|package| {
        package
            .files
            .iter()
            .find(|file| file.path == moa_skills::regression::REGRESSION_SUITE_PACKAGE_PATH)
    }) {
        match std::str::from_utf8(&file.content)
            .map_err(|error| error.to_string())
            .and_then(|text| toml::from_str::<TestSuite>(text).map_err(|error| error.to_string()))
        {
            Ok(suite) => {
                source_count += 1;
                cases.extend(prefixed_cases("prev", suite));
            }
            Err(error) => skipped.push(format!("previous revision suite unreadable: {error}")),
        }
    }

    for (index, contribution) in contributions
        .iter()
        .filter(|contribution| contribution.kind == SuiteContributionKind::Accumulated)
        .enumerate()
    {
        match toml::from_str::<TestSuite>(&contribution.suite_source) {
            Ok(suite) => {
                source_count += 1;
                cases.extend(prefixed_cases(&format!("sib{index}"), suite));
            }
            Err(error) => skipped.push(format!(
                "sibling suite `{}` unreadable: {error}",
                contribution.suite_name
            )),
        }
    }

    let suite = (!cases.is_empty()).then(|| TestSuite {
        schema_version: moa_eval_core::types::TEST_CASE_SCHEMA_VERSION,
        name: "held-out-pool".to_string(),
        description: Some(
            "Pooled held-out suites from prior revisions and sibling sessions".to_string(),
        ),
        cases,
        default_timeout_seconds: DEFAULT_SKILL_SUITE_TIMEOUT_SECONDS,
        tags: vec!["skill".to_string(), "held-out".to_string()],
    });
    HeldOutPool {
        suite,
        source_count,
        skipped,
    }
}

/// Prefixes pooled case names by source so merged cases stay unique.
fn prefixed_cases(
    prefix: &str,
    suite: TestSuite,
) -> impl Iterator<Item = moa_eval_core::TestCase> + '_ {
    suite.cases.into_iter().map(move |mut case| {
        case.name = format!("{prefix}-{}", case.name);
        case
    })
}

/// Suite source format; generated suites are always TOML.
const GENERATED_SUITE_SOURCE_FORMAT: &str = "toml";

/// Report fields describing the candidate's own generated suite.
pub(super) trait GeneratedSuiteReport {
    /// Describes the persisted generated-suite source without parsed suite fields.
    fn summary(&self) -> Value;
    /// Describes the persisted source together with fields from its parsed suite.
    fn summary_with_suite(&self, suite: &TestSuite) -> Value;
}

impl GeneratedSuiteReport for StoredSuiteContribution {
    fn summary(&self) -> Value {
        json!({
            "relative_path": self.suite_name,
            "source_format": GENERATED_SUITE_SOURCE_FORMAT,
            "source_text_present": true,
        })
    }

    fn summary_with_suite(&self, suite: &TestSuite) -> Value {
        json!({
            "relative_path": self.suite_name,
            "source_format": GENERATED_SUITE_SOURCE_FORMAT,
            "source_text_present": true,
            "suite_name": suite.name,
            "case_count": suite.cases.len(),
        })
    }
}

/// Returns the candidate's own generated suite from its contribution rows.
pub(super) fn generated_suite_contribution(
    contributions: &[StoredSuiteContribution],
) -> Option<&StoredSuiteContribution> {
    contributions
        .iter()
        .find(|contribution| contribution.kind == SuiteContributionKind::Generated)
}

/// Resolves the skill name captured by the learning candidate.
pub(super) fn skill_name(candidate: &LearningCandidate) -> Option<String> {
    candidate
        .payload
        .get("artifact_name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| candidate.target_label.clone())
}

#[derive(Debug, Clone, PartialEq)]
/// Structured execution input resolved across every case in one regression suite.
pub(super) enum RegressionExecutionInput {
    /// No case declares an explicit execution input.
    Missing,
    /// Every declared execution input has the same canonical value.
    Resolved(Value),
    /// Cases declare multiple distinct execution inputs.
    Ambiguous,
}

/// Resolves a suite's explicit structured input without parsing free-form case text.
pub(super) fn resolve_regression_execution_input(
    suite: &TestSuite,
) -> Result<RegressionExecutionInput> {
    let mut canonical_inputs = Vec::new();
    for input in suite
        .cases
        .iter()
        .filter_map(|case| case.metadata.get(EXECUTION_INPUT_METADATA_KEY))
    {
        let canonical = canonical_json_bytes(input)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        if canonical_inputs
            .iter()
            .any(|(existing, _)| existing == &canonical)
        {
            continue;
        }
        canonical_inputs.push((canonical, input.clone()));
    }

    match canonical_inputs.as_slice() {
        [] => Ok(RegressionExecutionInput::Missing),
        [(_, input)] => Ok(RegressionExecutionInput::Resolved(input.clone())),
        _ => Ok(RegressionExecutionInput::Ambiguous),
    }
}
