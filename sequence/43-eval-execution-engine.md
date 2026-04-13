# Step 43 — Eval Execution Engine

_Build the runner that constructs agents from configs, executes test cases, captures traces and trajectories, supports multi-config comparison._

---

## 1. What this step is about

This step builds the execution engine inside `moa-eval` — the code that takes a `TestSuite` + one or more `AgentConfig`s and actually runs them. For each (config, test case) pair, the engine:

1. Constructs an isolated agent environment from the `AgentConfig` (custom memory snapshot, skill overrides, model selection, tool restrictions, permission overrides)
2. Runs the agent against the test case input using the real brain harness
3. Captures the full execution result: final response, tool call trajectory, token/cost metrics, OTel trace ID
4. Packages the result as an `EvalResult` for evaluators and reporters (Step 44)

The engine supports running multiple configs against the same suite — the core of A/B testing and regression testing.

---

## 2. Files/directories to read

- **`moa-eval/src/types.rs`** — `TestSuite`, `TestCase`, `AgentConfig`, all override structs (from Step 42).
- **`moa-eval/src/results.rs`** — `EvalResult`, `EvalMetrics`, `TrajectoryStep` (from Step 42).
- **`moa-brain/src/harness.rs`** — `run_brain_turn()`. The engine calls this (or a wrapper) to execute agent turns. Understand inputs/outputs.
- **`moa-brain/src/pipeline/mod.rs`** — `ContextPipeline::new()`. The engine builds a custom pipeline per config.
- **`moa-orchestrator/src/local.rs`** — `LocalOrchestrator`. The engine may use a stripped-down orchestrator or call the harness directly.
- **`moa-memory/src/`** — `FileMemoryStore`. The engine creates temporary memory stores from config snapshots.
- **`moa-providers/src/factory.rs`** — Provider factory. The engine creates providers based on `AgentConfig.model`.
- **`moa-hands/src/router.rs`** — `ToolRouter`. The engine builds a custom router with restricted tools.
- **`moa-session/src/`** — `SessionStore`. The engine needs a temporary session store per run.
- **`moa-core/src/config.rs`** — `MoaConfig`. The engine loads base config and applies overrides.
- **`moa-core/src/telemetry.rs`** — Must be initialized before runs so OTel spans are captured.

---

## 3. Goal

A developer can programmatically run:

```rust
let suite = load_suite("tests/suites/deploy.toml")?;
let configs = vec![
    load_agent_config("tests/configs/baseline.toml")?,
    load_agent_config("tests/configs/new-skills.toml")?,
];

let engine = EvalEngine::new(base_config, engine_options)?;
let run = engine.run_suite(&suite, &configs).await?;

// run.results is a Vec<EvalResult> — one per (config, case) pair
// Each result has: response text, trajectory, metrics, trace_id
```

Or from a CLI (wired in Step 44): `moa eval run --suite tests/suites/deploy.toml --config tests/configs/baseline.toml --config tests/configs/new-skills.toml`

---

## 4. Rules

- **Isolation between runs.** Each (config, test case) execution uses its own temporary session, memory store, and workspace directory. No shared mutable state between runs.
- **Real brain harness.** The engine uses the actual `run_brain_turn()` loop — not a mock. The point is to test the real agent behavior.
- **Real LLM calls.** Eval runs make real API calls (they cost money). The engine should report estimated cost before starting and support `--dry-run` to show what would execute without calling LLMs.
- **Timeout enforcement.** Each test case has a `timeout_seconds`. The engine enforces it — if the agent doesn't complete within the timeout, the result is `EvalStatus::Timeout`.
- **Approval bypass.** When `AgentConfig.permissions.auto_approve_all = true`, the engine auto-approves all tool calls without human interaction.
- **Trace ID capture.** The engine must capture the OTel trace ID for each run so results can be linked to Langfuse/Tempo traces.
- **Concurrent execution optional.** Support `--parallel N` to run N test cases concurrently (default: sequential). Concurrent runs must not interfere.
- **Deterministic ordering.** Results are always ordered by (config_index, case_index) regardless of execution order.
- **No Langfuse dependency.** The engine captures trace IDs but does NOT call Langfuse APIs. That's a reporter concern (Step 44).

