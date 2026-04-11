# Step 44 — CLI Subcommand, Built-in Evaluators, and Reporters

_Add `moa eval` CLI, implement trajectory/output/cost evaluators, terminal + JSON reporters, optional Langfuse score export._

---

## 1. What this step is about

This step completes the eval system by adding:

1. **`moa eval` CLI subcommand** — run suites, compare configs, view results from the terminal
2. **Built-in evaluators** — trajectory match, output containment, cost/latency thresholds, regex matching
3. **Reporters** — terminal table output, JSON file export, and an optional Langfuse score reporter behind a feature flag
4. **CI/CD exit codes** — `moa eval run` returns non-zero exit codes when quality gates fail, enabling integration into CI pipelines

After this step, the full eval workflow is operational:

```bash
# Compare two agent configs
moa eval run \
  --suite tests/suites/deploy.toml \
  --config tests/configs/baseline.toml \
  --config tests/configs/new-skills.toml \
  --report terminal \
  --report json:results/run-001.json

# CI mode: exit 1 if any case fails
moa eval run --suite tests/suites/regression.toml --ci
```

---

## 2. Files/directories to read

- **`moa-eval/src/evaluator.rs`** — `Evaluator` trait (from Step 42).
- **`moa-eval/src/reporter.rs`** — `Reporter` trait (from Step 42).
- **`moa-eval/src/results.rs`** — `EvalResult`, `EvalScore`, `ScoreValue`.
- **`moa-eval/src/engine.rs`** — `EvalEngine`, `EvalRun`, `RunSummary` (from Step 43).
- **`moa-cli/src/main.rs`** — CLI entry point. Add `Eval` variant to `CommandKind`.
- **`moa-cli/Cargo.toml`** — Add `moa-eval` dependency.

---

## 3. Goal

Three concrete workflows work end-to-end:

**A/B comparison:**
```bash
$ moa eval run --suite suites/deploy.toml --config configs/v1.toml --config configs/v2.toml

╔══════════════════════════════════════════════════════════════════╗
║  Suite: deploy-skill-comparison  │  2 configs × 3 cases = 6 runs ║
╠══════════════════════════════════════════════════════════════════╣
║                    │  v1-baseline  │  v2-new-skills              ║
║ ─────────────────────────────────────────────────────────────── ║
║ basic-staging      │  ✅ PASS       │  ✅ PASS                   ║
║   trajectory       │  1.0           │  1.0                       ║
║   output_match     │  1.0           │  1.0                       ║
║   cost ($)         │  0.012         │  0.008  ↓ 33%             ║
║   latency (ms)     │  3200          │  2100  ↓ 34%              ║
║ ─────────────────────────────────────────────────────────────── ║
║ prod-deploy        │  ❌ FAIL       │  ✅ PASS                   ║
║   trajectory       │  0.5           │  1.0                       ║
║   output_match     │  0.8           │  1.0                       ║
╚══════════════════════════════════════════════════════════════════╝
  Total: 5/6 passed  │  Tokens: 12.3k  │  Cost: $0.045  │  Time: 18.2s
```

**Regression test in CI:**
```bash
$ moa eval run --suite suites/regression.toml --ci
# Exit code 0 = all pass, 1 = failures, 2 = errors
```

**Export to JSON for custom analysis:**
```bash
$ moa eval run --suite suites/deploy.toml --report json:results/latest.json
# results/latest.json contains the full EvalRun with all results, scores, metrics
```

---

## 4. Rules

- **Evaluators run after all test cases complete.** The engine runs all cases, then all evaluators score all results. This decouples execution from evaluation.
- **Multiple evaluators compose.** You can run trajectory + output + cost evaluators on the same result. Each evaluator adds its own scores.
- **Pass/fail is determined by evaluators.** The engine produces `EvalStatus::Passed` as a default. Evaluators can downgrade to `Failed` if scores don't meet thresholds.
- **Reporters are additive.** `--report terminal --report json:out.json` runs both reporters.
- **CI mode is strict.** Any `Failed`, `Error`, or `Timeout` status means exit code 1.
- **Langfuse reporter is feature-gated.** `moa-eval` has a `langfuse` feature flag. When enabled, the `LangfuseReporter` posts scores to Langfuse's REST API. When disabled, the reporter is not compiled.
- **No new external dependencies** except `reqwest` (already in workspace) for the Langfuse reporter.

