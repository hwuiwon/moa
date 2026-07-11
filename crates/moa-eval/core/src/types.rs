//! Serializable suite and agent configuration types for MOA evaluations.

use std::collections::HashMap;
use std::path::PathBuf;

use moa_core::types::action_policy::ActionPolicyEffect;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A complete test suite with multiple test cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(into = "TestSuiteDocument", from = "TestSuiteDocument")]
pub struct TestSuite {
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

/// A single test case: input plus evaluation expectations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TestCase {
    /// Case execution kind.
    #[serde(skip_serializing_if = "TestCaseKind::is_single")]
    pub kind: TestCaseKind,
    /// Stable case name.
    pub name: String,
    /// User input sent to the agent.
    pub input: String,
    /// Flexible expected-output rules.
    pub expected_output: Option<ExpectedOutput>,
    /// Expected tool-call trajectory, in order.
    pub expected_trajectory: Option<Vec<String>>,
    /// Per-case timeout override in seconds.
    pub timeout_seconds: Option<u64>,
    /// Tags applied to this case.
    pub tags: Vec<String>,
    /// Arbitrary case metadata.
    pub metadata: HashMap<String, Value>,
    /// Long-conversation case details when `kind = "long"`.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub long: Option<LongTestCase>,
}

impl TestCase {
    /// Returns long-conversation details for a long test case.
    pub fn long_case(&self) -> crate::Result<&LongTestCase> {
        if self.kind != TestCaseKind::Long {
            return Err(crate::EvalError::InvalidConfig(format!(
                "test case '{}' is not a long-conversation case",
                self.name
            )));
        }

        let long = self.long.as_ref().ok_or_else(|| {
            crate::EvalError::InvalidConfig(format!(
                "long test case '{}' is missing transcript details",
                self.name
            ))
        })?;
        match long.mode {
            LongConversationMode::Recorded => {
                if long.transcript.as_os_str().is_empty() {
                    return Err(crate::EvalError::InvalidConfig(format!(
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
                    return Err(crate::EvalError::InvalidConfig(format!(
                        "long test case '{}' must set goal_card for scripted_user mode",
                        self.name
                    )));
                }
                if long
                    .scripted_user
                    .as_ref()
                    .is_none_or(|path| path.as_os_str().is_empty())
                {
                    return Err(crate::EvalError::InvalidConfig(format!(
                        "long test case '{}' must set scripted_user for scripted_user mode",
                        self.name
                    )));
                }
            }
        }
        if long.expectations.as_os_str().is_empty() {
            return Err(crate::EvalError::InvalidConfig(format!(
                "long test case '{}' must set expectations",
                self.name
            )));
        }
        if long
            .secondary_session
            .as_ref()
            .is_some_and(|secondary| secondary.transcript.as_os_str().is_empty())
        {
            return Err(crate::EvalError::InvalidConfig(format!(
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

/// Expected-output rules for an agent response.
///
/// Every field below is a hard requirement: the `output_match` evaluator emits a
/// diagnostic fractional score plus an `output_match_required` boolean gate, and
/// any unmet rule (a missing required fragment, a present exclusion, a regex or
/// exact mismatch) fails the scenario regardless of the fractional score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExpectedOutput {
    /// Response text must contain all of these fragments (required, all-of).
    pub contains: Vec<String>,
    /// Response text must not contain any of these fragments (required exclusion).
    pub not_contains: Vec<String>,
    /// Regular expression the response must match when set (required).
    pub regex: Option<String>,
    /// Exact response text the agent must return when set (required).
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
struct TestSuiteHeader {
    name: String,
    description: Option<String>,
    default_timeout_seconds: u64,
    tags: Vec<String>,
}

impl From<TestSuiteDocument> for TestSuite {
    fn from(value: TestSuiteDocument) -> Self {
        Self {
            name: value.suite.name,
            description: value.suite.description,
            cases: value.cases,
            default_timeout_seconds: value.suite.default_timeout_seconds,
            tags: value.suite.tags,
        }
    }
}

impl From<TestSuite> for TestSuiteDocument {
    fn from(value: TestSuite) -> Self {
        Self {
            suite: TestSuiteHeader {
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
