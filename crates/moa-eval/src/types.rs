//! Serializable suite and agent configuration types for MOA evaluations.

use std::collections::HashMap;
use std::path::PathBuf;

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
}

/// Flexible expected-output rules for an agent response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExpectedOutput {
    /// Response text must contain all of these fragments.
    pub contains: Vec<String>,
    /// Response text must not contain any of these fragments.
    pub not_contains: Vec<String>,
    /// Regular expression the response should match.
    pub regex: Option<String>,
    /// Exact response text expected from the agent.
    pub exact: Option<String>,
    /// Key facts that should appear in the response.
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
    /// Skill-selection overrides.
    pub skills: SkillOverride,
    /// Memory overrides.
    pub memory: MemoryOverride,
    /// Instruction overrides.
    pub instructions: InstructionOverride,
    /// Tool-selection overrides.
    pub tools: ToolOverride,
    /// Permission overrides.
    pub permissions: PermissionOverride,
    /// Arbitrary metadata labels for comparison and reporting.
    pub metadata: HashMap<String, String>,
}

/// Skill-selection overrides for an agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SkillOverride {
    /// Skill paths to include.
    pub include: Vec<String>,
    /// Skill paths to exclude.
    pub exclude: Vec<String>,
    /// When true, only the listed skills are enabled.
    pub exclusive: bool,
}

/// Memory overrides for an agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MemoryOverride {
    /// Workspace memory snapshot path.
    pub workspace_memory_path: Option<PathBuf>,
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

/// Permission overrides for an agent config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PermissionOverride {
    /// Auto-approves all tool requests.
    pub auto_approve_all: bool,
    /// Auto-approves the listed tools.
    pub auto_approve: Vec<String>,
    /// Always denies the listed tools.
    pub always_deny: Vec<String>,
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
    skills: SkillOverride,
    memory: MemoryOverride,
    instructions: InstructionOverride,
    tools: ToolOverride,
    permissions: PermissionOverride,
    metadata: HashMap<String, String>,
}

impl From<AgentConfigDocument> for AgentConfig {
    fn from(value: AgentConfigDocument) -> Self {
        Self {
            name: value.agent.name,
            model: value.agent.model,
            skills: value.agent.skills,
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
                skills: value.skills,
                memory: value.memory,
                instructions: value.instructions,
                tools: value.tools,
                permissions: value.permissions,
                metadata: value.metadata,
            },
        }
    }
}