---

## 5. Tasks

### 5a. Create `EvalEngine` in `moa-eval/src/engine.rs`

```rust
pub struct EvalEngine {
    base_config: MoaConfig,
    options: EngineOptions,
}

pub struct EngineOptions {
    pub parallel: usize,            // Max concurrent test case runs (default: 1)
    pub temp_dir: PathBuf,          // Base directory for temporary workspaces
    pub dry_run: bool,              // Show plan without executing
    pub capture_content: bool,      // Include full I/O in results (default: true)
    pub content_max_bytes: usize,   // Truncate content capture (default: 32KB)
}

impl EvalEngine {
    pub fn new(base_config: MoaConfig, options: EngineOptions) -> Result<Self>;
    
    /// Run all test cases in a suite against all configs.
    /// Returns results in deterministic (config, case) order.
    pub async fn run_suite(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
    ) -> Result<EvalRun>;
    
    /// Run a single test case against a single config.
    pub async fn run_single(
        &self,
        case: &TestCase,
        config: &AgentConfig,
    ) -> Result<EvalResult>;
    
    /// Dry run: return plan without executing.
    pub fn plan(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
    ) -> EvalPlan;
}

/// A complete eval run — all results from a suite execution.
pub struct EvalRun {
    pub suite_name: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub results: Vec<EvalResult>,
    pub summary: RunSummary,
}

pub struct RunSummary {
    pub total_cases: usize,
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    pub timeouts: usize,
    pub total_tokens: usize,
    pub total_cost_dollars: f64,
    pub total_duration_ms: u64,
}

pub struct EvalPlan {
    pub suite_name: String,
    pub configs: Vec<String>,
    pub cases: Vec<String>,
    pub total_runs: usize,
    pub estimated_cost_range: (f64, f64),   // min/max based on model pricing
}
```

### 5b. Build agent environment from `AgentConfig` in `moa-eval/src/setup.rs`

This is the core setup logic — constructing an isolated agent from a config:

```rust
pub struct AgentEnvironment {
    pub session_store: Arc<dyn SessionStore>,
    pub memory_store: Arc<dyn MemoryStore>,
    pub llm_provider: Arc<dyn LLMProvider>,
    pub tool_router: Arc<ToolRouter>,
    pub pipeline: ContextPipeline,
    pub workspace_dir: PathBuf,
    pub session_id: SessionId,
}

/// Construct an isolated agent environment from an AgentConfig + base config.
pub async fn build_agent_environment(
    base_config: &MoaConfig,
    agent_config: &AgentConfig,
    temp_dir: &Path,
) -> Result<AgentEnvironment> {
    // 1. Create temp workspace directory
    let workspace_dir = temp_dir.join(format!("eval-{}", Uuid::now_v7()));
    fs::create_dir_all(&workspace_dir).await?;
    
    // 2. Set up memory
    let memory_store = setup_memory(&agent_config.memory, &workspace_dir).await?;
    
    // 3. Create session store (in-memory or temp SQLite)
    let session_store = create_temp_session_store(&workspace_dir).await?;
    
    // 4. Create LLM provider from config
    let model = agent_config.model.as_deref()
        .unwrap_or(&base_config.general.default_model);
    let llm_provider = create_provider_for_model(base_config, model)?;
    
    // 5. Build tool router with overrides
    let tool_router = build_tool_router(&agent_config.tools, &agent_config.permissions)?;
    
    // 6. Build pipeline with skill/instruction overrides
    let pipeline = build_pipeline(
        &agent_config.skills,
        &agent_config.instructions,
        &memory_store,
        &llm_provider,
    ).await?;
    
    // 7. Create session
    let session_id = session_store.create_session(SessionMeta { ... }).await?;
    
    Ok(AgentEnvironment { ... })
}
```

### 5c. Handle memory snapshot setup

When `AgentConfig.memory.workspace_memory_path` is set, copy the snapshot into the temp workspace:

```rust
async fn setup_memory(
    memory_config: &MemoryOverride,
    workspace_dir: &Path,
) -> Result<Arc<dyn MemoryStore>> {
    let memory_dir = workspace_dir.join("memory");
    
    if let Some(snapshot_path) = &memory_config.workspace_memory_path {
        // Copy the entire memory snapshot directory
        copy_dir_recursive(snapshot_path, &memory_dir).await?;
    } else if memory_config.clear_defaults {
        // Start with empty memory
        fs::create_dir_all(&memory_dir).await?;
    } else {
        // Use default memory location (from base config)
        // ... copy or symlink default memory
    }
    
    let store = FileMemoryStore::new(&memory_dir).await?;
    Ok(Arc::new(store))
}
```

### 5d. Handle skill overrides

When `AgentConfig.skills.exclusive = true`, only load the listed skills. Otherwise, merge with defaults:

```rust
fn apply_skill_overrides(
    default_skills: Vec<SkillMetadata>,
    overrides: &SkillOverride,
) -> Vec<SkillMetadata> {
    if overrides.exclusive {
        // Only include explicitly listed skills
        default_skills.into_iter()
            .filter(|s| overrides.include.iter().any(|inc| s.path.contains(inc)))
            .collect()
    } else {
        // Start with defaults, add includes, remove excludes
        let mut skills = default_skills;
        // Add any new skills from include paths
        // Remove any skills matching exclude patterns
        skills.retain(|s| !overrides.exclude.iter().any(|exc| s.path.contains(exc)));
        skills
    }
}
```

### 5e. Handle auto-approval in tool execution

When `auto_approve_all = true`, the engine must bypass the normal approval flow. Implement this by injecting a universal auto-approve policy:

```rust
fn build_approval_policy(permissions: &PermissionOverride) -> ToolPolicies {
    if permissions.auto_approve_all {
        ToolPolicies::allow_all()
    } else {
        let mut rules = Vec::new();
        for tool in &permissions.auto_approve {
            rules.push(ToolRule { tool: tool.clone(), action: PolicyAction::Allow, .. });
        }
        for tool in &permissions.always_deny {
            rules.push(ToolRule { tool: tool.clone(), action: PolicyAction::Deny, .. });
        }
        ToolPolicies { rules }
    }
}
```

### 5f. Implement `run_single()` — the core execution loop

```rust
async fn run_single(&self, case: &TestCase, config: &AgentConfig) -> Result<EvalResult> {
    let started_at = Utc::now();
    let timeout = Duration::from_secs(
        case.timeout_seconds.unwrap_or(self.options.default_timeout)
    );
    
    // Build isolated environment
    let env = build_agent_environment(&self.base_config, config, &self.options.temp_dir).await?;
    
    // Submit the test case input as a user message
    env.session_store.emit_event(env.session_id, Event::UserMessage {
        text: case.input.clone(),
        attachments: vec![],
    }).await?;
    
    // Create a root span for this eval run (captures trace_id)
    let span = tracing::info_span!(
        "eval_run",
        moa.eval.suite = tracing::field::Empty,
        moa.eval.case = %case.name,
        moa.eval.config = %config.name,
        langfuse.session.id = %env.session_id,
    );
    let trace_id = extract_trace_id(&span);
    
    // Run brain turns until completion or timeout
    let run_result = tokio::time::timeout(timeout, async {
        run_agent_until_complete(&env, &span).await
    }).await;
    
    let completed_at = Utc::now();
    
    // Build result
    match run_result {
        Ok(Ok(agent_output)) => {
            EvalResult {
                test_case: case.name.clone(),
                agent_config: config.name.clone(),
                status: EvalStatus::Passed, // evaluators may downgrade this
                response: Some(agent_output.response),
                trajectory: agent_output.trajectory,
                metrics: agent_output.metrics,
                trace_id: Some(trace_id),
                error: None,
                started_at,
                completed_at,
            }
        }
        Ok(Err(e)) => EvalResult { status: EvalStatus::Error, error: Some(e.to_string()), .. },
        Err(_) => EvalResult { status: EvalStatus::Timeout, .. },
    }
    // Cleanup temp workspace
}
```