---

## 5. Tasks

### 5a. Implement built-in evaluators

Create `moa-eval/src/evaluators/` with these evaluators:

**`trajectory_match.rs` — Trajectory evaluator**

Compares actual tool call sequence against expected:

```rust
pub struct TrajectoryMatchEvaluator;

impl Evaluator for TrajectoryMatchEvaluator {
    fn name(&self) -> &str { "trajectory_match" }
    
    async fn evaluate(&self, case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        let Some(expected) = &case.expected_trajectory else {
            return Ok(vec![]); // No trajectory expectation, skip
        };
        
        let actual: Vec<&str> = result.trajectory.iter()
            .map(|s| s.tool_name.as_str())
            .collect();
        
        // Calculate similarity: Longest Common Subsequence / max(len_expected, len_actual)
        let lcs_len = lcs(expected, &actual);
        let max_len = expected.len().max(actual.len());
        let score = if max_len == 0 { 1.0 } else { lcs_len as f64 / max_len as f64 };
        
        let comment = if score < 1.0 {
            format!(
                "Expected: [{}], Actual: [{}]",
                expected.join(", "),
                actual.join(", ")
            )
        } else {
            "Exact match".into()
        };
        
        Ok(vec![EvalScore {
            evaluator: self.name().into(),
            name: "trajectory_match".into(),
            value: ScoreValue::Numeric(score),
            comment: Some(comment),
        }])
    }
}
```

**`output_match.rs` — Output containment evaluator**

Checks that the response contains/excludes expected strings:

```rust
pub struct OutputMatchEvaluator;

impl Evaluator for OutputMatchEvaluator {
    fn name(&self) -> &str { "output_match" }
    
    async fn evaluate(&self, case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        let Some(expected) = &case.expected_output else {
            return Ok(vec![]);
        };
        let response = result.response.as_deref().unwrap_or("");
        let response_lower = response.to_lowercase();
        
        let mut matched = 0;
        let mut total = 0;
        let mut failures = Vec::new();
        
        // Check contains
        for phrase in &expected.contains {
            total += 1;
            if response_lower.contains(&phrase.to_lowercase()) {
                matched += 1;
            } else {
                failures.push(format!("missing: '{}'", phrase));
            }
        }
        
        // Check not_contains
        for phrase in &expected.not_contains {
            total += 1;
            if !response_lower.contains(&phrase.to_lowercase()) {
                matched += 1;
            } else {
                failures.push(format!("should not contain: '{}'", phrase));
            }
        }
        
        // Check regex
        if let Some(pattern) = &expected.regex {
            total += 1;
            if Regex::new(pattern)?.is_match(response) {
                matched += 1;
            } else {
                failures.push(format!("regex mismatch: '{}'", pattern));
            }
        }
        
        let score = if total == 0 { 1.0 } else { matched as f64 / total as f64 };
        
        Ok(vec![EvalScore {
            evaluator: self.name().into(),
            name: "output_match".into(),
            value: ScoreValue::Numeric(score),
            comment: if failures.is_empty() { None } else { Some(failures.join("; ")) },
        }])
    }
}
```

**`threshold.rs` — Cost/latency threshold evaluator**

Fails if metrics exceed configured thresholds:

