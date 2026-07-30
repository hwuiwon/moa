//! Serializable suite and agent configuration types for MOA evaluations.

use std::collections::HashMap;
use std::path::PathBuf;

use moa_core::types::action_policy::ActionPolicyEffect;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::assertion::AssertionSpec;

/// Schema version of the [`TestCase`] model.
///
/// Version 2 replaced the untyped `expected_output`/`expected_trajectory` pair
/// with [`TestCase::assertions`]. There is deliberately no compatibility
/// deserializer: a suite document that does not declare this exact version is
/// rejected by [`crate::loader::load_suite`], because a silently-ignored v1
/// expectation block would turn into a vacuous pass.
pub const TEST_CASE_SCHEMA_VERSION: u32 = 2;

/// A complete test suite with multiple test cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(into = "TestSuiteDocument", from = "TestSuiteDocument")]
pub struct TestSuite {
    /// Case-model schema version the document declares.
    pub schema_version: u32,
    /// Stable suite name.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Cases included in the suite.
    pub cases: Vec<TestCase>,
    /// Default timeout in seconds for cases without an explicit override.
    pub default_timeout_seconds: u64,
    /// Tags applied to the suite as a whole.
    pub tags: Vec<String>,
}

impl Default for TestSuite {
    fn default() -> Self {
        Self {
            schema_version: TEST_CASE_SCHEMA_VERSION,
            name: String::new(),
            description: None,
            cases: Vec::new(),
            default_timeout_seconds: 0,
            tags: Vec::new(),
        }
    }
}

impl TestSuite {
    /// Rejects a suite this build cannot execute exactly as authored.
    ///
    /// Checks the declared schema version, the suite name, and every case's
    /// assertion set against the built-in evaluator registry, so an unusable
    /// suite is refused before it can burn a provider call.
    pub fn validate(&self) -> crate::Result<()> {
        if self.schema_version != TEST_CASE_SCHEMA_VERSION {
            return Err(crate::Error::InvalidConfig(format!(
                "suite '{}' declares case schema version {} but this build requires {}",
                self.name, self.schema_version, TEST_CASE_SCHEMA_VERSION
            )));
        }
        if self.name.trim().is_empty() {
            return Err(crate::Error::InvalidConfig(
                "suite is missing [suite].name".to_string(),
            ));
        }
        let registry = crate::assertion::builtin_registry();
        for case in &self.cases {
            registry.check_case(case)?;
        }
        Ok(())
    }
}

/// A single test case: input plus the typed assertions it claims.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "TestCaseDocument", into = "TestCaseDocument")]
pub struct TestCase {
    /// Case-model schema version, stamped from the suite header on load.
    ///
    /// Not serialized: the version is a document-level fact carried by
    /// `[suite].schema_version`, and duplicating it per case would let the two
    /// disagree.
    #[serde(skip)]
    pub schema_version: u32,
    /// Case execution kind.
    #[serde(skip_serializing_if = "TestCaseKind::is_single")]
    pub kind: TestCaseKind,
    /// Stable case name.
    pub name: String,
    /// User input sent to the agent.
    pub input: String,
    /// Per-case timeout override in seconds.
    pub timeout_seconds: Option<u64>,
    /// Tags applied to this case.
    pub tags: Vec<String>,
    /// Which oracle produced the case's text assertions, when the case was
    /// machine-generated. Reporting distinguishes fact-grounded cases from
    /// keyword-fallback cases; execution treats both identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<SuiteOracle>,
    /// Arbitrary case metadata.
    pub metadata: HashMap<String, Value>,
    /// Typed assertions this case claims, evaluated against captured evidence.
    ///
    /// Each entry names a server-registered evaluator and hands it pure JSON
    /// parameters. A case can never carry an executable oracle.
    pub assertions: Vec<AssertionSpec>,
    /// Long-conversation case details when `kind = "long"`.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub long: Option<LongTestCase>,
}

