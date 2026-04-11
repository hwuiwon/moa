//! Terminal reporter for human-readable eval summaries.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tokio::io::AsyncWriteExt;

use crate::engine::EvalRun;
use crate::{
    AgentConfig, EvalError, EvalResult, EvalScore, EvalStatus, Reporter, Result, TestCase,
    TestSuite,
};

/// Renders a completed run to stdout.
pub struct TerminalReporter {
    /// Whether to include per-case score and metric detail.
    pub verbose: bool,
    /// Whether to use ANSI color in status labels.
    pub color: bool,
}

#[async_trait::async_trait]
impl Reporter for TerminalReporter {
    async fn report(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        run: &EvalRun,
    ) -> Result<()> {
        let rendered = self.render(suite, configs, run);
        let mut stdout = tokio::io::stdout();
        stdout
            .write_all(rendered.as_bytes())
            .await
            .map_err(|source| EvalError::Io {
                path: PathBuf::from("<stdout>"),
                source,
            })?;
        stdout.flush().await.map_err(|source| EvalError::Io {
            path: PathBuf::from("<stdout>"),
            source,
        })?;
        Ok(())
    }
}

impl TerminalReporter {
    fn render(&self, suite: &TestSuite, configs: &[AgentConfig], run: &EvalRun) -> String {
        let mut output = String::new();
        output.push_str(&format!(
            "Suite: {} | {} configs x {} cases = {} runs\n",
            suite.name,
            configs.len(),
            suite.cases.len(),
            run.results.len()
        ));
        if let Some(description) = &suite.description {
            output.push_str(&format!("{description}\n"));
        }
        output.push('\n');
        output.push_str(&self.render_status_matrix(suite, configs, run));

        if self.verbose || configs.len() > 1 {
            output.push('\n');
            output.push_str(&self.render_case_details(suite, configs, run));
        }

        output.push('\n');
        output.push_str(&format!(
            "Summary: passed={} failed={} errors={} timeouts={} tokens={} cost=${:.4} duration={}ms\n",
            run.summary.passed,
            run.summary.failed,
            run.summary.errors,
            run.summary.timeouts,
            run.summary.total_tokens,
            run.summary.total_cost_dollars,
            run.summary.total_duration_ms
        ));
        output
    }

    fn render_status_matrix(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        run: &EvalRun,
    ) -> String {
        let case_width = suite
            .cases
            .iter()
            .map(|case| case.name.len())
            .max()
            .unwrap_or(4)
            .max(4);
        let config_widths: Vec<usize> = configs
            .iter()
            .map(|config| config.name.len().max(8))
            .collect();

        let mut output = String::new();
        output.push_str(&format!("{:<case_width$}", "Case"));
        for (config, width) in configs.iter().zip(&config_widths) {
            output.push_str(&format!(" | {:<width$}", config.name, width = *width));
        }
        output.push('\n');
        output.push_str(&"-".repeat(case_width));
        for width in &config_widths {
            output.push_str("-+-");
            output.push_str(&"-".repeat(*width));
        }
        output.push('\n');

        let by_case_and_config = result_index(run);
        for case in &suite.cases {
            output.push_str(&format!(
                "{:<case_width$}",
                case.name,
                case_width = case_width
            ));
            for (config, width) in configs.iter().zip(&config_widths) {
                let status = by_case_and_config
                    .get(&(case.name.clone(), config.name.clone()))
                    .map(|result| self.status_label(result.status.clone()))
                    .unwrap_or_else(|| "-".to_string());
                output.push_str(&format!(" | {:<width$}", status, width = *width));
            }
            output.push('\n');
        }

        output
    }

    fn render_case_details(
        &self,
        suite: &TestSuite,
        configs: &[AgentConfig],
        run: &EvalRun,
    ) -> String {
        let mut output = String::new();
        let by_case_and_config = result_index(run);

        for case in &suite.cases {
            output.push_str(&format!("Case: {}\n", case.name));
            for config in configs {
                if let Some(result) =
                    by_case_and_config.get(&(case.name.clone(), config.name.clone()))
                {
                    output.push_str(&format!(
                        "  {:<18} {:<10} cost=${:.4} latency={}ms tokens={} turns={} tools={}\n",
                        config.name,
                        self.status_label(result.status.clone()),
                        result.metrics.cost_dollars,
                        result.metrics.latency_ms,
                        result.metrics.total_tokens,
                        result.metrics.turn_count,
                        result.metrics.tool_call_count
                    ));
                    let scores = format_scores(&result.scores);
                    if !scores.is_empty() {
                        output.push_str(&format!("    scores: {scores}\n"));
                    }
                    if self.verbose {
                        render_verbose_case(&mut output, case, result);
                    }
                }
            }
        }

        output
    }