```rust
pub struct ThresholdEvaluator {
    pub max_cost_dollars: Option<f64>,
    pub max_latency_ms: Option<u64>,
    pub max_tokens: Option<usize>,
    pub max_tool_calls: Option<usize>,
    pub max_turns: Option<usize>,
}

impl Evaluator for ThresholdEvaluator {
    fn name(&self) -> &str { "threshold" }
    
    async fn evaluate(&self, _case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        let mut scores = Vec::new();
        
        if let Some(max_cost) = self.max_cost_dollars {
            scores.push(EvalScore {
                name: "cost_within_budget".into(),
                value: ScoreValue::Boolean(result.metrics.cost_dollars <= max_cost),
                comment: Some(format!("${:.4} / ${:.4} max", result.metrics.cost_dollars, max_cost)),
                ..
            });
        }
        
        if let Some(max_latency) = self.max_latency_ms {
            scores.push(EvalScore {
                name: "latency_within_threshold".into(),
                value: ScoreValue::Boolean(result.metrics.latency_ms <= max_latency),
                comment: Some(format!("{}ms / {}ms max", result.metrics.latency_ms, max_latency)),
                ..
            });
        }
        
        // ... similar for max_tokens, max_tool_calls, max_turns
        
        Ok(scores)
    }
}
```

**`tool_success.rs` — Tool success rate evaluator**

Scores based on how many tool calls succeeded:

```rust
pub struct ToolSuccessEvaluator;

impl Evaluator for ToolSuccessEvaluator {
    fn name(&self) -> &str { "tool_success" }
    
    async fn evaluate(&self, _case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        if result.trajectory.is_empty() {
            return Ok(vec![]);
        }
        let success_count = result.trajectory.iter().filter(|s| s.success).count();
        let total = result.trajectory.len();
        let rate = success_count as f64 / total as f64;
        
        Ok(vec![EvalScore {
            name: "tool_success_rate".into(),
            value: ScoreValue::Numeric(rate),
            comment: Some(format!("{}/{} succeeded", success_count, total)),
            ..
        }])
    }
}
```

### 5b. Implement reporters

**`terminal.rs` — Terminal table reporter**

Uses formatted terminal output (no external table crate needed — plain format with padding):

```rust
pub struct TerminalReporter {
    pub verbose: bool,
    pub color: bool,
}

impl Reporter for TerminalReporter {
    async fn report(&self, suite: &TestSuite, configs: &[AgentConfig], results: &[EvalResult]) -> Result<()> {
        // Header
        println!("Suite: {} │ {} configs × {} cases = {} runs",
            suite.name, configs.len(), suite.cases.len(), results.len());
        
        // Comparison table: configs as columns, cases as rows
        // For each cell: status icon + key scores
        
        // Summary row: total pass/fail, aggregate cost, tokens, time
        
        // If verbose: per-case detail with trajectory diff and score comments
        
        Ok(())
    }
}
```

**`json.rs` — JSON file reporter**

Serializes the full `EvalRun` to a JSON file:

```rust
pub struct JsonReporter {
    pub output_path: PathBuf,
    pub pretty: bool,
}

impl Reporter for JsonReporter {
    async fn report(&self, _suite: &TestSuite, _configs: &[AgentConfig], results: &[EvalResult]) -> Result<()> {
        let json = if self.pretty {
            serde_json::to_string_pretty(results)?
        } else {
            serde_json::to_string(results)?
        };
        fs::write(&self.output_path, json).await?;
        println!("Results written to {}", self.output_path.display());
        Ok(())
    }
}
```

**`langfuse.rs` — Langfuse score reporter (feature-gated)**

Posts scores to Langfuse's REST API so they appear alongside traces:

```rust
#[cfg(feature = "langfuse")]
pub struct LangfuseReporter {
    pub base_url: String,      // e.g., "http://langfuse:3000"
    pub public_key: String,
    pub secret_key: String,
}

#[cfg(feature = "langfuse")]
impl Reporter for LangfuseReporter {
    async fn report(&self, _suite: &TestSuite, _configs: &[AgentConfig], results: &[EvalResult]) -> Result<()> {
        let client = reqwest::Client::new();
        let auth = base64::encode(format!("{}:{}", self.public_key, self.secret_key));
        
        for result in results {
            let Some(trace_id) = &result.trace_id else { continue };
            
            for score in &result.scores {
                let body = json!({
                    "traceId": trace_id,
                    "name": score.name,
                    "value": match &score.value {
                        ScoreValue::Numeric(v) => json!(v),
                        ScoreValue::Boolean(v) => json!(if *v { 1.0 } else { 0.0 }),
                        ScoreValue::Categorical(v) => json!(v),
                    },
                    "dataType": match &score.value {
                        ScoreValue::Numeric(_) => "NUMERIC",
                        ScoreValue::Boolean(_) => "BOOLEAN",
                        ScoreValue::Categorical(_) => "CATEGORICAL",
                    },
                    "source": "API",
                    "comment": score.comment,
                });
                
                client.post(format!("{}/api/public/scores", self.base_url))
                    .header("Authorization", format!("Basic {}", auth))
                    .json(&body)
                    .send().await?;
            }
        }
        
        Ok(())
    }
}
```

### 5c. Wire the `moa eval` CLI subcommand

In `moa-cli/src/main.rs`, add:

```rust
/// Agent evaluation and testing.
Eval {
    #[command(subcommand)]
    command: EvalCommand,
},
```

```rust
#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Run a test suite against one or more agent configs.
    Run(EvalRunArgs),
    /// Show the plan without executing (dry run).
    Plan(EvalPlanArgs),
    /// List available test suites.
    List {
        /// Directory to scan for suites.
        #[arg(default_value = "tests/suites")]
        dir: PathBuf,
    },
}

#[derive(Debug, Args)]
struct EvalRunArgs {
    /// Path to the test suite TOML file.
    #[arg(long)]
    suite: PathBuf,
    
    /// Paths to agent config TOML files (repeat for comparison).
    #[arg(long, required = true)]
    config: Vec<PathBuf>,
    
    /// Report output: "terminal", "json:<path>", "langfuse".
    #[arg(long, default_value = "terminal")]
    report: Vec<String>,
    
    /// Max concurrent test case runs.
    #[arg(long, default_value = "1")]
    parallel: usize,
    
    /// CI mode: exit 1 on any failure, minimal output.
    #[arg(long)]
    ci: bool,
    
    /// Evaluators to run: "trajectory", "output", "threshold", "tool_success".
    #[arg(long, default_values_t = vec![
        "trajectory".into(), "output".into(), "tool_success".into()
    ])]
    evaluator: Vec<String>,
    
    /// Maximum cost per test case in dollars (threshold evaluator).
    #[arg(long)]
    max_cost: Option<f64>,
    
    /// Maximum latency per test case in ms (threshold evaluator).
    #[arg(long)]
    max_latency: Option<u64>,
    
    /// Verbose output with per-case details.
    #[arg(long, short)]
    verbose: bool,
}
```

### 5d. Implement the `moa eval run` handler

```rust
async fn handle_eval_run(args: EvalRunArgs, config: MoaConfig) -> Result<()> {
    // 1. Load suite and configs
    let suite = load_suite(&args.suite)?;
    let configs: Vec<AgentConfig> = args.config.iter()
        .map(|p| load_agent_config(p))
        .collect::<Result<_>>()?;
    
    // 2. Build evaluators
    let evaluators = build_evaluators(&args)?;
    
    // 3. Build reporters
    let reporters = build_reporters(&args)?;
    
    // 4. Create engine
    let engine = EvalEngine::new(config, EngineOptions {
        parallel: args.parallel,
        ..Default::default()
    })?;
    
    // 5. Run suite
    let mut run = engine.run_suite(&suite, &configs).await?;
    
    // 6. Score results with evaluators
    for result in &mut run.results {
        for evaluator in &evaluators {
            let case = suite.cases.iter().find(|c| c.name == result.test_case).unwrap();
            let scores = evaluator.evaluate(case, result).await?;
            
            // Downgrade status if any score fails
            for score in &scores {
                if score_is_failure(score) && result.status == EvalStatus::Passed {
                    result.status = EvalStatus::Failed;
                }
            }
            
            result.scores.extend(scores);
        }
    }
    
    // Recompute summary
    run.summary = RunSummary::from_results(&run.results);
    
    // 7. Run reporters
    for reporter in &reporters {
        reporter.report(&suite, &configs, &run.results).await?;
    }
    
    // 8. Exit code
    if args.ci {
        let failures = run.results.iter()
            .filter(|r| !matches!(r.status, EvalStatus::Passed | EvalStatus::Skipped))
            .count();
        if failures > 0 {
            std::process::exit(1);
        }
    }
    
    Ok(())
}

fn score_is_failure(score: &EvalScore) -> bool {
    match &score.value {
        ScoreValue::Numeric(v) => *v < 0.5,   // Below 50% is a failure
        ScoreValue::Boolean(v) => !v,
        ScoreValue::Categorical(_) => false,    // Categorical scores don't fail
    }
}
```

