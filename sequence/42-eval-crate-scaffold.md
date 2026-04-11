# Step 42 — moa-eval Crate Scaffold

_Core types, test suite format, agent configuration model, evaluator trait._

---

## 1. What this step is about

This step creates the `moa-eval` crate — a general-purpose agent evaluation framework that is **not tied to Langfuse or any specific backend**. It defines:

- **`AgentConfig`**: A serializable description of an agent variant — which skills, memory, system prompt, model, tools, and permissions to use
- **`TestSuite` / `TestCase`**: File-backed test definitions with inputs, expected outputs, and expected trajectories (tool call sequences)
- **`EvalResult`**: The outcome of running a test case, capturing the full execution trace
- **`Evaluator` trait**: A pluggable scoring interface — can be a simple string matcher, trajectory comparator, LLM-as-judge, or custom logic
- **`Reporter` trait**: A pluggable output sink — terminal, JSON file, or future custom platform

The crate defines no execution logic yet (that's Step 43) — only types, traits, and the file format.

---

## 2. Files/directories to read

- **`moa-core/src/types.rs`** — `SessionId`, `WorkspaceId`, `UserId`, `ModelCapabilities`, `CompletionRequest`. The eval crate will reference these.
- **`moa-core/src/traits.rs`** — `LLMProvider`, `SessionStore`, `MemoryStore`, `HandProvider`. The eval engine (Step 43) will construct these from `AgentConfig`.
- **`moa-skills/src/`** — Skill format, `SkillMetadata`. `AgentConfig` references skills by path.
- **`moa-memory/src/`** — `FileMemoryStore`. Eval suites can provide custom memory snapshots.
- **`moa-brain/src/pipeline/`** — Pipeline stages. `AgentConfig` can override stage behavior.
- **Existing sequence steps** — Scan `sequence/*.md` to understand the pattern for adding a new crate to the workspace.

---

## 3. Goal

A developer can write a test suite file like this:

```toml
# tests/suites/deploy-skill-comparison.toml
[suite]
name = "deploy-skill-comparison"
description = "Compare deploy skill v1 vs v2"

[[cases]]
name = "basic-staging-deploy"
input = "Deploy the app to staging"
expected_output.contains = ["staging", "deployed", "health check"]
expected_trajectory = ["bash", "bash", "bash"]  # fly status, fly deploy, curl health
timeout_seconds = 120
tags = ["deployment", "staging"]

[[cases]]
name = "production-deploy-with-confirmation"
input = "Deploy to production"
expected_output.contains = ["production", "deployed"]
expected_trajectory = ["bash", "bash", "bash", "bash"]
timeout_seconds = 180
tags = ["deployment", "production"]

[cases.metadata]
requires_approval = true
```

And define agent configurations to test against:

```toml
# tests/configs/deploy-v1.toml
[agent]
name = "deploy-v1"
model = "claude-sonnet-4-20250514"

[agent.skills]
include = ["skills/deploy-to-fly"]
# exclude = []  # optionally exclude default skills

[agent.memory]
workspace_memory_path = "tests/fixtures/memory/webapp-v1/"

[agent.instructions]
system_prompt_override = "You are a deployment assistant."

[agent.tools]
enabled = ["bash", "file_read", "file_write"]

[agent.permissions]
auto_approve_all = true  # skip approval flow in tests
```

---

## 4. Rules

- **No Langfuse dependency.** The eval crate must not import anything Langfuse-specific. Langfuse export is a reporter plugin, added in Step 44 behind a feature flag.
- **No network calls in this step.** This step defines types and file parsing only. Execution is Step 43.
- **Serializable everything.** All types must derive `Serialize` + `Deserialize`. Test suites, configs, and results are all file-backed.
- **Test suite format: TOML.** Consistent with MOA's config format. TOML is human-editable and diff-friendly.
- **`Evaluator` trait must be sync-compatible.** Some evaluators (string matching) are sync; others (LLM-as-judge) are async. Use `async_trait`.
- **Agent configs are compositional.** A config can say "use all defaults, but swap these skills" or "use this exact memory snapshot." Partial overrides, not complete specifications.

---

## 5. Tasks

### 5a. Create the crate

```bash
mkdir -p moa-eval/src
```

Add to workspace `Cargo.toml`:
```toml
members = [
    # ... existing ...
    "moa-eval",
]
```

`moa-eval/Cargo.toml`:
```toml
[package]
name = "moa-eval"
version = "0.1.0"
edition = "2021"

[dependencies]
moa-core = { path = "../moa-core" }
serde = { workspace = true }
serde_json = { workspace = true }
toml.workspace = true
chrono.workspace = true
uuid.workspace = true
thiserror.workspace = true
async-trait = "0.1"
tokio = { workspace = true, features = ["fs"] }
tracing.workspace = true
```

### 5b. Define core types in `moa-eval/src/types.rs`

```rust
/// A complete test suite with multiple test cases.
pub struct TestSuite {
    pub name: String,
    pub description: Option<String>,
    pub cases: Vec<TestCase>,
    pub default_timeout_seconds: u64,
    pub tags: Vec<String>,
}

/// A single test case: input + expectations.
pub struct TestCase {
    pub name: String,
    pub input: String,                           // The user prompt
    pub expected_output: Option<ExpectedOutput>,  // What the response should contain
    pub expected_trajectory: Option<Vec<String>>,  // Expected tool call names in order
    pub timeout_seconds: Option<u64>,
    pub tags: Vec<String>,
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Flexible output expectations.
pub struct ExpectedOutput {
    pub contains: Vec<String>,            // Response must contain all of these
    pub not_contains: Vec<String>,        // Response must not contain any of these
    pub regex: Option<String>,            // Response must match this regex
    pub exact: Option<String>,            // Exact match (rare for agents)
    pub facts: Vec<String>,              // Key facts that should be present (for LLM judge)
}

/// Description of an agent variant to test.
pub struct AgentConfig {
    pub name: String,
    pub model: Option<String>,                    // Override default model
    pub skills: SkillOverride,
    pub memory: MemoryOverride,
    pub instructions: InstructionOverride,
    pub tools: ToolOverride,
    pub permissions: PermissionOverride,
    pub metadata: HashMap<String, String>,        // Arbitrary labels for comparison
}

pub struct SkillOverride {
    pub include: Vec<String>,     // Skill paths to include (additive or exclusive)
    pub exclude: Vec<String>,     // Skill paths to exclude from defaults
    pub exclusive: bool,          // If true, ONLY include listed skills (ignore defaults)
}

pub struct MemoryOverride {
    pub workspace_memory_path: Option<PathBuf>,   // Path to a memory directory snapshot
    pub user_memory_path: Option<PathBuf>,
    pub clear_defaults: bool,                      // Start with empty memory
}

pub struct InstructionOverride {
    pub system_prompt_override: Option<String>,    // Replace the default system prompt
    pub system_prompt_append: Option<String>,      // Append to the default system prompt
    pub workspace_instructions_path: Option<PathBuf>,
}

pub struct ToolOverride {
    pub enabled: Option<Vec<String>>,              // Exact tool list (if set, replaces defaults)
    pub disable: Vec<String>,                      // Remove specific tools from defaults
}

pub struct PermissionOverride {
    pub auto_approve_all: bool,                    // Skip all approval prompts
    pub auto_approve: Vec<String>,                 // Auto-approve these tools
    pub always_deny: Vec<String>,                  // Always deny these tools
}
```

### 5c. Define result types in `moa-eval/src/results.rs`

```rust
/// The outcome of running one test case with one agent config.
pub struct EvalResult {
    pub test_case: String,
    pub agent_config: String,
    pub status: EvalStatus,
    pub response: Option<String>,              // Final agent response text
    pub trajectory: Vec<TrajectoryStep>,        // Actual tool call sequence
    pub scores: Vec<EvalScore>,                // Scores from evaluators
    pub metrics: EvalMetrics,
    pub trace_id: Option<String>,              // OTel trace ID for linking to Langfuse/Tempo
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

pub enum EvalStatus {
    Passed,
    Failed,
    Error,                                     // Agent errored, not an eval failure
    Timeout,
    Skipped,
}

pub struct TrajectoryStep {
    pub tool_name: String,
    pub input_summary: String,                 // Truncated input
    pub output_summary: String,                // Truncated output
    pub success: bool,
    pub duration_ms: u64,
}

pub struct EvalMetrics {
    pub total_tokens: usize,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cost_dollars: f64,
    pub latency_ms: u64,
    pub turn_count: usize,
    pub tool_call_count: usize,
    pub tool_error_count: usize,
}

pub struct EvalScore {
    pub evaluator: String,                     // Name of the evaluator that produced this
    pub name: String,                          // Score name (e.g., "trajectory_match")
    pub value: ScoreValue,
    pub comment: Option<String>,               // Evaluator reasoning
}

pub enum ScoreValue {
    Numeric(f64),                              // 0.0 to 1.0 typically
    Boolean(bool),
    Categorical(String),
}
```

### 5d. Define the `Evaluator` trait in `moa-eval/src/evaluator.rs`

```rust
#[async_trait]
pub trait Evaluator: Send + Sync {
    /// Human-readable name of this evaluator.
    fn name(&self) -> &str;
    
    /// Evaluate a single test case result.
    async fn evaluate(
        &self,
        test_case: &TestCase,
        result: &EvalResult,
    ) -> Result<Vec<EvalScore>>;
}
```

### 5e. Define the `Reporter` trait in `moa-eval/src/reporter.rs`

```rust
#[async_trait]
pub trait Reporter: Send + Sync {
    /// Report results after a full suite run.
    async fn report(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        results: &[EvalResult],
    ) -> Result<()>;
}
```

### 5f. Implement TOML parsing in `moa-eval/src/loader.rs`

```rust
/// Load a test suite from a TOML file.
pub fn load_suite(path: &Path) -> Result<TestSuite> { ... }

/// Load an agent config from a TOML file.
pub fn load_agent_config(path: &Path) -> Result<AgentConfig> { ... }

/// Discover all suites in a directory.
pub fn discover_suites(dir: &Path) -> Result<Vec<PathBuf>> { ... }

/// Discover all agent configs in a directory.
pub fn discover_configs(dir: &Path) -> Result<Vec<PathBuf>> { ... }
```

### 5g. Create example test suite and config files

Create `moa-eval/examples/` with:
- `example-suite.toml` — A simple test suite with 3 cases
- `example-config-baseline.toml` — A baseline agent config
- `example-config-variant.toml` — A variant with different skills/model

---

## 6. How it should be implemented

The crate structure should be:

```
moa-eval/
├── Cargo.toml
├── src/
│   ├── lib.rs           # Re-exports
│   ├── types.rs          # TestSuite, TestCase, AgentConfig, overrides
│   ├── results.rs        # EvalResult, EvalScore, EvalMetrics, TrajectoryStep
│   ├── evaluator.rs      # Evaluator trait
│   ├── reporter.rs       # Reporter trait
│   ├── loader.rs         # TOML parsing for suites and configs
│   └── error.rs          # EvalError type
└── examples/
    ├── example-suite.toml
    ├── example-config-baseline.toml
    └── example-config-variant.toml
```

All types should derive: `Debug, Clone, Serialize, Deserialize`. Use `#[serde(default)]` liberally for optional fields to keep TOML files concise.

For `AgentConfig`, make override structs default to "no override" (empty vecs, None values, false booleans). This way a minimal config only specifies what differs from default:

```toml
[agent]
name = "variant-a"
model = "gpt-4o"
# Everything else uses defaults
```

---

## 7. Deliverables

- [ ] `moa-eval/Cargo.toml` — New crate with dependencies
- [ ] `moa-eval/src/lib.rs` — Module declarations and public re-exports
- [ ] `moa-eval/src/types.rs` — `TestSuite`, `TestCase`, `ExpectedOutput`, `AgentConfig`, all override structs
- [ ] `moa-eval/src/results.rs` — `EvalResult`, `EvalStatus`, `TrajectoryStep`, `EvalMetrics`, `EvalScore`, `ScoreValue`
- [ ] `moa-eval/src/evaluator.rs` — `Evaluator` trait
- [ ] `moa-eval/src/reporter.rs` — `Reporter` trait
- [ ] `moa-eval/src/loader.rs` — `load_suite()`, `load_agent_config()`, `discover_suites()`, `discover_configs()`
- [ ] `moa-eval/src/error.rs` — `EvalError` enum
- [ ] `moa-eval/examples/example-suite.toml` — Example test suite
- [ ] `moa-eval/examples/example-config-baseline.toml` — Baseline agent config
- [ ] `moa-eval/examples/example-config-variant.toml` — Variant agent config
- [ ] `Cargo.toml` (workspace root) — `moa-eval` added to members

---

## 8. Acceptance criteria

1. **`cargo build -p moa-eval` compiles** with no errors or warnings.
2. **TOML roundtrip.** A `TestSuite` serialized to TOML and deserialized back produces an identical struct.
3. **AgentConfig roundtrip.** Same for `AgentConfig`.
4. **Minimal config works.** A TOML file with only `[agent] name = "test"` parses into a valid `AgentConfig` with all defaults.
5. **Example files parse.** `load_suite("examples/example-suite.toml")` and `load_agent_config("examples/example-config-baseline.toml")` succeed.
6. **Types are compatible with moa-core.** `AgentConfig.model` can be passed to `CompletionRequest`. `EvalResult.trace_id` is a valid OTel trace ID string.
7. **No runtime dependencies.** This step has zero network, filesystem (beyond file reads), or LLM dependencies.

---

## 9. Testing

### Unit tests (in `moa-eval/tests/`)

**Test 1: Parse example suite**
```rust
#[test]
fn parse_example_suite() {
    let suite = load_suite(Path::new("examples/example-suite.toml")).unwrap();
    assert!(!suite.name.is_empty());
    assert!(!suite.cases.is_empty());
    for case in &suite.cases {
        assert!(!case.name.is_empty());
        assert!(!case.input.is_empty());
    }
}
```

**Test 2: Parse example configs**
```rust
#[test]
fn parse_example_configs() {
    let baseline = load_agent_config(Path::new("examples/example-config-baseline.toml")).unwrap();
    let variant = load_agent_config(Path::new("examples/example-config-variant.toml")).unwrap();
    assert_ne!(baseline.name, variant.name);
}
```

**Test 3: Minimal config defaults**
```rust
#[test]
fn minimal_config_has_defaults() {
    let toml = r#"
        [agent]
        name = "minimal"
    "#;
    let config: AgentConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.name, "minimal");
    assert!(config.model.is_none());
    assert!(!config.permissions.auto_approve_all);
    assert!(config.skills.include.is_empty());
    assert!(!config.skills.exclusive);
}
```

**Test 4: Suite TOML roundtrip**
```rust
#[test]
fn suite_serialization_roundtrip() {
    let suite = TestSuite {
        name: "test".into(),
        cases: vec![TestCase {
            name: "case1".into(),
            input: "Hello".into(),
            expected_trajectory: Some(vec!["bash".into()]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&suite).unwrap();
    let parsed: TestSuite = toml::from_str(&toml_str).unwrap();
    assert_eq!(suite.name, parsed.name);
    assert_eq!(suite.cases.len(), parsed.cases.len());
}
```

**Test 5: EvalResult construction**
```rust
#[test]
fn eval_result_captures_metrics() {
    let result = EvalResult {
        test_case: "test".into(),
        agent_config: "baseline".into(),
        status: EvalStatus::Passed,
        metrics: EvalMetrics {
            total_tokens: 1500,
            cost_dollars: 0.012,
            latency_ms: 3200,
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(result.metrics.total_tokens, 1500);
}
```

**Test 6: Suite discovery**
```rust
#[test]
fn discover_suites_finds_toml_files() {
    let paths = discover_suites(Path::new("examples/")).unwrap();
    assert!(!paths.is_empty());
    assert!(paths.iter().all(|p| p.extension() == Some("toml".as_ref())));
}
```

---

## 10. Additional notes

- **Why TOML over YAML?** MOA uses TOML for all config. Consistency reduces cognitive load. TOML's explicit types (strings vs integers) prevent the "YAML Norway problem" (where `no` parses as boolean `false`).
- **Why not JSON Schema for expected outputs?** JSON Schema is over-specified for agent test cases. The `ExpectedOutput` struct covers the practical cases: contains/not-contains for string matching, regex for patterns, and facts for LLM-judge evaluation. Custom evaluators (via the `Evaluator` trait) handle everything else.
- **Future extension: fixtures.** Step 43 will need to handle `workspace_memory_path` by copying a memory directory snapshot into a temporary workspace. The `AgentConfig` type is designed for this — it points to fixture data, the runner handles setup/teardown.
- **Future extension: dataset items from production.** The `TestCase` type intentionally mirrors Langfuse's `DatasetItem` shape (`input` + `expected_output` + `metadata`). This makes it easy to export production traces into test cases later — but the conversion is a reporter concern, not a core type concern.
- **Trait object safety.** Both `Evaluator` and `Reporter` should be object-safe (`dyn Evaluator`, `dyn Reporter`) so the runner can hold a `Vec<Box<dyn Evaluator>>`.
