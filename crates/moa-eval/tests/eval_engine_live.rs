// No offline counterpart possible because: this smoke test verifies the eval engine's real configured provider path, while deterministic engine behavior is already covered by non-live eval tests.

//! Live eval-engine integration coverage that exercises the real provider path.
#![recursion_limit = "256"]

use std::time::Duration;

use moa_core::MoaConfig;
use moa_eval::EvalEngine;
use moa_eval_core::{AgentConfig, EngineOptions, EvalStatus, TestCase, ToolOverride};
use moa_test_support::postgres::test_database_url;
use tempfile::tempdir;

fn live_model() -> Option<&'static str> {
    if std::env::var("MOA_ANTHROPIC_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return Some("claude-sonnet-4-6");
    }
    if std::env::var("MOA_OPENAI_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return Some("gpt-5.4-mini");
    }
    if std::env::var("MOA_GOOGLE_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        return Some("gemini-3-flash-preview");
    }
    None
}

fn live_provider_tests_enabled() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    std::env::var("MOA_RUN_LIVE_PROVIDER_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_PROVIDER_TESTS=1 and provider API key env"]
async fn live_eval_engine_runs_single_case() {
    if !live_provider_tests_enabled() {
        return;
    }
    let model = live_model().expect(
        "MOA_RUN_LIVE_PROVIDER_TESTS=1 requires MOA_ANTHROPIC_API_KEY, MOA_OPENAI_API_KEY, or MOA_GOOGLE_API_KEY",
    );

    let temp = tempdir().unwrap();
    let mut config = MoaConfig::load().expect("live eval config should load from env");
    config.database.url = test_database_url();
    let engine = EvalEngine::new(
        config,
        EngineOptions {
            temp_dir: temp.path().to_path_buf(),
            ..EngineOptions::default()
        },
    )
    .unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(90),
        engine.run_single(
            &TestCase {
                name: "hello".to_string(),
                input: "Say hello in one short sentence.".to_string(),
                timeout_seconds: Some(45),
                ..TestCase::default()
            },
            &AgentConfig {
                name: "baseline".to_string(),
                model: Some(model.to_string()),
                tools: ToolOverride {
                    enabled: Some(Vec::new()),
                    ..ToolOverride::default()
                },
                ..AgentConfig::default()
            },
        ),
    )
    .await
    .expect("live eval smoke should not hang past the outer timeout")
    .unwrap();

    assert!(matches!(
        result.status,
        EvalStatus::Passed | EvalStatus::Failed
    ));
    assert!(result.response.is_some());
    assert!(result.metrics.total_tokens > 0);
}