impl Default for TestCase {
    fn default() -> Self {
        Self {
            schema_version: TEST_CASE_SCHEMA_VERSION,
            kind: TestCaseKind::default(),
            name: String::new(),
            input: String::new(),
            timeout_seconds: None,
            tags: Vec::new(),
            oracle: None,
            metadata: HashMap::new(),
            assertions: Vec::new(),
            long: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct TestCaseDocument {
    #[serde(skip_serializing_if = "TestCaseKind::is_single")]
    kind: TestCaseKind,
    name: String,
    input: String,
    timeout_seconds: Option<u64>,
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oracle: Option<SuiteOracle>,
    metadata: HashMap<String, Value>,
    assertions: Vec<AssertionSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    goal_card: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transcript: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scripted_user: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secondary_session: Option<SecondaryLongSession>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expectations: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<LongConversationMode>,
}

impl From<TestCaseDocument> for TestCase {
    fn from(value: TestCaseDocument) -> Self {
        let has_long_fields = value.goal_card.is_some()
            || value.transcript.is_some()
            || value.scripted_user.is_some()
            || value.secondary_session.is_some()
            || value.expectations.is_some()
            || value.mode.is_some();
        let long = has_long_fields.then(|| LongTestCase {
            goal_card: value.goal_card,
            transcript: value.transcript.unwrap_or_default(),
            scripted_user: value.scripted_user,
            secondary_session: value.secondary_session,
            expectations: value.expectations.unwrap_or_default(),
            mode: value.mode.unwrap_or_default(),
        });
        Self {
            schema_version: TEST_CASE_SCHEMA_VERSION,
            kind: value.kind,
            name: value.name,
            input: value.input,
            timeout_seconds: value.timeout_seconds,
            tags: value.tags,
            oracle: value.oracle,
            metadata: value.metadata,
            assertions: value.assertions,
            long,
        }
    }
}

impl From<TestCase> for TestCaseDocument {
    fn from(value: TestCase) -> Self {
        let (goal_card, transcript, scripted_user, secondary_session, expectations, mode) =
            match value.long {
                Some(long) => (
                    long.goal_card,
                    Some(long.transcript),
                    long.scripted_user,
                    long.secondary_session,
                    Some(long.expectations),
                    Some(long.mode),
                ),
                None => (None, None, None, None, None, None),
            };
        Self {
            kind: value.kind,
            name: value.name,
            input: value.input,
            timeout_seconds: value.timeout_seconds,
            tags: value.tags,
            oracle: value.oracle,
            metadata: value.metadata,
            assertions: value.assertions,
            goal_card,
            transcript,
            scripted_user,
            secondary_session,
            expectations,
            mode,
        }
    }
}

/// Provenance of a generated case's text assertions.
///
/// Auto-generated skill regression cases derive their `contains` expectations
/// from session events. This records which extraction strategy produced them so
/// gate reports can weight a fact-grounded oracle differently from a weaker
/// keyword fallback. It carries no execution semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteOracle {
    /// Expectations are verifiable facts present in both tool results and the response.
    GroundedFacts,
    /// Expectations fell back to the longest response keywords (no grounded facts found).
    Keywords,
}

impl TestCase {
    /// Returns long-conversation details for a long test case.
    pub fn long_case(&self) -> crate::Result<&LongTestCase> {
        if self.kind != TestCaseKind::Long {
            return Err(crate::Error::InvalidConfig(format!(
                "test case '{}' is not a long-conversation case",
                self.name
            )));
        }

        let long = self.long.as_ref().ok_or_else(|| {
            crate::Error::InvalidConfig(format!(
                "long test case '{}' is missing transcript details",
                self.name
            ))
        })?;
        match long.mode {
            LongConversationMode::Recorded => {
                if long.transcript.as_os_str().is_empty() {
                    return Err(crate::Error::InvalidConfig(format!(
                        "long test case '{}' must set transcript",
                        self.name
                    )));
                }
            }
            LongConversationMode::ScriptedUser => {
                if long
                    .goal_card
                    .as_ref()
                    .is_none_or(|path| path.as_os_str().is_empty())
                {
                    return Err(crate::Error::InvalidConfig(format!(
                        "long test case '{}' must set goal_card for scripted_user mode",
                        self.name
                    )));
                }
                if long
                    .scripted_user
                    .as_ref()
                    .is_none_or(|path| path.as_os_str().is_empty())
                {
                    return Err(crate::Error::InvalidConfig(format!(
                        "long test case '{}' must set scripted_user for scripted_user mode",
                        self.name
                    )));
                }
            }
        }
        if long.expectations.as_os_str().is_empty() {
            return Err(crate::Error::InvalidConfig(format!(
                "long test case '{}' must set expectations",
                self.name
            )));
        }
        if long
            .secondary_session
            .as_ref()
            .is_some_and(|secondary| secondary.transcript.as_os_str().is_empty())
        {
            return Err(crate::Error::InvalidConfig(format!(
                "long test case '{}' secondary_session must set transcript",
                self.name
            )));
        }

        Ok(long)
    }
}

/// Discriminator for single-turn vs long-conversation test cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TestCaseKind {
    /// Existing single-turn eval behavior.
    #[default]
    Single,
    /// Multi-turn long-conversation eval behavior.
    Long,
}

impl TestCaseKind {
    /// Returns true when this is the default single-turn case kind.
    #[must_use]
    pub const fn is_single(&self) -> bool {
        matches!(self, Self::Single)
    }
}

/// Long-conversation execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LongConversationMode {
    /// Replays a recorded provider transcript.
    #[default]
    Recorded,
    /// Simulates a user from a goal card.
    ScriptedUser,
}