    fn status_label(&self, status: EvalStatus) -> String {
        let label = match status {
            EvalStatus::Passed => "PASS",
            EvalStatus::Failed => "FAIL",
            EvalStatus::Error => "ERROR",
            EvalStatus::Timeout => "TIMEOUT",
            EvalStatus::Skipped => "SKIPPED",
        };

        if !self.color {
            return label.to_string();
        }

        let color_code = match status {
            EvalStatus::Passed => "32",
            EvalStatus::Failed | EvalStatus::Error | EvalStatus::Timeout => "31",
            EvalStatus::Skipped => "33",
        };
        format!("\u{1b}[{color_code}m{label}\u{1b}[0m")
    }
}

fn result_index(run: &EvalRun) -> BTreeMap<(String, String), &EvalResult> {
    run.results
        .iter()
        .map(|result| {
            (
                (result.test_case.clone(), result.agent_config.clone()),
                result,
            )
        })
        .collect()
}

fn format_scores(scores: &[EvalScore]) -> String {
    scores
        .iter()
        .map(|score| {
            let value = match &score.value {
                crate::ScoreValue::Numeric(value) => format!("{value:.2}"),
                crate::ScoreValue::Boolean(value) => value.to_string(),
                crate::ScoreValue::Categorical(value) => value.clone(),
            };
            format!("{}={value}", score.name)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_verbose_case(output: &mut String, case: &TestCase, result: &EvalResult) {
    if let Some(response) = &result.response
        && !response.is_empty()
    {
        output.push_str(&format!("    response: {}\n", truncate(response, 240)));
    }
    if let Some(expected) = &case.expected_trajectory
        && !expected.is_empty()
    {
        output.push_str(&format!(
            "    expected trajectory: {}\n",
            expected.join(" -> ")
        ));
    }
    if !result.trajectory.is_empty() {
        let actual = result
            .trajectory
            .iter()
            .map(|step| step.tool_name.clone())
            .collect::<Vec<_>>()
            .join(" -> ");
        output.push_str(&format!("    actual trajectory: {actual}\n"));
    }
    if let Some(error) = &result.error {
        output.push_str(&format!("    error: {}\n", truncate(error, 240)));
    }
    for score in &result.scores {
        if let Some(comment) = &score.comment
            && !comment.is_empty()
        {
            output.push_str(&format!("    {}: {}\n", score.name, truncate(comment, 240)));
        }
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::TerminalReporter;
    use crate::{
        AgentConfig, EvalMetrics, EvalResult, EvalRun, EvalScore, EvalStatus, ScoreValue, TestCase,
        TestSuite,
    };

    #[test]
    fn render_includes_case_names_and_summary() {
        let reporter = TerminalReporter {
            verbose: true,
            color: false,
        };
        let suite = TestSuite {
            name: "demo".to_string(),
            cases: vec![TestCase {
                name: "arithmetic".to_string(),
                input: "2+2".to_string(),
                ..TestCase::default()
            }],
            ..TestSuite::default()
        };
        let configs = vec![AgentConfig {
            name: "baseline".to_string(),
            ..AgentConfig::default()
        }];
        let run = EvalRun {
            suite_name: suite.name.clone(),
            started_at: chrono::Utc::now(),
            completed_at: chrono::Utc::now(),
            results: vec![EvalResult {
                test_case: "arithmetic".to_string(),
                agent_config: "baseline".to_string(),
                status: EvalStatus::Passed,
                response: Some("4".to_string()),
                scores: vec![EvalScore {
                    evaluator: "output_match".to_string(),
                    name: "output_match".to_string(),
                    value: ScoreValue::Numeric(1.0),
                    comment: None,
                }],
                metrics: EvalMetrics {
                    total_tokens: 12,
                    ..EvalMetrics::default()
                },
                ..EvalResult::default()
            }],
            summary: crate::RunSummary {
                total_cases: 1,
                passed: 1,
                ..crate::RunSummary::default()
            },
        };

        let rendered = reporter.render(&suite, &configs, &run);
        assert!(rendered.contains("arithmetic"));
        assert!(rendered.contains("Summary:"));
    }
}
