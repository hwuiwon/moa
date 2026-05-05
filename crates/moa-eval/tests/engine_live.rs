//! Live eval-engine integration coverage that exercises the real provider path.

use moa_core::MoaConfig;
use moa_eval::{AgentConfig, EngineOptions, EvalEngine, EvalStatus, TestCase};
use tempfile::tempdir;

#[tokio::test]
#[ignore = "requires provider API key env"]
async fn live_run_single_produces_eval_result() {
    let temp = tempdir().unwrap();
    let engine = EvalEngine::new(
        MoaConfig::default(),
        EngineOptions {
            temp_dir: temp.path().to_path_buf(),
            ..EngineOptions::default()
        },
    )
    .unwrap();

    let result = engine
        .run_single(
            &TestCase {
                name: "hello".to_string(),
                input: "Say hello in one short sentence.".to_string(),
                timeout_seconds: Some(30),
                ..TestCase::default()
            },
            &AgentConfig {
                name: "baseline".to_string(),
                ..AgentConfig::default()
            },
        )
        .await
        .unwrap();

    assert!(matches!(
        result.status,
        EvalStatus::Passed | EvalStatus::Failed
    ));
    assert!(result.response.is_some());
    assert!(result.metrics.total_tokens > 0);
}