### 5e. Implement `moa eval plan`

```rust
async fn handle_eval_plan(args: EvalPlanArgs, config: MoaConfig) -> Result<()> {
    let suite = load_suite(&args.suite)?;
    let configs: Vec<AgentConfig> = args.config.iter()
        .map(|p| load_agent_config(p))
        .collect::<Result<_>>()?;
    
    let engine = EvalEngine::new(config, EngineOptions::default())?;
    let plan = engine.plan(&suite, &configs);
    
    println!("Suite: {}", plan.suite_name);
    println!("Configs: {}", plan.configs.join(", "));
    println!("Cases: {}", plan.cases.join(", "));
    println!("Total runs: {}", plan.total_runs);
    println!("Estimated cost: ${:.3} – ${:.3}", plan.estimated_cost_range.0, plan.estimated_cost_range.1);
    
    Ok(())
}
```

### 5f. Implement `moa eval list`

```rust
async fn handle_eval_list(dir: PathBuf) -> Result<()> {
    let suites = discover_suites(&dir)?;
    for path in suites {
        let suite = load_suite(&path)?;
        println!("{:30} │ {} cases │ {}",
            suite.name,
            suite.cases.len(),
            suite.description.as_deref().unwrap_or("")
        );
    }
    Ok(())
}
```

---

## 6. How it should be implemented

File structure additions:

```
moa-eval/src/
├── evaluators/
│   ├── mod.rs               # Evaluator registry, build_evaluators()
│   ├── trajectory_match.rs
│   ├── output_match.rs
│   ├── threshold.rs
│   └── tool_success.rs
├── reporters/
│   ├── mod.rs               # Reporter registry, build_reporters()
│   ├── terminal.rs
│   ├── json.rs
│   └── langfuse.rs          # #[cfg(feature = "langfuse")]
└── ...existing...

moa-eval/Cargo.toml:
  [features]
  default = []
  langfuse = ["reqwest"]
```

The evaluators should be stateless where possible. The `ThresholdEvaluator` is the exception — it needs config-provided thresholds. Use the builder pattern:

```rust
let evaluators: Vec<Box<dyn Evaluator>> = vec![
    Box::new(TrajectoryMatchEvaluator),
    Box::new(OutputMatchEvaluator),
    Box::new(ToolSuccessEvaluator),
    Box::new(ThresholdEvaluator {
        max_cost_dollars: args.max_cost,
        max_latency_ms: args.max_latency,
        ..Default::default()
    }),
];
```

---

## 7. Deliverables

- [ ] `moa-eval/src/evaluators/mod.rs` — Module declaration, `build_evaluators()` factory
- [ ] `moa-eval/src/evaluators/trajectory_match.rs` — LCS-based trajectory comparison
- [ ] `moa-eval/src/evaluators/output_match.rs` — Contains/not-contains/regex output validation
- [ ] `moa-eval/src/evaluators/threshold.rs` — Cost/latency/token threshold enforcement
- [ ] `moa-eval/src/evaluators/tool_success.rs` — Tool success rate scoring
- [ ] `moa-eval/src/reporters/mod.rs` — Module declaration, `build_reporters()` factory
- [ ] `moa-eval/src/reporters/terminal.rs` — Terminal comparison table output
- [ ] `moa-eval/src/reporters/json.rs` — JSON file export
- [ ] `moa-eval/src/reporters/langfuse.rs` — Langfuse score API reporter (feature-gated)
- [ ] `moa-eval/Cargo.toml` — Add `langfuse` feature flag, `regex` dep, `reqwest` dep (optional)
- [ ] `moa-cli/src/main.rs` — `Eval` subcommand with `Run`, `Plan`, `List`
- [ ] `moa-cli/Cargo.toml` — Add `moa-eval` dependency