### 5g. Capture trajectory during execution

Hook into the event stream to capture tool calls as they happen:

```rust
struct TrajectoryCollector {
    steps: Vec<TrajectoryStep>,
    metrics: EvalMetrics,
}

impl TrajectoryCollector {
    fn process_event(&mut self, event: &Event) {
        match event {
            Event::ToolCall { tool_name, input, .. } => {
                self.steps.push(TrajectoryStep {
                    tool_name: tool_name.clone(),
                    input_summary: truncate(input, 1024),
                    ..Default::default()
                });
            }
            Event::ToolResult { output, success, duration_ms, .. } => {
                if let Some(last) = self.steps.last_mut() {
                    last.output_summary = truncate(output, 1024);
                    last.success = *success;
                    last.duration_ms = *duration_ms;
                }
            }
            Event::BrainResponse { input_tokens, output_tokens, cost_cents, duration_ms, .. } => {
                self.metrics.input_tokens += input_tokens;
                self.metrics.output_tokens += output_tokens;
                self.metrics.total_tokens += input_tokens + output_tokens;
                self.metrics.cost_dollars += *cost_cents as f64 / 100.0;
                self.metrics.latency_ms += duration_ms;
                self.metrics.turn_count += 1;
            }
            _ => {}
        }
    }
}
```

### 5h. Implement `run_suite()` with optional parallelism

```rust
async fn run_suite(&self, suite: &TestSuite, configs: &[AgentConfig]) -> Result<EvalRun> {
    let started_at = Utc::now();
    let mut results = Vec::new();
    
    // Build all (config, case) pairs
    let pairs: Vec<_> = configs.iter()
        .flat_map(|c| suite.cases.iter().map(move |t| (c, t)))
        .collect();
    
    if self.options.parallel <= 1 {
        // Sequential
        for (config, case) in &pairs {
            let result = self.run_single(case, config).await?;
            results.push(result);
        }
    } else {
        // Parallel with bounded concurrency
        let semaphore = Arc::new(Semaphore::new(self.options.parallel));
        let mut handles = Vec::new();
        
        for (config, case) in pairs {
            let permit = semaphore.clone().acquire_owned().await?;
            let engine = self.clone(); // engine must be Clone
            let case = case.clone();
            let config = config.clone();
            
            handles.push(tokio::spawn(async move {
                let result = engine.run_single(&case, &config).await;
                drop(permit);
                result
            }));
        }
        
        for handle in handles {
            results.push(handle.await??);
        }
    }
    
    // Sort results by (config_index, case_index) for determinism
    // ... sorting logic ...
    
    let summary = RunSummary::from_results(&results);
    
    Ok(EvalRun {
        suite_name: suite.name.clone(),
        started_at,
        completed_at: Utc::now(),
        results,
        summary,
    })
}
```

### 5i. Extract OTel trace ID from span

```rust
use opentelemetry::trace::TraceContextExt;
use tracing_opentelemetry::OpenTelemetrySpanExt;

fn extract_trace_id(span: &tracing::Span) -> String {
    let context = span.context();
    let otel_context = context.span();
    otel_context.span_context().trace_id().to_string()
}
```

### 5j. Implement cleanup

After each run, clean up the temporary workspace:

```rust
async fn cleanup_environment(env: &AgentEnvironment) -> Result<()> {
    // Remove temp workspace directory
    if env.workspace_dir.exists() {
        fs::remove_dir_all(&env.workspace_dir).await.ok();
    }
    Ok(())
}
```

---

## 6. How it should be implemented

The file structure for this step adds to `moa-eval/src/`:

```
moa-eval/src/
├── engine.rs       # EvalEngine, EngineOptions, run_suite(), run_single()
├── setup.rs        # build_agent_environment(), memory/skill/tool setup
├── collector.rs    # TrajectoryCollector, event processing
└── plan.rs         # EvalPlan, dry_run(), cost estimation
```