/// TOML-loadable long-conversation test-case details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LongTestCase {
    /// Optional goal card used by future scripted-user mode.
    pub goal_card: Option<PathBuf>,
    /// JSONL transcript used by recorded mode.
    pub transcript: PathBuf,
    /// JSONL script used by scripted-user mode.
    pub scripted_user: Option<PathBuf>,
    /// Optional secondary session for multi-session long-conversation scenarios.
    pub secondary_session: Option<SecondaryLongSession>,
    /// Scenario expectations file.
    pub expectations: PathBuf,
    /// Long-conversation execution mode.
    pub mode: LongConversationMode,
}

/// Secondary session configuration for a long-conversation test case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecondaryLongSession {
    /// JSONL transcript used by the secondary recorded session.
    pub transcript: PathBuf,
    /// Deterministic interleaving strategy used to drive both sessions.
    pub interleaving: LongSessionInterleaving,
}

impl Default for SecondaryLongSession {
    fn default() -> Self {
        Self {
            transcript: PathBuf::new(),
            interleaving: LongSessionInterleaving::Sequential,
        }
    }
}

/// Deterministic multi-session execution strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LongSessionInterleaving {
    /// Finish the primary session before starting the secondary session.
    #[default]
    Sequential,
    /// Alternate primary and secondary user turns.
    RoundRobin,
    /// Run phase one to completion, then phase two with shared workspace state.
    Phased,
}

/// Response-text rules: the parameter block of the `text_match` evaluator.
///
/// This is response-text matching and nothing else. It cannot express an
/// environment or action oracle, which is exactly why those live in their own
/// assertion categories instead of being smuggled in here.
///
/// Every field is a hard all-of requirement: any unmet rule (a missing required
/// fragment, a present exclusion, a regex or exact mismatch) fails the
/// assertion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ExpectedOutput {
    /// Response text must contain all of these fragments (required, all-of).
    pub contains: Vec<String>,
    /// Response text must not contain any of these fragments (required exclusion).
    pub not_contains: Vec<String>,
    /// Regular expression the response must match when set (required).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    /// Exact response text the agent must return when set (required).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact: Option<String>,
    /// Key facts that must all appear in the response (required, all-of).
    pub facts: Vec<String>,
}

/// Serializable description of an agent variant to test.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(into = "AgentConfigDocument", from = "AgentConfigDocument")]
pub struct AgentConfig {
    /// Stable config name.
    pub name: String,
    /// Optional model override.
    pub model: Option<String>,
    /// Memory overrides.
    pub memory: MemoryOverride,
    /// Instruction overrides.
    pub instructions: InstructionOverride,
    /// Tool-selection overrides.
    pub tools: ToolOverride,
    /// Action-policy overrides.
    pub permissions: ActionPolicyOverride,
    /// Arbitrary metadata labels for comparison and reporting.
    pub metadata: HashMap<String, String>,
}

/// Memory overrides for an agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MemoryOverride {
    /// Tenant knowledge snapshot path.
    pub tenant_memory_path: Option<PathBuf>,
    /// User memory snapshot path.
    pub user_memory_path: Option<PathBuf>,
    /// When true, start from empty memory instead of defaults.
    pub clear_defaults: bool,
}

/// Instruction overrides for an agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InstructionOverride {
    /// Replaces the default system prompt entirely.
    pub system_prompt_override: Option<String>,
    /// Appends additional text to the default system prompt.
    pub system_prompt_append: Option<String>,
    /// Optional workspace instructions fixture path.
    pub workspace_instructions_path: Option<PathBuf>,
}