---

## 8. Acceptance criteria

1. **`moa eval run` works.** Running with a suite + config produces terminal output showing pass/fail status and scores.
2. **Multi-config comparison.** Two configs produce a side-by-side comparison table in terminal output.
3. **Trajectory evaluator catches mismatches.** A test case expecting `["bash", "bash"]` but getting `["bash", "file_write", "bash"]` scores < 1.0 and the comment shows the diff.
4. **Output evaluator validates contains.** A test case expecting `contains = ["deployed"]` passes when response includes "deployed" and fails when it doesn't.
5. **Threshold evaluator enforces limits.** `--max-cost 0.001` fails a case that costs $0.005.
6. **JSON reporter writes valid JSON.** Output file parses correctly and contains all results with scores.
7. **CI mode exits non-zero on failure.** `moa eval run --ci` returns exit code 1 when any case fails.
8. **Langfuse reporter posts scores.** (When `langfuse` feature enabled) Scores appear in Langfuse UI attached to the correct traces.
9. **`moa eval plan` shows dry run.** Displays run count and estimated cost without making LLM calls.
10. **`moa eval list` discovers suites.** Lists all .toml files in the specified directory.
11. **Evaluators compose.** Running trajectory + output + threshold together produces scores from all three on each result.

---

## 9. Testing

### Unit tests

**Test 1: Trajectory match — exact match**
```rust
#[tokio::test]
async fn trajectory_exact_match_scores_1() {
    let evaluator = TrajectoryMatchEvaluator;
    let case = TestCase {
        expected_trajectory: Some(vec!["bash".into(), "file_read".into()]),
        ..Default::default()
    };
    let result = EvalResult {
        trajectory: vec![
            TrajectoryStep { tool_name: "bash".into(), .. },
            TrajectoryStep { tool_name: "file_read".into(), .. },
        ],
        ..Default::default()
    };
    let scores = evaluator.evaluate(&case, &result).await.unwrap();
    assert_eq!(scores[0].value, ScoreValue::Numeric(1.0));
}
```

**Test 2: Trajectory match — partial match**
```rust
#[tokio::test]
async fn trajectory_partial_match_scores_less_than_1() {
    let case = TestCase {
        expected_trajectory: Some(vec!["bash".into(), "file_read".into()]),
        ..
    };
    let result = EvalResult {
        trajectory: vec![
            TrajectoryStep { tool_name: "bash".into(), .. },
            TrajectoryStep { tool_name: "web_search".into(), .. },
            TrajectoryStep { tool_name: "file_read".into(), .. },
        ],
        ..
    };
    let scores = evaluator.evaluate(&case, &result).await.unwrap();
    let score = match &scores[0].value { ScoreValue::Numeric(v) => *v, _ => panic!() };
    assert!(score > 0.0 && score < 1.0);
}
```

**Test 3: Output match — contains check**
```rust
#[tokio::test]
async fn output_match_contains_passes() {
    let evaluator = OutputMatchEvaluator;
    let case = TestCase {
        expected_output: Some(ExpectedOutput {
            contains: vec!["deployed".into(), "staging".into()],
            ..Default::default()
        }),
        ..
    };
    let result = EvalResult {
        response: Some("App deployed to staging successfully".into()),
        ..
    };
    let scores = evaluator.evaluate(&case, &result).await.unwrap();
    assert_eq!(scores[0].value, ScoreValue::Numeric(1.0));
}
```