The engine should use the real `run_brain_turn()` from `moa-brain`, NOT re-implement the turn logic. The eval engine is an orchestrator that:
1. Sets up the environment
2. Submits input
3. Calls the brain harness
4. Collects results

The brain harness doesn't know it's running in an eval context — it behaves identically to production.

For approval bypass, the cleanest approach is passing a policy that auto-approves everything. The brain harness already checks policies before requesting approval — if the policy says "allow", no approval is requested.

---

## 7. Deliverables

- [ ] `moa-eval/src/engine.rs` — `EvalEngine`, `EngineOptions`, `EvalRun`, `RunSummary`, `EvalPlan`, `run_suite()`, `run_single()`, `plan()`
- [ ] `moa-eval/src/setup.rs` — `AgentEnvironment`, `build_agent_environment()`, memory/skill/tool/pipeline setup, approval policy injection
- [ ] `moa-eval/src/collector.rs` — `TrajectoryCollector`, event processing, metrics aggregation
- [ ] `moa-eval/src/plan.rs` — Dry run planning, cost estimation
- [ ] `moa-eval/Cargo.toml` — Add dependencies on `moa-brain`, `moa-memory`, `moa-session`, `moa-hands`, `moa-providers`, `moa-skills`, `moa-security`, `moa-orchestrator`
- [ ] `moa-eval/src/lib.rs` — Updated module declarations

---

## 8. Acceptance criteria

1. **Single run works.** `engine.run_single(case, config).await` returns an `EvalResult` with populated response, trajectory, metrics, and trace_id.
2. **Multi-config comparison.** Running 2 configs × 3 cases produces 6 `EvalResult`s in deterministic order.
3. **Isolation.** Running config A does not affect config B's memory, session, or tool state.
4. **Memory snapshots.** A config pointing to `tests/fixtures/memory/v1/` uses that memory, not the default.
5. **Skill overrides.** A config with `skills.exclusive = true, include = ["deploy-to-fly"]` only has that one skill available.
6. **Tool restrictions.** A config with `tools.enabled = ["bash", "file_read"]` cannot use `web_search`.
7. **Auto-approval.** A config with `auto_approve_all = true` never blocks on approval.
8. **Timeout enforcement.** A test case with `timeout_seconds = 5` that doesn't complete in 5s returns `EvalStatus::Timeout`.
9. **Trace ID captured.** Every result has a non-empty `trace_id` that corresponds to a real OTel trace.
10. **Dry run.** `engine.plan()` returns the run plan without making any LLM calls.
11. **Cleanup.** Temporary workspace directories are removed after runs.
12. **Parallel execution.** `parallel = 2` runs two cases concurrently without interference.

---

## 9. Testing

### Unit tests

**Test 1: AgentEnvironment setup with memory snapshot**
```rust
#[tokio::test]
async fn setup_copies_memory_snapshot() {
    let temp = tempdir().unwrap();
    let fixture_memory = create_test_memory_fixture().await;
    
    let config = AgentConfig {
        name: "test".into(),
        memory: MemoryOverride {
            workspace_memory_path: Some(fixture_memory.path().into()),
            ..Default::default()
        },
        ..Default::default()
    };
    
    let env = build_agent_environment(&base_config(), &config, temp.path()).await.unwrap();
    
    // Verify memory was copied
    let index = env.memory_store.get_index(MemoryScope::Workspace(..)).await.unwrap();
    assert!(!index.is_empty());
}
```

**Test 2: Tool restriction enforcement**
```rust
#[tokio::test]
async fn tool_override_restricts_available_tools() {
    let config = AgentConfig {
        tools: ToolOverride {
            enabled: Some(vec!["file_read".into()]),
            ..Default::default()
        },
        ..Default::default()
    };
    
    let env = build_agent_environment(&base_config(), &config, temp.path()).await.unwrap();
    
    // file_read should work
    assert!(env.tool_router.has_tool("file_read"));
    // bash should NOT be available
    assert!(!env.tool_router.has_tool("bash"));
}
```