/// Tool-selection overrides for an agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ToolOverride {
    /// Exact enabled tool list, when replacing defaults.
    pub enabled: Option<Vec<String>>,
    /// Tools disabled from the default set.
    pub disable: Vec<String>,
}

/// Action-policy overrides for an agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ActionPolicyOverride {
    /// Default effect when no rule or tool-specific config matches.
    pub default_effect: Option<ActionPolicyEffect>,
    /// Explicit policy rules seeded before eval execution.
    pub allow_rules: Vec<ActionPolicyRuleOverride>,
    /// Tools that should be recorded for tenant-admin review.
    pub admin_review: Vec<String>,
    /// Always denies the listed tools.
    pub always_deny: Vec<String>,
}

/// Action-policy rule seeded by an eval agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionPolicyRuleOverride {
    /// Tool name this rule applies to.
    pub tool: String,
    /// Glob pattern used for matching normalized inputs.
    pub pattern: String,
    /// Optional human-readable reason attached to the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
struct TestSuiteDocument {
    suite: TestSuiteHeader,
    cases: Vec<TestCase>,
}

/// Suite header. `schema_version` has no serde default beyond the container
/// zero, so a legacy document that never declared one deserializes to `0` and
/// is rejected by [`TestSuite::validate`] instead of being reinterpreted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
struct TestSuiteHeader {
    schema_version: u32,
    name: String,
    description: Option<String>,
    default_timeout_seconds: u64,
    tags: Vec<String>,
}

impl From<TestSuiteDocument> for TestSuite {
    fn from(value: TestSuiteDocument) -> Self {
        let schema_version = value.suite.schema_version;
        Self {
            schema_version,
            name: value.suite.name,
            description: value.suite.description,
            // Stamp the document version onto every case so a case can never
            // disagree with the file it came from.
            cases: value
                .cases
                .into_iter()
                .map(|case| TestCase {
                    schema_version,
                    ..case
                })
                .collect(),
            default_timeout_seconds: value.suite.default_timeout_seconds,
            tags: value.suite.tags,
        }
    }
}

impl From<TestSuite> for TestSuiteDocument {
    fn from(value: TestSuite) -> Self {
        Self {
            suite: TestSuiteHeader {
                schema_version: value.schema_version,
                name: value.name,
                description: value.description,
                default_timeout_seconds: value.default_timeout_seconds,
                tags: value.tags,
            },
            cases: value.cases,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
struct AgentConfigDocument {
    agent: AgentConfigBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
struct AgentConfigBody {
    name: String,
    model: Option<String>,
    memory: MemoryOverride,
    instructions: InstructionOverride,
    tools: ToolOverride,
    permissions: ActionPolicyOverride,
    metadata: HashMap<String, String>,
}

impl From<AgentConfigDocument> for AgentConfig {
    fn from(value: AgentConfigDocument) -> Self {
        Self {
            name: value.agent.name,
            model: value.agent.model,
            memory: value.agent.memory,
            instructions: value.agent.instructions,
            tools: value.agent.tools,
            permissions: value.agent.permissions,
            metadata: value.agent.metadata,
        }
    }
}

impl From<AgentConfig> for AgentConfigDocument {
    fn from(value: AgentConfig) -> Self {
        Self {
            agent: AgentConfigBody {
                name: value.name,
                model: value.model,
                memory: value.memory,
                instructions: value.instructions,
                tools: value.tools,
                permissions: value.permissions,
                metadata: value.metadata,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TestSuite;

    #[test]
    fn test_case_rejects_legacy_and_unknown_fields() {
        // Pins: stale v1 expectations and misspelled case keys cannot deserialize
        // into a case with an empty assertion list and vacuously pass.
        for unknown_field in [
            "expected_output = { contains = [\"needle\"] }",
            "expected_trajectory = []",
            "assertion = []",
        ] {
            let document = format!(
                r#"
[suite]
schema_version = 2
name = "strict-cases"

[[cases]]
name = "case-a"
input = "hello"
{unknown_field}
"#
            );

            let error = toml::from_str::<TestSuite>(&document)
                .expect_err("unknown case fields must be rejected");
            assert!(
                error.to_string().contains("unknown field"),
                "unexpected error for {unknown_field}: {error}"
            );
        }
    }
}