**Test 4: Output match — missing phrase**
```rust
#[tokio::test]
async fn output_match_missing_phrase_fails() {
    let case = TestCase {
        expected_output: Some(ExpectedOutput {
            contains: vec!["deployed".into(), "production".into()],
            ..Default::default()
        }),
        ..
    };
    let result = EvalResult {
        response: Some("App deployed to staging".into()), // "production" missing
        ..
    };
    let scores = evaluator.evaluate(&case, &result).await.unwrap();
    let score = match &scores[0].value { ScoreValue::Numeric(v) => *v, _ => panic!() };
    assert_eq!(score, 0.5); // 1/2 phrases matched
}
```

**Test 5: Threshold evaluator — cost over budget**
```rust
#[tokio::test]
async fn threshold_cost_over_budget_fails() {
    let evaluator = ThresholdEvaluator {
        max_cost_dollars: Some(0.01),
        ..Default::default()
    };
    let result = EvalResult {
        metrics: EvalMetrics { cost_dollars: 0.05, .. },
        ..
    };
    let scores = evaluator.evaluate(&TestCase::default(), &result).await.unwrap();
    assert_eq!(scores[0].value, ScoreValue::Boolean(false));
}
```

**Test 6: Score failure detection**
```rust
#[test]
fn score_is_failure_detects_low_numeric() {
    assert!(score_is_failure(&EvalScore {
        value: ScoreValue::Numeric(0.3),
        ..
    }));
    assert!(!score_is_failure(&EvalScore {
        value: ScoreValue::Numeric(0.8),
        ..
    }));
}
```

**Test 7: JSON reporter roundtrip**
```rust
#[tokio::test]
async fn json_reporter_writes_valid_json() {
    let temp = tempdir().unwrap();
    let out_path = temp.path().join("results.json");
    let reporter = JsonReporter { output_path: out_path.clone(), pretty: true };
    
    let results = vec![test_eval_result()];
    reporter.report(&test_suite(), &[test_config()], &results).await.unwrap();
    
    let content = fs::read_to_string(&out_path).await.unwrap();
    let parsed: Vec<EvalResult> = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed.len(), 1);
}
```

### Integration test

**Test 8: Full CLI end-to-end**
```bash
# Create test suite and config
cat > /tmp/test-suite.toml << 'EOF'
[suite]
name = "simple"
[[cases]]
name = "arithmetic"
input = "What is 2+2?"
[cases.expected_output]
contains = ["4"]
EOF

cat > /tmp/test-config.toml << 'EOF'
[agent]
name = "baseline"
[agent.permissions]
auto_approve_all = true
EOF

# Run
moa eval run --suite /tmp/test-suite.toml --config /tmp/test-config.toml --ci
echo "Exit code: $?"
```

---

## 10. Additional notes

- **LCS algorithm for trajectory matching.** Use the standard dynamic-programming LCS — it's O(n*m) where n and m are trajectory lengths. For agent trajectories (typically < 50 steps), this is instant.
- **Terminal reporter width.** Detect terminal width via `crossterm::terminal::size()` and adjust column widths accordingly. Fall back to 80 cols.
- **Score failure threshold.** The 0.5 threshold for `ScoreValue::Numeric` is a sensible default. Consider making it configurable per-evaluator or per-suite.
- **Future: LLM-as-judge evaluator.** A natural extension is an evaluator that calls an LLM to judge response quality against expected facts. This would use the `facts` field in `ExpectedOutput`. Leave this for a future step — it requires careful prompt engineering and adds cost per eval run.
- **Future: GitHub Actions integration.** The CI exit codes make it straightforward to add `moa eval run --ci` as a GitHub Actions step. The JSON reporter output can be parsed by a follow-up step to post results as PR comments.
- **Future: production trace → test case pipeline.** The Langfuse reporter opens the door to using Langfuse's dataset API to convert scored production traces into test cases. This closes the loop: production → observe → evaluate → test → improve.