**Test 3: Auto-approval bypass**
```rust
#[tokio::test]
async fn auto_approve_all_skips_approval() {
    let config = AgentConfig {
        permissions: PermissionOverride {
            auto_approve_all: true,
            ..Default::default()
        },
        ..Default::default()
    };
    
    let env = build_agent_environment(&base_config(), &config, temp.path()).await.unwrap();
    let policy = env.tool_router.check_policy("bash", "rm -rf /tmp/test");
    assert_eq!(policy, PolicyAction::Allow);
}
```

**Test 4: Trajectory collection from events**
```rust
#[test]
fn trajectory_collector_captures_tool_calls() {
    let mut collector = TrajectoryCollector::new();
    
    collector.process_event(&Event::ToolCall {
        tool_id: Uuid::now_v7(),
        tool_name: "bash".into(),
        input: json!({"command": "ls"}),
        ..
    });
    collector.process_event(&Event::ToolResult {
        tool_id: Uuid::now_v7(),
        output: "file1.txt\nfile2.txt".into(),
        success: true,
        duration_ms: 50,
    });
    
    assert_eq!(collector.steps.len(), 1);
    assert_eq!(collector.steps[0].tool_name, "bash");
    assert!(collector.steps[0].success);
}
```

**Test 5: Timeout enforcement**
```rust
#[tokio::test]
async fn timeout_produces_timeout_status() {
    let case = TestCase {
        name: "slow-test".into(),
        input: "Do something that takes forever".into(),
        timeout_seconds: Some(1), // 1 second timeout
        ..Default::default()
    };
    
    // Use a mock LLM that sleeps for 10 seconds
    let config = AgentConfig { /* with slow mock provider */ .. };
    let result = engine.run_single(&case, &config).await.unwrap();
    assert_eq!(result.status, EvalStatus::Timeout);
}
```

**Test 6: Dry run plan**
```rust
#[test]
fn plan_shows_correct_run_count() {
    let suite = TestSuite { cases: vec![case1, case2, case3], .. };
    let configs = vec![config_a, config_b];
    
    let plan = engine.plan(&suite, &configs);
    assert_eq!(plan.total_runs, 6); // 2 configs × 3 cases
    assert_eq!(plan.configs.len(), 2);
    assert_eq!(plan.cases.len(), 3);
}
```

### Integration test (requires API key)

**Test 7: End-to-end single run**
```rust
#[tokio::test]
#[ignore] // requires ANTHROPIC_API_KEY
async fn e2e_single_run_produces_result() {
    let suite = load_suite("examples/example-suite.toml").unwrap();
    let config = load_agent_config("examples/example-config-baseline.toml").unwrap();
    let engine = EvalEngine::new(MoaConfig::load().unwrap(), EngineOptions::default()).unwrap();
    
    let result = engine.run_single(&suite.cases[0], &config).await.unwrap();
    
    assert!(matches!(result.status, EvalStatus::Passed | EvalStatus::Failed));
    assert!(result.response.is_some());
    assert!(result.metrics.total_tokens > 0);
    assert!(result.trace_id.is_some());
}
```

---

## 10. Additional notes

- **Cost awareness.** Agent eval runs are expensive — each run makes real LLM calls. The `plan()` method should estimate costs based on typical token counts per model. Consider adding a `--budget` flag that aborts if estimated cost exceeds a threshold.
- **Temp session store.** Use `TursoSessionStore` with a temp SQLite file (`/tmp/moa-eval-{uuid}/sessions.db`), not in-memory, so that session events survive crashes and can be inspected post-mortem.
- **Event stream subscription.** The `TrajectoryCollector` needs to subscribe to events as they're emitted. Two approaches: (a) subscribe to the broadcast channel from `LocalOrchestrator`, or (b) read events from the session store after the run completes. Approach (b) is simpler and sufficient since eval runs are not real-time.
- **Provider mock for testing.** Create a `MockLLMProvider` that returns canned responses for testing the engine without making real API calls. This is essential for unit testing setup/teardown logic.
- **Future: snapshot versioning.** Memory snapshots in `tests/fixtures/memory/` should be versioned alongside the codebase (committed to git). This makes eval runs reproducible — the same commit always uses the same memory state.
